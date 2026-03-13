use alloy::primitives::{Address, Uint, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use tracing::{debug, info, warn};

use crate::abi::{IQuoterV2, ISolidlyRouter, ISyncSwapPool, IUniswapV2Router02};
use crate::config::RouterType;
use crate::pool_cache::{VOLATILE_MAX_AGE, STABLE_MAX_AGE};
use crate::types::{PairScanInfo, TriangularOpportunity};
use super::{Strategy, token_amount_to_usd, parse_amount_capped, run_multicall, MIN_V3_LIQUIDITY};

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
        use std::collections::HashMap;

        // ── Build token info from all pairs on this chain ─────────────────────────
        // addr_lower → (symbol, decimals)
        let mut token_info: HashMap<String, (String, u8)> = HashMap::new();
        // addr_lower → trade amount (from the first pair that starts with this token)
        let mut token_amounts: HashMap<String, U256> = HashMap::new();

        for p in self.pairs.iter().filter(|p| p.chain_id == self.chain_id) {
            let key_in  = p.token_in.to_lowercase();
            let key_out = p.token_out.to_lowercase();
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

        // Parse unique token addresses
        let tokens: Vec<(Address, String)> = token_info.keys()
            .filter_map(|k| k.parse::<Address>().ok().map(|a| (a, k.clone())))
            .collect();

        if tokens.len() < 3 {
            return (vec![], 0.0);
        }

        let chain_routers: Vec<&crate::config::RouterConfig> = self.routers.iter()
            .filter(|r| r.chain_id == self.chain_id)
            .collect();

        if chain_routers.is_empty() {
            return (vec![], 0.0);
        }

        // ── Build all ordered triplets ────────────────────────────────────────────
        // For N tokens: N×(N-1)×(N-2) ordered triplets. Capped at 6 tokens → max 120.
        struct Triplet {
            token_a: Address, key_a: String,
            token_b: Address,
            token_c: Address,
            amount_in: U256,
            triplet_id: String,
        }

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
                    let Some((sym_c, _)) = token_info.get(key_c) else { continue };

                    let _ = sym_c; // used only for triplet_id
                    triplets.push(Triplet {
                        token_a, key_a: key_a.clone(),
                        token_b, token_c,
                        amount_in,
                        triplet_id: format!("{}→{}→{}", sym_a, sym_b, sym_c),
                    });
                }
            }
        }

        // Targeted scan: keep only triplets that contain at least one of the target tokens.
        if let Some(filter) = token_filter {
            triplets.retain(|t| {
                filter.iter().any(|f| *f == t.token_a || *f == t.token_b || *f == t.token_c)
            });
        }

        // Compute disabled directed token pairs from config — used both to filter the scan
        // and to mark blocked triplets in tri_scan (so they stay visible in the dashboard).
        let disabled_legs: std::collections::HashSet<(Address, Address)> =
            if disabled_triplets.is_empty() {
                std::collections::HashSet::new()
            } else {
                self.pairs.iter()
                    .filter(|p| p.chain_id == self.chain_id && disabled_triplets.contains(&p.id))
                    .filter_map(|p| {
                        let ti: Address = p.token_in.parse().ok()?;
                        let to: Address = p.token_out.parse().ok()?;
                        Some((ti, to))
                    })
                    .collect()
            };

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
            tri.dedup_by_key(|t| t.pair_id.clone());
            drop(opp_counts);
            return (vec![], 0.0);
        }

        if triplets.is_empty() {
            return (vec![], 0.0);
        }

        // _make_quote: previously used for multicall fallbacks, now unused.
        // All DEX types use local state (0 eth_call) in P1/P2/P3.
        let _make_quote = |router: &crate::config::RouterConfig, token_in: Address, token_out: Address, amount: U256| -> Option<(Address, Vec<u8>)> {
            let router_addr: Address = router.address.parse().ok()?;
            match router.router_type {
                RouterType::V2 => {
                    let cd = IUniswapV2Router02::getAmountsOutCall {
                        amountIn: amount,
                        path: vec![token_in, token_out],
                    }.abi_encode();
                    Some((router_addr, cd))
                }
                RouterType::V3 => {
                    // Per-router quoter takes priority; fall back to chain-level
                    let quoter = router.quoter_address.as_deref()
                        .and_then(|s| s.parse::<Address>().ok())
                        .or(self.quoter_v2_address)?;
                    // Use first fee tier only (limits explosion; most liquid pool usually first)
                    let fee = router.fee_tiers.first().copied().unwrap_or(500);
                    // Skip pools not discovered at startup — almost certainly don't exist.
                    // Prevents QuoterV2 calls (expensive gas, may fail multicall chunks) on
                    // non-configured triplet pairs (e.g. WBTC→USDT has no V3 pool on Linea).
                    // Key format matches pool_cache.rs insert_v3(): "router_id:t0:t1:fee"
                    let t_in = format!("{:?}", token_in).to_lowercase();
                    let t_out = format!("{:?}", token_out).to_lowercase();
                    if !self.pool_cache.v3_by_key.contains_key(
                        &format!("{}:{}:{}:{}", router.id, t_in, t_out, fee)
                    ) {
                        return None;
                    }
                    // Depth check: skip dead/empty pools (avoids wasted QuoterV2 calls)
                    if let Some((_spot, liquidity)) = self.pool_cache.quote_v3_spot(
                        &router.id, token_in, token_out, fee, amount,
                    ) {
                        if liquidity < MIN_V3_LIQUIDITY {
                            return None;
                        }
                    }
                    let cd = IQuoterV2::quoteExactInputSingleCall {
                        params: IQuoterV2::QuoteExactInputSingleParams {
                            tokenIn: token_in,
                            tokenOut: token_out,
                            amountIn: amount,
                            fee: Uint::from(fee),
                            sqrtPriceLimitX96: Uint::ZERO,
                        },
                    }.abi_encode();
                    Some((quoter, cd))
                }
                RouterType::Solidly => {
                    // Only use volatile pool (fee=0) for triangular to limit explosion.
                    // Stable pools are designed for stablecoin pairs, not cross-asset loops.
                    // Skip if volatile pool not discovered at startup — avoids Solidly echo
                    // responses (amountIn=amountOut) for non-existent pairs that waste batch slots.
                    // Key format matches chain.rs discover_pools + pool_cache insert():
                    // "router_id::volatile:token_in:token_out"
                    let t_in = format!("{:?}", token_in).to_lowercase();
                    let t_out = format!("{:?}", token_out).to_lowercase();
                    if !self.pool_cache.by_key.contains_key(
                        &format!("{}::volatile:{}:{}", router.id, t_in, t_out)
                    ) {
                        return None;
                    }
                    let cd = ISolidlyRouter::getAmountsOutCall {
                        amountIn: amount,
                        routes: vec![ISolidlyRouter::Route {
                            from: token_in,
                            to: token_out,
                            stable: false,
                        }],
                    }.abi_encode();
                    Some((router_addr, cd))
                }
                RouterType::SyncSwap => {
                    // Look up pool from cache
                    let token_in_lower = format!("{token_in}").to_lowercase();
                    let token_out_lower = format!("{token_out}").to_lowercase();
                    let cache_key = format!("{}:{}:{}", router.id, token_in_lower, token_out_lower);
                    let pool_addr = self.syncswap_pool_cache.get(&cache_key)
                        .and_then(|opt| opt.as_ref().copied())?;
                    let cd = ISyncSwapPool::getAmountOutCall {
                        tokenIn: token_in,
                        amountIn: amount,
                        sender: Address::ZERO,
                    }.abi_encode();
                    Some((pool_addr, cd))
                }
            }
        };

        let decode_amount = |raw: &[u8], rtype: &RouterType| -> Option<U256> {
            match rtype {
                RouterType::V2 => IUniswapV2Router02::getAmountsOutCall::abi_decode_returns(raw)
                    .ok().and_then(|v| v.last().copied()),
                RouterType::V3 => IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(raw)
                    .ok().map(|r| r.amountOut),
                RouterType::Solidly => {
                    ISolidlyRouter::getAmountsOutCall::abi_decode_returns(raw)
                        .ok().and_then(|v| v.last().copied())
                }
                RouterType::SyncSwap => {
                    ISyncSwapPool::getAmountOutCall::abi_decode_returns(raw).ok()
                }
            }
        };

        // ── Phase 1: A→B ─────────────────────────────────────────────────────────
        // V2 and Solidly-volatile: try pool cache first (zero eth_call).
        // V3 / SyncSwap / Solidly-stable: multicall.

        struct P1Entry {
            triplet_idx: usize,
            router_id: String,
            router_addr: Address,
            router_type: RouterType,
            fee: u32,
            /// Pre-computed A→B amount from local xy=k cache. None = needs multicall result.
            local_result: Option<U256>,
            /// Actual capital deployed for leg A→B.
            /// May be < trip.amount_in when V3 capacity cap is applied.
            effective_amount_in: U256,
        }

        let mut p1_entries: Vec<P1Entry> = Vec::new();
        let mut p1_mc: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut p1_mc_entry_idx: Vec<usize> = Vec::new();
        // Triangular V3 diagnostic counter (no multicall in triangular)
        let mut tri_v3_spot: usize = 0;

        for (ti, trip) in triplets.iter().enumerate() {
            for router in &chain_routers {
                let router_addr: Address = router.address.parse().unwrap_or_default();
                let (fee, effective_amount_in, local_ab): (u32, U256, Option<U256>) = match router.router_type {
                    RouterType::V3 => {
                        // Iterate all fee tiers; pick the one yielding the highest output.
                        // No capacity cap — xy=k price impact naturally penalizes thin pools,
                        // and pre-flight simulation catches any phantom profits.
                        let result = router.fee_tiers.iter().filter_map(|&f| {
                            self.pool_cache.quote_v3_spot(&router.id, trip.token_a, trip.token_b, f, trip.amount_in)
                                .and_then(|(out, liq)| if liq < MIN_V3_LIQUIDITY { None } else { Some((out, f)) })
                        }).max_by_key(|(out, _)| *out);
                        match result {
                            Some((out, f)) => (f, trip.amount_in, Some(out)),
                            None => continue,
                        }
                    }
                    RouterType::V2 => (0, trip.amount_in, self.pool_cache.get_amount_out_by_key(
                        &router.id, trip.token_a, trip.token_b, trip.amount_in, None,
                    )),
                    RouterType::Solidly => {
                        let vol = format!("{}::volatile", router.id);
                        let sta = format!("{}::stable", router.id);
                        (0, trip.amount_in, self.pool_cache.get_amount_out_by_key(
                            &vol, trip.token_a, trip.token_b, trip.amount_in, Some(VOLATILE_MAX_AGE),
                        ).or_else(|| self.pool_cache.get_amount_out_by_key(
                            &sta, trip.token_a, trip.token_b, trip.amount_in, Some(STABLE_MAX_AGE),
                        )))
                    }
                    RouterType::SyncSwap => (0, trip.amount_in, self.pool_cache.get_amount_out_by_key(
                        &router.id, trip.token_a, trip.token_b, trip.amount_in, Some(VOLATILE_MAX_AGE),
                    )),
                };
                if local_ab.is_none() { continue; }
                if router.router_type == RouterType::V3 { tri_v3_spot += 1; }

                // No multicall fallback in triangular — local or skip.
                let mc_call: Option<(Address, Vec<u8>)> = None;

                let entry_idx = p1_entries.len();
                p1_entries.push(P1Entry {
                    triplet_idx: ti,
                    router_id: router.id.clone(),
                    router_addr,
                    router_type: router.router_type.clone(),
                    fee,
                    local_result: local_ab,
                    effective_amount_in,
                });
                if let Some(call) = mc_call {
                    p1_mc_entry_idx.push(entry_idx);
                    p1_mc.push(call);
                }
            }
        }

        let p1_raw = run_multicall(provider, p1_mc, self.rpc_concurrency).await;
        let mut p1_results_by_entry: Vec<Option<Vec<u8>>> = vec![None; p1_entries.len()];
        for (mc_i, &entry_i) in p1_mc_entry_idx.iter().enumerate() {
            p1_results_by_entry[entry_i] = p1_raw.get(mc_i).and_then(|r| r.clone());
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

        let mut p2_entries: Vec<P2Entry> = Vec::new();
        let mut p2_mc: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut p2_mc_entry_idx: Vec<usize> = Vec::new();

        for (p1e, raw_opt) in p1_entries.iter().zip(p1_results_by_entry.iter()) {
            let amount_b: U256 = if let Some(local) = p1e.local_result {
                local
            } else {
                let raw = match raw_opt { Some(r) => r, None => continue };
                match decode_amount(raw, &p1e.router_type).filter(|b| !b.is_zero()) {
                    Some(b) => b, None => continue,
                }
            };
            let trip = &triplets[p1e.triplet_idx];

            for router in &chain_routers {
                let router_addr: Address = router.address.parse().unwrap_or_default();
                let (fee, local_bc): (u32, Option<U256>) = match router.router_type {
                    RouterType::V3 => {
                        let result = router.fee_tiers.iter().filter_map(|&f| {
                            self.pool_cache.quote_v3_spot(&router.id, trip.token_b, trip.token_c, f, amount_b)
                                .and_then(|(out, liq)| if liq < MIN_V3_LIQUIDITY { None } else { Some((out, f)) })
                        }).max_by_key(|(out, _)| *out);
                        match result {
                            Some((out, f)) => (f, Some(out)),
                            None => continue,
                        }
                    }
                    RouterType::V2 => (0, self.pool_cache.get_amount_out_by_key(
                        &router.id, trip.token_b, trip.token_c, amount_b, None,
                    )),
                    RouterType::Solidly => {
                        let vol = format!("{}::volatile", router.id);
                        let sta = format!("{}::stable", router.id);
                        (0, self.pool_cache.get_amount_out_by_key(
                            &vol, trip.token_b, trip.token_c, amount_b, Some(VOLATILE_MAX_AGE),
                        ).or_else(|| self.pool_cache.get_amount_out_by_key(
                            &sta, trip.token_b, trip.token_c, amount_b, Some(STABLE_MAX_AGE),
                        )))
                    }
                    RouterType::SyncSwap => (0, self.pool_cache.get_amount_out_by_key(
                        &router.id, trip.token_b, trip.token_c, amount_b, Some(VOLATILE_MAX_AGE),
                    )),
                };
                if local_bc.is_none() { continue; }
                if router.router_type == RouterType::V3 { tri_v3_spot += 1; }

                let mc_call: Option<(Address, Vec<u8>)> = None;

                let entry_idx = p2_entries.len();
                p2_entries.push(P2Entry {
                    triplet_idx: p1e.triplet_idx,
                    router_ab_id: p1e.router_id.clone(),
                    router_ab_addr: p1e.router_addr,
                    router_ab_type: p1e.router_type.clone(),
                    fee_ab: p1e.fee,
                    router_bc_id: router.id.clone(),
                    router_bc_addr: router_addr,
                    router_bc_type: router.router_type.clone(),
                    fee_bc: fee,
                    local_result: local_bc,
                    effective_amount_in: p1e.effective_amount_in,
                });
                if let Some(call) = mc_call {
                    p2_mc_entry_idx.push(entry_idx);
                    p2_mc.push(call);
                }
            }
        }

        let p2_raw = run_multicall(provider, p2_mc, self.rpc_concurrency).await;
        let mut p2_results_by_entry: Vec<Option<Vec<u8>>> = vec![None; p2_entries.len()];
        for (mc_i, &entry_i) in p2_mc_entry_idx.iter().enumerate() {
            p2_results_by_entry[entry_i] = p2_raw.get(mc_i).and_then(|r| r.clone());
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

        let mut p3_entries: Vec<P3Entry> = Vec::new();
        let mut p3_mc: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut p3_mc_entry_idx: Vec<usize> = Vec::new();

        for (p2e, raw_opt) in p2_entries.iter().zip(p2_results_by_entry.iter()) {
            let amount_c: U256 = if let Some(local) = p2e.local_result {
                local
            } else {
                let raw = match raw_opt { Some(r) => r, None => continue };
                match decode_amount(raw, &p2e.router_bc_type).filter(|c| !c.is_zero()) {
                    Some(c) => c, None => continue,
                }
            };
            let trip = &triplets[p2e.triplet_idx];

            for router in &chain_routers {
                let router_addr: Address = router.address.parse().unwrap_or_default();
                let (fee, local_ca): (u32, Option<U256>) = match router.router_type {
                    RouterType::V3 => {
                        let result = router.fee_tiers.iter().filter_map(|&f| {
                            self.pool_cache.quote_v3_spot(&router.id, trip.token_c, trip.token_a, f, amount_c)
                                .and_then(|(out, liq)| if liq < MIN_V3_LIQUIDITY { None } else { Some((out, f)) })
                        }).max_by_key(|(out, _)| *out);
                        match result {
                            Some((out, f)) => (f, Some(out)),
                            None => continue,
                        }
                    }
                    RouterType::V2 => (0, self.pool_cache.get_amount_out_by_key(
                        &router.id, trip.token_c, trip.token_a, amount_c, None,
                    )),
                    RouterType::Solidly => {
                        let vol = format!("{}::volatile", router.id);
                        let sta = format!("{}::stable", router.id);
                        (0, self.pool_cache.get_amount_out_by_key(
                            &vol, trip.token_c, trip.token_a, amount_c, Some(VOLATILE_MAX_AGE),
                        ).or_else(|| self.pool_cache.get_amount_out_by_key(
                            &sta, trip.token_c, trip.token_a, amount_c, Some(STABLE_MAX_AGE),
                        )))
                    }
                    RouterType::SyncSwap => (0, self.pool_cache.get_amount_out_by_key(
                        &router.id, trip.token_c, trip.token_a, amount_c, Some(VOLATILE_MAX_AGE),
                    )),
                };
                if local_ca.is_none() { continue; }
                if router.router_type == RouterType::V3 { tri_v3_spot += 1; }

                let mc_call: Option<(Address, Vec<u8>)> = None;

                let entry_idx = p3_entries.len();
                p3_entries.push(P3Entry {
                    triplet_idx: p2e.triplet_idx,
                    router_ab_id: p2e.router_ab_id.clone(),
                    router_ab_addr: p2e.router_ab_addr,
                    router_ab_type: p2e.router_ab_type.clone(),
                    fee_ab: p2e.fee_ab,
                    router_bc_id: p2e.router_bc_id.clone(),
                    router_bc_addr: p2e.router_bc_addr,
                    router_bc_type: p2e.router_bc_type.clone(),
                    fee_bc: p2e.fee_bc,
                    router_ca_id: router.id.clone(),
                    router_ca_addr: router_addr,
                    router_ca_type: router.router_type.clone(),
                    fee_ca: fee,
                    local_result: local_ca,
                    effective_amount_in: p2e.effective_amount_in,
                });
                if let Some(call) = mc_call {
                    p3_mc_entry_idx.push(entry_idx);
                    p3_mc.push(call);
                }
            }
        }

        let p3_raw = run_multicall(provider, p3_mc, self.rpc_concurrency).await;
        let mut p3_results_by_entry: Vec<Option<Vec<u8>>> = vec![None; p3_entries.len()];
        for (mc_i, &entry_i) in p3_mc_entry_idx.iter().enumerate() {
            p3_results_by_entry[entry_i] = p3_raw.get(mc_i).and_then(|r| r.clone());
        }

        // ── Calculate profits ─────────────────────────────────────────────────────

        info!(
            "[{}] triangular scan: {} V3 spot, {} triplets, P1={} P2={} P3={} routes",
            self.chain_id, tri_v3_spot, triplets.len(),
            p1_entries.len(), p2_entries.len(), p3_entries.len()
        );

        let mut opportunities: Vec<TriangularOpportunity> = Vec::new();
        let mut best_raw_usd: f64 = 0.0;
        let mut best_tri_spread: f64 = f64::NEG_INFINITY;

        for (p3e, raw_opt) in p3_entries.iter().zip(p3_results_by_entry.iter()) {
            let amount_a_final: U256 = if let Some(local) = p3e.local_result {
                local
            } else {
                let raw = match raw_opt { Some(r) => r, None => continue };
                match decode_amount(raw, &p3e.router_ca_type) {
                    Some(a) => a, None => continue,
                }
            };
            let trip = &triplets[p3e.triplet_idx];

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
            if (amount_a_final - p3e.effective_amount_in).saturating_mul(U256::from(20)) > p3e.effective_amount_in {
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

            // Track best profit seen regardless of threshold (for status logging)
            if profit_usd > best_raw_usd {
                best_raw_usd = profit_usd;
            }

            if profit_usd < self.min_profit_usd { continue; }

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
                router_ab_type: p3e.router_ab_type.clone(),
                router_bc_type: p3e.router_bc_type.clone(),
                router_ca_type: p3e.router_ca_type.clone(),
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

        opportunities.sort_by(|a, b| b.expected_profit.cmp(&a.expected_profit));
        // Deduplicate identical paths (same triplet + same 3 routers)
        opportunities.dedup_by_key(|o| {
            format!("{}|{}|{}|{}", o.triplet_id, o.router_ab_id, o.router_bc_id, o.router_ca_id)
        });
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
                tri.dedup_by_key(|t| t.pair_id.clone());
            }
        }

        if best_tri_spread.is_finite() && !p3_entries.is_empty() {
            info!(
                "[{}] triangular best spread: {:+.3}% ({} routes evaluated)",
                self.chain_id, best_tri_spread * 100.0, p3_entries.len()
            );
        }

        (opportunities, best_raw_usd)
    }
}
