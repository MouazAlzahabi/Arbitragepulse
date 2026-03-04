use alloy::primitives::{Address, Uint, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use futures::future::join_all;
use futures::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use crate::abi::{IERC20, IMulticall3, IQuoterV2, ISolidlyRouter, ISyncSwapClassicPoolFactory, ISyncSwapPool, IUniswapV2Pair, IUniswapV2Router02};
use crate::config::{PairConfig, RouterConfig, RouterType};
use crate::pool_cache::{PoolCache, PoolInfo, VOLATILE_MAX_AGE, STABLE_MAX_AGE};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Minimum active-tick liquidity for a V3 pool to be considered tradeable.
/// Real Linea pools: 10^15 – 10^20. Dead/empty pools: near zero.
/// This filters out uninitialized pools and ghost liquidity without needing USD valuation.
const MIN_V3_LIQUIDITY: u128 = 1_000_000_000; // 10^9

/// Max calls per Multicall3 batch.
/// 10 keeps each batch well under ~30M gas (Alchemy eth_call limit) even with expensive
/// V3 quoteExactInputSingle calls that can reach 5M gas/call when tracing ticks.
/// After pool-existence filtering (Changes 3 & 4) the V3/Solidly entry count drops
/// dramatically, so chunk size primarily matters for SyncSwap batches now.
const MULTICALL_CHUNK_SIZE: usize = 10;

// ─── Opportunity ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
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
    /// After the forward quote, this field is REPURPOSED to carry the token_out amount.
    amount_in: U256,
    /// The actual input amount used for the forward quote (may be capped for V3 via
    /// estimate_safe_v3_capacity to ensure the trade stays within one tick).
    /// Used as the reference amount for spread and profit calculations.
    capped_amount_in: U256,
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
    /// Pre-computed reverse amount from pool cache (zero eth_call). None = needs multicall.
    local_back: Option<U256>,
}


// ─── Per-pair scan snapshot ───────────────────────────────────────────────────

/// Statistics for a single configured pair from the last evaluate() call.
/// Exposed via the /pair-scan API endpoint and the dashboard Pairs tab.
#[derive(Debug, Clone, Serialize)]
pub struct PairScanInfo {
    pub pair_id: String,
    pub chain_id: u64,
    /// Display name without chain prefix, e.g. "USDC→WETH"
    pub display_name: String,
    /// Number of distinct DEXes (router IDs) that produced a non-zero forward quote.
    pub dex_count: usize,
    /// Router IDs that produced a non-zero forward quote.
    pub dex_ids: Vec<String>,
    /// Number of cross-DEX arb combinations examined in reverse phase (≥2 DEXes).
    pub cross_count: usize,
    /// Whether any DEX quoted this pair in the last scan.
    pub was_quoted: bool,
    /// Number of profitable opportunities found for this pair this session.
    pub opp_count: u64,
    /// Whether this pair is disabled via the dashboard toggle.
    pub disabled: bool,
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
    /// Max concurrent Multicall3 chunks per scan (from config rpc_concurrency).
    pub rpc_concurrency: usize,
    /// SyncSwap pool address cache.
    /// Key: "router_id:token_in_lowercase:token_out_lowercase"
    /// Value: Some(pool_address) if pool exists, None if no pool for this pair.
    /// Populated at startup and after config hot-reload via populate_syncswap_pools().
    pub syncswap_pool_cache: HashMap<String, Option<Address>>,
    /// V2/Solidly-volatile pool reserve cache — updated live from on-chain Sync events.
    /// When populated, forward and reverse quotes for V2/Solidly-volatile routers are
    /// computed locally (zero eth_call) instead of via multicall.
    pub pool_cache: Arc<PoolCache>,
    /// Per-pair scan snapshot from the last evaluate() call (2-hop pairs).
    /// Updated atomically at the end of each evaluate(); read by chain.rs for heartbeat publishing.
    pub pair_scan: Mutex<Vec<PairScanInfo>>,
    /// Per-triplet scan snapshot from the last detect_triangular() call.
    /// Updated atomically at the end of each detect_triangular(); merged with pair_scan at heartbeat.
    pub tri_scan: Mutex<Vec<PairScanInfo>>,
    /// Running count of opportunities found per pair/triplet this session.
    pub opp_session_counts: Mutex<HashMap<String, u64>>,
}

impl Strategy {
    pub fn new(
        chain_id: u64,
        pairs: Vec<PairConfig>,
        routers: Vec<RouterConfig>,
        min_profit_usd: f64,
        quoter_v2_address: Option<Address>,
        pool_cache: Arc<PoolCache>,
        rpc_concurrency: usize,
    ) -> Self {
        Self {
            chain_id,
            pairs,
            routers,
            native_price_usd: 2500.0,
            min_profit_usd,
            quoter_v2_address,
            syncswap_pool_cache: HashMap::new(),
            pool_cache,
            rpc_concurrency,
            pair_scan: Mutex::new(Vec::new()),
            tri_scan: Mutex::new(Vec::new()),
            opp_session_counts: Mutex::new(HashMap::new()),
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

        let results = run_multicall(provider, mc_calls, self.rpc_concurrency).await;
        let mut found = 0usize;

        // Collect pool entries for the second round (seeding pool_cache with reserves).
        struct SyncSwapPoolEntry { pool: Address, router_id: String, token_in: Address, token_out: Address, is_stable: bool }
        let mut pool_entries: Vec<SyncSwapPoolEntry> = Vec::new();
        let mut rv_calls: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut seen_pools: std::collections::HashSet<Address> = std::collections::HashSet::new();

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

            // Seed this pool into pool_cache for local computation (0 multicall detection).
            if let Some(pool_addr) = pool_opt {
                if seen_pools.insert(pool_addr) {
                    // SyncSwap Stable pools use Curve StableSwap (An^n Σxi + D = const),
                    // NOT Solidly x³y+xy³=k. The Solidly Newton-Raphson solver produces
                    // wildly wrong outputs for Curve pools → phantom profitable quotes.
                    // Exclude stable pools until a Curve StableSwap solver is implemented.
                    if router_id.contains("stable") { continue; }
                    let token_in: Address = match token_in_lower.parse() { Ok(a) => a, Err(_) => continue };
                    let token_out: Address = match token_out_lower.parse() { Ok(a) => a, Err(_) => continue };
                    let is_stable = false; // SyncSwap classic only (volatile xy=k)
                    rv_calls.push((pool_addr, IUniswapV2Pair::token0Call {}.abi_encode()));
                    rv_calls.push((pool_addr, IUniswapV2Pair::getReservesCall {}.abi_encode()));
                    rv_calls.push((token_in, IERC20::decimalsCall {}.abi_encode()));
                    rv_calls.push((token_out, IERC20::decimalsCall {}.abi_encode()));
                    pool_entries.push(SyncSwapPoolEntry { pool: pool_addr, router_id: router_id.clone(), token_in, token_out, is_stable });
                }
            }
        }

        // Second multicall round: fetch token0 + reserves + decimals to seed pool_cache.
        // After this, the Sync event listener will keep reserves fresh with zero RPC cost.
        if !pool_entries.is_empty() {
            let rv_raw = run_multicall(provider, rv_calls, self.rpc_concurrency).await;

            for (i, pe) in pool_entries.iter().enumerate() {
                let t0_raw  = match rv_raw.get(4 * i)     { Some(Some(r)) => r, _ => continue };
                let res_raw = match rv_raw.get(4 * i + 1) { Some(Some(r)) => r, _ => continue };
                let dec_ti: u8 = rv_raw.get(4 * i + 2).and_then(|r| r.as_ref())
                    .and_then(|r| IERC20::decimalsCall::abi_decode_returns(r).ok()).unwrap_or(18);
                let dec_to: u8 = rv_raw.get(4 * i + 3).and_then(|r| r.as_ref())
                    .and_then(|r| IERC20::decimalsCall::abi_decode_returns(r).ok()).unwrap_or(18);

                let token0   = match IUniswapV2Pair::token0Call::abi_decode_returns(t0_raw).ok()       { Some(t) => t, None => continue };
                let reserves = match IUniswapV2Pair::getReservesCall::abi_decode_returns(res_raw).ok() { Some(r) => r, None => continue };

                let token1 = if pe.token_in == token0 { pe.token_out } else { pe.token_in };
                let (decimals0, decimals1) = if pe.token_in == token0 { (dec_ti, dec_to) } else { (dec_to, dec_ti) };

                self.pool_cache.insert(pe.pool, PoolInfo {
                    token0,
                    token1,
                    reserve0: U256::from(reserves.reserve0),
                    reserve1: U256::from(reserves.reserve1),
                    fee_bps: 10, // SyncSwap default ≈ 0.1% = 10 bps
                    router_id: pe.router_id.clone(),
                    is_stable: pe.is_stable,
                    decimals0,
                    decimals1,
                    last_sync: std::time::Instant::now(), // startup reserves are accurate (just fetched)
                });
            }
            debug!("[chain={}] SyncSwap: {} pools seeded into pool_cache", self.chain_id, pool_entries.len());
        }

        debug!(
            "[chain={}] SyncSwap pool cache populated: {}/{} pairs have pools",
            self.chain_id,
            found,
            self.syncswap_pool_cache.len() / 2
        );
    }

    /// Returns indices into `self.pairs` for all pairs that trade `token_a` or `token_b`.
    /// Used by the targeted scan to narrow evaluation to pairs affected by a Swap event.
    pub fn pairs_for_tokens(&self, token_a: Address, token_b: Address) -> Vec<usize> {
        self.pairs.iter().enumerate()
            .filter(|(_, p)| {
                let ta: Address = p.token_in.parse().unwrap_or_default();
                let tb: Address = p.token_out.parse().unwrap_or_default();
                ta == token_a || ta == token_b || tb == token_a || tb == token_b
            })
            .map(|(i, _)| i)
            .collect()
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
    ///
    /// `pair_mask`: if Some, only evaluate pairs at those indices (targeted scan).
    /// Pass None for a full scan of all pairs.
    pub async fn evaluate<P: Provider + Clone>(
        &self,
        provider: &P,
        pair_mask: Option<&std::collections::HashSet<usize>>,
    ) -> (Vec<ArbOpportunity>, f64, usize, usize, f64, usize) {
        let chain_routers: Vec<&RouterConfig> = self
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

            for router in &chain_routers {
                let router_addr: Address = match router.address.parse() {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                match router.router_type {
                    RouterType::V2 => {
                        // Try local xy=k reserve cache first (no eth_call needed)
                        if let Some(out) = self.pool_cache.get_amount_out_by_key(
                            &router.id, token_in, token_out, amount_in,
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
                            let t_in_s = format!("{:?}", token_in).to_lowercase();
                            let t_out_s = format!("{:?}", token_out).to_lowercase();
                            let v3_key = format!("{}:{}:{}:{}", router.id, t_in_s, t_out_s, fee);
                            let pool_addr = match self.pool_cache.v3_by_key.get(&v3_key) {
                                Some(a) => *a,
                                None => { v3_no_cache += 1; continue; }
                            };
                            let safe_cap = self.pool_cache.estimate_safe_v3_capacity(pool_addr, token_in);
                            let capped_in = if !safe_cap.is_zero() { amount_in.min(safe_cap) } else { amount_in };

                            if let Some((spot_out, liquidity)) = self.pool_cache.quote_v3_spot(
                                &router.id, token_in, token_out, fee, capped_in,
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
                                    capped_amount_in: capped_in,
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
                    RouterType::Solidly => {
                        // Both volatile (fee=0) and stable (fee=1) pools use local reserve cache.
                        // Only adds to pair_quotes when the pool has had a recent Sync event
                        // (last_sync.elapsed() < max_age). Stale = no on-chain activity = skip.
                        for (suffix, fee, max_age) in [
                            ("volatile", 0u32, VOLATILE_MAX_AGE),
                            ("stable",   1u32, STABLE_MAX_AGE),
                        ] {
                            let eff_id = format!("{}::{}", router.id, suffix);
                            if let Some(out) = self.pool_cache.get_amount_out_by_key_fresh(
                                &eff_id, token_in, token_out, amount_in, max_age,
                            ) {
                                pair_quotes.entry(pi).or_default().push(ForwardTask {
                                    pair_idx: pi,
                                    router_id: eff_id,
                                    router_addr,
                                    router_type: RouterType::Solidly,
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
                        if let Some(out) = self.pool_cache.get_amount_out_by_key_fresh(
                            &router.id, token_in, token_out, amount_in, VOLATILE_MAX_AGE,
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
                            &q_b.router_id, token_out, token_in, token_out_amount,
                        ),
                        RouterType::Solidly => {
                            // q_b.router_id is "router::volatile" or "router::stable"
                            let max_age = if fee_b != 0 { STABLE_MAX_AGE } else { VOLATILE_MAX_AGE };
                            self.pool_cache.get_amount_out_by_key_fresh(
                                &q_b.router_id, token_out, token_in, token_out_amount, max_age,
                            )
                        }
                        RouterType::V3 => {
                            // Use cached sqrtPriceX96 spot for reverse V3 leg when fresh
                            self.pool_cache.quote_v3_spot(
                                &q_b.router_id, token_out, token_in, fee_b, token_out_amount,
                            ).map(|(spot_out, _)| spot_out)
                        }
                        RouterType::SyncSwap => {
                            self.pool_cache.get_amount_out_by_key_fresh(
                                &q_b.router_id, token_out, token_in, token_out_amount, VOLATILE_MAX_AGE,
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
        const P15_GATE_SPREAD: f64 = 0.003; // 0.3% — fires QuoterV2 only on real divergence
        let mut p15_gate: std::collections::HashSet<(usize, String, u32)> = std::collections::HashSet::new();

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
                    RouterType::Solidly => {
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

                    // Phase 1.5 gate: local cross-DEX spread is promising AND forward was V3 tick-capped.
                    if spread > P15_GATE_SPREAD && matches!(task.router_a_type, RouterType::V3) {
                        let pair = &self.pairs[task.pair_idx];
                        if let Some(full) = parse_amount_capped(
                            &pair.trade_amount, pair.max_trade.as_deref(), pair.token_in_decimals,
                        ) {
                            if task.amount_in < full {
                                p15_gate.insert((task.pair_idx, task.router_a_id.clone(), task.fee_a));
                            }
                        }
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

                    // Cross-router V3×V3: local sqrtPriceX96 comparison is unreliable —
                    // per-DEX state divergence creates phantom spreads. Phase 1.5 (p15_gate
                    // above) handles these via QuoterV2 in the same scan cycle.
                    let is_v3_x_v3 = matches!(task.router_a_type, RouterType::V3)
                        && matches!(task.router_b_type, RouterType::V3);

                    if profit_usd >= self.min_profit_usd && !is_v3_x_v3 {
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
        if !p15_gate.is_empty() {
            let mut p15_list: Vec<(usize, String, u32, Address, Address, Address, U256)> = Vec::new();
            let mut p15_mc: Vec<(Address, Vec<u8>)> = Vec::new();

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

                let cd = IQuoterV2::quoteExactInputSingleCall {
                    params: IQuoterV2::QuoteExactInputSingleParams {
                        tokenIn: fwd_task.token_in,
                        tokenOut: fwd_task.token_out,
                        amountIn: full_amount,
                        fee: Uint::from(*fee_a),
                        sqrtPriceLimitX96: Uint::ZERO,
                    },
                }.abi_encode();

                p15_list.push((*pi, router_a_id.clone(), *fee_a, fwd_task.router_addr,
                               fwd_task.token_in, fwd_task.token_out, full_amount));
                p15_mc.push((quoter, cd));
            }

            if !p15_mc.is_empty() {
                debug!(
                    "[chain={}] Phase 1.5 (gated): {} V3 upgrade(s) — local spread > {:.1}%",
                    self.chain_id, p15_mc.len(), P15_GATE_SPREAD * 100.0
                );
                let p15_raw = run_multicall(provider, p15_mc, self.rpc_concurrency).await;

                for ((pi, router_a_id, fee_a, router_a_addr, token_in, token_out, full_amount), raw_opt)
                    in p15_list.into_iter().zip(p15_raw.into_iter())
                {
                    let raw = match raw_opt { Some(r) => r, None => continue };
                    let quoter_out = match IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&raw) {
                        Ok(r) if !r.amountOut.is_zero() => r.amountOut,
                        _ => continue,
                    };

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
                        let local_back = match q_b.router_type {
                            RouterType::V2 => self.pool_cache.get_amount_out_by_key(
                                &q_b.router_id, token_out, token_in, quoter_out,
                            ),
                            RouterType::Solidly => {
                                let max_age = if fee_b != 0 { STABLE_MAX_AGE } else { VOLATILE_MAX_AGE };
                                self.pool_cache.get_amount_out_by_key_fresh(
                                    &q_b.router_id, token_out, token_in, quoter_out, max_age,
                                )
                            }
                            RouterType::V3 => self.pool_cache.quote_v3_spot(
                                &q_b.router_id, token_out, token_in, fee_b, quoter_out,
                            ).map(|(out, _)| out),
                            RouterType::SyncSwap => self.pool_cache.get_amount_out_by_key_fresh(
                                &q_b.router_id, token_out, token_in, quoter_out, VOLATILE_MAX_AGE,
                            ),
                        };
                        let amount_back = match local_back.filter(|b| !b.is_zero()) {
                            Some(b) => b, None => continue,
                        };
                        if amount_back <= full_amount { continue; }

                        if (amount_back - full_amount).saturating_mul(U256::from(20)) > full_amount {
                            warn!("[{}] Phase 1.5 phantom: {} | {}/{}", self.chain_id, pair.id,
                                  router_a_id, q_b.router_id);
                            continue;
                        }
                        let profit = amount_back - full_amount;
                        let profit_usd = token_amount_to_usd(
                            profit, pair.token_in_decimals, &pair.token_in_symbol, self.native_price_usd,
                        );

                        let spread = u256_to_f64(amount_back) / u256_to_f64(full_amount) - 1.0;
                        if spread > best_spread_pct { best_spread_pct = spread; }
                        if profit_usd > best_raw_usd { best_raw_usd = profit_usd; }

                        if profit_usd >= self.min_profit_usd {
                            debug!("[{}] Phase 1.5 arb: {} | profit=${:.4} | {}/{}",
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

        opportunities.sort_by(|a, b| b.expected_profit.cmp(&a.expected_profit));

        // ── Update session opp counts and pair scan snapshot ─────────────────────
        {
            let mut opp_counts = self.opp_session_counts.lock().unwrap();
            for opp in &opportunities {
                *opp_counts.entry(opp.pair_id.clone()).or_insert(0) += 1;
            }

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

        // Save all displayable triplets (post token-filter, pre-disable-filter) so
        // tri_scan can show disabled triplets as dimmed rather than making them disappear.
        let all_displayable: Vec<(String, Address, Address, Address)> = triplets.iter()
            .map(|t| (t.triplet_id.clone(), t.token_a, t.token_b, t.token_c))
            .collect();
        let disabled_triplet_ids: std::collections::HashSet<String> = triplets.iter()
            .filter(|t| is_triplet_disabled(t))
            .map(|t| t.triplet_id.clone())
            .collect();

        // Remove disabled entries from the scan (they stay in all_displayable for the UI).
        if !disabled_triplets.is_empty() {
            triplets.retain(|t| !is_triplet_disabled(t));
        }

        if triplets.is_empty() && all_displayable.iter().all(|(id, ..)| disabled_triplet_ids.contains(id)) {
            // Nothing to scan; still need to update tri_scan with disabled markers.
            let mut opp_counts = self.opp_session_counts.lock().unwrap();
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
        let _make_quote = |router: &RouterConfig, token_in: Address, token_out: Address, amount: U256| -> Option<(Address, Vec<u8>)> {
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
                let fee = match router.router_type {
                    RouterType::V2 => 0,
                    RouterType::V3 => router.fee_tiers.first().copied().unwrap_or(500),
                    RouterType::Solidly => 0,
                    RouterType::SyncSwap => 0,
                };

                // For V3 Phase 1: apply Smart Guesser capacity cap so the QuoterV2 query
                // stays within the active tick. Capped amount = effective capital deployed.
                let effective_amount_in = if router.router_type == RouterType::V3 {
                    let t_in_l = format!("{}", trip.token_a).to_lowercase();
                    let t_out_l = format!("{}", trip.token_b).to_lowercase();
                    let v3_key = format!("{}:{}:{}:{}", router.id, t_in_l, t_out_l, fee);
                    let cap = self.pool_cache.v3_by_key.get(&v3_key)
                        .map(|p| self.pool_cache.estimate_safe_v3_capacity(*p, trip.token_a))
                        .unwrap_or(U256::ZERO);
                    if cap.is_zero() { trip.amount_in } else { trip.amount_in.min(cap) }
                } else {
                    trip.amount_in
                };

                // All DEX types use local reserve cache (0 eth_call).
                // Solidly: try volatile then stable, both with freshness check.
                // SyncSwap: pool_cache seeded at startup by populate_syncswap_pools.
                // V3: spot price from sqrtPriceX96; skip if stale (no QuoterV2 in triangular).
                let local_ab = match router.router_type {
                    RouterType::V2 => self.pool_cache.get_amount_out_by_key(
                        &router.id, trip.token_a, trip.token_b, effective_amount_in,
                    ),
                    RouterType::Solidly => {
                        let vol = format!("{}::volatile", router.id);
                        let sta = format!("{}::stable", router.id);
                        self.pool_cache.get_amount_out_by_key_fresh(
                            &vol, trip.token_a, trip.token_b, effective_amount_in, VOLATILE_MAX_AGE,
                        ).or_else(|| self.pool_cache.get_amount_out_by_key_fresh(
                            &sta, trip.token_a, trip.token_b, effective_amount_in, STABLE_MAX_AGE,
                        ))
                    }
                    RouterType::V3 => {
                        self.pool_cache.quote_v3_spot(
                            &router.id, trip.token_a, trip.token_b, fee, effective_amount_in,
                        ).and_then(|(spot_out, liquidity)| {
                            if liquidity < MIN_V3_LIQUIDITY { None } else { Some(spot_out) }
                        })
                    }
                    RouterType::SyncSwap => {
                        self.pool_cache.get_amount_out_by_key_fresh(
                            &router.id, trip.token_a, trip.token_b, effective_amount_in, VOLATILE_MAX_AGE,
                        )
                    }
                };

                // No multicall fallback in triangular — local or skip.
                let mc_call: Option<(Address, Vec<u8>)> = None;

                // Skip router if no local quote available
                if local_ab.is_none() {
                    continue;
                }

                if router.router_type == RouterType::V3 {
                    if local_ab.is_some() { tri_v3_spot += 1; } // mc_call always None — no QuoterV2 in triangular
                }

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
                let fee = match router.router_type {
                    RouterType::V2 => 0,
                    RouterType::V3 => router.fee_tiers.first().copied().unwrap_or(500),
                    RouterType::Solidly => 0,
                    RouterType::SyncSwap => 0,
                };

                let local_bc = match router.router_type {
                    RouterType::V2 => self.pool_cache.get_amount_out_by_key(
                        &router.id, trip.token_b, trip.token_c, amount_b,
                    ),
                    RouterType::Solidly => {
                        let vol = format!("{}::volatile", router.id);
                        let sta = format!("{}::stable", router.id);
                        self.pool_cache.get_amount_out_by_key_fresh(
                            &vol, trip.token_b, trip.token_c, amount_b, VOLATILE_MAX_AGE,
                        ).or_else(|| self.pool_cache.get_amount_out_by_key_fresh(
                            &sta, trip.token_b, trip.token_c, amount_b, STABLE_MAX_AGE,
                        ))
                    }
                    RouterType::V3 => {
                        let fee = router.fee_tiers.first().copied().unwrap_or(500);
                        self.pool_cache.quote_v3_spot(
                            &router.id, trip.token_b, trip.token_c, fee, amount_b,
                        ).and_then(|(spot_out, liquidity)| {
                            if liquidity < MIN_V3_LIQUIDITY { None } else { Some(spot_out) }
                        })
                    }
                    RouterType::SyncSwap => {
                        self.pool_cache.get_amount_out_by_key_fresh(
                            &router.id, trip.token_b, trip.token_c, amount_b, VOLATILE_MAX_AGE,
                        )
                    }
                };

                let mc_call: Option<(Address, Vec<u8>)> = None;

                if local_bc.is_none() {
                    continue;
                }

                if router.router_type == RouterType::V3 {
                    if local_bc.is_some() { tri_v3_spot += 1; }
                }

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
                let fee = match router.router_type {
                    RouterType::V2 => 0,
                    RouterType::V3 => router.fee_tiers.first().copied().unwrap_or(500),
                    RouterType::Solidly => 0,
                    RouterType::SyncSwap => 0,
                };

                let local_ca = match router.router_type {
                    RouterType::V2 => self.pool_cache.get_amount_out_by_key(
                        &router.id, trip.token_c, trip.token_a, amount_c,
                    ),
                    RouterType::Solidly => {
                        let vol = format!("{}::volatile", router.id);
                        let sta = format!("{}::stable", router.id);
                        self.pool_cache.get_amount_out_by_key_fresh(
                            &vol, trip.token_c, trip.token_a, amount_c, VOLATILE_MAX_AGE,
                        ).or_else(|| self.pool_cache.get_amount_out_by_key_fresh(
                            &sta, trip.token_c, trip.token_a, amount_c, STABLE_MAX_AGE,
                        ))
                    }
                    RouterType::V3 => {
                        let fee = router.fee_tiers.first().copied().unwrap_or(500);
                        self.pool_cache.quote_v3_spot(
                            &router.id, trip.token_c, trip.token_a, fee, amount_c,
                        ).and_then(|(spot_out, liquidity)| {
                            if liquidity < MIN_V3_LIQUIDITY { None } else { Some(spot_out) }
                        })
                    }
                    RouterType::SyncSwap => {
                        self.pool_cache.get_amount_out_by_key_fresh(
                            &router.id, trip.token_c, trip.token_a, amount_c, VOLATILE_MAX_AGE,
                        )
                    }
                };

                let mc_call: Option<(Address, Vec<u8>)> = None;

                if local_ca.is_none() {
                    continue;
                }

                if router.router_type == RouterType::V3 {
                    if local_ca.is_some() { tri_v3_spot += 1; }
                }

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

        debug!(
            "[{}] triangular scan: {} V3 spot (0 HTTP), {} triplets (no multicall)",
            self.chain_id, tri_v3_spot, triplets.len()
        );

        let mut opportunities: Vec<TriangularOpportunity> = Vec::new();
        let mut best_raw_usd: f64 = 0.0;

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
            let mut opp_counts = self.opp_session_counts.lock().unwrap();
            for opp in &opportunities {
                *opp_counts.entry(opp.triplet_id.clone()).or_insert(0) += 1;
            }

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
        max_balance: Option<U256>,
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

        // Upper bound: use max_trade if configured, otherwise trade_amount.
        // Previously used double_amount.min(cap) which capped at 2× the tick-capped
        // amount_in ($6 → $12), blocking the optimizer from probing up to $100.
        let double_amount = opp.amount_in * U256::from(2u32);
        let max_amount = self
            .pairs
            .iter()
            .find(|p| p.id == opp.pair_id)
            .and_then(|p| {
                let cap_str = p.max_trade.as_deref().unwrap_or(&p.trade_amount);
                parse_amount_capped(cap_str, None, p.token_in_decimals)
            })
            .unwrap_or(double_amount);

        // Also cap by the contract's known token_in balance (from periodic refresh).
        // Prevents the optimizer from probing amounts the contract cannot cover.
        let max_amount = if let Some(bal) = max_balance {
            if bal.is_zero() {
                // Contract has no balance — cannot execute, skip optimizer
                return opp.clone();
            }
            max_amount.min(bal)
        } else {
            max_amount
        };

        if min_amount >= max_amount || min_amount.is_zero() {
            return opp.clone();
        }

        // ── Resolve per-router QuoterV2 addresses ────────────────────────────
        // RouterConfig.quoter_address overrides the chain-level quoter_v2_address.
        // PancakeV3 and UniswapV3 have different QuoterV2 contracts on Linea.
        let quoter_a: Option<Address> = self.routers.iter()
            .find(|r| r.id == opp.router_a_id)
            .and_then(|r| r.quoter_address.as_deref())
            .and_then(|s| s.parse().ok())
            .or(self.quoter_v2_address);
        let quoter_b: Option<Address> = self.routers.iter()
            .find(|r| r.id == opp.router_b_id)
            .and_then(|r| r.quoter_address.as_deref())
            .and_then(|s| s.parse().ok())
            .or(self.quoter_v2_address);

        // ── Round 1: Coarse scan across full range ────────────────────────────
        let coarse_winner = probe_range(
            opp,
            provider,
            quoter_a,
            quoter_b,
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
            quoter_a,
            quoter_b,
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
/// Batch calls via Multicall3, chunking into groups of MULTICALL_CHUNK_SIZE.
/// All chunks are fired concurrently so total latency ≈ max(slowest chunk), not sum.
/// Falls back to sequential individual eth_calls for any chunk that fails entirely.
async fn run_multicall<P: Provider + Clone>(
    provider: &P,
    calls: Vec<(Address, Vec<u8>)>,
    concurrency: usize,
) -> Vec<Option<Vec<u8>>> {
    if calls.is_empty() {
        return vec![];
    }

    let mc3: Address = "0xcA11bde05977b3631167028862bE2a173976CA11"
        .parse()
        .expect("hardcoded multicall3 address");

    // Materialise chunks into owned Vecs so the stream doesn't hold a borrow across awaits
    // (borrowed slice refs across await → future !Send → tokio::spawn compile error).
    let owned_chunks: Vec<Vec<(Address, Vec<u8>)>> = calls
        .chunks(MULTICALL_CHUNK_SIZE)
        .map(|c| c.to_vec())
        .collect();

    // Run at most MULTICALL_MAX_CONCURRENT chunks at a time; buffer_unordered smooths
    // the Alchemy CU burst that firing everything with join_all caused.
    let chunk_raw: Vec<Option<_>> = futures::stream::iter(owned_chunks.into_iter().map(|chunk| {
        let mc_calls: Vec<IMulticall3::Call3> = chunk
            .iter()
            .map(|(target, data)| IMulticall3::Call3 {
                target: *target,
                allowFailure: true,
                callData: data.clone().into(),
            })
            .collect();
        let agg_calldata = IMulticall3::aggregate3Call { calls: mc_calls }.abi_encode();
        let tx = TransactionRequest::default().to(mc3).input(agg_calldata.into());
        let p = provider.clone();
        async move { p.call(tx).await.ok() }
    }))
    .buffer_unordered(concurrency.max(1))
    .collect()
    .await;

    // Flatten results in order; fall back per-chunk if Multicall3 decode fails.
    let mut out: Vec<Option<Vec<u8>>> = Vec::with_capacity(calls.len());
    let mut call_offset = 0usize;
    for raw_opt in chunk_raw {
        let chunk_len = MULTICALL_CHUNK_SIZE.min(calls.len() - call_offset);
        match raw_opt.and_then(|raw| IMulticall3::aggregate3Call::abi_decode_returns(&raw).ok()) {
            Some(decoded) => {
                out.extend(decoded.into_iter().map(|r| {
                    if r.success { Some(r.returnData.to_vec()) } else { None }
                }));
            }
            None => {
                // Chunk failed (Multicall3 unavailable or gas exceeded) — fall back sequentially.
                warn!("Multicall3 chunk failed (gas limit exceeded?) — falling back to {} sequential eth_calls; reduce MULTICALL_CHUNK_SIZE if this persists", chunk_len);
                for (target, data) in calls[call_offset..call_offset + chunk_len].iter() {
                    let tx = TransactionRequest::default()
                        .to(*target)
                        .input(data.clone().into());
                    out.push(provider.call(tx).await.ok().map(|b| b.to_vec()));
                }
            }
        }
        call_offset += chunk_len;
    }
    out
}

// ─── Probe helper ─────────────────────────────────────────────────────────────

/// Fire `steps` linearly-spaced quote pairs concurrently across [min_amount, max_amount].
/// Returns `(amount_in, amount_back)` for the probe with the highest absolute profit,
/// or `None` if no probe was profitable.
async fn probe_range<P: Provider + Clone + 'static>(
    opp: &ArbOpportunity,
    provider: &P,
    quoter_a: Option<Address>, // QuoterV2 for router A (leg 1 — token_in → token_out)
    quoter_b: Option<Address>, // QuoterV2 for router B (leg 2 — token_out → token_in)
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
                RouterType::V3 => match quoter_a {
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
                RouterType::V3 => match quoter_b {
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
pub(crate) fn parse_amount_capped(trade_amount: &str, max_trade: Option<&str>, decimals: u8) -> Option<U256> {
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
