use alloy::primitives::{Address, Uint, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use tracing::{debug, info, warn};

use crate::abi::{IQuoterV2, ISolidlyRouter, ISyncSwapPool, IUniswapV2Router02};
use crate::config::RouterType;
use crate::pool_cache::{VOLATILE_MAX_AGE, STABLE_MAX_AGE};
use crate::types::{ArbOpportunity, PairScanInfo};
use crate::util::{addr_key, u256_to_f64};
use super::{ForwardTask, ReverseTask, Strategy, run_multicall, token_amount_to_usd, MIN_V3_LIQUIDITY, parse_amount_capped};

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
        let chain_routers: Vec<&crate::config::RouterConfig> = self
            .routers
            .iter()
            .filter(|r| r.chain_id == self.chain_id)
            .collect();

        // ── Phase 1: Forward quotes ───────────────────────────────────────────────
        // V2 and Solidly-volatile pools use xy=k (constant-product AMM), so we compute
        // quotes locally from cached reserves — zero eth_calls.
        // V3, SyncSwap, and Solidly-stable still need multicall (different curves / no cache).

        // pair_quotes is pre-populated here for locally-resolved routers.
        let mut pair_quotes: std::collections::HashMap<usize, Vec<ForwardTask>> =
            std::collections::HashMap::new();

        let fwd_tasks: Vec<ForwardTask> = Vec::new();
        let fwd_mc: Vec<(Address, Vec<u8>)> = Vec::new();
        // V3 scan diagnostic counters: cached=used spot (0 HTTP), no_cache=QuoterV2 needed.
        let mut v3_cached: usize = 0;
        let mut v3_no_cache: usize = 0;

        // Pre-compute Solidly/Aerodrome pool-cache key suffixes once per router.
        // Avoids format!() inside the O(pairs × routers) hot loop (~200+ allocs/scan).
        let solidly_vol_ids: Vec<String> = chain_routers.iter()
            .map(|r| format!("{}::volatile", r.id)).collect();
        let solidly_sta_ids: Vec<String> = chain_routers.iter()
            .map(|r| format!("{}::stable", r.id)).collect();

        for (pi, pair) in self.pairs.iter().enumerate()
            .filter(|(_, p)| p.chain_id == self.chain_id)
            .filter(|(i, _)| pair_mask.map_or(true, |m| m.contains(i)))
        {
            let token_in = match pair.token_in.parse::<Address>() {
                Ok(a) => a,
                Err(_) => continue,
            };
            let token_out = match pair.token_out.parse::<Address>() {
                Ok(a) => a,
                Err(_) => continue,
            };
            let amount_in = match parse_amount_capped(
                &pair.trade_amount,
                pair.max_trade.as_deref(),
                pair.token_in_decimals,
            ) {
                Some(a) => a,
                None => continue,
            };

            // Compute token address keys once per pair — reused across all routers/fee tiers.
            // Replaces ~1500 format!("{:?}", addr).to_lowercase() calls per scan.
            let token_in_key  = addr_key(token_in);
            let token_out_key = addr_key(token_out);

            for (ri, router) in chain_routers.iter().enumerate() {
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
                        if let Some(out) = self.pool_cache.get_amount_out_by_key(
                            &router.id, token_in, token_out, amount_in, Some(VOLATILE_MAX_AGE),
                        ) {
                            pair_quotes.entry(pi).or_default().push(ForwardTask {
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
                            });
                        }
                        // Cache miss = pool not discovered at startup = doesn't exist → skip
                    }
                    RouterType::V3 => {
                        // Per-router quoter takes priority; fall back to chain-level.
                        // This allows multiple V3 protocols on the same chain (e.g.
                        // PancakeSwap V3 + Uniswap V3) each using their own QuoterV2.
                        let quoter = router.quoter_address.as_deref()
                            .and_then(|s| s.parse::<Address>().ok())
                            .or(self.quoter_v2_address);
                        let quoter = match quoter {
                            Some(q) => q,
                            None => {
                                debug!("No QuoterV2 for router {} on chain {} — skipping V3 quotes", router.id, self.chain_id);
                                continue;
                            }
                        };
                        let tiers = if router.fee_tiers.is_empty() {
                            vec![500u32, 3000, 10000]
                        } else {
                            router.fee_tiers.clone()
                        };
                        for fee in tiers {
                            // ── V3 cache-first: use sqrtPriceX96 virtual-reserve spot ──
                            // Cap the input to a single-tick safe amount so the xy=k formula
                            // is exact. Without the cap, a trade crossing into a tick with
                            // L=0 would cause the formula to overestimate output.
                            let v3_key = format!("{}:{}:{}:{}", router.id, token_in_key, token_out_key, fee);
                            if self.pool_cache.v3_by_key.get(&v3_key).is_none() {
                                v3_no_cache += 1; continue;
                            }

                            if let Some((spot_out, liquidity)) = self.pool_cache.quote_v3_spot(
                                &router.id, token_in, token_out, fee, amount_in,
                            ) {
                                if liquidity < MIN_V3_LIQUIDITY {
                                    v3_cached += 1;
                                    continue;
                                }
                                pair_quotes.entry(pi).or_default().push(ForwardTask {
                                    pair_idx: pi,
                                    router_id: router.id.clone(),
                                    router_addr,
                                    router_type: RouterType::V3,
                                    fee,
                                    amount_in: spot_out, // repurposed: carries token_out amount
                                    capped_amount_in: amount_in,
                                    token_in,
                                    token_out,
                                    quoter_addr: Some(quoter),
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
                        for (eff_id, fee, max_age) in [
                            (solidly_vol_ids[ri].as_str(), 0u32, VOLATILE_MAX_AGE),
                            (solidly_sta_ids[ri].as_str(), 1u32, STABLE_MAX_AGE),
                        ] {
                            if let Some(out) = self.pool_cache.get_amount_out_by_key(
                                eff_id, token_in, token_out, amount_in, Some(max_age),
                            ) {
                                pair_quotes.entry(pi).or_default().push(ForwardTask {
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
                                });
                            }
                        }
                    }
                    RouterType::SyncSwap => {
                        // SyncSwap pools are now seeded into pool_cache at startup
                        // (populate_syncswap_pools also calls pool_cache.insert).
                        // Use local reserve state with freshness check (0 eth_call).
                        if let Some(out) = self.pool_cache.get_amount_out_by_key(
                            &router.id, token_in, token_out, amount_in, Some(VOLATILE_MAX_AGE),
                        ) {
                            pair_quotes.entry(pi).or_default().push(ForwardTask {
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
                            });
                        }
                    }
                }
            }
        }

        // Run multicall for routers that couldn't be resolved locally
        let fwd_raw = run_multicall(provider, fwd_mc, self.rpc_concurrency).await;

        // Add multicall results to pair_quotes (locally-resolved already added above)
        for (task, raw_opt) in fwd_tasks.into_iter().zip(fwd_raw.into_iter()) {
            let raw = match raw_opt { Some(r) => r, None => continue };
            let amount_out = match task.router_type {
                RouterType::V2 => IUniswapV2Router02::getAmountsOutCall::abi_decode_returns(&raw)
                    .ok()
                    .and_then(|v| v.last().copied()),
                RouterType::V3 => IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&raw)
                    .ok()
                    .map(|r| r.amountOut),
                RouterType::Solidly | RouterType::Aerodrome => {
                    ISolidlyRouter::getAmountsOutCall::abi_decode_returns(&raw)
                        .ok()
                        .and_then(|v| v.last().copied())
                }
                RouterType::SyncSwap => {
                    ISyncSwapPool::getAmountOutCall::abi_decode_returns(&raw).ok()
                }
            };
            if let Some(out) = amount_out.filter(|o| !o.is_zero()) {
                let mut t = task;
                t.amount_in = out; // repurpose field to carry token_out amount
                pair_quotes.entry(t.pair_idx).or_default().push(t);
            }
        }

        // ── Diagnostic: compute multi-router coverage stats ─────────────────────
        // Returned to caller (chain.rs) which broadcasts to live feed once per 60s.
        // Not logged here to avoid terminal flood on every block.
        let total_fwd_ok: usize = pair_quotes.values().map(|v| v.len()).sum();
        let pairs_with_any: usize = pair_quotes.len(); // pairs with ≥1 quote from any DEX
        let pairs_with_multi: usize = pair_quotes.values().filter(|v| {
            v.first().map_or(false, |first| v[1..].iter().any(|q| q.router_id != first.router_id))
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

        let mut rev_tasks: Vec<ReverseTask> = Vec::new();
        let rev_mc: Vec<(Address, Vec<u8>)> = Vec::new();
        // Maps rev_mc[i] → rev_tasks[j] so we can write results back after multicall.
        let rev_mc_task_idx: Vec<usize> = Vec::new();

        for (pi, quotes) in &pair_quotes {
            if quotes.len() < 2 {
                continue;
            }
            let pair = &self.pairs[*pi];
            let _original_amount_in = match parse_amount_capped(
                &pair.trade_amount,
                pair.max_trade.as_deref(),
                pair.token_in_decimals,
            ) {
                Some(a) => a,
                None => continue,
            };

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
                    if q_a.router_id == q_b.router_id
                        && matches!(q_a.router_type, RouterType::V3)
                    {
                        continue;
                    }

                    let token_in = q_a.token_in;
                    let token_out = q_a.token_out;
                    let rb_addr = q_b.router_addr;
                    let fee_b = q_b.fee;

                    // Try local reverse quote for all DEX types.
                    // Solidly and SyncSwap now use pool_cache with freshness check;
                    // stale reserves (>VOLATILE/STABLE_MAX_AGE since last Sync event) → None → skip.
                    let local_back = match q_b.router_type {
                        RouterType::V2 => self.pool_cache.get_amount_out_by_key(
                            &q_b.router_id, token_out, token_in, token_out_amount, Some(VOLATILE_MAX_AGE),
                        ),
                        RouterType::Solidly | RouterType::Aerodrome => {
                            // q_b.router_id is "router::volatile" or "router::stable"
                            let max_age = if fee_b != 0 { STABLE_MAX_AGE } else { VOLATILE_MAX_AGE };
                            self.pool_cache.get_amount_out_by_key(
                                &q_b.router_id, token_out, token_in, token_out_amount, Some(max_age),
                            )
                        }
                        RouterType::V3 => {
                            // Use cached sqrtPriceX96 spot for reverse V3 leg when fresh
                            self.pool_cache.quote_v3_spot(
                                &q_b.router_id, token_out, token_in, fee_b, token_out_amount,
                            ).map(|(spot_out, _)| spot_out)
                        }
                        RouterType::SyncSwap => {
                            self.pool_cache.get_amount_out_by_key(
                                &q_b.router_id, token_out, token_in, token_out_amount, Some(VOLATILE_MAX_AGE),
                            )
                        }
                    };

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

                    // All DEX types: stale/absent cache = no recent activity = no arb → skip.
                    if local_back.is_none() {
                        rev_tasks.pop();
                        continue;
                    }
                }
            }
        }

        // Run multicall for reverse quotes that need it
        let rev_raw = run_multicall(provider, rev_mc, self.rpc_concurrency).await;

        // Map multicall results back to rev_tasks
        let mut rev_raw_by_task: Vec<Option<Vec<u8>>> = vec![None; rev_tasks.len()];
        for (mc_i, &task_i) in rev_mc_task_idx.iter().enumerate() {
            rev_raw_by_task[task_i] = rev_raw.get(mc_i).and_then(|r| r.clone());
        }

        // ── Find profitable opportunities ──────────────────────────────────────

        let mut opportunities = Vec::new();
        let mut best_raw_usd: f64 = 0.0;
        // Best signed spread ratio: (reverse_out / amount_in) - 1.0
        // Negative = below break-even; positive = profitable.
        let mut best_spread_pct: f64 = f64::NEG_INFINITY;

        // Phase 1.5 gate: collect V3 tick-capped forward tasks that show real cross-DEX
        // divergence. QuoterV2 fires only for these (0 RPC when market is quiet).
        const P15_GATE_SPREAD: f64 = 0.0005; // 0.05% — gate for Phase 1.5/1.5c QuoterV2 batches
        let mut p15_gate: std::collections::HashSet<(usize, String, u32)> = std::collections::HashSet::new();
        // Best QuoterV2-verified spread (signed). NEG_INFINITY when Phase 1.5 didn't run or
        // all reverse lookups failed. Exposed in heartbeat as "bestV=" to distinguish real
        // profit ($0.001 verified) from V3 spot phantom ($0.29 raw).
        let mut best_verified_spread: f64 = f64::NEG_INFINITY;

        for (task, raw_opt) in rev_tasks.iter().zip(rev_raw_by_task.into_iter()) {
            let amount_back_opt: Option<U256> = if let Some(local) = task.local_back {
                Some(local)
            } else {
                let raw = match raw_opt { Some(r) => r, None => continue };
                match task.router_b_type {
                    RouterType::V2 => IUniswapV2Router02::getAmountsOutCall::abi_decode_returns(&raw)
                        .ok()
                        .and_then(|v| v.last().copied()),
                    RouterType::V3 => IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&raw)
                        .ok()
                        .map(|r| r.amountOut),
                    RouterType::Solidly | RouterType::Aerodrome => {
                        ISolidlyRouter::getAmountsOutCall::abi_decode_returns(&raw)
                            .ok()
                            .and_then(|v| v.last().copied())
                    }
                    RouterType::SyncSwap => {
                        ISyncSwapPool::getAmountOutCall::abi_decode_returns(&raw).ok()
                    }
                }
            };
            if let Some(amount_back) = amount_back_opt.filter(|&b| !b.is_zero()) {
                if amount_back > task.amount_in {
                    // Sanity cap: profit > 5% of input is almost certainly a V3 spot
                    // mismatch phantom (different routers' sqrtPriceX96 values compound
                    // errors across the fwd/rev legs, producing billion-dollar phantoms).
                    if (amount_back - task.amount_in).saturating_mul(U256::from(20)) > task.amount_in {
                        warn!(
                            "[{}] 2-hop phantom skipped: {} | profit/input > 5% | {}/{}",
                            self.chain_id, task.pair_id, task.router_a_id, task.router_b_id
                        );
                        continue;
                    }
                }

                // Track signed spread % for all non-phantom quotes (even unprofitable ones).
                // Must be after the sanity cap to prevent V3 spot mismatch artifacts from
                // corrupting the spread metric with astronomical values.
                let amount_in_f64 = task.amount_in.to::<u128>() as f64;
                let amount_back_f64 = amount_back.to::<u128>() as f64;
                if amount_in_f64 > 0.0 {
                    let spread = amount_back_f64 / amount_in_f64 - 1.0;
                    if spread > best_spread_pct {
                        best_spread_pct = spread;
                    }

                    // Phase 1.5 gate: local cross-DEX spread is promising AND forward router is V3.
                    // Fire for ALL V3 forwards with spread > threshold, not just tick-capped ones.
                    // QuoterV2 verifies the actual on-chain amount at full trade size. Without this,
                    // liquid pools (safe_cap >= trade_amount, task.amount_in == full) never trigger
                    // Phase 1.5 and V3×V3 spreads are permanently blocked from all_opportunities.
                    if spread > P15_GATE_SPREAD && matches!(task.router_a_type, RouterType::V3) {
                        p15_gate.insert((task.pair_idx, task.router_a_id.clone(), task.fee_a));
                    }
                }

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
                    let pair_min_profit = self.pairs[task.pair_idx].min_profit_usd.unwrap_or(self.min_profit_usd);
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
                let pair = &self.pairs[*pi];
                let full_amount = match parse_amount_capped(
                    &pair.trade_amount, pair.max_trade.as_deref(), pair.token_in_decimals,
                ) {
                    Some(a) => a, None => continue,
                };
                let fwd_task = match pair_quotes.get(pi)
                    .and_then(|ts| ts.iter().find(|t| &t.router_id == router_a_id && t.fee == *fee_a))
                {
                    Some(t) => t.clone(), None => continue,
                };
                let quoter = match fwd_task.quoter_addr { Some(q) => q, None => continue };

                // Cache key matches pool_cache.v3_by_key format.
                let v3_cache_key = format!("{}:{}:{}:{}", router_a_id,
                    addr_key(fwd_task.token_in), addr_key(fwd_task.token_out), fee_a);

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
                            // Update p15_cache: store result under current sqrtPriceX96.
                            let key = format!("{}:{}:{}:{}", router_a_id,
                                addr_key(*token_in), addr_key(*token_out), fee_a);
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
                                let p15b_key = format!("{}:{}:{}:{}", q_b.router_id,
                                    addr_key(token_out), addr_key(token_in), fee_b);
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
                            RouterType::V2 => self.pool_cache.get_amount_out_by_key(
                                &q_b.router_id, token_out, token_in, quoter_out, Some(VOLATILE_MAX_AGE),
                            ),
                            RouterType::Solidly | RouterType::Aerodrome => {
                                let max_age = if fee_b != 0 { STABLE_MAX_AGE } else { VOLATILE_MAX_AGE };
                                self.pool_cache.get_amount_out_by_key(
                                    &q_b.router_id, token_out, token_in, quoter_out, Some(max_age),
                                )
                            }
                            RouterType::SyncSwap => self.pool_cache.get_amount_out_by_key(
                                &q_b.router_id, token_out, token_in, quoter_out, Some(VOLATILE_MAX_AGE),
                            ),
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
        // V2-forward / V3-reverse routes: quote_v3_spot overestimates multi-tick output.
        let mut p15c_mc: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut p15c_candidates: Vec<(usize, String, u32, Address, Address, Address, U256, u32, Address, String)> = Vec::new();
        {
            for task in &rev_tasks {
                if !matches!(task.router_a_type, RouterType::V2) { continue; }
                if !matches!(task.router_b_type, RouterType::V3) { continue; }

                let local_back = match task.local_back { Some(v) => v, None => continue };
                let spread = u256_to_f64(local_back) / u256_to_f64(task.amount_in) - 1.0;
                if spread <= P15_GATE_SPREAD { continue; }

                let pair = &self.pairs[task.pair_idx];
                let full_amount = match parse_amount_capped(
                    &pair.trade_amount, pair.max_trade.as_deref(), pair.token_in_decimals,
                ) {
                    Some(a) => a, None => continue,
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
            debug!("[chain={}] Phase 1.5c: {} V2→V3 reverse quote(s)", self.chain_id, p15c_mc.len());
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
                    // Write to p15b_cache keyed on pool B sqrtPrice + quoter_out input.
                    let p15b_key = format!("{}:{}:{}:{}", q_b.router_id,
                        addr_key(token_out), addr_key(token_in), q_b.fee);
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
        for ((pi, router_a_id, fee_a, router_a_addr, token_in, token_out, full_amount, fee_b, router_b_addr, router_b_id), raw_opt)
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
                    router_a_type: RouterType::V2,
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
                for (pi, pair) in self.pairs.iter().enumerate()
                    .filter(|(_, p)| p.chain_id == self.chain_id)
                {
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

        (opportunities, best_raw_usd, best_verified_spread, total_fwd_ok, pairs_with_multi, best_spread_pct, pairs_with_any)
    }
}
