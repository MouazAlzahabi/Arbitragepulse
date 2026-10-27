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
fn verify_bracket_bps() -> u64 { crate::config::tuning().tri_verify_bracket_bps }

/// Outside its quoted points, a V3 leg's real input is bounded by the nearest point
/// (chord through the origin below it, flat output above it). Accepted when within this
/// many bps of that point; otherwise the leg is re-quoted at its real input next round.
/// In the last round the bound is used regardless — every bound is conservative.
fn verify_max_gap_bps() -> u64 { crate::config::tuning().tri_verify_max_gap_bps }

/// How much one V3 leg's cached spot quote overstates output (tick crossings aren't
/// modelled) — historical Base data: ~0.30% on a 270 USDT trade. In optimistic mode, where
/// V3-leg routes aren't QuoterV2-verified, this is deducted once per V3 leg.
fn v3_spot_overestimate_bps() -> u64 { crate::config::tuning().v3_spot_overestimate_bps }

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
fn v3_leg_lower_bound(points: &[(U256, U256)], amt: U256) -> Option<(U256, bool)> {
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
    /// `token_filter`: if Some, only consider triplets that include at least one of the
    /// given token addresses (targeted scan). Pass None for a full sweep.
    pub async fn detect_triangular<P: Provider + Clone>(
        &self,
        provider: &P,
        max_opportunities: usize,
        token_filter: Option<&[Address]>,
        disabled_triplets: &std::collections::HashSet<String>,
    ) -> (Vec<TriangularOpportunity>, f64) {
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
        // For N tokens: N×(N-1)×(N-2) ordered triplets. Capped at 6 tokens → max 120.
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

        let filter_set: Option<HashSet<Address>> = token_filter
            .map(|f| f.iter().copied().collect());

        let max_tokens = 25usize;
        let mut triplets: Vec<Triplet> = Vec::new();

        for &(token_a, ref key_a) in tokens.iter().take(max_tokens) {
            let Some(&amount_in) = token_amounts.get(key_a) else { continue };
            let Some((sym_a, _)) = token_info.get(key_a) else { continue };

            for &(token_b, ref key_b) in &tokens {
                if token_b == token_a { continue; }
                let Some((sym_b, _)) = token_info.get(key_b) else { continue };

                for &(token_c, ref key_c) in &tokens {
                    if token_c == token_a || token_c == token_b { continue; }
                    if let Some(ref fs) = filter_set {
                        if !fs.contains(&token_a) && !fs.contains(&token_b) && !fs.contains(&token_c) {
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
        // Targeted scans (token_filter is Some) evaluate only 1-2 triplets, so rebuilding
        // the full tri_scan snapshot would mark all others as "not quoted" — corrupting data.
        let is_full_scan = token_filter.is_none();
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

        for (ti, trip) in triplets.iter().enumerate() {
            for &ri in &self.chain_router_indices {
                let router = &self.routers[ri];
                let router_addr: Address = self.router_addr_map.get(&router.id).copied().unwrap_or_default();
                let tiers: &[u32] = if router.fee_tiers.is_empty() {
                    default_v3_tiers
                } else {
                    &router.fee_tiers
                };
                let (fee, effective_amount_in, local_ab): (u32, U256, Option<U256>) = match router.router_type {
                    RouterType::V3 => {
                        let result = best_v3_spot(
                            &self.pool_cache,
                            &router.id,
                            trip.token_a,
                            trip.token_b,
                            trip.amount_in,
                            tiers,
                        );
                        // `out` may be for a capped input (see quote_v3_spot_key); the route
                        // still trades the full amount_in, so this only under-scores it.
                        match result {
                            Some((out, f)) => (f, trip.amount_in, Some(out)),
                            None => continue,
                        }
                    }
                    _ => {
                        let best = self.best_local_leg(
                            &router.id, router.router_type, trip.token_a, trip.token_b, trip.amount_in,
                        );
                        (best.map_or(0, |(_, f)| f), trip.amount_in, best.map(|(out, _)| out))
                    }
                };
                if local_ab.is_none() { continue; }
                if router.router_type == RouterType::V3 { tri_v3_spot += 1; }
                p1_entries.push(P1Entry {
                    triplet_idx: ti,
                    router_id: router.id.clone(),
                    router_addr,
                    router_type: router.router_type,
                    fee,
                    local_result: local_ab,
                    effective_amount_in,
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

        for p1e in p1_entries.iter() {
            let amount_b: U256 = match p1e.local_result.filter(|b| !b.is_zero()) {
                Some(b) => b,
                None => continue,
            };
            let trip = &triplets[p1e.triplet_idx];

            for &ri in &self.chain_router_indices {
                let router = &self.routers[ri];
                let router_addr: Address = self.router_addr_map.get(&router.id).copied().unwrap_or_default();
                let tiers: &[u32] = if router.fee_tiers.is_empty() {
                    default_v3_tiers
                } else {
                    &router.fee_tiers
                };
                let (fee, local_bc): (u32, Option<U256>) = match router.router_type {
                    RouterType::V3 => {
                        let result = best_v3_spot(
                            &self.pool_cache,
                            &router.id,
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

        for p2e in p2_entries.iter() {
            let amount_c: U256 = match p2e.local_result.filter(|c| !c.is_zero()) {
                Some(c) => c,
                None => continue,
            };
            let trip = &triplets[p2e.triplet_idx];

            for &ri in &self.chain_router_indices {
                let router = &self.routers[ri];
                let router_addr: Address = self.router_addr_map.get(&router.id).copied().unwrap_or_default();
                let tiers: &[u32] = if router.fee_tiers.is_empty() {
                    default_v3_tiers
                } else {
                    &router.fee_tiers
                };
                let (fee, local_ca): (u32, Option<U256>) = match router.router_type {
                    RouterType::V3 => {
                        let result = best_v3_spot(
                            &self.pool_cache,
                            &router.id,
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

        for p3e in p3_entries.iter() {
            let amount_a_final: U256 = match p3e.local_result {
                Some(a) if !a.is_zero() => a,
                _ => continue,
            };
            let amount_a_final = apply_exec_quote_haircut(amount_a_final);
            let trip = &triplets[p3e.triplet_idx];
            let v3_legs = v3_leg_count(p3e.router_ab_type, p3e.router_bc_type, p3e.router_ca_type);
            let v3_route = v3_legs > 0;
            // Optimistic mode submits V3-leg routes unverified: deduct the known V3 spot
            // overestimate (per V3 leg — each leg overstates independently) BEFORE the
            // spread diagnostic, the 5% cap and the floor, so all see a realistic output.
            let amount_a_final = if self.optimistic_submission && v3_route {
                let overestimate = p3e.effective_amount_in
                    * U256::from(v3_spot_overestimate_bps() * v3_legs) / U256::from(10_000u32);
                amount_a_final.saturating_sub(overestimate)
            } else {
                amount_a_final
            };

            // Track best triangular spread (even negative) for diagnostics
            let tri_spread = if !p3e.effective_amount_in.is_zero() {
                let final_f = amount_a_final.to::<u128>() as f64;
                let input_f = p3e.effective_amount_in.to::<u128>() as f64;
                (final_f / input_f) - 1.0
            } else { 0.0 };
            if tri_spread > best_tri_spread { best_tri_spread = tri_spread; }

            // Compare against effective_amount_in (may be < trip.amount_in if V3 leg was capped)
            if amount_a_final <= p3e.effective_amount_in { continue; }

            // Sanity cap: profit > 5% of input is almost certainly a detection artifact.
            // Catches Solidly-echo phantoms and V3 spot mismatch phantoms (including
            // medium-range $37-$121 phantoms that passed the old 3× absolute cap).
            if crate::config::tuning().is_phantom(amount_a_final - p3e.effective_amount_in, p3e.effective_amount_in) {
                warn!(
                    "[{}] Triangular phantom skipped: {} | profit/input > 5%",
                    self.chain_id,
                    trip.triplet_id,
                );
                continue;
            }

            let profit = amount_a_final - p3e.effective_amount_in;

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
                amount_in: p3e.effective_amount_in,
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

        // ── Revert plan 1.4: verify routes with a V3 leg before submit ───────────
        // Cached sqrtPriceX96 ignores tick crossings, so V3 legs overstate output
        // (~0.30% on a 270 USDT trade) by far more than the 2 bps haircut recovers.
        // Runs before the truncate so routes dropped here are backfilled by lower-ranked ones.
        // Optimistic mode skips QuoterV2 (like 2-hop): V3-leg routes go out on spot quotes,
        // warned once at Strategy construction.
        if !self.optimistic_submission && opportunities.iter().any(has_v3_leg) {
            let (mut v3_routes, mut rest): (Vec<_>, Vec<_>) =
                opportunities.into_iter().partition(has_v3_leg);
            // Bounded batch: ≤ 2 × max routes × 3 legs QuoterV2 calls in one multicall round.
            v3_routes.truncate(max_opportunities.saturating_mul(2));
            let t = std::time::Instant::now();
            let (verified, best_verified_usd) =
                self.verify_v3_triangular(provider, v3_routes, &token_info).await;
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
        let kept: Vec<TriangularOpportunity> = opportunities.into_iter().zip(final_amounts).filter_map(|(mut opp, amt)| {
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
                return None;
            }
            opp.expected_profit = profit?;
            opp.profit_usd = profit_usd;
            Some(opp)
        }).collect();
        (kept, best_verified_usd)
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
    router_id: &str,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    fee_tiers: &[u32],
) -> Option<(U256, u32)> {
    fee_tiers
        .iter()
        .filter_map(|&f| {
            let key = V3PoolKey::new(router_id, token_in, token_out, f);
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
}
