use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use tracing::{debug, info, warn};
use std::sync::atomic::Ordering::Relaxed;

use crate::config::RouterType;
use crate::pool_cache::{V2PoolKey, V3PoolKey, volatile_max_age, stable_max_age};
use crate::types::{ArbOpportunity, PairScanInfo};
use crate::util::u256_to_f64;
use super::{ForwardTask, QuoteCache, QuoterBatchHealth, ReverseTask, Strategy, apply_exec_quote_haircut, decode_v3_quote, run_multicall_answered, token_amount_to_usd, min_v3_liquidity};

/// Phase 1.5 gate: true when a Phase 2 route should be QuoterV2-verified at full size.
/// Requires a V3 forward leg, a local spread above the gate (`amount_back / capped_in >
/// mul / den`, integer math) and an uncapped forward leg (`capped_in == full_in`): a leg
/// tick-capped below the full size can't take the full size at spot.
pub(super) fn p15_gate_eligible(
    amount_back: U256,
    capped_in: U256,
    full_in: Option<U256>,
    fwd: &RouterType,
    mul: u64,
    den: u64,
) -> bool {
    matches!(fwd, RouterType::V3)
        && full_in == Some(capped_in)
        && amount_back.saturating_mul(U256::from(den)) > capped_in.saturating_mul(U256::from(mul))
}

/// True when a 2-hop route may pair leg A with leg B. Legs on different router contracts
/// always may. On the same router contract only two different V3 fee tiers (or Slipstream
/// tick spacings) of one router may: both legs are QuoterV2-verified at full size (1.5 →
/// 1.5b), so same-router spot phantoms never become opportunities — hence `verified`
/// (false under `optimistic_submission`, where spot V3 routes are submitted directly).
/// Still excluded: the identical pool (same router and fee), and Aerodrome volatile ×
/// stable, which share one router_addr and fail on-chain (InsufficientOutputAmount).
pub(super) fn route_legs_eligible(
    (a_id, a_addr, a_type, a_fee): (&str, Address, &RouterType, u32),
    (b_id, b_addr, b_type, b_fee): (&str, Address, &RouterType, u32),
    verified: bool,
) -> bool {
    if a_addr != b_addr {
        return true;
    }
    verified
        && a_id == b_id
        && a_fee != b_fee
        && matches!(a_type, RouterType::V3)
        && matches!(b_type, RouterType::V3)
}

/// True when the local V3 spot quote of `amount_in` on `key` is not tick-capped
/// (the pool's current tick can absorb the whole amount). Missing pool state = false.
/// Speculative 1.5b request: (pair index, router A, fee A, reverse router, reverse fee, input,
/// reverse pool state at request time).
type SpecKey = (usize, String, u32, String, u32, U256, Option<(U256, u128)>);

fn v3_spot_uncapped(cache: &crate::pool_cache::PoolCache, key: &V3PoolKey, token_in: Address, amount_in: U256) -> bool {
    cache
        .quote_v3_spot_key(key, token_in, amount_in)
        .is_some_and(|(_, _, effective_in)| effective_in == amount_in)
}

/// Shared QuoteCache pre-check for one QuoterV2 request. Hit: pushes `Some(v)` to `pre`
/// and returns false. Miss: pushes `None` to `pre` and the pool's current price state to
/// `sent_state` (see [`QuoteCache::insert`]) and returns true — the caller queues the call.
fn quote_cache_precheck(
    quote_cache: &QuoteCache,
    pools: &crate::pool_cache::PoolCache,
    key: &V3PoolKey,
    amount_in: U256,
    pre: &mut Vec<Option<U256>>,
    sent_state: &mut Vec<Option<(U256, u128)>>,
) -> bool {
    match quote_cache.get(pools, key, amount_in) {
        Some(v) => {
            pre.push(Some(v));
            false
        }
        None => {
            pre.push(None);
            sent_state.push(pools.v3_price_state(key));
            true
        }
    }
}

impl Strategy {
    /// Evaluate all pairs on all router combinations for arb opportunities.
    /// All quotes run concurrently (two phases: forward then reverse).
    /// Returns `(opportunities, best_raw_profit_usd, best_verified_spread, fwd_quotes_ok, pairs_with_multi, best_spread_pct, pairs_with_any)`:
    /// - `best_raw_profit_usd`: highest profit even if below min_profit_usd (for status logging).
    ///   Like `best_spread_pct`, it skips unverified V3 spot routes (only their QuoterV2
    ///   results count), so it never shows an edge that can't become an opportunity.
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
        self.timings.p15_ms.store(0, Relaxed);
        self.timings.p15bc_ms.store(0, Relaxed);

        // ── Phase 1: Forward quotes ───────────────────────────────────────────────
        // V2 and Solidly-volatile pools use xy=k (constant-product AMM), so we compute
        // quotes locally from cached reserves — zero eth_calls.
        // V3 legs are quoted from the local V3 spot cache, then QuoterV2-verified.

        // Build target index list first so we can use .len() for capacity hints below.
        let masked_indices: Vec<usize>;
        let target_indices: &[usize] = match pair_mask {
            Some(m) => {
                masked_indices = m.iter().copied()
                    .filter(|&i| i < self.pairs.len())
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
                        // volatile_max_age() (300s) freshness check: V2 reserves CAN go stale
                        // between Sync events. Without a freshness check, reserves from the
                        // last trade 10+ minutes ago produce phantom opportunities — the local
                        // xy=k formula is exact given correct reserves, but stale reserves
                        // overestimate output and cause on-chain "too little received" reverts.
                        let v2_key = V2PoolKey::new(&router.id, token_in, token_out);
                        if let Some(out) = self.pool_cache.get_amount_out_by_v2_key(
                            &v2_key, token_in, amount_in, Some(volatile_max_age()),
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
                                if liquidity < min_v3_liquidity() {
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
                    RouterType::Aerodrome => {
                        // Both volatile (fee=0) and stable (fee=1) pools use local reserve cache.
                        // Only adds to pair_quotes when the pool has had a recent Sync event
                        // (last_sync.elapsed() < max_age). Stale = no on-chain activity = skip.
                        // Aerodrome pools emit Sync(uint256,uint256) and use xy=k (volatile)
                        // or x³y+y³x=k (stable) curves.
                        let rtype = router.router_type.clone();
                        let Some((vol_id, sta_id)) = self.solidly_pool_keys.get(&router.id) else {
                            continue;
                        };
                        for (eff_id, fee, max_age) in [
                            (vol_id.as_str(), 0u32, volatile_max_age()),
                            (sta_id.as_str(), 1u32, stable_max_age()),
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
        // V3 reverse legs are verified with QuoterV2 (Phase 1.5b/1.5c).

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

                    // Same router contract: only cross-fee-tier V3 routes, which 1.5/1.5b
                    // verify with QuoterV2 (local spot math alone phantoms between tiers).
                    if !route_legs_eligible(
                        (&q_a.router_id, q_a.router_addr, &q_a.router_type, q_a.fee),
                        (&q_b.router_id, q_b.router_addr, &q_b.router_type, q_b.fee),
                        !self.optimistic_submission,
                    ) {
                        continue;
                    }

                    let token_in = q_a.token_in;
                    let token_out = q_a.token_out;
                    let rb_addr = q_b.router_addr;
                    let fee_b = q_b.fee;

                    // Try local reverse quote — keys carried on ForwardTask from phase 1.
                    let local_back = match q_b.router_type {
                        RouterType::V2 => q_b.rev_v2_key.as_ref().and_then(|k| {
                            self.pool_cache.get_amount_out_by_v2_key(
                                k, token_out, token_out_amount, Some(volatile_max_age()),
                            )
                        }),
                        RouterType::Aerodrome => {
                            let max_age = if fee_b != 0 { stable_max_age() } else { volatile_max_age() };
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

        // Phase 1.5 gate: collect uncapped V3 forward tasks that show real cross-DEX
        // divergence. QuoterV2 fires only for these (0 RPC when market is quiet).
        let p15_gate_spread = crate::config::tuning().p15_gate_spread;
        // Integer spread gate: amount_back/amount_in > 1.00005  (avoids float in hot loop)
        let p15_gate_mul: u64 = 1_000_000 + (p15_gate_spread * 1_000_000.0).round() as u64;
        let p15_gate_den: u64 = 1_000_000;
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
                && crate::config::tuning().is_phantom(amount_back - task.amount_in, task.amount_in)
            {
                warn!(
                    "[{}] 2-hop phantom skipped: {} | profit/input > 5% | {}/{}",
                    self.chain_id, task.pair_id, task.router_a_id, task.router_b_id
                );
                continue;
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

            // Track signed spread % for all non-phantom quotes (even unprofitable ones).
            // Must be after the sanity cap to prevent V3 spot mismatch artifacts from
            // corrupting the spread metric with astronomical values. Unverified V3 spot
            // routes can never become opportunities here: they feed best=/spread= only via
            // their QuoterV2-verified results in 1.5/1.5b/1.5c.
            if !task.amount_in.is_zero() {
                let spread = u256_to_f64(amount_back) / u256_to_f64(task.amount_in) - 1.0;
                if v3_allowed && spread > best_spread_pct {
                    best_spread_pct = spread;
                }

                // Phase 1.5 gate: local cross-DEX spread is promising AND forward router is V3
                // AND its forward leg was not tick-capped. QuoterV2 verifies at full trade size;
                // liquid pools (task.amount_in == full) must reach it, or V3×V3 spreads are
                // permanently blocked. A capped forward can't take the full size at spot, so a
                // full-size QuoterV2 call there is wasted RPC (and the bestV -99.99% artefact).
                let full_in = self
                    .pair_resolved
                    .get(task.pair_idx)
                    .and_then(|o| o.as_ref())
                    .map(|r| r.amount_in);
                if p15_gate_eligible(
                    amount_back, task.amount_in, full_in, &task.router_a_type, p15_gate_mul, p15_gate_den,
                ) {
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

                // Track best profit seen regardless of threshold (for status logging);
                // unverified V3 spot routes excluded, as for the spread above.
                if v3_allowed && profit_usd > best_raw_usd {
                    best_raw_usd = profit_usd;
                }

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

        // ── Phase 1.5c candidate collection ──────────────────────────────────────────
        // Collected before Phase 1.5: it needs only Phase 2 data, so its QuoterV2 batch runs
        // concurrently with Phase 1.5's (and, if that made no call, with 1.5b's).
        // Local-forward (V2/Solidly/Aerodrome) / V3-reverse routes: quote_v3_spot overestimates
        // multi-tick output. QuoterV2 verifies the exact V3 reverse output before submission.
        let mut p15c_mc: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut p15c_candidates: Vec<(usize, String, u32, Address, RouterType, Address, Address, U256, u32, Address, String, U256)> = Vec::new();
        // Same QuoteCache rule as 1.5/1.5b: Some(v) = cache hit (ZERO = failed/skip), None = in p15c_mc.
        let mut p15c_pre: Vec<Option<U256>> = Vec::new();
        let mut p15c_sent_state: Vec<Option<(U256, u128)>> = Vec::new();
        {
            for task in &rev_tasks {
                let router_a_is_local = matches!(task.router_a_type, RouterType::V2 | RouterType::Aerodrome);
                if !router_a_is_local { continue; }
                if !matches!(task.router_b_type, RouterType::V3) { continue; }

                let local_back = match task.local_back { Some(v) => v, None => continue };
                let p15_gate_hit = local_back.saturating_mul(U256::from(p15_gate_den))
                    > task.amount_in.saturating_mul(U256::from(p15_gate_mul));
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

                let p15c_key = V3PoolKey::new(&task.router_b_id, task.token_out, task.token_in, task.fee_b);
                if quote_cache_precheck(&self.quote_cache, &self.pool_cache, &p15c_key, task.token_out_amount,
                                        &mut p15c_pre, &mut p15c_sent_state) {
                    p15c_mc.push((quoter_b, self.encode_quote(quoter_b, task.token_out, task.token_in, task.fee_b, task.token_out_amount)));
                }
                p15c_candidates.push((
                    task.pair_idx,
                    task.router_a_id.clone(), task.fee_a, task.router_a_addr,
                    task.router_a_type.clone(),
                    task.token_in, task.token_out, full_amount,
                    task.fee_b, task.router_b_addr, task.router_b_id.clone(),
                    task.token_out_amount,
                ));
            }

        }

        // Not polled until joined with Phase 1.5 (or 1.5b below); MaybeDone keeps the output
        // so the later join just collects it.
        let p15c_fut = futures::future::maybe_done(async {
            if p15c_mc.is_empty() {
                vec![]
            } else if let Some(ref hp) = self.http_provider {
                run_multicall_answered(hp, p15c_mc, self.rpc_concurrency, self.quote_block).await
            } else {
                run_multicall_answered(provider, p15c_mc, self.rpc_concurrency, self.quote_block).await
            }
        });
        tokio::pin!(p15c_fut);
        // Quiet V2/Aerodrome pools of the routes Phase 1.5 / 1.5c may verify: their reserves are
        // read alongside the QuoterV2 round (see Strategy::prefetch_quiet_reserves).
        let mut prefetch_pools: Vec<Address> = Vec::new();
        if !p15_gate.is_empty() && !self.optimistic_submission {
            for (pi, ..) in &p15_gate {
                for t in pair_quotes.get(pi).into_iter().flatten()
                    .filter(|t| matches!(t.router_type, RouterType::V2 | RouterType::Aerodrome))
                {
                    prefetch_pools.extend(crate::race::route_pools(&self.pool_cache, &[
                        (&t.router_id, t.router_type, t.fee, t.token_in, t.token_out),
                    ]));
                }
            }
        }
        for c in &p15c_candidates {
            prefetch_pools.extend(crate::race::route_pools(&self.pool_cache, &[(&c.1, c.4, c.2, c.5, c.6)]));
        }
        let prefetch_fut = futures::future::maybe_done(self.prefetch_quiet_reserves(provider, prefetch_pools));
        tokio::pin!(prefetch_fut);
        let p15c_queued = p15c_candidates.len() - p15c_pre.iter().filter(|v| v.is_some()).count();

        // ── Phase 1.5: Gated V3 QuoterV2 upgrade ─────────────────────────────────────
        // Fires only when Phase 2 local data showed cross-DEX spread > p15_gate_spread
        // on an uncapped V3 forward task. One batched multicall for all candidates.
        let mut t_after_p15: u128 = 0;
        // p15b state declared here so it can be consumed by the parallel fire section
        // below, which runs outside the p15_gate block.
        let mut p15b_candidates: Vec<(usize, String, u32, Address, Address, Address, U256, ForwardTask, U256)> = Vec::new();
        let mut p15b_mc: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut p15b_pre: Vec<Option<U256>> = Vec::new();
        // Pool (sqrtPriceX96, liquidity) when each p15b_mc request was built — see QuoteCache::insert.
        let mut p15b_sent_state: Vec<Option<(U256, u128)>> = Vec::new();
        // 1.5b reverse quotes answered by a speculative bracket instead of a second round trip.
        let mut p15b_spec_hits: usize = 0;
        if !p15_gate.is_empty() && !self.optimistic_submission {
            let mut p15_list: Vec<(usize, String, u32, Address, Address, Address, U256)> = Vec::new();
            let mut p15_mc: Vec<(Address, Vec<u8>)> = Vec::new();
            // Pool state when each p15_mc request was built — see QuoteCache::insert.
            let mut p15_sent_state: Vec<Option<(U256, u128)>> = Vec::new();
            // For each p15_list[i]: Some(v) = cache hit (v=ZERO means failed/skip),
            // None = cache miss → has a corresponding entry in p15_mc.
            let mut p15_pre: Vec<Option<U256>> = Vec::new();
            // Speculative 1.5b: each V3 reverse leg quoted at the forward leg's upper-bound
            // output and the bracket's lower point, in the Phase 1.5 batch. The real forward
            // output then usually falls between the two points, and the chord gives a lower
            // bound on the reverse output — no second round trip (see v3_leg_lower_bound).
            let mut spec_routes: Vec<SpecKey> = Vec::new();
            let mut spec_mc: Vec<(Address, Vec<u8>)> = Vec::new();

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

                // Shared QuoteCache rule: pool price + liquidity unchanged and entry fresh.
                let cached = self.quote_cache.get(&self.pool_cache, &v3_cache_key, full_amount);

                p15_list.push((*pi, router_a_id.clone(), *fee_a, fwd_task.router_addr,
                               fwd_task.token_in, fwd_task.token_out, full_amount));

                if cached.is_some() {
                    p15_pre.push(cached);       // cache hit — no HTTP call needed
                } else {
                    p15_pre.push(None);         // cache miss — queue for MC
                    p15_sent_state.push(self.pool_cache.v3_price_state(&v3_cache_key));
                    p15_mc.push((quoter, self.encode_quote(quoter, fwd_task.token_in, fwd_task.token_out, *fee_a, full_amount)));
                }

                let Some(hi) = self.pool_cache.quote_v3_marginal(&v3_cache_key, fwd_task.token_in, full_amount)
                    .filter(|a| !a.is_zero()) else { continue };
                let lo = hi * U256::from(10_000 - super::triangular::verify_bracket_bps()) / U256::from(10_000u32);
                for q_b in pair_quotes.get(pi).into_iter().flatten().filter(|t| {
                    matches!(t.router_type, RouterType::V3)
                        && route_legs_eligible(
                            (router_a_id, fwd_task.router_addr, &RouterType::V3, *fee_a),
                            (&t.router_id, t.router_addr, &t.router_type, t.fee),
                            true,
                        )
                        && t.rev_v3_key.as_ref().is_some_and(|k| v3_spot_uncapped(&self.pool_cache, k, fwd_task.token_out, hi))
                }) {
                    let Some(quoter_b) = q_b.quoter_addr else { continue };
                    let state = q_b.rev_v3_key.as_ref().and_then(|k| self.pool_cache.v3_price_state(k));
                    for amt in [hi, lo] {
                        spec_routes.push((*pi, router_a_id.clone(), *fee_a, q_b.router_id.clone(), q_b.fee, amt, state));
                        spec_mc.push((quoter_b, self.encode_quote(quoter_b, fwd_task.token_out, fwd_task.token_in, q_b.fee, amt)));
                    }
                }
            }
            // Only worth it when Phase 1.5 makes a round trip anyway.
            if p15_mc.is_empty() {
                spec_routes.clear();
                spec_mc.clear();
            }
            let spec_n = spec_mc.len();
            p15_mc.extend(spec_mc);

            let cache_hits  = p15_pre.iter().filter(|v| v.is_some()).count();
            let cache_misses = p15_mc.len();
            if cache_hits > 0 || cache_misses > 0 {
                if cache_misses > 0 {
                    debug!(
                        "[chain={}] Phase 1.5 (gated): {} QuoterV2 call(s) + {} from cache — local spread > {:.1}%",
                        self.chain_id, cache_misses, cache_hits, p15_gate_spread * 100.0
                    );
                } else {
                    debug!(
                        "[chain={}] Phase 1.5 (gated): all {} from cache — 0 HTTP calls",
                        self.chain_id, cache_hits
                    );
                }
                let p15_raw = if cache_misses > 0 {
                    let t = std::time::Instant::now();
                    let p15 = async {
                        if let Some(ref hp) = self.http_provider {
                            run_multicall_answered(hp, p15_mc, self.rpc_concurrency, self.quote_block).await
                        } else {
                            run_multicall_answered(provider, p15_mc, self.rpc_concurrency, self.quote_block).await
                        }
                    };
                    let (raw, (), ()) = tokio::join!(p15, p15c_fut.as_mut(), prefetch_fut.as_mut());
                    self.timings.p15_ms.store(t.elapsed().as_millis() as u64, Relaxed);
                    raw
                } else {
                    vec![]
                };
                t_after_p15 = scan_start.elapsed().as_millis();
                let mut p15_raw = p15_raw;
                let spec_raw = p15_raw.split_off(p15_raw.len().saturating_sub(spec_n));
                // (route, reverse router, fee) → reverse-leg QuoterV2 points (input, output), sorted.
                // Plus the reverse pool's state when they were requested (for the quote cache).
                let mut spec_points: std::collections::HashMap<(usize, String, u32, String, u32), (Vec<(U256, U256)>, Option<(U256, u128)>)> =
                    std::collections::HashMap::new();
                for ((pi, a_id, a_fee, b_id, b_fee, amt, state), (raw, _)) in spec_routes.into_iter().zip(spec_raw) {
                    if let Some(out) = decode_v3_quote(raw.as_ref()) {
                        let (pts, st) = spec_points.entry((pi, a_id, a_fee, b_id, b_fee)).or_default();
                        pts.push((amt, out));
                        pts.sort_by(|a, b| a.0.cmp(&b.0));
                        *st = state;
                    }
                }

                // Merge cache hits + MC results into final quoter_outs.
                // MC results are also stored in quote_cache so the next scan with unchanged
                // pool state gets a free cache hit (within quote_cache_max_age()).
                // Cache a failed quote (ZERO) only if the node answered that call's chunk —
                // then the failure is a real revert, not a transport error that would
                // suppress the pool (chunks fail independently).
                let mut mc_raw_iter = p15_raw.into_iter();
                let mut sent_state_iter = p15_sent_state.into_iter();
                let mut p15_health = QuoterBatchHealth::default();
                let p15_quoter_outs: Vec<U256> = p15_list.iter()
                    .zip(p15_pre.iter())
                    .map(|((_, router_a_id, fee_a, _, token_in, token_out, full_amount), pre)| {
                        if let Some(cached_val) = pre {
                            *cached_val  // served from cache — 0 HTTP
                        } else {
                            let (raw_opt, answered) = mc_raw_iter.next().unwrap_or((None, false));
                            let state = sent_state_iter.next().flatten();
                            let quote = decode_v3_quote(raw_opt.as_ref());
                            p15_health.record(router_a_id, quote.is_some());
                            let result = quote.unwrap_or(U256::ZERO);
                            if let Some(state) = state.filter(|_| quote.is_some() || answered) {
                                let key = V3PoolKey::new(router_a_id, *token_in, *token_out, *fee_a);
                                self.quote_cache.insert(&self.pool_cache, key, *full_amount, state, result);
                            }
                            result
                        }
                    })
                    .collect();
                self.note_quoter_health(p15_health);

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
                    // V3 reverse pools whose spot quote for quoter_out is tick-capped can't
                    // absorb it at spot — skip them rather than spend a 1.5b QuoterV2 call.
                    let q_b_list: Vec<ForwardTask> = match pair_quotes.get(&pi) {
                        Some(ts) => ts.iter()
                            .filter(|t| route_legs_eligible(
                                (&router_a_id, router_a_addr, &RouterType::V3, fee_a),
                                (&t.router_id, t.router_addr, &t.router_type, t.fee),
                                true,
                            ))
                            .filter(|t| !matches!(t.router_type, RouterType::V3)
                                || t.rev_v3_key.as_ref().is_some_and(|k| {
                                    v3_spot_uncapped(&self.pool_cache, k, token_out, quoter_out)
                                }))
                            .cloned()
                            .collect(),
                        None => continue,
                    };

                    for q_b in q_b_list {
                        let fee_b = q_b.fee;
                        // V3 router_b: check quote_cache first, otherwise defer to
                        // Phase 1.5b QuoterV2 batch. Cache key matches v3_by_key format
                        // for the reverse direction (tokenIn=token_out, tokenOut=token_in).
                        // Keyed on (pool B, quoter_out input); shared QuoteCache validity rule.
                        if matches!(q_b.router_type, RouterType::V3) {
                            if let Some(quoter_b) = q_b.quoter_addr {
                                let p15b_key = q_b
                                    .rev_v3_key
                                    .clone()
                                    .unwrap_or_else(|| V3PoolKey::new(&q_b.router_id, token_out, token_in, fee_b));
                                let cached_b = self.quote_cache.get(&self.pool_cache, &p15b_key, quoter_out)
                                    .or_else(|| {
                                        let (pts, state) = spec_points
                                            .get(&(pi, router_a_id.clone(), fee_a, q_b.router_id.clone(), fee_b))?;
                                        let (bound, tight) = super::triangular::v3_leg_lower_bound(pts, quoter_out)?;
                                        if !tight || bound.is_zero() { return None; }
                                        p15b_spec_hits += 1;
                                        // Conservative, so as good as a quote for the next scan's cache hit.
                                        if let Some(state) = *state {
                                            self.quote_cache.insert(&self.pool_cache, p15b_key.clone(), quoter_out, state, bound);
                                        }
                                        Some(bound)
                                    });
                                p15b_candidates.push((pi, router_a_id.clone(), fee_a, router_a_addr,
                                                      token_in, token_out, full_amount, q_b.clone(), quoter_out));
                                if cached_b.is_some() {
                                    p15b_pre.push(cached_b);
                                } else {
                                    p15b_pre.push(None);
                                    p15b_sent_state.push(self.pool_cache.v3_price_state(&p15b_key));
                                    p15b_mc.push((quoter_b, self.encode_quote(quoter_b, token_out, token_in, fee_b, quoter_out)));
                                }
                            }
                            continue;
                        }
                        let local_back = match q_b.router_type {
                            RouterType::V2 => q_b.rev_v2_key.as_ref().and_then(|k| {
                                self.pool_cache.get_amount_out_by_v2_key(
                                    k, token_out, quoter_out, Some(volatile_max_age()),
                                )
                            }),
                            RouterType::Aerodrome => {
                                let max_age = if fee_b != 0 { stable_max_age() } else { volatile_max_age() };
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
                        // for real arb — indicates stale Aerodrome reserves or V3 spot
                        // mismatch. Reject before updating any metrics.
                        if amount_back > full_amount
                            && crate::config::tuning().is_phantom(amount_back - full_amount, full_amount)
                        {
                            warn!("[{}] Phase 1.5 phantom: {} | {}/{}", self.chain_id, pair.id,
                                  router_a_id, q_b.router_id);
                            continue;
                        }

                        // Apply quoter_haircut_bps() buffer (Phase 1.5: V3 forward + Aerodrome/V2 reverse).
                        // Aerodrome reverse uses local pool cache (volatile_max_age()=120s).
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

        // ── Fire Phase 1.5b and Phase 1.5c in parallel ───────────────────────────────
        // Both are independent: 1.5b needs Phase 1.5 output (already computed above);
        // 1.5c needs only rev_tasks (collected just above). Running them concurrently
        // saves ~50–80ms when both fire in the same scan.
        let p15b_cache_hits  = p15b_pre.iter().filter(|v| v.is_some()).count();
        let p15b_cache_misses = p15b_mc.len();
        if p15b_cache_hits > 0 || p15b_cache_misses > 0 {
            if p15b_cache_misses > 0 {
                debug!("[chain={}] Phase 1.5b: {} V3×V3 reverse quote(s) + {} from cache ({} from speculative brackets)",
                       self.chain_id, p15b_cache_misses, p15b_cache_hits, p15b_spec_hits);
            } else {
                debug!("[chain={}] Phase 1.5b: all {} from p15b_cache — 0 HTTP calls",
                       self.chain_id, p15b_cache_hits);
            }
        }
        if !p15c_candidates.is_empty() {
            debug!("[chain={}] Phase 1.5c: {} local→V3 reverse quote(s) + {} from cache", self.chain_id,
                   p15c_queued, p15c_candidates.len() - p15c_queued);
        }

        // 1.5c already finished if it was joined with Phase 1.5; polling a done MaybeDone is free.
        let t = std::time::Instant::now();
        let p15b = async {
            if p15b_cache_misses == 0 {
                vec![]
            } else if let Some(ref hp) = self.http_provider {
                run_multicall_answered(hp, p15b_mc, self.rpc_concurrency, self.quote_block).await
            } else {
                run_multicall_answered(provider, p15b_mc, self.rpc_concurrency, self.quote_block).await
            }
        };
        // The prefetch rides along only when this join is a round trip anyway (or Phase 1.5
        // already ran it); otherwise it's left unpolled and the recheck reads what it needs.
        let rides = p15b_cache_misses > 0 || (p15c_queued > 0 && p15c_fut.as_mut().output_mut().is_none());
        let prefetch = async {
            if rides || prefetch_fut.as_mut().output_mut().is_some() {
                prefetch_fut.as_mut().await;
            }
        };
        let (p15b_raw_mc, (), ()) = tokio::join!(p15b, p15c_fut.as_mut(), prefetch);
        let p15c_raw = p15c_fut.as_mut().take_output().unwrap_or_default();
        if p15b_cache_misses > 0 || p15c_queued > 0 {
            self.timings.p15bc_ms.store(t.elapsed().as_millis() as u64, Relaxed);
        }

        // ── Process Phase 1.5b results ────────────────────────────────────────────────
        {
            let mut mc_iter = p15b_raw_mc.into_iter();
            let mut sent_state_iter = p15b_sent_state.into_iter();
            let mut p15b_health = QuoterBatchHealth::default();
            for ((pi, router_a_id, fee_a, router_a_addr, token_in, token_out, full_amount, q_b, quoter_out), pre)
                in p15b_candidates.into_iter().zip(p15b_pre.into_iter())
            {
                let amount_back = if let Some(cached_val) = pre {
                    if cached_val.is_zero() { continue; }
                    cached_val
                } else {
                    let (raw_opt, answered) = mc_iter.next().unwrap_or((None, false));
                    let state = sent_state_iter.next().flatten();
                    let quote = decode_v3_quote(raw_opt.as_ref());
                    p15b_health.record(&q_b.router_id, quote.is_some());
                    let result = quote.unwrap_or(U256::ZERO);
                    // Cache ZERO only when this call's chunk was answered (real revert, see 1.5).
                    if let Some(state) = state.filter(|_| quote.is_some() || answered) {
                        let p15b_key = q_b
                            .rev_v3_key
                            .clone()
                            .unwrap_or_else(|| V3PoolKey::new(&q_b.router_id, token_out, token_in, q_b.fee));
                        self.quote_cache.insert(&self.pool_cache, p15b_key, quoter_out, state, result);
                    }
                    if result.is_zero() { continue; }
                    result
                };

                // Phantom check FIRST (same logic as Phase 1.5 non-b).
                if amount_back > full_amount
                    && crate::config::tuning().is_phantom(amount_back - full_amount, full_amount)
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
            self.note_quoter_health(p15b_health);
        }

        // ── Process Phase 1.5c results ────────────────────────────────────────────────
        let mut p15c_health = QuoterBatchHealth::default();
        let mut p15c_mc_iter = p15c_raw.into_iter();
        let mut p15c_state_iter = p15c_sent_state.into_iter();
        for ((pi, router_a_id, fee_a, router_a_addr, router_a_type, token_in, token_out, full_amount, fee_b, router_b_addr, router_b_id, token_out_amount), pre)
            in p15c_candidates.into_iter().zip(p15c_pre.into_iter())
        {
            let amount_back = if let Some(cached_val) = pre {
                if cached_val.is_zero() { continue; }
                cached_val
            } else {
                let (raw_opt, answered) = p15c_mc_iter.next().unwrap_or((None, false));
                let state = p15c_state_iter.next().flatten();
                let quote = decode_v3_quote(raw_opt.as_ref());
                p15c_health.record(&router_b_id, quote.is_some());
                let result = quote.unwrap_or(U256::ZERO);
                // Cache ZERO only when this call's chunk was answered (real revert, see 1.5).
                if let Some(state) = state.filter(|_| quote.is_some() || answered) {
                    let key = V3PoolKey::new(&router_b_id, token_out, token_in, fee_b);
                    self.quote_cache.insert(&self.pool_cache, key, token_out_amount, state, result);
                }
                if result.is_zero() { continue; }
                result
            };

            // Phantom check first (>5% positive spread = physically impossible)
            if amount_back > full_amount
                && crate::config::tuning().is_phantom(amount_back - full_amount, full_amount)
            {
                warn!("[{}] Phase 1.5c phantom: {}/{}", self.chain_id, router_a_id, router_b_id);
                continue;
            }

            // Forward leg uses local pool cache (volatile_max_age()=120s); V3 reverse is QuoterV2-exact.
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
        self.note_quoter_health(p15c_health);

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
                        chain_id: self.chain_id,
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
                        chain_id: self.chain_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool_cache::{PoolCache, V3PoolState};
    use std::time::{Duration, Instant};

    // Gate 0.005%: amount_back / amount_in > 1_000_050 / 1_000_000.
    const MUL: u64 = 1_000_050;
    const DEN: u64 = 1_000_000;

    fn back(amount_in: u64, spread_ppm: u64) -> U256 {
        U256::from(amount_in) * U256::from(1_000_000 + spread_ppm) / U256::from(1_000_000u64)
    }

    #[test]
    fn same_router_routes_only_across_v3_fee_tiers() {
        let (uni, aero, pancake) = (Address::repeat_byte(1), Address::repeat_byte(2), Address::repeat_byte(3));
        let (v3, aer) = (RouterType::V3, RouterType::Aerodrome);
        // Cross-DEX: always eligible, verified or not.
        assert!(route_legs_eligible(("uni", uni, &v3, 500), ("pancake", pancake, &v3, 500), false));
        // Same V3 router, different fee tiers: eligible only when QuoterV2-verified.
        assert!(route_legs_eligible(("uni", uni, &v3, 500), ("uni", uni, &v3, 3000), true));
        assert!(!route_legs_eligible(("uni", uni, &v3, 500), ("uni", uni, &v3, 3000), false));
        // Identical pool: never.
        assert!(!route_legs_eligible(("uni", uni, &v3, 500), ("uni", uni, &v3, 500), true));
        // Aerodrome volatile × stable (one router contract, two ids): never.
        assert!(!route_legs_eligible(("aero_vol", aero, &aer, 0), ("aero_sta", aero, &aer, 1), true));
        assert!(!route_legs_eligible(("aero", aero, &aer, 0), ("aero", aero, &aer, 1), true));
    }

    #[test]
    fn p15_gate_skips_capped_forward_legs() {
        let full = U256::from(10_000_000_000u64);
        let capped = U256::from(5_000_000u64);
        // (a) Tick-capped thin pool with a +0.5% spread on its capped input (WETH/AERO case).
        assert!(!p15_gate_eligible(back(5_000_000, 5_000), capped, Some(full), &RouterType::V3, MUL, DEN));
        // (b) Uncapped leg with +0.01% → gated.
        assert!(p15_gate_eligible(back(10_000_000_000, 100), full, Some(full), &RouterType::V3, MUL, DEN));
        // (c) Uncapped leg with +0.004% → below the 0.005% gate.
        assert!(!p15_gate_eligible(back(10_000_000_000, 40), full, Some(full), &RouterType::V3, MUL, DEN));
        // (d) V2 forward → never gated (handled by Phase 2 / 1.5c).
        assert!(!p15_gate_eligible(back(10_000_000_000, 100), full, Some(full), &RouterType::V2, MUL, DEN));
        // Unresolved pair → not gated.
        assert!(!p15_gate_eligible(back(10_000_000_000, 100), full, None, &RouterType::V3, MUL, DEN));
    }

    /// Price 1, L = 1e6 → vr0 = vr1 = 1e6; 2% cap = 20_000. Last moved `ago`.
    fn pools_with_v3(t0: Address, t1: Address, ago: Duration) -> PoolCache {
        let cache = PoolCache::new();
        cache.insert_v3(Address::repeat_byte(0xcc), V3PoolState {
            token0: t0,
            token1: t1,
            fee: 500,
            swap_fee: 500,
            sqrt_price_x96: U256::from(1u64) << 96,
            liquidity: 1_000_000,
            tick: 0,
            router_id: "r".into(),
            last_updated: Instant::now() - ago,
            vr0: U256::ZERO,
            vr1: U256::ZERO,
            fee_num_v3: U256::ZERO,
        });
        cache
    }

    #[test]
    fn v3_spot_uncapped_detects_tick_capped_reverse_pools() {
        let (t0, t1) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let cache = pools_with_v3(t0, t1, Duration::ZERO);
        let key = V3PoolKey::new("r", t1, t0, 500);
        assert!(v3_spot_uncapped(&cache, &key, t1, U256::from(10_000u64)));
        assert!(!v3_spot_uncapped(&cache, &key, t1, U256::from(100_000u64)));
        // Unknown pool → treated as not quotable at spot.
        let missing = V3PoolKey::new("other", t1, t0, 500);
        assert!(!v3_spot_uncapped(&cache, &missing, t1, U256::from(10_000u64)));
    }

    /// Phase 1.5c: a second scan on unchanged pool state is served from QuoteCache.
    #[test]
    fn p15c_second_scan_on_unchanged_state_hits_quote_cache() {
        let (t0, t1) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let pools = pools_with_v3(t0, t1, Duration::from_secs(5));
        let qc = QuoteCache::default();
        let key = V3PoolKey::new("r", t1, t0, 500);
        let amount = U256::from(10_000u64);
        let (mut pre, mut sent) = (Vec::new(), Vec::new());

        // Scan 1: miss → queued, state captured; result stored as the 1.5c loop does.
        assert!(quote_cache_precheck(&qc, &pools, &key, amount, &mut pre, &mut sent));
        assert_eq!(pre, vec![None]);
        qc.insert(&pools, key.clone(), amount, sent[0].unwrap(), U256::from(9_990u64));

        // Scan 2: hit → no QuoterV2 call.
        assert!(!quote_cache_precheck(&qc, &pools, &key, amount, &mut pre, &mut sent));
        assert_eq!(pre, vec![None, Some(U256::from(9_990u64))]);
        assert_eq!(sent.len(), 1);
    }
}
