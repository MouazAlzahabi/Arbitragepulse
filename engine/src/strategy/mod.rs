use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use dashmap::DashMap;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use crate::abi::{IERC20, IMulticall3, ISyncSwapClassicPoolFactory, IUniswapV2Pair};
use crate::config::{PairConfig, RouterConfig, RouterType};
use crate::pool_cache::{PoolCache, PoolInfo};
pub use crate::types::*;
use crate::util::u256_to_f64;

pub mod evaluate;
pub mod triangular;
pub mod optimize;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Minimum active-tick liquidity for a V3 pool to be considered tradeable.
/// Real Linea pools: 10^15 – 10^20. Dead/empty pools: near zero.
/// This filters out uninitialized pools and ghost liquidity without needing USD valuation.
pub(crate) const MIN_V3_LIQUIDITY: u128 = 1_000_000_000; // 10^9

/// Max calls per Multicall3 batch.
/// 10 keeps each batch well under ~30M gas (Alchemy eth_call limit) even with expensive
/// V3 quoteExactInputSingle calls that can reach 5M gas/call when tracing ticks.
/// After pool-existence filtering (Changes 3 & 4) the V3/Solidly entry count drops
/// dramatically, so chunk size primarily matters for SyncSwap batches now.
pub(crate) const MULTICALL_CHUNK_SIZE: usize = 10;

// ─── Internal task metadata ───────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct ForwardTask {
    pub pair_idx: usize,
    pub router_id: String,
    pub router_addr: Address,
    pub router_type: RouterType,
    pub fee: u32,
    /// After the forward quote, this field is REPURPOSED to carry the token_out amount.
    pub amount_in: U256,
    /// The actual input amount used for the forward quote.
    /// Used as the reference amount for spread and profit calculations.
    pub capped_amount_in: U256,
    pub token_in: Address,
    pub token_out: Address,
    /// V3 only: the QuoterV2 address used for this task's quote.
    /// Carried through so the reverse phase uses the same quoter for that router.
    pub quoter_addr: Option<Address>,
}

#[derive(Clone)]
pub(crate) struct ReverseTask {
    pub pair_idx: usize,
    pub pair_id: String,
    pub token_in_symbol: String,
    pub token_in_decimals: u8,
    pub router_a_id: String,
    pub router_a_addr: Address,
    pub router_a_type: RouterType,
    pub fee_a: u32,
    pub amount_in: U256,
    pub router_b_id: String,
    pub router_b_addr: Address,
    pub router_b_type: RouterType,
    pub fee_b: u32,
    pub token_out_amount: U256,
    pub token_in: Address,
    pub token_out: Address,
    /// Pre-computed reverse amount from pool cache (zero eth_call). None = needs multicall.
    pub local_back: Option<U256>,
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
    /// Phase 1.5 QuoterV2 result cache, keyed on forward V3 pool state.
    /// Key:   "router_id:token_in_lower:token_out_lower:fee" (same as pool_cache.v3_by_key)
    /// Value: (sqrtPriceX96 at cache time, quoter_out — U256::ZERO = failed/returned 0)
    ///
    /// Cache hit condition: current pool_cache.sqrt_price_x96 == stored sqrtPriceX96.
    /// This auto-invalidates whenever a V3 Swap event updates the pool state, so the
    /// cache is always consistent with on-chain reality without any explicit TTL.
    ///
    /// Effect: reduces Phase 1.5 from 1 QuoterV2 call per scan (~100–200/min) to
    /// 1 call per V3 Swap event per pool — typically 5–20× fewer HTTP requests.
    pub p15_cache: Arc<DashMap<String, (U256, U256)>>,
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
            p15_cache: Arc::new(DashMap::new()),
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
pub(crate) async fn run_multicall<P: Provider + Clone>(
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

// ─── Utility ──────────────────────────────────────────────────────────────────

/// Parse trade amount, capping at max_trade if set.
/// Uses integer-only arithmetic to avoid f64 precision loss on 18-decimal tokens.
pub fn parse_amount_capped(trade_amount: &str, max_trade: Option<&str>, decimals: u8) -> Option<U256> {
    fn str_to_raw(s: &str, decimals: u8) -> Option<U256> {
        let s = s.trim();
        let (whole_str, frac_str) = match s.split_once('.') {
            Some((w, f)) => (w, f),
            None => (s, ""),
        };
        let whole: U256 = whole_str.parse().ok()?;
        let dec = decimals as usize;
        let scale = U256::from(10u64).pow(U256::from(dec));
        if frac_str.is_empty() {
            return Some(whole * scale);
        }
        // Truncate or pad fractional part to exactly `decimals` digits
        let adj_frac = if frac_str.len() > dec {
            &frac_str[..dec]
        } else {
            frac_str
        };
        let frac_val: U256 = adj_frac.parse().ok()?;
        let frac_scale = U256::from(10u64).pow(U256::from(dec - adj_frac.len()));
        Some(whole * scale + frac_val * frac_scale)
    }

    let parsed = str_to_raw(trade_amount, decimals)?;
    let capped = if let Some(max_str) = max_trade {
        let max_val = str_to_raw(max_str, decimals)?;
        parsed.min(max_val)
    } else {
        parsed
    };
    if capped.is_zero() { return None; }
    Some(capped)
}

pub(crate) fn token_amount_to_usd(amount: U256, decimals: u8, symbol: &str, native_price: f64) -> f64 {
    let scale = 10_f64.powi(decimals as i32);
    let float_amount = u256_to_f64(amount) / scale;
    match symbol.to_uppercase().as_str() {
        "USDC" | "USDT" | "DAI" | "WXDAI" | "USDC.E" | "USDBC" => float_amount,
        "WETH" | "ETH" | "WMATIC" | "MATIC" => float_amount * native_price,
        _ => float_amount,
    }
}
