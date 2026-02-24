use alloy::primitives::{Address, Uint, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use futures::future::join_all;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tracing::debug;

use crate::abi::{IMulticall3, IQuoterV2, ISolidlyRouter, ISyncSwapClassicPoolFactory, ISyncSwapPool, IUniswapV2Router02};
use crate::config::{PairConfig, RouterConfig, RouterType};

// ─── Opportunity ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArbOpportunity {
    pub chain_id: u64,
    pub pair_id: String,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub router_a: Address,
    pub router_b: Address,
    pub router_a_type: RouterType,
    pub router_b_type: RouterType,
    pub fee_a: u32,
    pub fee_b: u32,
    pub expected_profit: U256,
    pub profit_usd: f64,
    pub router_a_id: String,
    pub router_b_id: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TriangularOpportunity {
    pub chain_id: u64,
    pub triplet_id: String, // "USDC-WETH-OP"
    pub token_a: Address,
    pub token_b: Address,
    pub token_c: Address,
    pub amount_in: U256,
    pub router_ab: Address,
    pub router_bc: Address,
    pub router_ca: Address,
    pub router_ab_type: RouterType,
    pub router_bc_type: RouterType,
    pub router_ca_type: RouterType,
    pub fee_ab: u32,
    pub fee_bc: u32,
    pub fee_ca: u32,
    pub expected_profit: U256,
    pub profit_usd: f64,
    pub router_ab_id: String,
    pub router_bc_id: String,
    pub router_ca_id: String,
}

/// Unified opportunity enum for 2-hop and triangular arbitrage.
/// Allows chain.rs to handle both types in a single evaluation loop.
#[derive(Debug, Clone)]
pub enum Opportunity {
    TwoHop(ArbOpportunity),
    Triangular(TriangularOpportunity),
}

impl Opportunity {
    pub fn profit_usd(&self) -> f64 {
        match self {
            Opportunity::TwoHop(o) => o.profit_usd,
            Opportunity::Triangular(o) => o.profit_usd,
        }
    }

    pub fn pair_id(&self) -> &str {
        match self {
            Opportunity::TwoHop(o) => &o.pair_id,
            Opportunity::Triangular(o) => &o.triplet_id,
        }
    }

    /// Returns true if this opportunity can be sent to the contract for execution.
    /// All router types (V2, V3, Solidly, SyncSwap) are supported by the deployed contract.
    pub fn is_executable(&self) -> bool {
        true
    }

    /// Full opportunity fingerprint for deduplication.
    /// Includes pair, routers, and amount to prevent re-evaluating identical opportunities.
    pub fn fingerprint(&self) -> String {
        match self {
            Opportunity::TwoHop(o) => {
                format!("{}|{}|{}|{}",
                    o.pair_id,
                    o.router_a_id,
                    o.router_b_id,
                    o.amount_in
                )
            }
            Opportunity::Triangular(o) => {
                format!("{}|{}|{}|{}|{}",
                    o.triplet_id,
                    o.router_ab_id,
                    o.router_bc_id,
                    o.router_ca_id,
                    o.amount_in
                )
            }
        }
    }
}

// ─── Internal task metadata ───────────────────────────────────────────────────

#[derive(Clone)]
struct ForwardTask {
    pair_idx: usize,
    router_id: String,
    router_addr: Address,
    router_type: RouterType,
    fee: u32,
    amount_in: U256,
    token_in: Address,
    token_out: Address,
    /// V3 only: the QuoterV2 address used for this task's quote.
    /// Carried through so the reverse phase uses the same quoter for that router.
    quoter_addr: Option<Address>,
}

#[derive(Clone)]
#[allow(dead_code)]
struct ReverseTask {
    pair_idx: usize,
    pair_id: String,
    token_in_symbol: String,
    token_in_decimals: u8,
    router_a_id: String,
    router_a_addr: Address,
    router_a_type: RouterType,
    fee_a: u32,
    amount_in: U256,
    router_b_id: String,
    router_b_addr: Address,
    router_b_type: RouterType,
    fee_b: u32,
    token_out_amount: U256,
    token_in: Address,
    token_out: Address,
}


// ─── Strategy ─────────────────────────────────────────────────────────────────

pub struct Strategy {
    pub chain_id: u64,
    pub pairs: Vec<PairConfig>,
    pub routers: Vec<RouterConfig>,
    pub native_price_usd: f64,
    pub min_profit_usd: f64,
    /// QuoterV2 contract address for V3 quotes (chain-specific, NOT the swap router)
    pub quoter_v2_address: Option<Address>,
    /// SyncSwap pool address cache.
    /// Key: "router_id:token_in_lowercase:token_out_lowercase"
    /// Value: Some(pool_address) if pool exists, None if no pool for this pair.
    /// Populated at startup and after config hot-reload via populate_syncswap_pools().
    pub syncswap_pool_cache: HashMap<String, Option<Address>>,
}

impl Strategy {
    pub fn new(
        chain_id: u64,
        pairs: Vec<PairConfig>,
        routers: Vec<RouterConfig>,
        min_profit_usd: f64,
        quoter_v2_address: Option<Address>,
    ) -> Self {
        Self {
            chain_id,
            pairs,
            routers,
            native_price_usd: 2500.0,
            min_profit_usd,
            quoter_v2_address,
            syncswap_pool_cache: HashMap::new(),
        }
    }

    /// Populate the SyncSwap pool address cache by calling getPool() on each factory
    /// for every configured pair. Must be awaited at startup and after config hot-reload.
    ///
    /// SyncSwap config entries use the `address` field as the Pool Factory address.
    /// Both forward (A→B) and reverse (B→A) directions are stored (same pool address).
    pub async fn populate_syncswap_pools<P: Provider + Clone>(&mut self, provider: &P) {
        let syncswap_routers: Vec<RouterConfig> = self
            .routers
            .iter()
            .filter(|r| r.chain_id == self.chain_id && r.router_type == RouterType::SyncSwap)
            .cloned()
            .collect();

        if syncswap_routers.is_empty() {
            return;
        }

        let chain_pairs: Vec<&PairConfig> = self
            .pairs
            .iter()
            .filter(|p| p.chain_id == self.chain_id)
            .collect();

        let mut mc_calls: Vec<(Address, Vec<u8>)> = Vec::new();
        // Metadata: (router_id, token_in_lower, token_out_lower)
        let mut mc_meta: Vec<(String, String, String)> = Vec::new();

        for pair in &chain_pairs {
            let token_in: Address = match pair.token_in.parse() {
                Ok(a) => a,
                Err(_) => continue,
            };
            let token_out: Address = match pair.token_out.parse() {
                Ok(a) => a,
                Err(_) => continue,
            };
            // Normalize addresses by parsing then formatting then lowercasing
            let token_in_lower = format!("{token_in}").to_lowercase();
            let token_out_lower = format!("{token_out}").to_lowercase();

            for router in &syncswap_routers {
                let factory_addr: Address = match router.address.parse() {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                let calldata = ISyncSwapClassicPoolFactory::getPoolCall {
                    tokenA: token_in,
                    tokenB: token_out,
                }
                .abi_encode();

                mc_calls.push((factory_addr, calldata));
                mc_meta.push((router.id.clone(), token_in_lower.clone(), token_out_lower.clone()));
            }
        }

        if mc_calls.is_empty() {
            return;
        }

        let results = run_multicall(provider, mc_calls).await;
        let mut found = 0usize;

        for ((router_id, token_in_lower, token_out_lower), raw_opt) in
            mc_meta.into_iter().zip(results.into_iter())
        {
            let pool_opt: Option<Address> = raw_opt
                .and_then(|raw| {
                    ISyncSwapClassicPoolFactory::getPoolCall::abi_decode_returns(&raw).ok()
                })
                .filter(|a: &Address| !a.is_zero());

            if pool_opt.is_some() {
                found += 1;
            }

            // Store under forward AND reverse key (same pool serves both directions)
            let key_fwd = format!("{}:{}:{}", router_id, token_in_lower, token_out_lower);
            let key_rev = format!("{}:{}:{}", router_id, token_out_lower, token_in_lower);
            self.syncswap_pool_cache.insert(key_fwd, pool_opt);
            self.syncswap_pool_cache.insert(key_rev, pool_opt);
        }

        debug!(
            "[chain={}] SyncSwap pool cache populated: {}/{} pairs have pools",
            self.chain_id,
            found,
            self.syncswap_pool_cache.len() / 2
        );
    }

    /// Evaluate all pairs on all router combinations for arb opportunities.
    /// All quotes run concurrently (two phases: forward then reverse).
    /// Returns `(opportunities, best_raw_profit_usd, fwd_quotes_ok, pairs_with_multi, best_spread_pct, pairs_with_any)`:
    /// - `best_raw_profit_usd`: highest profit even if below min_profit_usd (for status logging)
    /// - `fwd_quotes_ok`: total number of successful non-zero forward quotes
    /// - `pairs_with_multi`: number of pairs with quotes from ≥2 distinct router IDs
    /// - `best_spread_pct`: best (reverse_out/amount_in - 1.0) seen, even when negative.
    ///   Negative = market is efficient (e.g. -0.002 = 0.2% below break-even).
    ///   Positive = profitable spread found.
    /// - `pairs_with_any`: number of pairs with at least 1 non-zero quote (shows coverage)
    pub async fn evaluate<P: Provider + Clone>(&self, provider: &P) -> (Vec<ArbOpportunity>, f64, usize, usize, f64, usize) {
        let chain_routers: Vec<&RouterConfig> = self
            .routers
            .iter()
            .filter(|r| r.chain_id == self.chain_id)
            .collect();

        // ── Phase 1: Forward quotes via multicall3 (1 RPC round-trip) ───────────

        let mut fwd_tasks: Vec<ForwardTask> = Vec::new();
        let mut fwd_mc: Vec<(Address, Vec<u8>)> = Vec::new();

        for (pi, pair) in self.pairs.iter().enumerate().filter(|(_, p)| p.chain_id == self.chain_id) {
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

            for router in &chain_routers {
                let router_addr: Address = match router.address.parse() {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                match router.router_type {
                    RouterType::V2 => {
                        let calldata = IUniswapV2Router02::getAmountsOutCall {
                            amountIn: amount_in,
                            path: vec![token_in, token_out],
                        }
                        .abi_encode();
                        fwd_mc.push((router_addr, calldata));
                        fwd_tasks.push(ForwardTask {
                            pair_idx: pi,
                            router_id: router.id.clone(),
                            router_addr,
                            router_type: RouterType::V2,
                            fee: 0,
                            amount_in,
                            token_in,
                            token_out,
                            quoter_addr: None,
                        });
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
                            let calldata = IQuoterV2::quoteExactInputSingleCall {
                                params: IQuoterV2::QuoteExactInputSingleParams {
                                    tokenIn: token_in,
                                    tokenOut: token_out,
                                    amountIn: amount_in,
                                    fee: Uint::from(fee),
                                    sqrtPriceLimitX96: Uint::ZERO,
                                },
                            }
                            .abi_encode();
                            fwd_mc.push((quoter, calldata));
                            fwd_tasks.push(ForwardTask {
                                pair_idx: pi,
                                router_id: router.id.clone(),
                                router_addr,
                                router_type: RouterType::V3,
                                fee,
                                amount_in,
                                token_in,
                                token_out,
                                quoter_addr: Some(quoter),
                            });
                        }
                    }
                    RouterType::Solidly => {
                        // Try both volatile (fee=0) and stable (fee=1) pools.
                        // fee encoding: 0=volatile (vAMM xy=k), 1=stable (sAMM x³y+y³x=k)
                        for stable_flag in [0u32, 1u32] {
                            let stable = stable_flag != 0;
                            let calldata = ISolidlyRouter::getAmountsOutCall {
                                amountIn: amount_in,
                                routes: vec![ISolidlyRouter::Route {
                                    from: token_in,
                                    to: token_out,
                                    stable,
                                }],
                            }
                            .abi_encode();
                            fwd_mc.push((router_addr, calldata));
                            fwd_tasks.push(ForwardTask {
                                pair_idx: pi,
                                router_id: format!("{}::{}", router.id, if stable { "stable" } else { "volatile" }),
                                router_addr,
                                router_type: RouterType::Solidly,
                                fee: stable_flag,
                                amount_in,
                                token_in,
                                token_out,
                                quoter_addr: None,
                            });
                        }
                    }
                    RouterType::SyncSwap => {
                        // Look up pool address from cache (populated at startup).
                        let token_in_lower = format!("{token_in}").to_lowercase();
                        let token_out_lower = format!("{token_out}").to_lowercase();
                        let cache_key = format!("{}:{}:{}", router.id, token_in_lower, token_out_lower);
                        if let Some(Some(pool_addr)) = self.syncswap_pool_cache.get(&cache_key) {
                            let pool_addr = *pool_addr;
                            let calldata = ISyncSwapPool::getAmountOutCall {
                                tokenIn: token_in,
                                amountIn: amount_in,
                                sender: Address::ZERO,
                            }
                            .abi_encode();
                            fwd_mc.push((pool_addr, calldata));
                            fwd_tasks.push(ForwardTask {
                                pair_idx: pi,
                                router_id: router.id.clone(),
                                router_addr,
                                router_type: RouterType::SyncSwap,
                                fee: 0,
                                amount_in,
                                token_in,
                                token_out,
                                // quoter_addr carries the pool address for reverse-phase lookup
                                quoter_addr: Some(pool_addr),
                            });
                        }
                    }
                }
            }
        }

        let fwd_raw = run_multicall(provider, fwd_mc).await;

        // Group successful forward quotes by pair_idx
        let mut pair_quotes: std::collections::HashMap<usize, Vec<ForwardTask>> =
            std::collections::HashMap::new();

        for (task, raw_opt) in fwd_tasks.into_iter().zip(fwd_raw.into_iter()) {
            let raw = match raw_opt { Some(r) => r, None => continue };
            let amount_out = match task.router_type {
                RouterType::V2 => IUniswapV2Router02::getAmountsOutCall::abi_decode_returns(&raw)
                    .ok()
                    .and_then(|v| v.last().copied()),
                RouterType::V3 => IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&raw)
                    .ok()
                    .map(|r| r.amountOut),
                RouterType::Solidly => {
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
            let unique_routers: std::collections::HashSet<&String> =
                v.iter().map(|q| &q.router_id).collect();
            unique_routers.len() >= 2
        }).count();
        debug!(
            "[chain={}] Forward quotes: ok={} pairs_with_any={} pairs_with_multi_dex={}",
            self.chain_id, total_fwd_ok, pairs_with_any, pairs_with_multi
        );

        // ── Phase 2: Reverse quotes via multicall3 (1 RPC round-trip) ───────────

        let mut rev_tasks: Vec<ReverseTask> = Vec::new();
        let mut rev_mc: Vec<(Address, Vec<u8>)> = Vec::new();

        for (pi, quotes) in &pair_quotes {
            if quotes.len() < 2 {
                continue;
            }
            let pair = &self.pairs[*pi];
            let original_amount_in = match parse_amount_capped(
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

                    let token_in = q_a.token_in;
                    let token_out = q_a.token_out;
                    let rb_addr = q_b.router_addr;
                    let fee_b = q_b.fee;

                    let (target, calldata) = match q_b.router_type {
                        RouterType::V2 => {
                            let cd = IUniswapV2Router02::getAmountsOutCall {
                                amountIn: token_out_amount,
                                path: vec![token_out, token_in],
                            }
                            .abi_encode();
                            (rb_addr, cd)
                        }
                        RouterType::V3 => {
                            // Use the same quoter that worked for q_b's forward quote
                            let quoter = match q_b.quoter_addr.or(self.quoter_v2_address) {
                                Some(q) => q,
                                None => continue,
                            };
                            let cd = IQuoterV2::quoteExactInputSingleCall {
                                params: IQuoterV2::QuoteExactInputSingleParams {
                                    tokenIn: token_out,
                                    tokenOut: token_in,
                                    amountIn: token_out_amount,
                                    fee: Uint::from(fee_b),
                                    sqrtPriceLimitX96: Uint::ZERO,
                                },
                            }
                            .abi_encode();
                            (quoter, cd)
                        }
                        RouterType::Solidly => {
                            // fee_b: 0=volatile, 1=stable — same encoding as forward
                            let stable = fee_b != 0;
                            let cd = ISolidlyRouter::getAmountsOutCall {
                                amountIn: token_out_amount,
                                routes: vec![ISolidlyRouter::Route {
                                    from: token_out,
                                    to: token_in,
                                    stable,
                                }],
                            }
                            .abi_encode();
                            (rb_addr, cd)
                        }
                        RouterType::SyncSwap => {
                            // quoter_addr carries the pool address from the forward phase
                            let pool_addr = match q_b.quoter_addr {
                                Some(p) => p,
                                None => continue,
                            };
                            let cd = ISyncSwapPool::getAmountOutCall {
                                tokenIn: token_out,
                                amountIn: token_out_amount,
                                sender: Address::ZERO,
                            }
                            .abi_encode();
                            (pool_addr, cd)
                        }
                    };

                    rev_mc.push((target, calldata));
                    rev_tasks.push(ReverseTask {
                        pair_idx: *pi,
                        pair_id: pair.id.clone(),
                        token_in_symbol: pair.token_in_symbol.clone(),
                        token_in_decimals: pair.token_in_decimals,
                        router_a_id: q_a.router_id.clone(),
                        router_a_addr: q_a.router_addr,
                        router_a_type: q_a.router_type.clone(),
                        fee_a: q_a.fee,
                        amount_in: original_amount_in,
                        router_b_id: q_b.router_id.clone(),
                        router_b_addr: rb_addr,
                        router_b_type: q_b.router_type.clone(),
                        fee_b,
                        token_out_amount,
                        token_in,
                        token_out,
                    });
                }
            }
        }

        let rev_raw = run_multicall(provider, rev_mc).await;

        // ── Find profitable opportunities ──────────────────────────────────────

        let mut opportunities = Vec::new();
        let mut best_raw_usd: f64 = 0.0;
        // Best signed spread ratio: (reverse_out / amount_in) - 1.0
        // Negative = below break-even; positive = profitable.
        let mut best_spread_pct: f64 = f64::NEG_INFINITY;

        for (task, raw_opt) in rev_tasks.iter().zip(rev_raw.into_iter()) {
            let raw = match raw_opt { Some(r) => r, None => continue };
            let amount_back_opt = match task.router_b_type {
                RouterType::V2 => IUniswapV2Router02::getAmountsOutCall::abi_decode_returns(&raw)
                    .ok()
                    .and_then(|v| v.last().copied()),
                RouterType::V3 => IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&raw)
                    .ok()
                    .map(|r| r.amountOut),
                RouterType::Solidly => {
                    ISolidlyRouter::getAmountsOutCall::abi_decode_returns(&raw)
                        .ok()
                        .and_then(|v| v.last().copied())
                }
                RouterType::SyncSwap => {
                    ISyncSwapPool::getAmountOutCall::abi_decode_returns(&raw).ok()
                }
            };
            if let Some(amount_back) = amount_back_opt.filter(|&b| !b.is_zero()) {
                // Track signed spread % for all quotes (even unprofitable)
                let amount_in_f64 = task.amount_in.to::<u128>() as f64;
                let amount_back_f64 = amount_back.to::<u128>() as f64;
                if amount_in_f64 > 0.0 {
                    let spread = amount_back_f64 / amount_in_f64 - 1.0;
                    if spread > best_spread_pct {
                        best_spread_pct = spread;
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

                    if profit_usd >= self.min_profit_usd {
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

        opportunities.sort_by(|a, b| b.expected_profit.cmp(&a.expected_profit));
        (opportunities, best_raw_usd, total_fwd_ok, pairs_with_multi, best_spread_pct, pairs_with_any)
    }

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
    pub async fn detect_triangular<P: Provider + Clone>(
        &self,
        provider: &P,
        max_opportunities: usize,
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

        let chain_routers: Vec<&RouterConfig> = self.routers.iter()
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

        let max_tokens = 6usize;
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

        if triplets.is_empty() {
            return (vec![], 0.0);
        }

        // Helper: build a single quote call for (token_in → token_out, amount, router)
        // Returns (multicall_target, calldata) or None if unsupported.
        let make_quote = |router: &RouterConfig, token_in: Address, token_out: Address, amount: U256| -> Option<(Address, Vec<u8>)> {
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

        struct P1Entry {
            triplet_idx: usize,
            router_id: String,
            router_addr: Address,
            router_type: RouterType,
            fee: u32,
        }

        let mut p1_entries: Vec<P1Entry> = Vec::new();
        let mut p1_mc: Vec<(Address, Vec<u8>)> = Vec::new();

        for (ti, trip) in triplets.iter().enumerate() {
            for router in &chain_routers {
                if let Some(call) = make_quote(router, trip.token_a, trip.token_b, trip.amount_in) {
                    let router_addr: Address = router.address.parse().unwrap_or_default();
                    let fee = match router.router_type {
                        RouterType::V2 => 0,
                        RouterType::V3 => router.fee_tiers.first().copied().unwrap_or(500),
                        RouterType::Solidly => 0, // volatile pool (fee=0) for triangular
                        RouterType::SyncSwap => 0,
                    };
                    p1_mc.push(call);
                    p1_entries.push(P1Entry {
                        triplet_idx: ti,
                        router_id: router.id.clone(),
                        router_addr,
                        router_type: router.router_type.clone(),
                        fee,
                    });
                }
            }
        }

        let p1_results = run_multicall(provider, p1_mc).await;

        // ── Phase 2: B→C with actual amount_b ────────────────────────────────────

        struct P2Entry {
            triplet_idx: usize,
            router_ab_id: String, router_ab_addr: Address,
            router_ab_type: RouterType, fee_ab: u32,
            router_bc_id: String, router_bc_addr: Address,
            router_bc_type: RouterType, fee_bc: u32,
        }

        let mut p2_entries: Vec<P2Entry> = Vec::new();
        let mut p2_mc: Vec<(Address, Vec<u8>)> = Vec::new();

        for (p1e, raw_opt) in p1_entries.iter().zip(p1_results.iter()) {
            let raw = match raw_opt { Some(r) => r, None => continue };
            let amount_b = match decode_amount(raw, &p1e.router_type).filter(|b| !b.is_zero()) {
                Some(b) => b, None => continue,
            };
            let trip = &triplets[p1e.triplet_idx];

            for router in &chain_routers {
                if let Some(call) = make_quote(router, trip.token_b, trip.token_c, amount_b) {
                    let router_addr: Address = router.address.parse().unwrap_or_default();
                    let fee = match router.router_type {
                        RouterType::V2 => 0,
                        RouterType::V3 => router.fee_tiers.first().copied().unwrap_or(500),
                        RouterType::Solidly => 0,
                        RouterType::SyncSwap => 0,
                    };
                    p2_mc.push(call);
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
                    });
                }
            }
        }

        let p2_results = run_multicall(provider, p2_mc).await;

        // ── Phase 3: C→A with actual amount_c ────────────────────────────────────

        struct P3Entry {
            triplet_idx: usize,
            router_ab_id: String, router_ab_addr: Address,
            router_ab_type: RouterType, fee_ab: u32,
            router_bc_id: String, router_bc_addr: Address,
            router_bc_type: RouterType, fee_bc: u32,
            router_ca_id: String, router_ca_addr: Address,
            router_ca_type: RouterType, fee_ca: u32,
        }

        let mut p3_entries: Vec<P3Entry> = Vec::new();
        let mut p3_mc: Vec<(Address, Vec<u8>)> = Vec::new();

        for (p2e, raw_opt) in p2_entries.iter().zip(p2_results.iter()) {
            let raw = match raw_opt { Some(r) => r, None => continue };
            let amount_c = match decode_amount(raw, &p2e.router_bc_type).filter(|c| !c.is_zero()) {
                Some(c) => c, None => continue,
            };
            let trip = &triplets[p2e.triplet_idx];

            for router in &chain_routers {
                if let Some(call) = make_quote(router, trip.token_c, trip.token_a, amount_c) {
                    let router_addr: Address = router.address.parse().unwrap_or_default();
                    let fee = match router.router_type {
                        RouterType::V2 => 0,
                        RouterType::V3 => router.fee_tiers.first().copied().unwrap_or(500),
                        RouterType::Solidly => 0,
                        RouterType::SyncSwap => 0,
                    };
                    p3_mc.push(call);
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
                    });
                }
            }
        }

        let p3_results = run_multicall(provider, p3_mc).await;

        // ── Calculate profits ─────────────────────────────────────────────────────

        let mut opportunities: Vec<TriangularOpportunity> = Vec::new();
        let mut best_raw_usd: f64 = 0.0;

        for (p3e, raw_opt) in p3_entries.iter().zip(p3_results.iter()) {
            let raw = match raw_opt { Some(r) => r, None => continue };
            let amount_a_final = match decode_amount(raw, &p3e.router_ca_type) {
                Some(a) => a, None => continue,
            };
            let trip = &triplets[p3e.triplet_idx];
            if amount_a_final <= trip.amount_in { continue; }

            let profit = amount_a_final - trip.amount_in;

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
                amount_in: trip.amount_in,
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
        (opportunities, best_raw_usd)
    }

    /// Two-round adaptive size optimizer (~2 RTTs, ~200ms).
    ///
    /// Round 1 (coarse): 5 probes linearly across [10% → 200%] of `amount_in`,
    /// capped by `max_trade` if the pair has one configured.
    ///
    /// Round 2 (fine): 5 probes in a tight window (±one coarse step) around
    /// the Round 1 winner.
    ///
    /// Returns the opportunity with the amount that maximises absolute profit.
    /// Falls back to the original opportunity unchanged if no improvement found.
    ///
    /// Fast-path: if the detected spread is already >5%, the optimizer is
    /// skipped entirely (~100ms savings). At that spread the marginal gain from
    /// probing is small compared to the latency cost of 20 extra RPC calls.
    pub async fn optimize<P: Provider + Clone + 'static>(
        &self,
        opp: &ArbOpportunity,
        provider: &P,
    ) -> ArbOpportunity {
        const COARSE: usize = 5;
        const FINE: usize = 5;
        const LARGE_SPREAD_THRESHOLD: f64 = 0.05; // 5 %

        // Skip optimizer for wide spreads — the extra 20 RPC calls cost more
        // latency than they gain in profit precision.
        if !opp.amount_in.is_zero() {
            let spread = u256_to_f64(opp.expected_profit) / u256_to_f64(opp.amount_in);
            if spread >= LARGE_SPREAD_THRESHOLD {
                debug!(
                    "[{}] Spread {:.1}% ≥ {:.0}% — skipping optimizer",
                    opp.pair_id,
                    spread * 100.0,
                    LARGE_SPREAD_THRESHOLD * 100.0,
                );
                return opp.clone();
            }
        }

        // Lower bound: 10% of configured trade amount
        let min_amount = opp.amount_in / U256::from(10u32);

        // Upper bound: 2× amount_in, capped by max_trade when configured
        let double_amount = opp.amount_in * U256::from(2u32);
        let max_amount = self
            .pairs
            .iter()
            .find(|p| p.id == opp.pair_id)
            .and_then(|p| p.max_trade.as_deref())
            .and_then(|s| {
                let pair = self.pairs.iter().find(|p| p.id == opp.pair_id)?;
                parse_amount_capped(s, None, pair.token_in_decimals)
            })
            .map(|cap| double_amount.min(cap))
            .unwrap_or(double_amount);

        if min_amount >= max_amount || min_amount.is_zero() {
            return opp.clone();
        }

        // ── Round 1: Coarse scan across full range ────────────────────────────
        let coarse_winner = probe_range(
            opp,
            provider,
            self.quoter_v2_address,
            min_amount,
            max_amount,
            COARSE,
        )
        .await;

        let Some((coarse_amount, _)) = coarse_winner else {
            return opp.clone();
        };

        // ── Round 2: Fine scan around Round 1 winner ──────────────────────────
        let coarse_step = (max_amount - min_amount) / U256::from((COARSE - 1) as u128);
        let fine_min = coarse_amount.saturating_sub(coarse_step).max(min_amount);
        let fine_max = (coarse_amount + coarse_step).min(max_amount);

        let fine_winner = probe_range(
            opp,
            provider,
            self.quoter_v2_address,
            fine_min,
            fine_max,
            FINE,
        )
        .await;

        // Pick whichever gave more absolute profit
        let (opt_amount, opt_back) = [coarse_winner, fine_winner]
            .into_iter()
            .flatten()
            .max_by_key(|(amt, back)| *back - *amt)
            .unwrap_or((opp.amount_in, opp.amount_in + opp.expected_profit));

        let opt_profit = if opt_back > opt_amount {
            opt_back - opt_amount
        } else {
            return opp.clone();
        };

        // Only update if strictly better than the originally detected opportunity
        if opt_profit <= opp.expected_profit {
            return opp.clone();
        }

        let profit_scale = u256_to_f64(opt_profit) / u256_to_f64(opp.expected_profit);

        debug!(
            "[{}] Size optimized: ${:.4} → ${:.4} (+{:.1}%)",
            opp.pair_id,
            opp.profit_usd,
            opp.profit_usd * profit_scale,
            (profit_scale - 1.0) * 100.0,
        );

        ArbOpportunity {
            amount_in: opt_amount,
            expected_profit: opt_profit,
            profit_usd: opp.profit_usd * profit_scale,
            ..opp.clone()
        }
    }

    pub fn update_native_price(&mut self, price: f64) {
        self.native_price_usd = price;
    }

}

// ─── Multicall3 batch helper ───────────────────────────────────────────────────

/// Batch all calls into a single `eth_call` to Multicall3.
/// Returns `Some(Vec<u8>)` for each call that succeeded, `None` for failures.
/// Falls back to parallel individual `eth_call`s if Multicall3 is unavailable.
async fn run_multicall<P: Provider>(
    provider: &P,
    calls: Vec<(Address, Vec<u8>)>,
) -> Vec<Option<Vec<u8>>> {
    if calls.is_empty() {
        return vec![];
    }

    let mc3: Address = "0xcA11bde05977b3631167028862bE2a173976CA11"
        .parse()
        .expect("hardcoded multicall3 address");

    let mc_calls: Vec<IMulticall3::Call3> = calls
        .iter()
        .map(|(target, data)| IMulticall3::Call3 {
            target: *target,
            allowFailure: true,
            callData: data.clone().into(),
        })
        .collect();

    let agg_calldata = IMulticall3::aggregate3Call { calls: mc_calls }.abi_encode();
    let tx = TransactionRequest::default()
        .to(mc3)
        .input(agg_calldata.into());

    if let Ok(raw) = provider.call(tx).await {
        if let Ok(ret) = IMulticall3::aggregate3Call::abi_decode_returns(&raw) {
            return ret
                .into_iter()
                .map(|r| if r.success { Some(r.returnData.to_vec()) } else { None })
                .collect();
        }
    }

    // Fallback: sequential individual eth_calls (e.g. local chain without Multicall3)
    debug!("Multicall3 unavailable — using sequential individual calls");
    let mut results: Vec<Option<Vec<u8>>> = Vec::with_capacity(calls.len());
    for (target, data) in calls.iter() {
        let tx = TransactionRequest::default()
            .to(*target)
            .input(data.clone().into());
        results.push(provider.call(tx).await.ok().map(|b| b.to_vec()));
    }
    results
}

// ─── Probe helper ─────────────────────────────────────────────────────────────

/// Fire `steps` linearly-spaced quote pairs concurrently across [min_amount, max_amount].
/// Returns `(amount_in, amount_back)` for the probe with the highest absolute profit,
/// or `None` if no probe was profitable.
async fn probe_range<P: Provider + Clone + 'static>(
    opp: &ArbOpportunity,
    provider: &P,
    quoter: Option<Address>,
    min_amount: U256,
    max_amount: U256,
    steps: usize,
) -> Option<(U256, U256)> {
    if steps == 0 || min_amount >= max_amount {
        return None;
    }

    let range = max_amount - min_amount;
    let token_in = opp.token_in;
    let token_out = opp.token_out;
    let ra = opp.router_a;
    let ra_type = opp.router_a_type.clone();
    let rb = opp.router_b;
    let rb_type = opp.router_b_type.clone();
    let fa = opp.fee_a;
    let fb = opp.fee_b;

    type BoxFut = Pin<Box<dyn Future<Output = Option<(U256, U256)>> + Send>>;
    let mut futs: Vec<BoxFut> = Vec::with_capacity(steps);

    for i in 0..steps {
        let p = provider.clone();
        let probe_amount = if steps == 1 {
            (min_amount + max_amount) / U256::from(2u32)
        } else {
            min_amount + range * U256::from(i as u128) / U256::from((steps - 1) as u128)
        };
        let ra_t = ra_type.clone();
        let rb_t = rb_type.clone();

        futs.push(Box::pin(async move {
            let mid = match ra_t {
                RouterType::V2 => quote_v2(&p, ra, probe_amount, token_in, token_out).await,
                RouterType::V3 => match quoter {
                    Some(q) => quote_v3(&p, q, probe_amount, token_in, token_out, fa).await,
                    None => None,
                },
                RouterType::Solidly => {
                    quote_solidly(&p, ra, probe_amount, token_in, token_out, fa).await
                }
                RouterType::SyncSwap => None, // pool address not available in probe_range
            };
            let mid = mid.filter(|m| !m.is_zero())?;

            let back = match rb_t {
                RouterType::V2 => quote_v2(&p, rb, mid, token_out, token_in).await,
                RouterType::V3 => match quoter {
                    Some(q) => quote_v3(&p, q, mid, token_out, token_in, fb).await,
                    None => None,
                },
                RouterType::Solidly => {
                    quote_solidly(&p, rb, mid, token_out, token_in, fb).await
                }
                RouterType::SyncSwap => None,
            };
            back.filter(|&b| b > probe_amount).map(|b| (probe_amount, b))
        }));
    }

    join_all(futs)
        .await
        .into_iter()
        .flatten()
        .max_by_key(|(amt, back)| *back - *amt)
}

// ─── Quote helpers ────────────────────────────────────────────────────────────

async fn quote_v2<P: Provider>(
    provider: &P,
    router: Address,
    amount_in: U256,
    token_in: Address,
    token_out: Address,
) -> Option<U256> {
    let path = vec![token_in, token_out];
    match IUniswapV2Router02::new(router, provider)
        .getAmountsOut(amount_in, path)
        .call()
        .await
    {
        Ok(amounts) => amounts.last().copied(),
        Err(e) => {
            debug!("V2 quote failed on {:?}: {}", router, e);
            None
        }
    }
}

async fn quote_v3<P: Provider>(
    provider: &P,
    quoter: Address,
    amount_in: U256,
    token_in: Address,
    token_out: Address,
    fee: u32,
) -> Option<U256> {
    let fee_u24: Uint<24, 1> = Uint::from(fee);
    let sqrt_zero: Uint<160, 3> = Uint::ZERO;

    match IQuoterV2::new(quoter, provider)
        .quoteExactInputSingle(IQuoterV2::QuoteExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount_in,
            fee: fee_u24,
            sqrtPriceLimitX96: sqrt_zero,
        })
        .call()
        .await
    {
        Ok(r) => Some(r.amountOut),
        Err(e) => {
            debug!("V3 quote failed on {:?} fee={}: {}", quoter, fee, e);
            None
        }
    }
}

async fn quote_solidly<P: Provider>(
    provider: &P,
    router: Address,
    amount_in: U256,
    token_in: Address,
    token_out: Address,
    fee: u32, // 0=volatile, 1=stable
) -> Option<U256> {
    let stable = fee != 0;
    match ISolidlyRouter::new(router, provider)
        .getAmountsOut(
            amount_in,
            vec![ISolidlyRouter::Route {
                from: token_in,
                to: token_out,
                stable,
            }],
        )
        .call()
        .await
    {
        Ok(amounts) => amounts.last().copied(),
        Err(e) => {
            debug!("Solidly quote failed on {:?} stable={}: {}", router, stable, e);
            None
        }
    }
}

// ─── Utility ──────────────────────────────────────────────────────────────────

/// Parse trade amount, capping at max_trade if set.
fn parse_amount_capped(trade_amount: &str, max_trade: Option<&str>, decimals: u8) -> Option<U256> {
    let parsed: f64 = trade_amount.parse().ok()?;
    let capped = if let Some(max_str) = max_trade {
        let max: f64 = max_str.parse().ok()?;
        parsed.min(max)
    } else {
        parsed
    };
    let scale = 10_u128.pow(decimals as u32);
    let raw = (capped * scale as f64) as u128;
    if raw == 0 {
        return None;
    }
    Some(U256::from(raw))
}

fn token_amount_to_usd(amount: U256, decimals: u8, symbol: &str, native_price: f64) -> f64 {
    let scale = 10_f64.powi(decimals as i32);
    let float_amount = u256_to_f64(amount) / scale;
    match symbol.to_uppercase().as_str() {
        "USDC" | "USDT" | "DAI" | "WXDAI" | "USDC.E" | "USDBC" => float_amount,
        "WETH" | "ETH" | "WMATIC" | "MATIC" => float_amount * native_price,
        _ => float_amount,
    }
}

fn u256_to_f64(value: U256) -> f64 {
    value.to_string().parse::<f64>().unwrap_or(0.0)
}
