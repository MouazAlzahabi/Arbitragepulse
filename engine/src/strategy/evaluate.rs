use alloy::primitives::{Address, Uint, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use tracing::{debug, info, warn};

use crate::abi::IQuoterV2;
use crate::config::RouterType;
use crate::pool_cache::{V2PoolKey, V3PoolKey, VOLATILE_MAX_AGE, STABLE_MAX_AGE};
use crate::types::{ArbOpportunity, PairScanInfo};
use crate::util::u256_to_f64;
use super::{ForwardTask, ReverseTask, Strategy, apply_exec_quote_haircut, run_multicall, token_amount_to_usd, MIN_V3_LIQUIDITY};

impl Strategy {
    /// Evaluate all pairs on all router combinations for arb opportunities.
    /// All quotes run concurrently (two phases: forward then reverse).
    /// Returns `(opportunities, best_raw_profit_usd, best_verified_spread, fwd_quotes_ok, pairs_with_multi, best_spread_pct, pairs_with_any)`:
    /// - `best_raw_profit_usd`: highest profit even if below min_profit_usd (for status logging)
    /// - `best_verified_spread`: best signed spread confirmed by Phase 1.5 QuoterV2 (can be negative).
    ///   NEG_INFINITY = Phase 1.5 didn't run or reverse lookup always failed.
    ///   Negative = QuoterV2 confirmed loss after V3 fees (V3 phantom correctly deflated).
    ///   Positive = real profit confirmed; if opps=0, it was below min_profit_usd threshold.
    /// - `fwd_quotes_ok`: total number of successful non-zero forward quotes
    /// - `pairs_with_multi`: number of pairs with quotes from ≥2 distinct router IDs
    /// - `best_spread_pct`: best (reverse_out/amount_in - 1.0) seen, even when negative.
    ///   Negative = market is efficient (e.g. -0.002 = 0.2% below break-even).
    ///   Positive = profitable spread found.
    /// - `pairs_with_any`: number of pairs with at least 1 non-zero quote (shows coverage)
    ///
    /// `pair_mask`: if Some, only evaluate pairs at those indices (targeted scan).
    /// Pass None for a full scan of all pairs.
    pub async fn evaluate<P: Provider + Clone>(
        &self,
        provider: &P,
        pair_mask: Option<&std::collections::HashSet<usize>>,
        // True when called from a block/poll full scan — always rebuilds pair_scan.
        // False when called from a swap-event targeted scan — pair_scan is left unchanged
        // (a targeted scan only evaluates 1–2 pairs; rebuilding would mark all others stale).
        is_full_scan: bool,
    ) -> (Vec<ArbOpportunity>, f64, f64, usize, usize, f64, usize) {
        let scan_start = std::time::Instant::now();

        // ── Phase 1: Forward quotes ───────────────────────────────────────────────
        // V2 and Solidly-volatile pools use xy=k (constant-product AMM), so we compute
        // quotes locally from cached reserves — zero eth_calls.
        // V3, SyncSwap, and Solidly-stable still need multicall (different curves / no cache).

        // Build target index list first so we can use .len() for capacity hints below.
        let masked_indices: Vec<usize>;
        let target_indices: &[usize] = match pair_mask {
            Some(m) => {
                masked_indices = m.iter().copied()
                    .filter(|&i| self.pairs.get(i).map_or(false, |p| p.chain_id == self.chain_id))
                    .collect();
                &masked_indices
            }
            None => &self.chain_pair_indices,
        };

        // pair_quotes: one entry per pair → pre-size to avoid all rehashes in the hot loop.
        let mut pair_quotes: std::collections::HashMap<usize, Vec<ForwardTask>> =
            std::collections::HashMap::with_capacity(target_indices.len());

        // V3 scan diagnostic counters: cached=used spot (0 HTTP), no_cache=QuoterV2 needed.
        let mut v3_cached: usize = 0;
        let mut v3_no_cache: usize = 0;

        for &pi in target_indices {
            let resolved = match self.pair_resolved.get(pi).and_then(|o| o.as_ref()) {
                Some(r) => r,
                None => continue,
            };
            let token_in = resolved.token_in;
            let token_out = resolved.token_out;
            let amount_in = resolved.amount_in;
            for &ri in &self.chain_router_indices {
                let router = &self.routers[ri];
                let router_addr: Address = match self.router_addr_map.get(&router.id) {
                    Some(&a) => a,
                    None => continue,
                };

                match router.router_type {
                    RouterType::V2 => {
                        // Try local xy=k reserve cache first (no eth_call needed).
                        // VOLATILE_MAX_AGE (300s) freshness check: V2 reserves CAN go stale
                        // between Sync events. Without a freshness check, reserves from the
                        // last trade 10+ minutes ago produce phantom opportunities — the local
                        // xy=k formula is exact given correct reserves, but stale reserves
                        // overestimate output and cause on-chain "too little received" reverts.
                        let v2_key = V2PoolKey::new(&router.id, token_in, token_out);
                        if let Some(out) = self.pool_cache.get_amount_out_by_v2_key(
                            &v2_key, token_in, amount_in, Some(VOLATILE_MAX_AGE),
                        ) {
                            pair_quotes.entry(pi).or_insert_with(|| Vec::with_capacity(self.chain_router_indices.len() * 2)).push(ForwardTask {
                                pair_idx: pi,
                                router_id: router.id.clone(),
                                router_addr,
                                router_type: RouterType::V2,
                                fee: 0,
                                amount_in: out, // repurposed: carries token_out amount
                                capped_amount_in: amount_in,
                                token_in,
                                token_out,
                                quoter_addr: None,
                                v2_key: Some(v2_key),
                                v3_key: None,
                                rev_v2_key: Some(V2PoolKey::new(&router.id, token_out, token_in)),
                                rev_v3_key: None,
                            });
                        }
                        // Cache miss = pool not discovered at startup = doesn't exist → skip
                    }
                    RouterType::V3 => {
                        let quoter = match self.quoter_by_router.get(&router.id) {
                            Some(&q) => q,
                            None => {
                                debug!("No QuoterV2 for router {} on chain {} — skipping V3 quotes", router.id, self.chain_id);
                                continue;
                            }
                        };
                        const DEFAULT_FEE_TIERS: &[u32] = &[500, 3000, 10000];
                        let tiers: &[u32] = if router.fee_tiers.is_empty() {
                            DEFAULT_FEE_TIERS
                        } else {
                            &router.fee_tiers
                        };
                        for &fee in tiers {
                            // ── V3 cache-first: use sqrtPriceX96 virtual-reserve spot ──
                            // Cap the input to a single-tick safe amount so the xy=k formula
                            // is exact. Without the cap, a trade crossing into a tick with
                            // L=0 would cause the formula to overestimate output.
                            let v3_key = V3PoolKey::new(&router.id, token_in, token_out, fee);
                            if let Some((spot_out, liquidity, effective_in)) = self.pool_cache.quote_v3_spot_key(
                                &v3_key, token_in, amount_in,
                            ) {
                                if liquidity < MIN_V3_LIQUIDITY {
                                    v3_cached += 1;
                                    continue;
                                }
                                pair_quotes.entry(pi).or_insert_with(|| Vec::with_capacity(self.chain_router_indices.len() * 2)).push(ForwardTask {
                                    pair_idx: pi,
                                    router_id: router.id.clone(),
                                    router_addr,
                                    router_type: RouterType::V3,
                                    fee,
                                    amount_in: spot_out, // repurposed: carries token_out amount
                                    capped_amount_in: effective_in,
                                    token_in,
                                    token_out,
                                    quoter_addr: Some(quoter),
                                    v2_key: None,
                                    v3_key: Some(v3_key.clone()),
                                    rev_v2_key: None,
                                    rev_v3_key: Some(V3PoolKey::new(&router.id, token_out, token_in, fee)),
                                });
                                v3_cached += 1;
                            } else {
                                // V3 state stale or pool not found — skip (no QuoterV2 fallback).
                                v3_no_cache += 1;
                                continue;
                            }
                        }
                    }
                    RouterType::Solidly | RouterType::Aerodrome => {
                        // Both volatile (fee=0) and stable (fee=1) pools use local reserve cache.
                        // Only adds to pair_quotes when the pool has had a recent Sync event
                        // (last_sync.elapsed() < max_age). Stale = no on-chain activity = skip.
                        // Aerodrome pools use the same Sync(uint256,uint256) events and xy=k /
                        // x³y+y³x=k curves as Solidly — pool discovery and quoting are identical.
                        let rtype = router.router_type.clone();
                        let Some((vol_id, sta_id)) = self.solidly_pool_keys.get(&router.id) else {
                            continue;
                        };
                        for (eff_id, fee, max_age) in [
                            (vol_id.as_str(), 0u32, VOLATILE_MAX_AGE),
                            (sta_id.as_str(), 1u32, STABLE_MAX_AGE),
                        ] {
                            let v2_key = V2PoolKey::new(eff_id, token_in, token_out);
                            if let Some(out) = self.pool_cache.get_amount_out_by_v2_key(
                                &v2_key, token_in, amount_in, Some(max_age),
                            ) {
                                pair_quotes.entry(pi).or_insert_with(|| Vec::with_capacity(self.chain_router_indices.len() * 2)).push(ForwardTask {
                                    pair_idx: pi,
                                    router_id: eff_id.to_string(),
                                    router_addr,
                                    router_type: rtype.clone(),
                                    fee,
                                    amount_in: out, // repurposed: carries token_out amount
                                    capped_amount_in: amount_in,
                                    token_in,
                                    token_out,
                                    quoter_addr: None,
                                    v2_key: Some(v2_key),
                                    v3_key: None,
                                    rev_v2_key: Some(V2PoolKey::new(eff_id, token_out, token_in)),
                                    rev_v3_key: None,
                                });
                            }
                        }
                    }
                    RouterType::SyncSwap => {
                        // SyncSwap pools are now seeded into pool_cache at startup
                        // (populate_syncswap_pools also calls pool_cache.insert).
                        // Use local reserve state with freshness check (0 eth_call).
                        let v2_key = V2PoolKey::new(&router.id, token_in, token_out);
                        if let Some(out) = self.pool_cache.get_amount_out_by_v2_key(
                            &v2_key, token_in, amount_in, Some(VOLATILE_MAX_AGE),
                        ) {
                            pair_quotes.entry(pi).or_insert_with(|| Vec::with_capacity(self.chain_router_indices.len() * 2)).push(ForwardTask {
                                pair_idx: pi,
                                router_id: router.id.clone(),
                                router_addr,
                                router_type: RouterType::SyncSwap,
                                fee: 0,
                                amount_in: out, // repurposed: carries token_out amount
                                capped_amount_in: amount_in,
                                token_in,
                                token_out,
                                quoter_addr: None,
                                v2_key: Some(v2_key),
                                v3_key: None,
                                rev_v2_key: Some(V2PoolKey::new(&router.id, token_out, token_in)),
                                rev_v3_key: None,
                            });
                        }
                    }
                }
            }
        }

        // ── Diagnostic: compute multi-router coverage stats ─────────────────────
        // Returned to caller (chain.rs) which broadcasts to live feed once per 60s.
        // Not logged here to avoid terminal flood on every block.
        let total_fwd_ok: usize = pair_quotes.values().map(|v| v.len()).sum();
        let pairs_with_any: usize = pair_quotes.len(); // pairs with ≥1 quote from any DEX
        let pairs_with_multi: usize = pair_quotes.values().filter(|v| {
            v.len() > 1 && v[1..].iter().any(|q| q.router_id != v[0].router_id)
        }).count();
        debug!(
            "[chain={}] Forward quotes: ok={} pairs_with_any={} pairs_with_multi_dex={}",
            self.chain_id, total_fwd_ok, pairs_with_any, pairs_with_multi
        );
        if v3_cached + v3_no_cache > 0 {
            debug!(
                "[chain={}] V3 scan: {} cached (0 HTTP), {} skipped (stale/startup — no QuoterV2 fallback)",
                self.chain_id, v3_cached, v3_no_cache
            );
        }

        // ── Phase 2: Reverse quotes ───────────────────────────────────────────────
        // V2 and Solidly-volatile reverse quotes use the same local xy=k cache.
        // V3 uses cached spot when fresh; falls back to QuoterV2 only when stale.
        // SyncSwap and Solidly-stable still need multicall.

        // Pre-size rev_tasks to the exact number of A×B combinations across all pairs with ≥2 quotes.
        let rev_cap: usize = pair_quotes.values()
            .map(|v| if v.len() >= 2 { v.len() * (v.len() - 1) } else { 0 })
            .sum();
        let mut rev_tasks: Vec<ReverseTask> = Vec::with_capacity(rev_cap);

        for (pi, quotes) in &pair_quotes {
            if quotes.len() < 2 {
                continue;
            }
            let pair = &self.pairs[*pi];

            for (ai, q_a) in quotes.iter().enumerate() {
                let token_out_amount = q_a.amount_in;

                for (bi, q_b) in quotes.iter().enumerate() {
                    if ai == bi {
                        continue;
                    }

                    // Skip same-router V3 combinations entirely.
                    // The single-tick sqrtPriceX96 approximation is only reliable for
                    // cross-DEX comparisons. Within the same router, fee-tier pools share
                    // the same overall market price — spot divergences between fee=500 and
                    // fee=3000 (or even same fee with integer rounding) produce phantom arbs
                    // that consistently fail on-chain. Real arbs always cross DEX/router
                    // boundaries; same-router V3 × V3 is never reliably detectable with
                    // local spot math.
                    // Never route both legs through the same router contract.
                    // Aerodrome volatile and stable share one router_addr but have different
                    // router_ids — without this guard, vol×sta creates a same-DEX "arb" that
                    // always fails on-chain (InsufficientOutputAmount from the stable pool).
                    if q_a.router_addr == q_b.router_addr {
                        continue;
                    }
                    if q_a.router_id == q_b.router_id
                        && matches!(q_a.router_type, RouterType::V3)
                    {
                        continue;
                    }

                    let token_in = q_a.token_in;
                    let token_out = q_a.token_out;
                    let rb_addr = q_b.router_addr;
                    let fee_b = q_b.fee;

                    // Try local reverse quote — keys carried on ForwardTask from phase 1.
                    let local_back = match q_b.router_type {
                        RouterType::V2 | RouterType::SyncSwap => q_b.rev_v2_key.as_ref().and_then(|k| {
                            self.pool_cache.get_amount_out_by_v2_key(
                                k, token_out, token_out_amount, Some(VOLATILE_MAX_AGE),
                            )
                        }),
                        RouterType::Solidly | RouterType::Aerodrome => {
                            let max_age = if fee_b != 0 { STABLE_MAX_AGE } else { VOLATILE_MAX_AGE };
                            q_b.rev_v2_key.as_ref().and_then(|k| {
                                self.pool_cache.get_amount_out_by_v2_key(
                                    k, token_out, token_out_amount, Some(max_age),
                                )
                            })
                        }
                        RouterType::V3 => q_b.rev_v3_key.as_ref().and_then(|k| {
                            self.pool_cache
                                .quote_v3_spot_key(k, token_out, token_out_amount)
                                .map(|(spot_out, _, _)| spot_out)
                        }),
                    };

                    // All DEX types: stale/absent cache = no recent activity = no arb → skip.
                    // Check before constructing ReverseTask to avoid 2 String clones on the
                    // stale path (~20-50% of iterations depending on pool activity).
                    if local_back.is_none() {
                        continue;
                    }

                    rev_tasks.push(ReverseTask {
                        pair_idx: *pi,
                        pair_id: pair.id.clone(),
                        token_in_symbol: pair.token_in_symbol.clone(),
                        token_in_decimals: pair.token_in_decimals,
                        router_a_id: q_a.router_id.clone(),
                        router_a_addr: q_a.router_addr,
                        router_a_type: q_a.router_type.clone(),
                        fee_a: q_a.fee,
                        // Use the capped amount so spread/profit is relative to what was
                        // actually quoted (V3 may have been capped to single-tick capacity).
                        amount_in: q_a.capped_amount_in,
                        router_b_id: q_b.router_id.clone(),
                        router_b_addr: rb_addr,
                        router_b_type: q_b.router_type.clone(),
                        fee_b,
                        token_out_amount,
                        token_in,
                        token_out,
                        local_back,
                    });
                }
            }
        }

        // ── Find profitable opportunities ──────────────────────────────────────

        let mut opportunities = Vec::new();
        let mut best_raw_usd: f64 = 0.0;
        // Best signed spread ratio: (reverse_out / amount_in) - 1.0
        // Negative = below break-even; positive = profitable.
        let mut best_spread_pct: f64 = f64::NEG_INFINITY;

        // Phase 1.5 gate: collect V3 tick-capped forward tasks that show real cross-DEX
        // divergence. QuoterV2 fires only for these (0 RPC when market is quiet).
        const P15_GATE_SPREAD: f64 = 0.00005; // 0.005% — gate for Phase 1.5/1.5c QuoterV2 batches
        // Integer spread gate: amount_back/amount_in > 1.00005  (avoids float in hot loop)
        const P15_GATE_MUL: u64 = 1_000_050;
        const P15_GATE_DEN: u64 = 1_000_000;
        let mut p15_gate: std::collections::HashSet<(usize, String, u32)> = std::collections::HashSet::new();
        // Best QuoterV2-verified spread (signed). NEG_INFINITY when Phase 1.5 didn't run or
        // all reverse lookups failed. Exposed in heartbeat as "bestV=" to distinguish real
        // profit ($0.001 verified) from V3 spot phantom ($0.29 raw).
        let mut best_verified_spread: f64 = f64::NEG_INFINITY;

        for task in rev_tasks.iter() {
            // All tasks reaching here have local_back = Some(...); tasks with None were popped above.
            let amount_back = match task.local_back.filter(|b| !b.is_zero()) {
                Some(a) => a,
                None => continue,
            };
            // Sanity cap: profit > 5% of input is almost certainly a V3 spot
            // mismatch phantom (different routers' sqrtPriceX96 values compound
            // errors across the fwd/rev legs, producing billion-dollar phantoms).
            if amount_back > task.amount_in
                && (amount_back - task.amount_in).saturating_mul(U256::from(20)) > task.amount_in
            {
                warn!(
                    "[{}] 2-hop phantom skipped: {} | profit/input > 5% | {}/{}",
                    self.chain_id, task.pair_id, task.router_a_id, task.router_b_id
                );
                continue;
            }

            // Track signed spread % for all non-phantom quotes (even unprofitable ones).
            // Must be after the sanity cap to prevent V3 spot mismatch artifacts from
            // corrupting the spread metric with astronomical values.
            if !task.amount_in.is_zero() {
                let spread = u256_to_f64(amount_back) / u256_to_f64(task.amount_in) - 1.0;
                if spread > best_spread_pct {
                    best_spread_pct = spread;
                }

                // Phase 1.5 gate: local cross-DEX spread is promising AND forward router is V3.
                // Fire for ALL V3 forwards with spread > threshold (tick-capped spot when pool is thin).
                // QuoterV2 verifies the actual on-chain amount at full trade size. Without this,
                // liquid pools (safe_cap >= trade_amount, task.amount_in == full) never trigger
                // Phase 1.5 and V3×V3 spreads are permanently blocked from all_opportunities.
                let p15_gate_hit = amount_back.saturating_mul(U256::from(P15_GATE_DEN))
                    > task.amount_in.saturating_mul(U256::from(P15_GATE_MUL));
                if p15_gate_hit && matches!(task.router_a_type, RouterType::V3) {
                    p15_gate.insert((task.pair_idx, task.router_a_id.clone(), task.fee_a));
                }
            }

            let amount_back = apply_exec_quote_haircut(amount_back);
            if amount_back > task.amount_in {
                let profit = amount_back - task.amount_in;
                let profit_usd = token_amount_to_usd(
                    profit,
                    task.token_in_decimals,
                    &task.token_in_symbol,
                    self.native_price_usd,
                );

                // Track best profit seen regardless of threshold (for status logging)
                if profit_usd > best_raw_usd {
                    best_raw_usd = profit_usd;
                }

                // Any route with a V3 forward leg: the sqrtPriceX96 virtual-reserve
                // approximation can overestimate by 0.01–0.1% (single-tick math vs
                // actual multi-tick execution + fee). This creates phantom spreads on
                // V3→V2 and V3→V3 routes that consistently fail pre-flight.
                // Phase 1.5 handles ALL V3-forward routes via QuoterV2, which gives
                // the exact on-chain output. Letting them through here creates a second
                // unverified entry alongside the Phase 1.5 verified one — causing double
                // pre-flight attempts and cooldown spam for the same conceptual route.
                let router_a_is_v3 = matches!(task.router_a_type, RouterType::V3);
                // V2→V3 routes need Phase 1.5c QuoterV2 verification for the V3 reverse leg.
                // quote_v3_spot overestimates output in concentrated pools crossing multiple
                // ticks — same problem Phase 1.5b solves for V3→V3.
                let router_b_is_v3 = matches!(task.router_b_type, RouterType::V3);

                // Optimistic mode: bypass QuoterV2 and submit immediately on Phase 1 spot quotes.
                // Disable by setting optimistic_submission: false in config.yaml.
                let v3_allowed = self.optimistic_submission || (!router_a_is_v3 && !router_b_is_v3);
                let pair_min_profit = self
                    .pair_resolved
                    .get(task.pair_idx)
                    .and_then(|o| o.as_ref())
                    .map(|r| r.min_profit_usd)
                    .unwrap_or(self.min_profit_usd);
                if profit_usd >= pair_min_profit && v3_allowed {
                    debug!(
                        "[{}] Arb: {} | profit=${:.4} | {}/{}",
                        self.chain_id,
                        task.pair_id,
                        profit_usd,
                        task.router_a_id,
                        task.router_b_id
                    );
                    opportunities.push(ArbOpportunity {
                        chain_id: self.chain_id,
                        pair_id: task.pair_id.clone(),
                        token_in: task.token_in,
                        token_out: task.token_out,
                        amount_in: task.amount_in,
                        router_a: task.router_a_addr,
                        router_b: task.router_b_addr,
                        router_a_type: task.router_a_type.clone(),
                        router_b_type: task.router_b_type.clone(),
                        fee_a: task.fee_a,
                        fee_b: task.fee_b,
                        expected_profit: profit,
                        profit_usd,
                        router_a_id: task.router_a_id.clone(),
                        router_b_id: task.router_b_id.clone(),
                    });
                }
            }
        }

        // ── Phase 1.5: Gated V3 QuoterV2 upgrade ─────────────────────────────────────
        // Fires only when Phase 2 local data showed cross-DEX spread > P15_GATE_SPREAD
        // on a V3-tick-capped forward task. One batched multicall for all candidates.
        let mut t_after_p15: u128 = 0;
        // p15b state declared here so it can be consumed by the parallel fire section
        // below, which runs outside the p15_gate block.
        let mut p15b_candidates: Vec<(usize, String, u32, Address, Address, Address, U256, ForwardTask, U256)> = Vec::new();
        let mut p15b_mc: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut p15b_pre: Vec<Option<U256>> = Vec::new();
        if !p15_gate.is_empty() && !self.optimistic_submission {
            let mut p15_list: Vec<(usize, String, u32, Address, Address, Address, U256)> = Vec::new();
            let mut p15_mc: Vec<(Address, Vec<u8>)> = Vec::new();
            // For each p15_list[i]: Some(v) = cache hit (v=ZERO means failed/skip),
            // None = cache miss → has a corresponding entry in p15_mc.
            let mut p15_pre: Vec<Option<U256>> = Vec::new();

            for (pi, router_a_id, fee_a) in &p15_gate {
                let resolved = match self.pair_resolved.get(*pi).and_then(|o| o.as_ref()) {
                    Some(r) => r,
                    None => continue,
                };
                let full_amount = resolved.amount_in;
                let fwd_task = match pair_quotes.get(pi)
                    .and_then(|ts| ts.iter().find(|t| &t.router_id == router_a_id && t.fee == *fee_a))
                {
                    Some(t) => t, None => continue,
                };
                let quoter = match fwd_task.quoter_addr { Some(q) => q, None => continue };

                let v3_cache_key = fwd_task
                    .v3_key
                    .clone()
                    .unwrap_or_else(|| V3PoolKey::new(router_a_id, fwd_task.token_in, fwd_task.token_out, *fee_a));

                // Hit condition: current sqrtPriceX96 == sqrtPriceX96 stored in p15_cache.
                // A V3 Swap event updates pool_cache.sqrt_price_x96 → cache auto-invalidates.
                let cached = self.pool_cache.v3_by_key.get(&v3_cache_key)
                    .and_then(|pool_addr| self.pool_cache.v3_by_address.get(&*pool_addr))
                    .and_then(|state| {
                        self.p15_cache.get(&v3_cache_key)
                            .filter(|e| e.0 == state.sqrt_price_x96)
                            .map(|e| e.1)
                    });

                p15_list.push((*pi, router_a_id.clone(), *fee_a, fwd_task.router_addr,
                               fwd_task.token_in, fwd_task.token_out, full_amount));

                if cached.is_some() {
                    p15_pre.push(cached);       // cache hit — no HTTP call needed
                } else {
                    p15_pre.push(None);         // cache miss — queue for MC
                    let cd = IQuoterV2::quoteExactInputSingleCall {
                        params: IQuoterV2::QuoteExactInputSingleParams {
                            tokenIn: fwd_task.token_in,
                            tokenOut: fwd_task.token_out,
                            amountIn: full_amount,
                            fee: Uint::from(*fee_a),
                            sqrtPriceLimitX96: Uint::ZERO,
                        },
                    }.abi_encode();
                    p15_mc.push((quoter, cd));
                }
            }

            let cache_hits  = p15_pre.iter().filter(|v| v.is_some()).count();
            let cache_misses = p15_mc.len();
            if cache_hits > 0 || cache_misses > 0 {
                if cache_misses > 0 {
                    debug!(
                        "[chain={}] Phase 1.5 (gated): {} QuoterV2 call(s) + {} from cache — local spread > {:.1}%",
                        self.chain_id, cache_misses, cache_hits, P15_GATE_SPREAD * 100.0
                    );
                } else {
                    debug!(
                        "[chain={}] Phase 1.5 (gated): all {} from cache — 0 HTTP calls",
                        self.chain_id, cache_hits
                    );
                }
                let p15_raw = if cache_misses > 0 {
                    if let Some(ref hp) = self.http_provider {
                        run_multicall(hp, p15_mc, self.rpc_concurrency).await
                    } else {
                        run_multicall(provider, p15_mc, self.rpc_concurrency).await
                    }
                } else {
                    vec![]
                };
                t_after_p15 = scan_start.elapsed().as_millis();

                // Merge cache hits + MC results into final quoter_outs.
                // MC results are also stored in p15_cache keyed on current sqrtPriceX96
                // so the next scan with unchanged pool state gets a free cache hit.
                let mut mc_raw_iter = p15_raw.into_iter();
                let p15_quoter_outs: Vec<U256> = p15_list.iter()
                    .zip(p15_pre.iter())
                    .map(|((_, router_a_id, fee_a, _, token_in, token_out, _), pre)| {
                        if let Some(cached_val) = pre {
                            *cached_val  // served from cache — 0 HTTP
                        } else {
                            let raw_opt = mc_raw_iter.next().unwrap_or(None);
                            let result = raw_opt.as_ref()
                                .and_then(|raw| IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(raw).ok())
                                .filter(|r| !r.amountOut.is_zero())
                                .map(|r| r.amountOut)
                                .unwrap_or(U256::ZERO);
                            let key = V3PoolKey::new(router_a_id, *token_in, *token_out, *fee_a);
                            if let Some(sqrtp) = self.pool_cache.v3_by_key.get(&key)
                                .and_then(|pa| self.pool_cache.v3_by_address.get(&*pa))
                                .map(|s| s.sqrt_price_x96)
                            {
                                self.p15_cache.insert(key, (sqrtp, result));
                            }
                            result
                        }
                    })
                    .collect();

                // V3 router_b candidates that need a second QuoterV2 pass.
                // Local sqrtPriceX96 approximation overestimates V3 reverse legs by 0.01–0.1%,
                // causing phantom spreads that always fail pre-flight. Phase 1.5b runs a second
                // batched QuoterV2 multicall for these to get exact on-chain amounts.
                // p15b_* Vecs are declared above (outside p15_gate block) so they survive
                // into the parallel fire section below.

                for ((pi, router_a_id, fee_a, router_a_addr, token_in, token_out, full_amount), quoter_out)
                    in p15_list.into_iter().zip(p15_quoter_outs.into_iter())
                {
                    if quoter_out.is_zero() { continue; }

                    let pair = &self.pairs[pi];
                    let q_b_list: Vec<ForwardTask> = match pair_quotes.get(&pi) {
                        Some(ts) => ts.iter()
                            .filter(|t| t.router_id != router_a_id)
                            .cloned()
                            .collect(),
                        None => continue,
                    };

                    for q_b in q_b_list {
                        let fee_b = q_b.fee;
                        // V3 router_b: check p15b_cache first, otherwise defer to
                        // Phase 1.5b QuoterV2 batch. Cache key matches v3_by_key format
                        // for the reverse direction (tokenIn=token_out, tokenOut=token_in).
                        // Hit requires BOTH pool B sqrtPrice AND quoter_out (input) to match.
                        if matches!(q_b.router_type, RouterType::V3) {
                            if let Some(quoter_b) = q_b.quoter_addr {
                                let p15b_key = q_b
                                    .rev_v3_key
                                    .clone()
                                    .unwrap_or_else(|| V3PoolKey::new(&q_b.router_id, token_out, token_in, fee_b));
                                let cached_b = self.pool_cache.v3_by_key.get(&p15b_key)
                                    .and_then(|pa| self.pool_cache.v3_by_address.get(&*pa))
                                    .and_then(|state| {
                                        self.p15b_cache.get(&p15b_key)
                                            .filter(|e| e.0 == state.sqrt_price_x96 && e.1 == quoter_out)
                                            .map(|e| e.2)
                                    });
                                p15b_candidates.push((pi, router_a_id.clone(), fee_a, router_a_addr,
                                                      token_in, token_out, full_amount, q_b.clone(), quoter_out));
                                if cached_b.is_some() {
                                    p15b_pre.push(cached_b);
                                } else {
                                    p15b_pre.push(None);
                                    let cd = IQuoterV2::quoteExactInputSingleCall {
                                        params: IQuoterV2::QuoteExactInputSingleParams {
                                            tokenIn: token_out,
                                            tokenOut: token_in,
                                            amountIn: quoter_out,
                                            fee: Uint::from(fee_b),
                                            sqrtPriceLimitX96: Uint::ZERO,
                                        },
                                    }.abi_encode();
                                    p15b_mc.push((quoter_b, cd));
                                }
                            }
                            continue;
                        }
                        let local_back = match q_b.router_type {
                            RouterType::V2 | RouterType::SyncSwap => q_b.rev_v2_key.as_ref().and_then(|k| {
                                self.pool_cache.get_amount_out_by_v2_key(
                                    k, token_out, quoter_out, Some(VOLATILE_MAX_AGE),
                                )
                            }),
                            RouterType::Solidly | RouterType::Aerodrome => {
                                let max_age = if fee_b != 0 { STABLE_MAX_AGE } else { VOLATILE_MAX_AGE };
                                q_b.rev_v2_key.as_ref().and_then(|k| {
                                    self.pool_cache.get_amount_out_by_v2_key(
                                        k, token_out, quoter_out, Some(max_age),
                                    )
                                })
                            }
                            RouterType::V3 => unreachable!(),
                        };
                        let amount_back = match local_back.filter(|b| !b.is_zero()) {
                            Some(b) => b, None => continue,
                        };

                        // Phantom check FIRST: >5% positive spread is physically impossible
                        // for real arb — indicates stale Solidly/SyncSwap reserves or V3 spot
                        // mismatch. Reject before updating any metrics.
                        if amount_back > full_amount
                            && (amount_back - full_amount).saturating_mul(U256::from(20)) > full_amount
                        {
                            warn!("[{}] Phase 1.5 phantom: {} | {}/{}", self.chain_id, pair.id,
                                  router_a_id, q_b.router_id);
                            continue;
                        }

                        // Apply P15_QUOTER_HAIRCUT_BPS buffer (Phase 1.5: V3 forward + Aerodrome/V2 reverse).
                        // Aerodrome reverse uses local pool cache (VOLATILE_MAX_AGE=120s).
                        // Active pools get Sync events every few blocks; haircut covers quote→inclusion drift.
                        let amount_back = apply_exec_quote_haircut(amount_back);

                        // Track verified spread for ALL non-phantom results, including losses
                        // (negative value shows QuoterV2 confirmed the spread is a loss).
                        let verified_spread = u256_to_f64(amount_back) / u256_to_f64(full_amount) - 1.0;
                        if verified_spread > best_verified_spread { best_verified_spread = verified_spread; }

                        if amount_back <= full_amount { continue; }
                        let profit = amount_back - full_amount;
                        let profit_usd = token_amount_to_usd(
                            profit, pair.token_in_decimals, &pair.token_in_symbol, self.native_price_usd,
                        );

                        let spread = u256_to_f64(amount_back) / u256_to_f64(full_amount) - 1.0;
                        if spread > best_spread_pct { best_spread_pct = spread; }
                        if profit_usd > best_raw_usd { best_raw_usd = profit_usd; }

                        info!(
                            "[{}] Phase 1.5 result: {} | {}/{} | quoter_out={} amount_back={} profit_usd=${:.4} (min=${:.2})",
                            self.chain_id, pair.id, router_a_id, q_b.router_id,
                            quoter_out, amount_back, profit_usd, pair.min_profit_usd.unwrap_or(self.min_profit_usd)
                        );
                        if profit_usd >= pair.min_profit_usd.unwrap_or(self.min_profit_usd) {
                            info!("[{}] Phase 1.5 arb: {} | profit=${:.4} | {}/{}",
                                   self.chain_id, pair.id, profit_usd, router_a_id, q_b.router_id);
                            opportunities.push(ArbOpportunity {
                                chain_id: self.chain_id,
                                pair_id: pair.id.clone(),
                                token_in,
                                token_out,
                                amount_in: full_amount,
                                router_a: router_a_addr,
                                router_b: q_b.router_addr,
                                router_a_type: RouterType::V3,
                                router_b_type: q_b.router_type.clone(),
                                fee_a,
                                fee_b,
                                expected_profit: profit,
                                profit_usd,
                                router_a_id: router_a_id.clone(),
                                router_b_id: q_b.router_id.clone(),
                            });
                        }
                    }
                }

            }
        }

        // ── Phase 1.5c candidate collection ──────────────────────────────────────────
        // Collected before firing so it can run concurrently with Phase 1.5b below.
        // Local-forward (V2/Solidly/Aerodrome) / V3-reverse routes: quote_v3_spot overestimates
        // multi-tick output. QuoterV2 verifies the exact V3 reverse output before submission.
        let mut p15c_mc: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut p15c_candidates: Vec<(usize, String, u32, Address, RouterType, Address, Address, U256, u32, Address, String)> = Vec::new();
        {
            for task in &rev_tasks {
                let router_a_is_local = matches!(task.router_a_type, RouterType::V2 | RouterType::Solidly | RouterType::Aerodrome);
                if !router_a_is_local { continue; }
                if !matches!(task.router_b_type, RouterType::V3) { continue; }

                let local_back = match task.local_back { Some(v) => v, None => continue };
                let p15_gate_hit = local_back.saturating_mul(U256::from(P15_GATE_DEN))
                    > task.amount_in.saturating_mul(U256::from(P15_GATE_MUL));
                if !p15_gate_hit { continue; }

                let full_amount = match self
                    .pair_resolved
                    .get(task.pair_idx)
                    .and_then(|o| o.as_ref())
                    .map(|r| r.amount_in)
                {
                    Some(a) => a,
                    None => continue,
                };

                // Find quoter for the V3 reverse router from forward quotes of this pair
                let quoter_b = match pair_quotes.get(&task.pair_idx)
                    .and_then(|ts| ts.iter().find(|t| t.router_id == task.router_b_id && t.fee == task.fee_b))
                    .and_then(|t| t.quoter_addr)
                {
                    Some(q) => q, None => continue,
                };

                let cd = IQuoterV2::quoteExactInputSingleCall {
                    params: IQuoterV2::QuoteExactInputSingleParams {
                        tokenIn:           task.token_out,
                        tokenOut:          task.token_in,
                        amountIn:          task.token_out_amount,
                        fee:               Uint::from(task.fee_b),
                        sqrtPriceLimitX96: Uint::from(0u8),
                    },
                };
                p15c_mc.push((quoter_b, cd.abi_encode()));
                p15c_candidates.push((
                    task.pair_idx,
                    task.router_a_id.clone(), task.fee_a, task.router_a_addr,
                    task.router_a_type.clone(),
                    task.token_in, task.token_out, full_amount,
                    task.fee_b, task.router_b_addr, task.router_b_id.clone(),
                ));
            }

        }

        // ── Fire Phase 1.5b and Phase 1.5c in parallel ───────────────────────────────
        // Both are independent: 1.5b needs Phase 1.5 output (already computed above);
        // 1.5c needs only rev_tasks (collected just above). Running them concurrently
        // saves ~50–80ms when both fire in the same scan.
        let p15b_cache_hits  = p15b_pre.iter().filter(|v| v.is_some()).count();
        let p15b_cache_misses = p15b_mc.len();
        if p15b_cache_hits > 0 || p15b_cache_misses > 0 {
            if p15b_cache_misses > 0 {
                debug!("[chain={}] Phase 1.5b: {} V3×V3 reverse quote(s) + {} from cache",
                       self.chain_id, p15b_cache_misses, p15b_cache_hits);
            } else {
                debug!("[chain={}] Phase 1.5b: all {} from p15b_cache — 0 HTTP calls",
                       self.chain_id, p15b_cache_hits);
            }
        }
        if !p15c_mc.is_empty() {
            debug!("[chain={}] Phase 1.5c: {} local→V3 reverse quote(s)", self.chain_id, p15c_mc.len());
        }

        let (p15b_raw_mc, p15c_raw) = if let Some(ref hp) = self.http_provider {
            tokio::join!(
                async { if p15b_cache_misses > 0 { run_multicall(hp, p15b_mc, self.rpc_concurrency).await } else { vec![] } },
                async { if !p15c_mc.is_empty() { run_multicall(hp, p15c_mc, self.rpc_concurrency).await } else { vec![] } },
            )
        } else {
            tokio::join!(
                async { if p15b_cache_misses > 0 { run_multicall(provider, p15b_mc, self.rpc_concurrency).await } else { vec![] } },
                async { if !p15c_mc.is_empty() { run_multicall(provider, p15c_mc, self.rpc_concurrency).await } else { vec![] } },
            )
        };

        // ── Process Phase 1.5b results ────────────────────────────────────────────────
        {
            let mut mc_iter = p15b_raw_mc.into_iter();
            for ((pi, router_a_id, fee_a, router_a_addr, token_in, token_out, full_amount, q_b, quoter_out), pre)
                in p15b_candidates.into_iter().zip(p15b_pre.into_iter())
            {
                let amount_back = if let Some(cached_val) = pre {
                    if cached_val.is_zero() { continue; }
                    cached_val
                } else {
                    let raw_opt = mc_iter.next().unwrap_or(None);
                    let result = raw_opt.as_ref()
                        .and_then(|raw| IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(raw).ok())
                        .filter(|r| !r.amountOut.is_zero())
                        .map(|r| r.amountOut)
                        .unwrap_or(U256::ZERO);
                    let p15b_key = q_b
                        .rev_v3_key
                        .clone()
                        .unwrap_or_else(|| V3PoolKey::new(&q_b.router_id, token_out, token_in, q_b.fee));
                    if let Some(sqrtp_b) = self.pool_cache.v3_by_key.get(&p15b_key)
                        .and_then(|pa| self.pool_cache.v3_by_address.get(&*pa))
                        .map(|s| s.sqrt_price_x96)
                    {
                        self.p15b_cache.insert(p15b_key, (sqrtp_b, quoter_out, result));
                    }
                    if result.is_zero() { continue; }
                    result
                };

                // Phantom check FIRST (same logic as Phase 1.5 non-b).
                if amount_back > full_amount
                    && (amount_back - full_amount).saturating_mul(U256::from(20)) > full_amount
                {
                    warn!("[{}] Phase 1.5b phantom: {} | {}/{}", self.chain_id,
                          self.pairs[pi].id, router_a_id, q_b.router_id);
                    continue;
                }

                let amount_back = apply_exec_quote_haircut(amount_back);

                // Track verified spread for ALL non-phantom results (including losses).
                // Both legs are QuoterV2-confirmed here — most accurate signal available.
                let v_spread_1b = u256_to_f64(amount_back) / u256_to_f64(full_amount) - 1.0;
                if v_spread_1b > best_verified_spread { best_verified_spread = v_spread_1b; }

                if amount_back <= full_amount { continue; }
                let profit = amount_back - full_amount;
                let pair = &self.pairs[pi];
                let profit_usd = token_amount_to_usd(
                    profit, pair.token_in_decimals, &pair.token_in_symbol, self.native_price_usd,
                );

                let spread = u256_to_f64(amount_back) / u256_to_f64(full_amount) - 1.0;
                if spread > best_spread_pct { best_spread_pct = spread; }
                if profit_usd > best_raw_usd { best_raw_usd = profit_usd; }

                info!(
                    "[{}] Phase 1.5b result: {} | {}/{} | amount_back={} profit_usd=${:.4} (min=${:.2})",
                    self.chain_id, pair.id, router_a_id, q_b.router_id,
                    amount_back, profit_usd, pair.min_profit_usd.unwrap_or(self.min_profit_usd)
                );
                if profit_usd >= pair.min_profit_usd.unwrap_or(self.min_profit_usd) {
                    info!("[{}] Phase 1.5b V3×V3 arb: {} | profit=${:.4} | {}/{}",
                           self.chain_id, pair.id, profit_usd, router_a_id, q_b.router_id);
                    opportunities.push(ArbOpportunity {
                        chain_id: self.chain_id,
                        pair_id: pair.id.clone(),
                        token_in,
                        token_out,
                        amount_in: full_amount,
                        router_a: router_a_addr,
                        router_b: q_b.router_addr,
                        router_a_type: RouterType::V3,
                        router_b_type: RouterType::V3,
                        fee_a,
                        fee_b: q_b.fee,
                        expected_profit: profit,
                        profit_usd,
                        router_a_id: router_a_id.clone(),
                        router_b_id: q_b.router_id.clone(),
                    });
                }
            }
        }

        // ── Process Phase 1.5c results ────────────────────────────────────────────────
        for ((pi, router_a_id, fee_a, router_a_addr, router_a_type, token_in, token_out, full_amount, fee_b, router_b_addr, router_b_id), raw_opt)
            in p15c_candidates.into_iter().zip(p15c_raw.into_iter())
        {
            let raw = match raw_opt { Some(r) => r, None => continue };
            let amount_back = match IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&raw) {
                Ok(r) if !r.amountOut.is_zero() => r.amountOut,
                _ => continue,
            };

            // Phantom check first (>5% positive spread = physically impossible)
            if amount_back > full_amount
                && (amount_back - full_amount).saturating_mul(U256::from(20)) > full_amount
            {
                warn!("[{}] Phase 1.5c phantom: {}/{}", self.chain_id, router_a_id, router_b_id);
                continue;
            }

            // Forward leg uses local pool cache (VOLATILE_MAX_AGE=120s); V3 reverse is QuoterV2-exact.
            let amount_back = apply_exec_quote_haircut(amount_back);

            let v_spread = u256_to_f64(amount_back) / u256_to_f64(full_amount) - 1.0;
            if v_spread > best_verified_spread { best_verified_spread = v_spread; }

            if amount_back <= full_amount { continue; }
            let profit = amount_back - full_amount;
            let pair = &self.pairs[pi];
            let profit_usd = token_amount_to_usd(
                profit, pair.token_in_decimals, &pair.token_in_symbol, self.native_price_usd,
            );
            let spread = u256_to_f64(amount_back) / u256_to_f64(full_amount) - 1.0;
            if spread > best_spread_pct { best_spread_pct = spread; }
            if profit_usd > best_raw_usd { best_raw_usd = profit_usd; }

            info!(
                "[{}] Phase 1.5c result: {} | {}/{} | amount_back={} profit_usd=${:.4} (min=${:.2})",
                self.chain_id, pair.id, router_a_id, router_b_id, amount_back, profit_usd, pair.min_profit_usd.unwrap_or(self.min_profit_usd)
            );
            if profit_usd >= pair.min_profit_usd.unwrap_or(self.min_profit_usd) {
                info!("[{}] Phase 1.5c arb: {} | profit=${:.4} | {}/{}",
                       self.chain_id, pair.id, profit_usd, router_a_id, router_b_id);
                opportunities.push(ArbOpportunity {
                    chain_id: self.chain_id,
                    pair_id: pair.id.clone(),
                    token_in, token_out,
                    amount_in: full_amount,
                    router_a: router_a_addr,
                    router_b: router_b_addr,
                    router_a_type,
                    router_b_type: RouterType::V3,
                    fee_a, fee_b,
                    expected_profit: profit,
                    profit_usd,
                    router_a_id,
                    router_b_id,
                });
            }
        }

        opportunities.sort_by(|a, b| b.expected_profit.cmp(&a.expected_profit));

        // ── Update session opp counts and pair scan snapshot ─────────────────────
        // Only update pair_scan on full scans (block/poll — not swap-event targeted scans).
        // Targeted swap-event scans only evaluate 1-2 pairs, so rebuilding the full
        // snapshot would mark every other pair as "not quoted" — corrupting the data.
        // is_full_scan is passed explicitly so this works correctly even when pair_mask
        // is Some() due to disabled-pair filtering (which must not suppress the rebuild).
        {
            let mut opp_counts = self.opp_session_counts.lock().unwrap();
            for opp in &opportunities {
                *opp_counts.entry(opp.pair_id.clone()).or_insert(0) += 1;
            }

            if is_full_scan {
                let mut scan = self.pair_scan.lock().unwrap();
                scan.clear();

                // Pairs with at least one forward quote
                let mut seen_pis = std::collections::HashSet::new();
                for (pi, quotes) in &pair_quotes {
                    seen_pis.insert(*pi);
                    let pair = &self.pairs[*pi];
                    let mut dex_set = std::collections::HashSet::new();
                    for q in quotes { dex_set.insert(q.router_id.clone()); }
                    let dex_ids: Vec<String> = dex_set.into_iter().collect();
                    let dex_count = dex_ids.len();
                    // cross_count = number of ordered (A,B) pairs where A≠B, i.e. k*(k-1)
                    let cross_count = if dex_count >= 2 { dex_count * (dex_count - 1) } else { 0 };
                    let display_name = format!("{}→{}", pair.token_in_symbol, pair.token_out_symbol);
                    scan.push(PairScanInfo {
                        pair_id: pair.id.clone(),
                        chain_id: pair.chain_id,
                        display_name,
                        dex_count,
                        dex_ids,
                        cross_count,
                        was_quoted: true,
                        opp_count: *opp_counts.get(&pair.id).unwrap_or(&0),
                        disabled: false,
                    });
                }

                // Pairs with no quote this scan (cache miss or disabled)
                for &pi in &self.chain_pair_indices {
                    let pair = &self.pairs[pi];
                    if seen_pis.contains(&pi) { continue; }
                    let display_name = format!("{}→{}", pair.token_in_symbol, pair.token_out_symbol);
                    scan.push(PairScanInfo {
                        pair_id: pair.id.clone(),
                        chain_id: pair.chain_id,
                        display_name,
                        dex_count: 0,
                        dex_ids: vec![],
                        cross_count: 0,
                        was_quoted: false,
                        opp_count: *opp_counts.get(&pair.id).unwrap_or(&0),
                        disabled: false,
                    });
                }
            }
        }

        debug!(
            "[chain={}] scan timing: total={}ms p1.5b_start={}ms opps={}",
            self.chain_id, scan_start.elapsed().as_millis(), t_after_p15,
            opportunities.len()
        );

        let min_floor = self.min_profit_usd;
        if opportunities.is_empty() && best_raw_usd > 0.0 && best_raw_usd >= min_floor {
            warn!(
                chain_id = self.chain_id,
                best_raw_usd,
                min_profit_usd = min_floor,
                "evaluate: raw profit ≥ min but 0 opportunities (phantom cap, post-Quoter buffer, or router gating)",
            );
        }

        (opportunities, best_raw_usd, best_verified_spread, total_fwd_ok, pairs_with_multi, best_spread_pct, pairs_with_any)
    }
}
