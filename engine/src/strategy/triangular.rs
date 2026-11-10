use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use std::collections::HashSet;
use tracing::{debug, warn};

use crate::config::RouterType;
use crate::pool_cache::{PoolCache, V2PoolKey, V3PoolKey, volatile_max_age, stable_max_age};
use crate::types::{PairScanInfo, TriangularOpportunity};
use crate::util::addr_key;
use super::{Strategy, V3LegQuote, apply_exec_quote_haircut, token_amount_to_usd, parse_amount_capped, min_v3_liquidity};

/// Max QuoterV2 round trips when verifying V3-leg triangular routes. Round 1 brackets
/// every V3 leg (see [`verify_bracket_bps()`]), so later rounds are only needed when a real
/// input lands well outside its bracket.
fn verify_max_rounds() -> usize { crate::config::tuning().tri_verify_max_rounds }

/// A V3 leg whose input depends on earlier legs is quoted at two points in round 1: its
/// upper-bound input `s` and `s × (1 − this)`. The real input normally falls between them
/// (earlier legs' impact + tick crossings, ~30 bps on Base), where the chord between the
/// two quotes is a tight lower bound — so one round trip usually suffices.
pub(super) fn verify_bracket_bps() -> u64 { crate::config::tuning().tri_verify_bracket_bps }

/// Outside its quoted points, a V3 leg's real input is bounded by the nearest point
/// (chord through the origin below it, flat output above it). Accepted when within this
/// many bps of that point; otherwise the leg is re-quoted at its real input next round.
/// In the last round the bound is used regardless — every bound is conservative.
fn verify_max_gap_bps() -> u64 { crate::config::tuning().tri_verify_max_gap_bps }

/// How much one V3 leg's cached spot quote overstates output (tick crossings aren't
/// modelled) — historical Base data: ~0.30% on a 270 USDT trade. In optimistic mode, where
/// V3-leg routes aren't QuoterV2-verified, this is deducted once per V3 leg.
fn v3_spot_overestimate_bps() -> u64 { crate::config::tuning().v3_spot_overestimate_bps }

/// The local search yields to the runtime every this many triplets / route entries: it is
/// CPU-only, and runs joined with the 2-hop evaluation, whose QuoterV2 replies would
/// otherwise wait for it to finish.
const TRI_YIELD_EVERY: usize = 64;

/// Memo key of a triangle route: its triplet, routers and fees (fixes its pools).
fn tri_route_key(o: &TriangularOpportunity) -> String {
    format!(
        "{}|{}/{}/{}|{}/{}/{}",
        o.triplet_id, o.router_ab_id, o.router_bc_id, o.router_ca_id, o.fee_ab, o.fee_bc, o.fee_ca,
    )
}

/// Outcome of chaining a route through its legs during V3 verification.
enum RouteWalk {
    /// All legs resolved: final amount of token A.
    Done(U256),
    /// This V3 leg must be re-quoted at this input before the route can be resolved.
    Requote(usize, U256),
    /// A leg failed or has no liquidity (reason for the debug log).
    Dead(&'static str),
}

/// Lower bound on a V3 leg's output at `amt` from QuoterV2 points `(input, output)` sorted
/// by input, using that output is increasing and concave in input (zero at zero).
/// Returns the bound and whether it is tight (exact, inside the points, or within
/// [`verify_max_gap_bps()`] of the nearest point).
pub(super) fn v3_leg_lower_bound(points: &[(U256, U256)], amt: U256) -> Option<(U256, bool)> {
    let gap_ok = |near: U256, far: U256| {
        near.abs_diff(far).saturating_mul(U256::from(10_000u32))
            <= near.saturating_mul(U256::from(verify_max_gap_bps()))
    };
    let &(lo_in, lo_out) = points.first()?;
    let &(hi_in, hi_out) = points.last()?;
    if let Some(&(_, out)) = points.iter().find(|(i, _)| *i == amt) {
        return Some((out, true));
    }
    if amt < lo_in {
        // Chord from the origin to the lowest point lies below the curve.
        return Some((lo_out * amt / lo_in, gap_ok(lo_in, amt)));
    }
    if amt > hi_in {
        // Output is increasing, so output at the highest point is a lower bound.
        return Some((hi_out, gap_ok(amt, hi_in)));
    }
    // Between two points: the chord between them lies below the concave curve.
    let w = points.windows(2).find(|w| w[0].0 < amt && amt < w[1].0)?;
    let ((a_in, a_out), (b_in, b_out)) = (w[0], w[1]);
    let rise = b_out.saturating_sub(a_out) * (amt - a_in) / (b_in - a_in);
    Some((a_out + rise, true))
}

impl Strategy {
    /// Detect triangular arbitrage opportunities (A → B → C → A loops).
    ///
    /// Uses 3-phase chained multicall:
    ///   Phase 1: Quote A→B with the real `amount_in` for each (triplet × router_ab)
    ///   Phase 2: Quote B→C with the *actual* amount_b from Phase 1
    ///   Phase 3: Quote C→A with the *actual* amount_c from Phase 2
    ///
    /// This eliminates the phantom-profit bug caused by quoting legs with estimated
    /// amounts that don't reflect real exchange rates (especially USDC↔WETH scale).
    ///
    /// Returns `(opportunities, best_raw_profit_usd)` — best_raw_profit_usd tracks
    /// the highest profit seen even below `min_profit_usd` (for status logging).
    /// `pair_filter`: if Some, only consider triplets with a leg on one of the given token
    /// pairs (unordered, see [`super::unordered_pair`]) — the swapped pools of a targeted
    /// scan; other triangles' prices didn't move. Pass None for a full sweep.
    pub async fn detect_triangular<P: Provider + Clone>(
        &self,
        provider: &P,
        max_opportunities: usize,
        pair_filter: Option<&[(Address, Address)]>,
        disabled_triplets: &std::collections::HashSet<String>,
    ) -> (Vec<TriangularOpportunity>, f64) {
        let local_start = std::time::Instant::now();
        self.timings.tri_local_ms.store(0, std::sync::atomic::Ordering::Relaxed);
        self.timings.tri_verify_ms.store(0, std::sync::atomic::Ordering::Relaxed);
        self.timings.tri_verify_rounds.store(0, std::sync::atomic::Ordering::Relaxed);
        use std::collections::HashMap;

        // ── Build token info from all pairs on this chain ─────────────────────────
        let chain_pair_count = self.pairs.len();
        // addr_lower → (symbol, decimals)
        let mut token_info: HashMap<String, (String, u8)> = HashMap::with_capacity(chain_pair_count * 2);
        // addr_lower → trade amount (from the first pair that starts with this token)
        let mut token_amounts: HashMap<String, U256> = HashMap::with_capacity(chain_pair_count);

        for p in self.pairs.iter() {
            let Ok(addr_in) = p.token_in.parse::<Address>() else { continue };
            let Ok(addr_out) = p.token_out.parse::<Address>() else { continue };
            let key_in = addr_key(addr_in);
            let key_out = addr_key(addr_out);
            token_info.entry(key_in.clone())
                .or_insert_with(|| (p.token_in_symbol.clone(), p.token_in_decimals));
            token_info.entry(key_out.clone())
                .or_insert_with(|| (p.token_out_symbol.clone(), p.token_out_decimals));
            if !token_amounts.contains_key(&key_in) {
                if let Some(amt) = parse_amount_capped(&p.trade_amount, p.max_trade.as_deref(), p.token_in_decimals) {
                    token_amounts.insert(key_in, amt);
                }
            }
        }

        let tokens: Vec<(Address, String)> = token_info.iter()
            .filter_map(|(k, _)| k.parse::<Address>().ok().map(|a| (a, k.clone())))
            .collect();

        if tokens.len() < 3 {
            return (vec![], 0.0);
        }

        if self.chain_router_indices.is_empty() {
            return (vec![], 0.0);
        }

        // ── Build all ordered triplets ────────────────────────────────────────────
        // Start tokens × N × (N-1) ordered triplets (N ≤ max_tokens).
        struct Triplet {
            token_a: Address,
            key_a: String,
            token_b: Address,
            key_b: String,
            token_c: Address,
            key_c: String,
            amount_in: U256,
            triplet_id: String,
        }

        let filter_set: Option<HashSet<(Address, Address)>> = pair_filter
            .map(|f| f.iter().copied().collect());

        let max_tokens = 25usize;
        let mut triplets: Vec<Triplet> = Vec::new();

        // Start tokens are the ones with a trade size (USDC/USDT/WETH): filter BEFORE the cap,
        // or the HashMap's random order could cap a start token out of the scan.
        for &(token_a, ref key_a) in tokens.iter().filter(|(_, k)| token_amounts.contains_key(k)).take(max_tokens) {
            let Some(&amount_in) = token_amounts.get(key_a) else { continue };
            let Some((sym_a, _)) = token_info.get(key_a) else { continue };

            for &(token_b, ref key_b) in &tokens {
                if token_b == token_a { continue; }
                let Some((sym_b, _)) = token_info.get(key_b) else { continue };

                for &(token_c, ref key_c) in &tokens {
                    if token_c == token_a || token_c == token_b { continue; }
                    if let Some(ref fs) = filter_set {
                        let touches = |x, y| fs.contains(&super::unordered_pair(x, y));
                        if !touches(token_a, token_b) && !touches(token_b, token_c) && !touches(token_c, token_a) {
                            continue;
                        }
                    }
                    let Some((sym_c, _)) = token_info.get(key_c) else { continue };

                    triplets.push(Triplet {
                        token_a,
                        key_a: key_a.clone(),
                        token_b,
                        key_b: key_b.clone(),
                        token_c,
                        key_c: key_c.clone(),
                        amount_in,
                        triplet_id: format!("{}→{}→{}", sym_a, sym_b, sym_c),
                    });
                }
            }
        }

        // Compute disabled directed token pairs from config — used both to filter the scan
        // and to mark blocked triplets in tri_scan (so they stay visible in the dashboard).
        let disabled_legs = self.get_or_build_disabled_legs(disabled_triplets);

        // Helper: is this triplet blocked by any disabled rule?
        let is_triplet_disabled = |t: &Triplet| -> bool {
            if disabled_triplets.contains(&t.triplet_id) { return true; }
            if disabled_legs.contains(&(t.token_a, t.token_b)) { return true; }
            if disabled_legs.contains(&(t.token_b, t.token_c)) { return true; }
            if disabled_legs.contains(&(t.token_c, t.token_a)) { return true; }
            false
        };

        // Only save all_displayable / disabled_triplet_ids on full scans — they're only
        // needed for tri_scan dashboard updates, which we skip on targeted swap scans.
        // Targeted scans (pair_filter is Some) evaluate only a few triplets, so rebuilding
        // the full tri_scan snapshot would mark all others as "not quoted" — corrupting data.
        let is_full_scan = pair_filter.is_none();
        let all_displayable: Vec<(String, Address, Address, Address)> = if is_full_scan {
            triplets.iter()
                .map(|t| (t.triplet_id.clone(), t.token_a, t.token_b, t.token_c))
                .collect()
        } else {
            vec![]
        };
        let disabled_triplet_ids: std::collections::HashSet<String> = if is_full_scan {
            triplets.iter()
                .filter(|t| is_triplet_disabled(t))
                .map(|t| t.triplet_id.clone())
                .collect()
        } else {
            std::collections::HashSet::new()
        };

        // Remove disabled entries from the scan (they stay in all_displayable for the UI).
        if !disabled_triplets.is_empty() {
            triplets.retain(|t| !is_triplet_disabled(t));
        }

        if is_full_scan && triplets.is_empty() && all_displayable.iter().all(|(id, ..)| disabled_triplet_ids.contains(id)) {
            // Nothing to scan; still need to update tri_scan with disabled markers.
            let opp_counts = self.opp_session_counts.lock().unwrap();
            let mut tri = self.tri_scan.lock().unwrap();
            tri.clear();
            for (triplet_id, ..) in &all_displayable {
                let display_name = format!("▲ {}", triplet_id);
                tri.push(PairScanInfo {
                    pair_id: triplet_id.clone(),
                    chain_id: self.chain_id,
                    display_name,
                    dex_count: 0,
                    dex_ids: vec![],
                    cross_count: 0,
                    was_quoted: false,
                    opp_count: *opp_counts.get(triplet_id).unwrap_or(&0),
                    disabled: true,
                });
            }
            tri.dedup_by(|a, b| a.pair_id == b.pair_id);
            drop(opp_counts);
            return (vec![], 0.0);
        }

        if triplets.is_empty() {
            return (vec![], 0.0);
        }

        // ── Phase 1: A→B ─────────────────────────────────────────────────────────
        // All DEX types use local pool_cache (zero eth_call).

        struct P1Entry {
            triplet_idx: usize,
            router_id: String,
            router_addr: Address,
            router_type: RouterType,
            fee: u32,
            /// Pre-computed A→B amount from local xy=k cache. None = needs multicall result.
            local_result: Option<U256>,
            /// Capital deployed for leg A→B: always the full trip.amount_in. (A V3 spot
            /// quote caps its input, so its output can understate a full-size leg — the
            /// route is scored conservatively, never inflated; QuoterV2 verifies V3 legs.)
            effective_amount_in: U256,
        }

        let mut p1_entries: Vec<P1Entry> =
            Vec::with_capacity(triplets.len().saturating_mul(self.chain_router_indices.len()));
        let mut tri_v3_spot: usize = 0;
        let default_v3_tiers: &[u32] = &[500, 3000, 10000];
        // Per router (indexed like self.routers): its pool-key id, shared instead of
        // allocated per lookup, and its address.
        let router_keys: Vec<std::sync::Arc<str>> =
            self.routers.iter().map(|r| std::sync::Arc::from(r.id.as_str())).collect();
        let router_addrs: Vec<Address> = self.routers.iter()
            .map(|r| self.router_addr_map.get(&r.id).copied().unwrap_or_default())
            .collect();
        let tiers_of = |ri: usize| -> &[u32] {
            let router = &self.routers[ri];
            if router.fee_tiers.is_empty() { default_v3_tiers } else { &router.fee_tiers }
        };

        // A→B depends only on (A, B) — the input is A's trade size — so each pair's legs
        // (router, fee, output) are computed once and shared by every C.
        let mut ab_legs: HashMap<(Address, Address), Vec<(usize, u32, U256)>> = HashMap::new();
        for (ti, trip) in triplets.iter().enumerate() {
            if ti % TRI_YIELD_EVERY == TRI_YIELD_EVERY - 1 { tokio::task::yield_now().await; }
            let legs = ab_legs.entry((trip.token_a, trip.token_b)).or_insert_with(|| {
                self.chain_router_indices.iter().filter_map(|&ri| {
                    let router = &self.routers[ri];
                    match router.router_type {
                        // `out` may be for a capped input (see quote_v3_spot_key); the route
                        // still trades the full amount_in, so this only under-scores it.
                        RouterType::V3 => best_v3_spot(
                            &self.pool_cache, &router_keys[ri], trip.token_a, trip.token_b, trip.amount_in, tiers_of(ri),
                        ),
                        _ => self.best_local_leg(&router.id, router.router_type, trip.token_a, trip.token_b, trip.amount_in),
                    }.map(|(out, fee)| (ri, fee, out))
                }).collect()
            });
            for &(ri, fee, out) in legs.iter() {
                let router = &self.routers[ri];
                if router.router_type == RouterType::V3 { tri_v3_spot += 1; }
                p1_entries.push(P1Entry {
                    triplet_idx: ti,
                    router_id: router.id.clone(),
                    router_addr: router_addrs[ri],
                    router_type: router.router_type,
                    fee,
                    local_result: Some(out),
                    effective_amount_in: trip.amount_in,
                });
            }
        }

        // ── Phase 2: B→C with actual amount_b ────────────────────────────────────

        struct P2Entry {
            triplet_idx: usize,
            router_ab_id: String, router_ab_addr: Address,
            router_ab_type: RouterType, fee_ab: u32,
            router_bc_id: String, router_bc_addr: Address,
            router_bc_type: RouterType, fee_bc: u32,
            local_result: Option<U256>,
            /// Carried from P1: actual capital deployed for the initial A→B leg.
            effective_amount_in: U256,
        }

        let mut p2_entries: Vec<P2Entry> =
            Vec::with_capacity(p1_entries.len().saturating_mul(self.chain_router_indices.len()));

        for (i, p1e) in p1_entries.iter().enumerate() {
            if i % TRI_YIELD_EVERY == TRI_YIELD_EVERY - 1 { tokio::task::yield_now().await; }
            let amount_b: U256 = match p1e.local_result.filter(|b| !b.is_zero()) {
                Some(b) => b,
                None => continue,
            };
            let trip = &triplets[p1e.triplet_idx];

            for &ri in &self.chain_router_indices {
                let router = &self.routers[ri];
                let router_addr = router_addrs[ri];
                let tiers = tiers_of(ri);
                let (fee, local_bc): (u32, Option<U256>) = match router.router_type {
                    RouterType::V3 => {
                        let result = best_v3_spot(
                            &self.pool_cache,
                            &router_keys[ri],
                            trip.token_b,
                            trip.token_c,
                            amount_b,
                            tiers,
                        );
                        match result {
                            Some((out, f)) => (f, Some(out)),
                            None => continue,
                        }
                    }
                    _ => {
                        let best = self.best_local_leg(
                            &router.id, router.router_type, trip.token_b, trip.token_c, amount_b,
                        );
                        (best.map_or(0, |(_, f)| f), best.map(|(out, _)| out))
                    }
                };
                if local_bc.is_none() { continue; }
                if router.router_type == RouterType::V3 { tri_v3_spot += 1; }
                p2_entries.push(P2Entry {
                    triplet_idx: p1e.triplet_idx,
                    router_ab_id: p1e.router_id.clone(),
                    router_ab_addr: p1e.router_addr,
                    router_ab_type: p1e.router_type,
                    fee_ab: p1e.fee,
                    router_bc_id: router.id.clone(),
                    router_bc_addr: router_addr,
                    router_bc_type: router.router_type,
                    fee_bc: fee,
                    local_result: local_bc,
                    effective_amount_in: p1e.effective_amount_in,
                });
            }
        }

        // ── Phase 3: C→A with actual amount_c ────────────────────────────────────

        struct P3Entry {
            triplet_idx: usize,
            router_ab_id: String, router_ab_addr: Address,
            router_ab_type: RouterType, fee_ab: u32,
            router_bc_id: String, router_bc_addr: Address,
            router_bc_type: RouterType, fee_bc: u32,
            router_ca_id: String, router_ca_addr: Address,
            router_ca_type: RouterType, fee_ca: u32,
            local_result: Option<U256>,
            /// Carried from P1: actual capital deployed for the initial A→B leg.
            effective_amount_in: U256,
        }

        let mut p3_entries: Vec<P3Entry> =
            Vec::with_capacity(p2_entries.len().saturating_mul(self.chain_router_indices.len()));

        for (i, p2e) in p2_entries.iter().enumerate() {
            if i % TRI_YIELD_EVERY == TRI_YIELD_EVERY - 1 { tokio::task::yield_now().await; }
            let amount_c: U256 = match p2e.local_result.filter(|c| !c.is_zero()) {
                Some(c) => c,
                None => continue,
            };
            let trip = &triplets[p2e.triplet_idx];

            for &ri in &self.chain_router_indices {
                let router = &self.routers[ri];
                let router_addr = router_addrs[ri];
                let tiers = tiers_of(ri);
                let (fee, local_ca): (u32, Option<U256>) = match router.router_type {
                    RouterType::V3 => {
                        let result = best_v3_spot(
                            &self.pool_cache,
                            &router_keys[ri],
                            trip.token_c,
                            trip.token_a,
                            amount_c,
                            tiers,
                        );
                        match result {
                            Some((out, f)) => (f, Some(out)),
                            None => continue,
                        }
                    }
                    _ => {
                        let best = self.best_local_leg(
                            &router.id, router.router_type, trip.token_c, trip.token_a, amount_c,
                        );
                        (best.map_or(0, |(_, f)| f), best.map(|(out, _)| out))
                    }
                };
                if local_ca.is_none() { continue; }
                if router.router_type == RouterType::V3 { tri_v3_spot += 1; }
                p3_entries.push(P3Entry {
                    triplet_idx: p2e.triplet_idx,
                    router_ab_id: p2e.router_ab_id.clone(),
                    router_ab_addr: p2e.router_ab_addr,
                    router_ab_type: p2e.router_ab_type,
                    fee_ab: p2e.fee_ab,
                    router_bc_id: p2e.router_bc_id.clone(),
                    router_bc_addr: p2e.router_bc_addr,
                    router_bc_type: p2e.router_bc_type,
                    fee_bc: p2e.fee_bc,
                    router_ca_id: router.id.clone(),
                    router_ca_addr: router_addr,
                    router_ca_type: router.router_type,
                    fee_ca: fee,
                    local_result: local_ca,
                    effective_amount_in: p2e.effective_amount_in,
                });
            }
        }

        // ── Calculate profits ─────────────────────────────────────────────────────

        debug!(
            "[{}] triangular scan: {} V3 spot, {} triplets, P1={} P2={} P3={} routes",
            self.chain_id, tri_v3_spot, triplets.len(),
            p1_entries.len(), p2_entries.len(), p3_entries.len()
        );

        let mut opportunities: Vec<TriangularOpportunity> = Vec::new();
        let mut best_raw_usd: f64 = 0.0;
        let mut best_tri_spread: f64 = f64::NEG_INFINITY;

        for (i, p3e) in p3_entries.iter().enumerate() {
            if i % TRI_YIELD_EVERY == TRI_YIELD_EVERY - 1 { tokio::task::yield_now().await; }
            let amount_a_final: U256 = match p3e.local_result {
                Some(a) if !a.is_zero() => a,
                _ => continue,
            };
            let trip = &triplets[p3e.triplet_idx];
            // Re-size the route to its profit peak (closed form on the cached curves, at most
            // the configured size): kept only if it earns more than the configured size.
            let legs = [
                (p3e.router_ab_id.as_str(), p3e.router_ab_type, p3e.fee_ab, trip.token_a, trip.token_b),
                (p3e.router_bc_id.as_str(), p3e.router_bc_type, p3e.fee_bc, trip.token_b, trip.token_c),
                (p3e.router_ca_id.as_str(), p3e.router_ca_type, p3e.fee_ca, trip.token_c, trip.token_a),
            ];
            let (effective_amount_in, amount_a_final) = match self.size_route(&legs, p3e.effective_amount_in) {
                Some((x, back)) if back.saturating_add(p3e.effective_amount_in) > amount_a_final.saturating_add(x) => (x, back),
                _ => (p3e.effective_amount_in, amount_a_final),
            };
            let amount_a_final = apply_exec_quote_haircut(amount_a_final);
            let v3_legs = v3_leg_count(p3e.router_ab_type, p3e.router_bc_type, p3e.router_ca_type);
            let v3_route = v3_legs > 0;
            // Optimistic mode submits V3-leg routes unverified: deduct the known V3 spot
            // overestimate (per V3 leg — each leg overstates independently) BEFORE the
            // spread diagnostic, the 5% cap and the floor, so all see a realistic output.
            let amount_a_final = if self.optimistic_submission && v3_route {
                let overestimate = effective_amount_in
                    * U256::from(v3_spot_overestimate_bps() * v3_legs) / U256::from(10_000u32);
                amount_a_final.saturating_sub(overestimate)
            } else {
                amount_a_final
            };

            // Track best triangular spread (even negative) for diagnostics
            let tri_spread = if !effective_amount_in.is_zero() {
                let final_f = amount_a_final.to::<u128>() as f64;
                let input_f = effective_amount_in.to::<u128>() as f64;
                (final_f / input_f) - 1.0
            } else { 0.0 };

            // Sanity cap: profit > 5% of input is almost certainly a detection artifact.
            // Catches Solidly-echo phantoms and V3 spot mismatch phantoms (including
            // medium-range $37-$121 phantoms that passed the old 3× absolute cap), and
            // narrow-range CL pools far off the market (a tick-spacing-1 USDC/AAVE pool 26%
            // off price logged 2.35M of these in 5 h). Kept out of `tri=`, logged once per
            // route per 10 min.
            if amount_a_final > effective_amount_in
                && crate::config::tuning().is_phantom(amount_a_final - effective_amount_in, effective_amount_in)
            {
                let route = format!("{}|{}/{}/{}", trip.triplet_id, p3e.router_ab_id, p3e.router_bc_id, p3e.router_ca_id);
                if let Some(held) = crate::util::throttle(&route, crate::util::WARN_EVERY) {
                    let pools = crate::race::route_pools(&self.pool_cache, &[
                        (&p3e.router_ab_id, p3e.router_ab_type, p3e.fee_ab, trip.token_a, trip.token_b),
                        (&p3e.router_bc_id, p3e.router_bc_type, p3e.fee_bc, trip.token_b, trip.token_c),
                        (&p3e.router_ca_id, p3e.router_ca_type, p3e.fee_ca, trip.token_c, trip.token_a),
                    ]);
                    warn!(
                        "[{}] Triangular phantom skipped: {} | profit/input > 5% | {:+.1}% | {}/{}/{} | pools {:?}{}",
                        self.chain_id, trip.triplet_id, tri_spread * 100.0,
                        p3e.router_ab_id, p3e.router_bc_id, p3e.router_ca_id, pools, crate::util::held_suffix(held),
                    );
                }
                continue;
            }
            if tri_spread > best_tri_spread { best_tri_spread = tri_spread; }

            // Compare against effective_amount_in (may be < trip.amount_in if V3 leg was capped)
            if amount_a_final <= effective_amount_in { continue; }

            let profit = amount_a_final - effective_amount_in;

            // Use correct decimals for profit-to-USD conversion (token_a may be USDC, WETH, etc.)
            let (sym_a, dec_a) = match token_info.get(&trip.key_a) {
                Some(info) => info, None => continue,
            };
            let profit_usd = token_amount_to_usd(profit, *dec_a, sym_a, self.native_price_usd);

            // Track best profit seen regardless of threshold (for status logging).
            // V3-leg routes are verified below, and count only once verified — their spot
            // profit can be phantom. In optimistic mode they're submitted on (overestimate-
            // adjusted) spot, so count that.
            let verified_later = !self.optimistic_submission && v3_route;
            if !verified_later && profit_usd > best_raw_usd {
                best_raw_usd = profit_usd;
            }

            if profit_usd < self.min_triangular_profit_usd { continue; }

            debug!(
                "[{}] Triangular: {} | profit=${:.4} | {}/{}/{}",
                self.chain_id, trip.triplet_id, profit_usd,
                p3e.router_ab_id, p3e.router_bc_id, p3e.router_ca_id,
            );

            opportunities.push(TriangularOpportunity {
                chain_id: self.chain_id,
                triplet_id: trip.triplet_id.clone(),
                token_a: trip.token_a,
                token_b: trip.token_b,
                token_c: trip.token_c,
                amount_in: effective_amount_in,
                router_ab: p3e.router_ab_addr,
                router_bc: p3e.router_bc_addr,
                router_ca: p3e.router_ca_addr,
                router_ab_type: p3e.router_ab_type,
                router_bc_type: p3e.router_bc_type,
                router_ca_type: p3e.router_ca_type,
                fee_ab: p3e.fee_ab,
                fee_bc: p3e.fee_bc,
                fee_ca: p3e.fee_ca,
                expected_profit: profit,
                profit_usd,
                router_ab_id: p3e.router_ab_id.clone(),
                router_bc_id: p3e.router_bc_id.clone(),
                router_ca_id: p3e.router_ca_id.clone(),
            });
        }

        // Rank by USD, not raw expected_profit: start tokens differ in decimals, so raw
        // units would rank a $0.30 WETH-start route above a $5 USDC-start route.
        opportunities.sort_by(|a, b| b.profit_usd.total_cmp(&a.profit_usd));
        // One route per path (triplet + 3 routers), keeping the most profitable fee-tier
        // variant: they compete for the same liquidity, so submitting several is a double
        // send. Done BEFORE verification so variants don't use up its 2 × N budget.
        // HashSet, not dedup_by — after sorting by profit, variants aren't adjacent.
        let mut seen_paths: HashSet<(String, String, String, String)> = HashSet::new();
        opportunities.retain(|o| seen_paths.insert((
            o.triplet_id.clone(), o.router_ab_id.clone(), o.router_bc_id.clone(), o.router_ca_id.clone(),
        )));

        self.timings.tri_local_ms.store(local_start.elapsed().as_millis() as u64, std::sync::atomic::Ordering::Relaxed);

        // ── Revert plan 1.4: verify routes with a V3 leg before submit ───────────
        // Cached sqrtPriceX96 ignores tick crossings, so V3 legs overstate output
        // (~0.30% on a 270 USDT trade) by far more than the 0.5 bp haircut recovers.
        // Runs before the truncate so routes dropped here are backfilled by lower-ranked ones.
        // Optimistic mode skips QuoterV2 (like 2-hop): V3-leg routes go out on spot quotes,
        // warned once at Strategy construction.
        if !self.optimistic_submission && opportunities.iter().any(has_v3_leg) {
            let (mut v3_routes, mut rest): (Vec<_>, Vec<_>) =
                opportunities.into_iter().partition(has_v3_leg);
            // Already verified unprofitable and none of their pools has logged since: the
            // same state gives the same quotes, so don't spend QuoterV2 calls on them again.
            let memo_skips = self.drop_memoized_routes(&mut v3_routes);
            self.tri_cal.note_memo_skips(memo_skips);
            // Bounded batch: ≤ 2 × max routes × 3 legs QuoterV2 calls in one multicall round.
            v3_routes.truncate(max_opportunities.saturating_mul(2));
            // The candidates' quiet V2/Aerodrome pools are read alongside the QuoterV2 round,
            // so the scan's quiet-pool recheck needs no round trip of its own.
            let pools: Vec<Address> = v3_routes.iter().chain(rest.iter().take(max_opportunities))
                .flat_map(|o| crate::race::route_pools(&self.pool_cache, &[
                    (&o.router_ab_id, o.router_ab_type, o.fee_ab, o.token_a, o.token_b),
                    (&o.router_bc_id, o.router_bc_type, o.fee_bc, o.token_b, o.token_c),
                    (&o.router_ca_id, o.router_ca_type, o.fee_ca, o.token_c, o.token_a),
                ]))
                .collect();
            let t = std::time::Instant::now();
            let ((verified, best_verified_usd), ()) = tokio::join!(
                self.verify_v3_triangular(provider, v3_routes, &token_info),
                self.prefetch_quiet_reserves(provider, pools),
            );
            self.timings.tri_verify_ms.store(t.elapsed().as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
            best_raw_usd = best_raw_usd.max(best_verified_usd);
            rest.extend(verified);
            rest.sort_by(|a, b| b.profit_usd.total_cmp(&a.profit_usd));
            opportunities = rest;
        }
        opportunities.truncate(max_opportunities);

        // ── Update triangular scan snapshot ──────────────────────────────────────
        // Build per-triplet stats from p1_entries (represents all triplets that had
        // at least one router with a local cache hit in phase 1 = "was scanned").
        {
            // Update session opp counts (happens on every scan including targeted).
            {
                let mut opp_counts = self.opp_session_counts.lock().unwrap();
                for opp in &opportunities {
                    *opp_counts.entry(opp.triplet_id.clone()).or_insert(0) += 1;
                }
            } // opp_counts lock released here

            // Only rebuild tri_scan on full scans. Targeted swap-event scans evaluate
            // only 1-2 triplets and would corrupt the snapshot for all other triplets.
            if is_full_scan {
                // Re-acquire for read access in the snapshot build.
                let opp_counts = self.opp_session_counts.lock().unwrap();

                // Group p1_entries by triplet_id to get per-triplet DEX coverage.
                // p1_entries has one entry per (triplet, router) combination — filter to those
                // that succeeded (local_result.is_some() means the pool was in cache).
                let mut tri_dex_map: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
                for p1e in &p1_entries {
                    if p1e.local_result.is_some() {
                        let trip_id = &triplets[p1e.triplet_idx].triplet_id;
                        tri_dex_map.entry(trip_id.clone()).or_default().insert(p1e.router_id.clone());
                    }
                }
                // Count complete 3-leg paths per triplet (from p3_entries)
                let mut tri_path_count: HashMap<String, usize> = HashMap::new();
                for p3e in &p3_entries {
                    let trip_id = &triplets[p3e.triplet_idx].triplet_id;
                    *tri_path_count.entry(trip_id.clone()).or_insert(0) += 1;
                }

                let mut tri = self.tri_scan.lock().unwrap();
                tri.clear();
                // Use all_displayable so disabled triplets still appear in the dashboard (dimmed).
                // Active triplets get live DEX/path stats; disabled ones show zeros.
                for (triplet_id, ..) in &all_displayable {
                    let is_disabled = disabled_triplet_ids.contains(triplet_id);
                    let dex_ids: Vec<String> = if is_disabled {
                        vec![]
                    } else {
                        tri_dex_map.get(triplet_id)
                            .map(|s| s.iter().cloned().collect())
                            .unwrap_or_default()
                    };
                    let dex_count = dex_ids.len();
                    let cross_count = if is_disabled { 0 } else {
                        tri_path_count.get(triplet_id).copied().unwrap_or(0)
                    };
                    let was_quoted = !is_disabled && dex_count > 0;
                    let display_name = format!("▲ {}", triplet_id);
                    tri.push(PairScanInfo {
                        pair_id: triplet_id.clone(),
                        chain_id: self.chain_id,
                        display_name,
                        dex_count,
                        dex_ids,
                        cross_count,
                        was_quoted,
                        opp_count: *opp_counts.get(triplet_id).unwrap_or(&0),
                        disabled: is_disabled,
                    });
                }
                // Deduplicate: keep only unique triplet_id entries
                tri.dedup_by(|a, b| a.pair_id == b.pair_id);
            }
        }

        super::BeatSpreads::note(&self.beat.tri, best_tri_spread);
        if best_tri_spread.is_finite() && !p3_entries.is_empty() {
            debug!(
                "[{}] triangular best spread: {:+.3}% ({} routes evaluated)",
                self.chain_id, best_tri_spread * 100.0, p3_entries.len()
            );
        }

        (opportunities, best_raw_usd)
    }

    /// QuoterV2-verify triangular routes that have at least one V3 leg (revert plan 1.4).
    ///
    /// Always quotes live through [`Strategy::quote_v3_legs`] (batched, deduplicated, no
    /// cross-scan cache), normally in ONE round trip, at most [`verify_max_rounds()`]:
    ///
    /// 1. Each V3 leg's upper-bound input `s` is the route's input chained through earlier
    ///    legs at V3 *marginal* price (no impact, no tick cap — an upper bound on output)
    ///    and exact local quotes for non-V3 legs. A V3 leg whose input is exact (no earlier
    ///    V3 leg) is quoted at `s`; otherwise at `s` and `s × (1 − verify_bracket_bps())`.
    /// 2. Real amounts are chained leg by leg; each V3 leg uses
    ///    [`v3_leg_lower_bound`] over its quoted points. A loose bound (real input far
    ///    outside the points) is re-quoted at the real input next round; the last round
    ///    accepts the bound, which is always conservative.
    ///
    /// A route is dropped when a V3 leg has no quoter or its quote fails, any leg has no
    /// liquidity, verified profit is ≤ 0 or > 5% of input (bad quote), or it is below
    /// `min_triangular_profit_usd`. Returns the kept routes and the best verified profit
    /// seen (even below the floor) for heartbeat logging.
    async fn verify_v3_triangular<P: Provider + Clone>(
        &self,
        provider: &P,
        opportunities: Vec<TriangularOpportunity>,
        token_info: &std::collections::HashMap<String, (String, u8)>,
    ) -> (Vec<TriangularOpportunity>, f64) {
        // Drop routes with a V3 leg that has no quoter before spending any RPC on them.
        let opportunities: Vec<TriangularOpportunity> = opportunities.into_iter().filter(|opp| {
            let ok = (0..3).all(|leg| {
                let (router_id, router_type, ..) = leg_params(opp, leg);
                router_type != RouterType::V3 || self.quoter_by_router.contains_key(router_id)
            });
            if !ok {
                debug!("[{}] Triangular V3 verify dropped: {} | V3 leg has no quoter", self.chain_id, opp.triplet_id);
            }
            ok
        }).collect();

        // Pool log positions before any quote goes out: if a pool logs while the quotes are in
        // flight, the memo entry won't match its state and the route is verified again.
        let stamps: Vec<Vec<Option<(u64, u64)>>> = opportunities.iter().map(|o| self.tri_route_stamps(o)).collect();
        let memo_on = !crate::config::tuning().tri_verify_memo().is_zero();
        let mut unprofitable: Vec<(String, Vec<Option<(u64, u64)>>)> = Vec::new();

        // Pool key per V3 leg, built once and reused for marginal quotes and QuoterV2.
        let keys: Vec<[Option<V3PoolKey>; 3]> = opportunities.iter().map(|opp| {
            std::array::from_fn(|leg| {
                let (router_id, router_type, fee, token_in, token_out) = leg_params(opp, leg);
                (router_type == RouterType::V3).then(|| V3PoolKey::new(router_id, token_in, token_out, fee))
            })
        }).collect();

        // Round-1 requests: every V3 leg at its upper-bound input, plus the bracket's lower
        // point when an earlier V3 leg makes the input uncertain.
        let mut alive: Vec<bool> = vec![true; opportunities.len()];
        let mut requests: Vec<(usize, usize, U256)> = Vec::new(); // (route, leg, amount_in)
        for (i, opp) in opportunities.iter().enumerate() {
            let mut amt = opp.amount_in;
            let mut input_uncertain = false;
            for (leg, key) in keys[i].iter().enumerate() {
                let (router_id, router_type, fee, token_in, token_out) = leg_params(opp, leg);
                let next = match key {
                    Some(key) => {
                        requests.push((i, leg, amt));
                        if input_uncertain {
                            let lo = amt * U256::from(10_000 - verify_bracket_bps()) / U256::from(10_000u32);
                            requests.push((i, leg, lo));
                        }
                        input_uncertain = true;
                        self.pool_cache.quote_v3_marginal(key, token_in, amt)
                    }
                    None => self.local_leg_out(router_id, router_type, fee, token_in, token_out, amt),
                };
                match next.filter(|a| !a.is_zero()) {
                    Some(a) => amt = a,
                    None => {
                        debug!("[{}] Triangular V3 verify dropped: {} | leg {} has no cached liquidity",
                               self.chain_id, opp.triplet_id, leg);
                        alive[i] = false;
                        break;
                    }
                }
            }
        }
        requests.retain(|(i, ..)| alive[*i]);

        // QuoterV2 points (input, output) per route and leg, kept sorted by input.
        let mut points: Vec<[Vec<(U256, U256)>; 3]> = vec![Default::default(); opportunities.len()];
        let mut final_amounts: Vec<Option<U256>> = vec![None; opportunities.len()];
        for round in 0..verify_max_rounds() {
            self.timings.tri_verify_rounds.store(round as u64 + 1, std::sync::atomic::Ordering::Relaxed);
            let legs: Vec<V3LegQuote> = requests.iter().map(|&(i, leg, amount_in)| V3LegQuote {
                key: keys[i][leg].as_ref().expect("requests are V3 legs"),
                amount_in,
            }).collect();
            let results = self.quote_v3_legs(provider, &legs).await;
            for (&(i, leg, amount_in), q) in requests.iter().zip(results) {
                match q {
                    Some(out) => {
                        let pts = &mut points[i][leg];
                        pts.push((amount_in, out));
                        pts.sort_by(|a, b| a.0.cmp(&b.0));
                    }
                    None if alive[i] => {
                        debug!("[{}] Triangular V3 verify dropped: {} | QuoterV2 failed on leg {}",
                               self.chain_id, opportunities[i].triplet_id, leg);
                        alive[i] = false;
                    }
                    None => {}
                }
            }

            let last_round = round + 1 == verify_max_rounds();
            requests.clear();
            for (i, opp) in opportunities.iter().enumerate() {
                if !alive[i] || final_amounts[i].is_some() { continue; }
                match self.walk_verified_route(opp, &points[i], last_round) {
                    RouteWalk::Done(amt) => final_amounts[i] = Some(amt),
                    RouteWalk::Requote(leg, amt) => requests.push((i, leg, amt)),
                    RouteWalk::Dead(why) => {
                        debug!("[{}] Triangular V3 verify dropped: {} | {}", self.chain_id, opp.triplet_id, why);
                        alive[i] = false;
                    }
                }
            }
            if requests.is_empty() { break; }
            debug!("[{}] Triangular V3 verify: round {} re-quotes {} leg(s)", self.chain_id, round + 2, requests.len());
        }

        let mut best_verified_usd: f64 = 0.0;
        let kept: Vec<TriangularOpportunity> = opportunities.into_iter().zip(final_amounts).zip(stamps).filter_map(|((mut opp, amt), stamp)| {
            let final_amt = apply_exec_quote_haircut(amt?);

            let profit = final_amt.checked_sub(opp.amount_in).filter(|p| !p.is_zero());
            // Same 5% sanity cap as detection: a verified edge that large means a bad quote.
            if profit.is_some_and(|p| crate::config::tuning().is_phantom(p, opp.amount_in)) {
                warn!(
                    "[{}] Triangular V3 verify: {} | verified profit/input > 5% — bad quote, skipped",
                    self.chain_id, opp.triplet_id,
                );
                return None;
            }
            if !opp.amount_in.is_zero() {
                let spread = crate::util::u256_to_f64(final_amt) / crate::util::u256_to_f64(opp.amount_in) - 1.0;
                super::BeatSpreads::note(&self.beat.tri_v, spread);
                // expected_profit is still the local (spot) estimate here.
                let local = crate::util::u256_to_f64(opp.expected_profit) / crate::util::u256_to_f64(opp.amount_in);
                self.tri_cal.note(local, spread);
            }
            let (sym_a, dec_a) = token_info.get(&addr_key(opp.token_a))?;
            let profit_usd = profit
                .map(|p| token_amount_to_usd(p, *dec_a, sym_a, self.native_price_usd))
                .unwrap_or(0.0);
            best_verified_usd = best_verified_usd.max(profit_usd);
            if profit.is_none() || profit_usd < self.min_triangular_profit_usd {
                debug!(
                    "[{}] Triangular V3 verify dropped: {} | spot ${:.4} → verified ${:.4}",
                    self.chain_id, opp.triplet_id, opp.profit_usd, profit_usd,
                );
                if memo_on { unprofitable.push((tri_route_key(&opp), stamp)); }
                return None;
            }
            opp.expected_profit = profit?;
            opp.profit_usd = profit_usd;
            Some(opp)
        }).collect();
        if !unprofitable.is_empty() {
            let mut memo = self.tri_memo.lock().unwrap();
            if memo.len() > 4096 { memo.clear(); }
            let now = std::time::Instant::now();
            for (key, stamp) in unprofitable {
                memo.insert(key, (stamp, now));
            }
        }
        (kept, best_verified_usd)
    }

    /// Last log position of each pool on the route (see `PoolCache::log_position`).
    fn tri_route_stamps(&self, o: &TriangularOpportunity) -> Vec<Option<(u64, u64)>> {
        crate::race::route_pools(&self.pool_cache, &[
            (&o.router_ab_id, o.router_ab_type, o.fee_ab, o.token_a, o.token_b),
            (&o.router_bc_id, o.router_bc_type, o.fee_bc, o.token_b, o.token_c),
            (&o.router_ca_id, o.router_ca_type, o.fee_ca, o.token_c, o.token_a),
        ]).into_iter().map(|p| self.pool_cache.log_position(p)).collect()
    }

    /// Drop routes verified unprofitable within `tuning.tri_verify_memo_ms` whose pools have
    /// logged nothing since (prunes expired entries). Returns how many were dropped.
    fn drop_memoized_routes(&self, routes: &mut Vec<TriangularOpportunity>) -> usize {
        let max_age = crate::config::tuning().tri_verify_memo();
        if max_age.is_zero() { return 0; }
        let mut memo = self.tri_memo.lock().unwrap();
        memo.retain(|_, (_, at)| at.elapsed() <= max_age);
        if memo.is_empty() { return 0; }
        let before = routes.len();
        routes.retain(|o| memo.get(&tri_route_key(o)).is_none_or(|(stamp, _)| *stamp != self.tri_route_stamps(o)));
        before - routes.len()
    }

    /// Chain a route's real amounts through its legs using the V3 points gathered so far.
    fn walk_verified_route(
        &self,
        opp: &TriangularOpportunity,
        points: &[Vec<(U256, U256)>; 3],
        last_round: bool,
    ) -> RouteWalk {
        let mut amt = opp.amount_in;
        for (leg, pts) in points.iter().enumerate() {
            let (router_id, router_type, fee, token_in, token_out) = leg_params(opp, leg);
            let next = if router_type == RouterType::V3 {
                let Some((bound, tight)) = v3_leg_lower_bound(pts, amt) else {
                    return RouteWalk::Dead("V3 leg has no quote");
                };
                if !tight && !last_round {
                    return RouteWalk::Requote(leg, amt);
                }
                Some(bound)
            } else {
                self.local_leg_out(router_id, router_type, fee, token_in, token_out, amt)
            };
            match next.filter(|a| !a.is_zero()) {
                Some(a) => amt = a,
                None => return RouteWalk::Dead("leg has no liquidity"),
            }
        }
        RouteWalk::Done(amt)
    }

    /// Best local pool-cache quote for one non-V3 triangular leg, with the fee that makes
    /// the executor trade through the quoted pool (Aerodrome: 0 = volatile, 1 = stable).
    fn best_local_leg(
        &self,
        router_id: &str,
        router_type: RouterType,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Option<(U256, u32)> {
        match router_type {
            RouterType::V2 => self.pool_cache.get_amount_out_by_v2_key(
                &V2PoolKey::new(router_id, token_in, token_out),
                token_in,
                amount_in,
                None,
            ).map(|out| (out, 0)),
            RouterType::Aerodrome => solidly_out(
                &self.pool_cache,
                &self.solidly_pool_keys,
                router_id,
                token_in,
                token_out,
                amount_in,
            ),
            RouterType::V3 => None,
        }
    }

    /// One route leg `(router id, type, fee, token in, token out)` as an xy=k curve for
    /// sizing, through the pool the transaction will use. None for stable pools / no state.
    fn leg_curve(&self, (router_id, rtype, fee, token_in, token_out): (&str, RouterType, u32, Address, Address)) -> Option<crate::pool_cache::LegCurve> {
        match rtype {
            RouterType::V3 => self.pool_cache.leg_curve_v3(&V3PoolKey::new(router_id, token_in, token_out, fee), token_in),
            RouterType::V2 => self.pool_cache.leg_curve_v2(&V2PoolKey::new(router_id, token_in, token_out), token_in, None),
            RouterType::Aerodrome => {
                if fee != 0 { return None; } // stable curve
                let (vol, _) = self.solidly_pool_keys.get(router_id)?;
                self.pool_cache.leg_curve_v2(&V2PoolKey::new(vol, token_in, token_out), token_in, Some(volatile_max_age()))
            }
        }
    }

    /// The route re-quoted at its profit-maximising input (closed form on the cached curves,
    /// no RPC), at most `upper`: (input, final output). None when the route has no edge at
    /// the margin, can't be sized (stable pool) or the size is dust.
    fn size_route(&self, legs: &[(&str, RouterType, u32, Address, Address)], upper: U256) -> Option<(U256, U256)> {
        let curves: Option<Vec<_>> = legs.iter().map(|&l| self.leg_curve(l)).collect();
        let x = super::size::optimal_input(&curves?)?.min(crate::util::u256_to_f64(upper));
        // A hair under, so f64 rounding can't land just over a binding tick cap (see size_2hop).
        let x = U256::from((x * (1.0 - 1e-9)) as u128);
        if x.is_zero() || x < upper / U256::from(200u32) {
            return None;
        }
        let mut amount = x;
        for &(router_id, rtype, fee, token_in, token_out) in legs {
            amount = match rtype {
                // A tick-capped leg quotes only part of `amount`: not a valid sized route.
                RouterType::V3 => self.pool_cache
                    .quote_v3_spot_key(&V3PoolKey::new(router_id, token_in, token_out, fee), token_in, amount)
                    .filter(|&(_, _, eff)| eff == amount)
                    .map(|(out, _, _)| out),
                _ => self.local_leg_out(router_id, rtype, fee, token_in, token_out, amount),
            }?;
            if amount.is_zero() {
                return None;
            }
        }
        Some((x, amount))
    }

    /// Local pool-cache quote for one non-V3 leg of a found route, through exactly the pool
    /// its `fee` selects (the one the transaction will use).
    fn local_leg_out(
        &self,
        router_id: &str,
        router_type: RouterType,
        fee: u32,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Option<U256> {
        match router_type {
            RouterType::V2 => self.pool_cache.get_amount_out_by_v2_key(
                &V2PoolKey::new(router_id, token_in, token_out),
                token_in,
                amount_in,
                None,
            ),
            RouterType::Aerodrome => solidly_pool_out(
                &self.pool_cache,
                &self.solidly_pool_keys,
                router_id,
                fee != 0,
                token_in,
                token_out,
                amount_in,
            ),
            RouterType::V3 => None,
        }
    }
}

/// Number of V3 legs in a route — the one place that decides which routes need QuoterV2
/// verification (cached V3 spot quotes can be phantom).
fn v3_leg_count(ab: RouterType, bc: RouterType, ca: RouterType) -> u64 {
    [ab, bc, ca].iter().filter(|t| **t == RouterType::V3).count() as u64
}

fn has_v3_leg(opp: &TriangularOpportunity) -> bool {
    v3_leg_count(opp.router_ab_type, opp.router_bc_type, opp.router_ca_type) > 0
}

/// (router_id, router_type, fee, token_in, token_out) for leg 0 (A→B), 1 (B→C) or 2 (C→A).
fn leg_params(opp: &TriangularOpportunity, leg: usize) -> (&str, RouterType, u32, Address, Address) {
    match leg {
        0 => (&opp.router_ab_id, opp.router_ab_type, opp.fee_ab, opp.token_a, opp.token_b),
        1 => (&opp.router_bc_id, opp.router_bc_type, opp.fee_bc, opp.token_b, opp.token_c),
        _ => (&opp.router_ca_id, opp.router_ca_type, opp.fee_ca, opp.token_c, opp.token_a),
    }
}

fn best_v3_spot(
    cache: &PoolCache,
    router_id: &std::sync::Arc<str>,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    fee_tiers: &[u32],
) -> Option<(U256, u32)> {
    fee_tiers
        .iter()
        .filter_map(|&f| {
            let key = V3PoolKey { router_id: router_id.clone(), token_in, token_out, fee: f };
            cache
                .quote_v3_spot_key(&key, token_in, amount_in)
                .and_then(|(out, liq, _)| {
                    if liq < min_v3_liquidity() {
                        None
                    } else {
                        Some((out, f))
                    }
                })
        })
        .max_by_key(|(out, _)| *out)
}

/// Quote through one Aerodrome pool (volatile or stable), None when it is missing or stale.
fn solidly_pool_out(
    cache: &PoolCache,
    solidly_keys: &std::collections::HashMap<String, (String, String)>,
    router_id: &str,
    stable: bool,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Option<U256> {
    let (vol, sta) = solidly_keys.get(router_id)?;
    let (pool_id, max_age) = if stable { (sta, stable_max_age()) } else { (vol, volatile_max_age()) };
    cache.get_amount_out_by_v2_key(&V2PoolKey::new(pool_id, token_in, token_out), token_in, amount_in, Some(max_age))
}

/// Better of the fresh Aerodrome volatile and stable pools for this leg, with the fee the
/// contract reads as the pool choice (`stable = fee != 0`): 0 volatile, 1 stable.
fn solidly_out(
    cache: &PoolCache,
    solidly_keys: &std::collections::HashMap<String, (String, String)>,
    router_id: &str,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Option<(U256, u32)> {
    let vol = solidly_pool_out(cache, solidly_keys, router_id, false, token_in, token_out, amount_in);
    let sta = solidly_pool_out(cache, solidly_keys, router_id, true, token_in, token_out, amount_in);
    match (vol, sta) {
        (Some(v), Some(s)) if s > v => Some((s, 1)),
        (Some(v), _) => Some((v, 0)),
        (None, Some(s)) => Some((s, 1)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool_cache::PoolInfo;
    use std::time::{Duration, Instant};

    const USDC: Address = alloy::primitives::address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    const USDT: Address = alloy::primitives::address!("fde4C96c8593536E31F229EA8f37b2ADa2699bb2");

    fn pool(id: &str, stable: bool, r0: u64, r1: u64, fee_bps: u32, age_secs: u64) -> PoolInfo {
        PoolInfo {
            token0: USDC,
            token1: USDT,
            reserve0: U256::from(r0),
            reserve1: U256::from(r1),
            fee_bps,
            router_id: id.into(),
            is_stable: stable,
            decimals0: 6,
            decimals1: 6,
            last_sync: Instant::now().checked_sub(Duration::from_secs(age_secs)).unwrap(),
            fee_num: U256::ZERO,
        }
    }

    /// Aerodrome USDC/USDT on Base: ~$326 volatile pool next to an ~$8.2k stable pool.
    fn setup(vol_age: u64, vol_r: (u64, u64)) -> (PoolCache, std::collections::HashMap<String, (String, String)>) {
        let cache = PoolCache::new();
        cache.insert(Address::repeat_byte(1), pool("aero::volatile", false, vol_r.0, vol_r.1, 30, vol_age));
        cache.insert(Address::repeat_byte(2), pool("aero::stable", true, 3_727_000_000, 4_432_000_000, 5, 0));
        let keys = [("aero".to_string(), ("aero::volatile".to_string(), "aero::stable".to_string()))].into();
        (cache, keys)
    }

    #[test]
    fn aerodrome_leg_carries_the_fee_of_the_pool_it_quoted() {
        let amt = U256::from(1_000_000_000u64); // $1000 USDC
        let stable_quote = |c: &PoolCache, k| solidly_pool_out(c, k, "aero", true, USDC, USDT, amt).unwrap();

        // Stale volatile + fresh stable: quoted on the stable pool, so fee 1 (stable).
        let (cache, keys) = setup(200, (162_700_000, 163_300_000));
        let (out, fee) = solidly_out(&cache, &keys, "aero", USDC, USDT, amt).unwrap();
        assert_eq!(fee, 1);
        assert_eq!(out, stable_quote(&cache, &keys));
        assert!(out > U256::from(990_000_000u64), "stable quote {out}");
        // Verification quotes only the pool the leg's fee selects — no fallback.
        assert!(solidly_pool_out(&cache, &keys, "aero", false, USDC, USDT, amt).is_none());

        // Both fresh, thin volatile: the deep stable pool still wins.
        let (cache, keys) = setup(0, (162_700_000, 163_300_000));
        let (out, fee) = solidly_out(&cache, &keys, "aero", USDC, USDT, amt).unwrap();
        assert_eq!(fee, 1);
        assert_eq!(out, stable_quote(&cache, &keys));
        let vol = solidly_pool_out(&cache, &keys, "aero", false, USDC, USDT, amt).unwrap();
        assert!(vol < U256::from(150_000_000u64), "volatile quote {vol}");

        // Both fresh, deep volatile: volatile wins with fee 0.
        let (cache, keys) = setup(0, (1_000_000_000_000, 1_010_000_000_000));
        let (out, fee) = solidly_out(&cache, &keys, "aero", USDC, USDT, amt).unwrap();
        assert_eq!(fee, 0);
        assert_eq!(out, solidly_pool_out(&cache, &keys, "aero", false, USDC, USDT, amt).unwrap());
    }

    /// Targeted scans rescan exactly the triangles with a leg on a swapped pool's token pair:
    /// same routes and amounts as the full sweep for those, none of the others.
    #[tokio::test]
    async fn pair_filter_selects_the_full_scans_routes_on_that_pair() {
        let (b, c, d) = (Address::repeat_byte(0x11), Address::repeat_byte(0x22), Address::repeat_byte(0x33));
        let cache = PoolCache::new();
        let v2 = |pool: u8, (t0, d0, r0): (Address, u8, u128), (t1, d1, r1): (Address, u8, u128)| {
            assert!(t0 < t1);
            cache.insert(Address::repeat_byte(pool), PoolInfo {
                token0: t0, token1: t1, reserve0: U256::from(r0), reserve1: U256::from(r1), fee_bps: 30,
                router_id: "uni2".into(), is_stable: false, decimals0: d0, decimals1: d1,
                last_sync: Instant::now(), fee_num: U256::ZERO,
            });
        };
        let e18 = 10u128.pow(18);
        // $1M pools at 1 USDC = 1 B = 1 C = 1 D, except B/C: 1 B buys 1.03 C, so
        // USDC→B→C→USDC earns ~2% (under the 5% phantom cap).
        v2(0xb1, (b, 18, 1_000_000 * e18), (USDC, 6, 1_000_000_000_000));
        v2(0xc1, (c, 18, 1_000_000 * e18), (USDC, 6, 1_000_000_000_000));
        v2(0xd1, (d, 18, 1_000_000 * e18), (USDC, 6, 1_000_000_000_000));
        v2(0xbc, (b, 18, 1_000_000 * e18), (c, 18, 1_030_000 * e18));
        v2(0xcd, (c, 18, 1_000_000 * e18), (d, 18, 1_000_000 * e18));
        let routers: Vec<crate::config::RouterConfig> = serde_yaml::from_str(r#"
- { id: uni2, name: uni2, address: "0x0000000000000000000000000000000000000001", type: v2 }
"#).unwrap();
        let pairs: Vec<crate::config::PairConfig> = serde_yaml::from_str(&format!(r#"
- {{ id: usdc-b, token_in: "{USDC}", token_out: "{b}", token_in_symbol: USDC, token_out_symbol: BBB, token_in_decimals: 6, token_out_decimals: 18, trade_amount: "1000" }}
- {{ id: usdc-c, token_in: "{USDC}", token_out: "{c}", token_in_symbol: USDC, token_out_symbol: CCC, token_in_decimals: 6, token_out_decimals: 18, trade_amount: "1000" }}
- {{ id: usdc-d, token_in: "{USDC}", token_out: "{d}", token_in_symbol: USDC, token_out_symbol: DDD, token_in_decimals: 6, token_out_decimals: 18, trade_amount: "1000" }}
"#)).unwrap();
        let strategy = Strategy::new(8453, pairs, routers, 0.0, 0.0, None, std::sync::Arc::new(cache), 1, false, None);
        // V2-only routes are never QuoterV2-verified, so the provider is never called.
        let provider = alloy::providers::ProviderBuilder::new().connect_http("http://127.0.0.1:9".parse().unwrap());
        let none = HashSet::new();
        let ids = |v: &[TriangularOpportunity]| -> Vec<(String, U256, U256)> {
            let mut v: Vec<_> = v.iter().map(|o| (o.triplet_id.clone(), o.amount_in, o.expected_profit)).collect();
            v.sort();
            v
        };

        let (full, _) = strategy.detect_triangular(&provider, 5, None, &none).await;
        assert!(full.iter().any(|o| o.triplet_id == "USDC→BBB→CCC"), "full scan: {:?}", ids(&full));
        // Repeat sweeps give the same result (the per-scan A→B memo holds no state across scans).
        assert_eq!(ids(&strategy.detect_triangular(&provider, 5, None, &none).await.0), ids(&full));

        let on_pair = |x: Address, y: Address| -> Vec<(String, U256, U256)> {
            let key = crate::strategy::unordered_pair(x, y);
            let keep: Vec<TriangularOpportunity> = full.iter().filter(|o| {
                [(o.token_a, o.token_b), (o.token_b, o.token_c), (o.token_c, o.token_a)]
                    .iter().any(|&(p, q)| crate::strategy::unordered_pair(p, q) == key)
            }).cloned().collect();
            ids(&keep)
        };
        let targeted = |x: Address, y: Address| {
            let f = [crate::strategy::unordered_pair(x, y)];
            let strategy = &strategy;
            let provider = &provider;
            let none = &none;
            async move { ids(&strategy.detect_triangular(provider, 5, Some(&f), none).await.0) }
        };
        assert_eq!(targeted(b, c).await, on_pair(b, c));
        assert!(!targeted(b, c).await.is_empty());
        assert_eq!(targeted(c, USDC).await, on_pair(c, USDC));
        // C/D swapped: USDC→B→C has no C–D leg, so it is not rescanned.
        assert_eq!(targeted(c, d).await, on_pair(c, d));
        assert!(targeted(c, d).await.iter().all(|(id, ..)| id != "USDC→BBB→CCC"));
    }

    /// A route verified unprofitable is skipped while its pools log nothing new; a new log on
    /// any of its pools, or the memo's age limit, makes it eligible for verification again.
    #[test]
    fn memo_skips_unchanged_unprofitable_routes_only() {
        use crate::pool_cache::V3PoolState;
        let (a, b, c) = (Address::repeat_byte(1), Address::repeat_byte(2), Address::repeat_byte(3));
        let cache = PoolCache::new();
        let pools = [(Address::repeat_byte(0xa1), a, b), (Address::repeat_byte(0xa2), b, c), (Address::repeat_byte(0xa3), a, c)];
        for (pool, t0, t1) in pools {
            cache.insert_v3(pool, V3PoolState {
                token0: t0, token1: t1, fee: 500, swap_fee: 500,
                sqrt_price_x96: U256::from(1u64) << 96, liquidity: 1_000_000, tick: 0,
                router_id: "r".into(), last_updated: Instant::now(),
                vr0: U256::ZERO, vr1: U256::ZERO, fee_num_v3: U256::ZERO,
            });
            cache.advance_log_position(pool, (100, 1));
        }
        let strategy = Strategy::new(8453, vec![], vec![], 0.0, 0.0, None, std::sync::Arc::new(cache), 1, false, None);
        let route = TriangularOpportunity {
            chain_id: 8453, triplet_id: "A-B-C".into(), token_a: a, token_b: b, token_c: c,
            amount_in: U256::from(1000u64), router_ab: Address::ZERO, router_bc: Address::ZERO, router_ca: Address::ZERO,
            router_ab_type: RouterType::V3, router_bc_type: RouterType::V3, router_ca_type: RouterType::V3,
            fee_ab: 500, fee_bc: 500, fee_ca: 500, expected_profit: U256::from(1u64), profit_usd: 1.0,
            router_ab_id: "r".into(), router_bc_id: "r".into(), router_ca_id: "r".into(),
        };
        assert_eq!(strategy.tri_route_stamps(&route).len(), 3);
        let remember = |at: Instant| {
            strategy.tri_memo.lock().unwrap().insert(tri_route_key(&route), (strategy.tri_route_stamps(&route), at));
        };
        let mut routes = vec![route.clone()];
        assert_eq!(strategy.drop_memoized_routes(&mut routes), 0, "nothing remembered yet");

        remember(Instant::now());
        assert_eq!(strategy.drop_memoized_routes(&mut routes), 1);
        assert!(routes.is_empty());

        // The B/C pool logs again: its state may have changed, so the route is verified.
        strategy.pool_cache.advance_log_position(Address::repeat_byte(0xa2), (101, 0));
        let mut routes = vec![route.clone()];
        assert_eq!(strategy.drop_memoized_routes(&mut routes), 0);

        // Remembered at the current state, but longer ago than tri_verify_memo_ms.
        remember(Instant::now() - Duration::from_secs(60));
        let mut routes = vec![route.clone()];
        assert_eq!(strategy.drop_memoized_routes(&mut routes), 0);
        assert!(strategy.tri_memo.lock().unwrap().is_empty(), "expired entries are pruned");
    }
}
