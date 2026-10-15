use alloy::primitives::{Address, Uint, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use dashmap::DashMap;
use futures::StreamExt;
use std::collections::{hash_map::Entry, HashMap};
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use crate::abi::{IERC20, IMulticall3, IQuoterV2, ISyncSwapClassicPoolFactory, IUniswapV2Pair};
use crate::config::{PairConfig, RouterConfig, RouterType};
use crate::pool_cache::{PoolCache, PoolInfo, V2PoolKey, V3PoolKey};
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

/// QuoterV2 post-buffer (bps): removed from `amount_back` before profit vs `min_profit_usd`.
/// Used in Phase 1.5/1.5b/1.5c and optimize.rs — keep all paths on this single constant.
pub(crate) const P15_QUOTER_HAIRCUT_BPS: u64 = 15;

/// Detection / execution buffer applied to quoted output before profit checks (Phase 1.1).
pub(crate) fn apply_exec_quote_haircut(amount_out: U256) -> U256 {
    (amount_out * U256::from(10_000 - P15_QUOTER_HAIRCUT_BPS)) / U256::from(10_000u32)
}

/// QuoterV2 `quoteExactInputSingle` calldata (Uniswap V3 ABI, `uint24 fee`).
/// Shared by every QuoterV2 batch so the encoding lives in one place.
pub(crate) fn encode_v3_quote(token_in: Address, token_out: Address, fee: u32, amount_in: U256) -> Vec<u8> {
    IQuoterV2::quoteExactInputSingleCall {
        params: IQuoterV2::QuoteExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount_in,
            fee: Uint::from(fee),
            sqrtPriceLimitX96: Uint::ZERO,
        },
    }.abi_encode()
}

/// Decode one QuoterV2 result from [`run_multicall`]. None = call failed or quoted 0.
pub(crate) fn decode_v3_quote(raw: Option<&Vec<u8>>) -> Option<U256> {
    raw.and_then(|r| IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(r).ok())
        .map(|r| r.amountOut)
        .filter(|a| !a.is_zero())
}

/// Warn when a router's QuoterV2 has failed every call in this many consecutive batches
/// without ever having returned a quote — most likely a wrong quoter address or ABI.
const QUOTER_NEVER_OK_WARN_BATCHES: u32 = 5;

/// Warn when a router that HAS returned quotes before fails this many batches in a row.
/// Higher than the never-OK threshold: thin pools quoted at large inputs can revert in
/// QuoterV2 legitimately, and an RPC blip fails a whole batch.
const QUOTER_BROKEN_WARN_BATCHES: u32 = 60;

/// Max age of a cached 2-hop QuoterV2 result ([`QuoteCache`]). Cached quotes are also
/// invalidated when the pool's local sqrtPriceX96 or liquidity changes; the age bound
/// limits the damage when a Swap/Mint/Burn event is missed (WS reconnect, dropped log)
/// and the local state goes stale. ~3 blocks on Base.
pub(crate) const QUOTE_CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(6);

/// Entry cap for [`QuoteCache`]; cleared wholesale when exceeded.
const QUOTE_CACHE_MAX_ENTRIES: usize = 4096;

/// One V3 leg to quote via QuoterV2 — see [`Strategy::quote_v3_legs`].
pub(crate) struct V3LegQuote<'a> {
    pub router_id: &'a str,
    pub key: &'a V3PoolKey,
    pub token_in: Address,
    pub token_out: Address,
    pub fee: u32,
    pub amount_in: U256,
}

/// Per-router outcome of one QuoterV2 batch: true if any call for the router succeeded.
#[derive(Default)]
pub(crate) struct QuoterBatchHealth(HashMap<String, bool>);

impl QuoterBatchHealth {
    pub(crate) fn record(&mut self, router_id: &str, ok: bool) {
        *self.0.entry(router_id.to_string()).or_insert(false) |= ok;
    }
}

/// QuoterV2 result cache for the 2-hop Phase 1.5/1.5b paths, keyed on (pool, amount_in).
/// One invalidation rule for every user: an entry is served only while the pool's local
/// (sqrtPriceX96, liquidity) is unchanged AND it is younger than [`QUOTE_CACHE_MAX_AGE`].
#[derive(Default)]
pub struct QuoteCache {
    entries: DashMap<(V3PoolKey, U256), (U256, u128, U256, std::time::Instant)>,
}

impl QuoteCache {
    pub(crate) fn get(&self, pools: &PoolCache, key: &V3PoolKey, amount_in: U256) -> Option<U256> {
        let (sqrtp, liq) = pools.v3_price_state(key)?;
        let e = self.entries.get(&(key.clone(), amount_in))?;
        (e.0 == sqrtp && e.1 == liq && e.3.elapsed() <= QUOTE_CACHE_MAX_AGE).then_some(e.2)
    }

    /// Store a result (ZERO = failed / no output, also cached so it isn't re-sent).
    pub(crate) fn insert(&self, pools: &PoolCache, key: V3PoolKey, amount_in: U256, amount_out: U256) {
        let Some((sqrtp, liq)) = pools.v3_price_state(&key) else { return };
        if self.entries.len() > QUOTE_CACHE_MAX_ENTRIES {
            self.entries.clear();
        }
        self.entries.insert((key, amount_in), (sqrtp, liq, amount_out, std::time::Instant::now()));
    }
}

/// Pre-parsed per-pair scan context — built once per config change, not per block.
#[derive(Clone)]
pub(crate) struct PairResolved {
    pub token_in: Address,
    pub token_out: Address,
    pub token_in_key: String,
    pub token_out_key: String,
    pub amount_in: U256,
    /// Upper bound for optimizer probes (max_trade or trade_amount, uncapped by trade_amount).
    pub max_amount_in: U256,
    pub min_profit_usd: f64,
}

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
    /// Pre-built pool cache lookup keys (forward + reverse for phase 2).
    pub v2_key: Option<V2PoolKey>,
    pub v3_key: Option<V3PoolKey>,
    pub rev_v2_key: Option<V2PoolKey>,
    pub rev_v3_key: Option<V3PoolKey>,
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
    /// Separate minimum profit floor for triangular arbs (USD).
    /// Triangular V3 routes use cached sqrtPriceX96 which overestimates output by
    /// >0.24% on WETH routes due to tick-level liquidity not being modelled. This
    /// floor is set higher than min_profit_usd to filter phantom opportunities.
    pub min_triangular_profit_usd: f64,
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
    /// Phase 1.5 / 1.5b QuoterV2 result cache — see [`QuoteCache`] for the validity rule.
    /// Cuts Phase 1.5 from one QuoterV2 call per scan to roughly one per pool state change.
    /// Triangular verification does NOT use it: it always quotes live.
    pub quote_cache: Arc<QuoteCache>,
    /// When true: skip QuoterV2 verification entirely (phases 1.5/1.5b/1.5c) and
    /// submit immediately after Phase 1 local spot quotes. Saves ~80-160ms —
    /// enough to land in the same block on FCFS chains (Base).
    /// Disable by setting optimistic_submission: false in config.yaml.
    pub optimistic_submission: bool,
    /// Dedicated HTTP provider for QuoterV2 multicalls.
    /// HTTP/2 connection pooling is faster than WS for single request-response calls.
    /// Falls back to the WS provider passed to evaluate() when None.
    pub http_provider: Option<DynProvider>,
    /// Pre-parsed router addresses keyed by router ID.
    /// Populated once at construction — avoids Address::from_str() O(pairs × routers)
    /// times per scan (e.g. 50 pairs × 6 routers = 300 parses per block).
    pub router_addr_map: HashMap<String, Address>,
    /// Per-router QuoterV2 address (router.quoter_address ?? chain quoter_v2_address).
    pub quoter_by_router: HashMap<String, Address>,
    /// Solidly/Aerodrome effective pool-cache IDs: router_id → (volatile, stable).
    pub solidly_pool_keys: HashMap<String, (String, String)>,
    /// Reverse index: token address → pair indices that include that token.
    /// Populated once at construction and after config hot-reload.
    /// Replaces linear scan + Address::from_str() on every pending/swap event (~300–500µs saved).
    token_pair_index: HashMap<Address, Vec<usize>>,
    /// Pre-parsed addresses, cache keys, and trade size per pair index.
    pair_resolved: Vec<Option<PairResolved>>,
    /// Pair indices on this chain — avoids filter+enumerate every full scan.
    chain_pair_indices: Vec<usize>,
    /// Indices into `self.routers` for this chain — avoids filter+collect every scan.
    chain_router_indices: Vec<usize>,
    /// pair_id → index for O(1) lookup in optimize / execution paths.
    pair_id_to_index: HashMap<String, usize>,
    /// Bumped on `rebuild_scan_cache()` — invalidates triangular disabled-leg cache.
    scan_cache_generation: std::sync::atomic::AtomicU64,
    /// Cached (generation, disabled fingerprint) → disabled directed legs for triangular.
    tri_disabled_legs_cache: Mutex<Option<(u64, u64, std::collections::HashSet<(Address, Address)>)>>,
    /// Router ID → consecutive QuoterV2 batches in which every call for it failed.
    quoter_fail_streak: DashMap<String, u32>,
    /// Routers whose QuoterV2 has returned at least one quote since startup.
    quoter_ever_ok: DashMap<String, ()>,
}

impl Strategy {
    pub fn new(
        chain_id: u64,
        pairs: Vec<PairConfig>,
        routers: Vec<RouterConfig>,
        min_profit_usd: f64,
        min_triangular_profit_usd: f64,
        quoter_v2_address: Option<Address>,
        pool_cache: Arc<PoolCache>,
        rpc_concurrency: usize,
        optimistic_submission: bool,
        http_provider: Option<DynProvider>,
    ) -> Self {
        let router_addr_map: HashMap<String, Address> = routers
            .iter()
            .filter_map(|r| r.address.parse::<Address>().ok().map(|a| (r.id.clone(), a)))
            .collect();

        let quoter_by_router: HashMap<String, Address> = routers
            .iter()
            .filter_map(|r| {
                let q = r
                    .quoter_address
                    .as_deref()
                    .and_then(|s| s.parse::<Address>().ok())
                    .or(quoter_v2_address);
                q.map(|addr| (r.id.clone(), addr))
            })
            .collect();

        let solidly_pool_keys: HashMap<String, (String, String)> = routers
            .iter()
            .filter(|r| matches!(r.router_type, RouterType::Solidly | RouterType::Aerodrome))
            .map(|r| {
                (
                    r.id.clone(),
                    (format!("{}::volatile", r.id), format!("{}::stable", r.id)),
                )
            })
            .collect();

        if optimistic_submission
            && routers.iter().any(|r| r.chain_id == chain_id && r.router_type == RouterType::V3)
        {
            warn!(
                "[{}] optimistic_submission is on: triangular routes with a V3 leg are submitted on \
                 cached spot quotes WITHOUT QuoterV2 verification; a 0.30% V3 overestimate is deducted \
                 before min_triangular_profit_usd. Set optimistic_submission: false to verify them.",
                chain_id,
            );
        }

        let mut s = Self {
            chain_id,
            pairs,
            routers,
            native_price_usd: 2500.0,
            min_profit_usd,
            min_triangular_profit_usd,
            quoter_v2_address,
            syncswap_pool_cache: HashMap::new(),
            pool_cache,
            rpc_concurrency,
            pair_scan: Mutex::new(Vec::new()),
            tri_scan: Mutex::new(Vec::new()),
            opp_session_counts: Mutex::new(HashMap::new()),
            quote_cache: Arc::new(QuoteCache::default()),
            optimistic_submission,
            http_provider,
            router_addr_map,
            quoter_by_router,
            solidly_pool_keys,
            token_pair_index: HashMap::new(),
            pair_resolved: Vec::new(),
            chain_pair_indices: Vec::new(),
            chain_router_indices: Vec::new(),
            pair_id_to_index: HashMap::new(),
            scan_cache_generation: std::sync::atomic::AtomicU64::new(0),
            tri_disabled_legs_cache: Mutex::new(None),
            quoter_fail_streak: DashMap::new(),
            quoter_ever_ok: DashMap::new(),
        };
        s.rebuild_scan_cache();
        s
    }

    fn disabled_triplets_fingerprint(disabled: &std::collections::HashSet<String>) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut v: Vec<_> = disabled.iter().collect();
        v.sort();
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    fn build_disabled_legs_for(&self, disabled_triplets: &std::collections::HashSet<String>) -> std::collections::HashSet<(Address, Address)> {
        if disabled_triplets.is_empty() {
            return std::collections::HashSet::new();
        }
        self.pairs
            .iter()
            .filter(|p| p.chain_id == self.chain_id && disabled_triplets.contains(&p.id))
            .filter_map(|p| {
                let ti: Address = p.token_in.parse().ok()?;
                let to: Address = p.token_out.parse().ok()?;
                Some((ti, to))
            })
            .collect()
    }

    pub(crate) fn get_or_build_disabled_legs(
        &self,
        disabled_triplets: &std::collections::HashSet<String>,
    ) -> std::collections::HashSet<(Address, Address)> {
        let gen = self.scan_cache_generation.load(std::sync::atomic::Ordering::Acquire);
        let fp = Self::disabled_triplets_fingerprint(disabled_triplets);
        {
            let guard = self.tri_disabled_legs_cache.lock().unwrap();
            if let Some((g, f, legs)) = guard.as_ref() {
                if *g == gen && *f == fp {
                    return legs.clone();
                }
            }
        }
        let legs = self.build_disabled_legs_for(disabled_triplets);
        *self.tri_disabled_legs_cache.lock().unwrap() = Some((gen, fp, legs.clone()));
        legs
    }

    fn build_pair_resolved(pairs: &[PairConfig], default_min_profit: f64) -> Vec<Option<PairResolved>> {
        use crate::util::addr_key;
        use tracing::warn;
        pairs
            .iter()
            .map(|p| {
                let token_in: Address = match p.token_in.parse() {
                    Ok(a) => a,
                    Err(_) => {
                        warn!("Pair {} skipped — invalid token_in address", p.id);
                        return None;
                    }
                };
                let token_out: Address = match p.token_out.parse() {
                    Ok(a) => a,
                    Err(_) => {
                        warn!("Pair {} skipped — invalid token_out address", p.id);
                        return None;
                    }
                };
                let amount_in = match parse_amount_capped(
                    &p.trade_amount,
                    p.max_trade.as_deref(),
                    p.token_in_decimals,
                ) {
                    Some(a) => a,
                    None => {
                        warn!(
                            "Pair {} skipped — invalid trade_amount/max_trade ({}/{:?})",
                            p.id, p.trade_amount, p.max_trade
                        );
                        return None;
                    }
                };
                let cap_str = p.max_trade.as_deref().unwrap_or(&p.trade_amount);
                let max_amount_in = match parse_amount_capped(cap_str, None, p.token_in_decimals) {
                    Some(a) => a,
                    None => {
                        warn!("Pair {} skipped — invalid max_trade cap", p.id);
                        return None;
                    }
                };
                Some(PairResolved {
                    token_in,
                    token_out,
                    token_in_key: addr_key(token_in),
                    token_out_key: addr_key(token_out),
                    amount_in,
                    max_amount_in,
                    min_profit_usd: p.min_profit_usd.unwrap_or(default_min_profit),
                })
            })
            .collect()
    }

    /// Rebuild pair/router scan caches after pairs, routers, or min_profit change.
    pub fn rebuild_scan_cache(&mut self) {
        self.token_pair_index = Self::build_token_pair_index_for(&self.pairs);
        self.chain_pair_indices = self
            .pairs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.chain_id == self.chain_id)
            .map(|(i, _)| i)
            .collect();
        self.chain_router_indices = self
            .routers
            .iter()
            .enumerate()
            .filter(|(_, r)| r.chain_id == self.chain_id)
            .map(|(i, _)| i)
            .collect();
        self.pair_id_to_index = self
            .pairs
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.clone(), i))
            .collect();
        self.pair_resolved = Self::build_pair_resolved(&self.pairs, self.min_profit_usd);
        self.scan_cache_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        *self.tri_disabled_legs_cache.lock().unwrap() = None;
    }

    fn build_token_pair_index_for(pairs: &[PairConfig]) -> HashMap<Address, Vec<usize>> {
        let mut idx: HashMap<Address, Vec<usize>> = HashMap::new();
        for (i, p) in pairs.iter().enumerate() {
            if let Ok(a) = p.token_in.parse::<Address>() { idx.entry(a).or_default().push(i); }
            if let Ok(b) = p.token_out.parse::<Address>() { idx.entry(b).or_default().push(i); }
        }
        idx
    }

    /// Rebuild the token→pairs reverse index after `self.pairs` has been replaced (hot-reload).
    pub fn rebuild_token_pair_index(&mut self) {
        self.rebuild_scan_cache();
    }

    /// Rebuild router address / quoter / Solidly key maps after hot-reload.
    pub fn rebuild_router_maps(&mut self) {
        self.router_addr_map = self
            .routers
            .iter()
            .filter_map(|r| r.address.parse::<Address>().ok().map(|a| (r.id.clone(), a)))
            .collect();
        self.quoter_by_router = self
            .routers
            .iter()
            .filter_map(|r| {
                let q = r
                    .quoter_address
                    .as_deref()
                    .and_then(|s| s.parse::<Address>().ok())
                    .or(self.quoter_v2_address);
                q.map(|addr| (r.id.clone(), addr))
            })
            .collect();
        self.solidly_pool_keys = self
            .routers
            .iter()
            .filter(|r| matches!(r.router_type, RouterType::Solidly | RouterType::Aerodrome))
            .map(|r| {
                (
                    r.id.clone(),
                    (format!("{}::volatile", r.id), format!("{}::stable", r.id)),
                )
            })
            .collect();
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
                    fee_num: alloy::primitives::U256::ZERO, // overwritten by PoolCache::insert()
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
    /// O(1) lookup via pre-built reverse index — replaces O(n) linear scan with Address::from_str().
    pub fn pairs_for_tokens(&self, token_a: Address, token_b: Address) -> Vec<usize> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for addr in [token_a, token_b] {
            for &i in self.token_pair_index.get(&addr).into_iter().flatten() {
                if seen.insert(i) { out.push(i); }
            }
        }
        out
    }

    /// Quote V3 legs via QuoterV2 in one batched multicall, always live (no cross-scan
    /// cache — callers verifying before submit must not trust local state). Identical legs
    /// (same pool and amount) are sent once. Results are in input order; None = router has
    /// no quoter, the call failed, or it quoted 0. Feeds the per-router quoter health check.
    pub(crate) async fn quote_v3_legs<P: Provider + Clone>(
        &self,
        provider: &P,
        legs: &[V3LegQuote<'_>],
    ) -> Vec<Option<U256>> {
        let mut out: Vec<Option<U256>> = vec![None; legs.len()];
        let mut mc: Vec<(Address, Vec<u8>)> = Vec::new();
        // Per multicall slot: router and the legs it answers.
        let mut sent: Vec<(&str, Vec<usize>)> = Vec::new();
        let mut dedup: HashMap<(&V3PoolKey, U256), usize> = HashMap::new();

        for (i, leg) in legs.iter().enumerate() {
            let Some(&quoter) = self.quoter_by_router.get(leg.router_id) else { continue };
            match dedup.entry((leg.key, leg.amount_in)) {
                Entry::Occupied(e) => sent[*e.get()].1.push(i),
                Entry::Vacant(e) => {
                    e.insert(sent.len());
                    mc.push((quoter, encode_v3_quote(leg.token_in, leg.token_out, leg.fee, leg.amount_in)));
                    sent.push((leg.router_id, vec![i]));
                }
            }
        }
        if mc.is_empty() {
            return out;
        }

        let raw = match self.http_provider {
            Some(ref hp) => run_multicall(hp, mc, self.rpc_concurrency).await,
            None => run_multicall(provider, mc, self.rpc_concurrency).await,
        };
        let mut health = QuoterBatchHealth::default();
        for ((router_id, slots), r) in sent.into_iter().zip(raw) {
            let q = decode_v3_quote(r.as_ref());
            health.record(router_id, q.is_some());
            for i in slots {
                out[i] = q;
            }
        }
        self.note_quoter_health(health);
        out
    }

    /// Update per-router QuoterV2 health after a batch and warn on persistent failure:
    /// - never returned a quote and failed [`QUOTER_NEVER_OK_WARN_BATCHES`] batches in a
    ///   row → likely wrong quoter address/ABI (or every pool too thin to quote);
    /// - worked before but failed [`QUOTER_BROKEN_WARN_BATCHES`] batches in a row →
    ///   persistent breakage (RPC or quoter).
    ///
    /// The streak resets on any success, so a later breakage is warned again.
    pub(crate) fn note_quoter_health(&self, health: QuoterBatchHealth) {
        for (router_id, ok) in health.0 {
            if ok {
                self.quoter_fail_streak.remove(&router_id);
                self.quoter_ever_ok.insert(router_id, ());
                continue;
            }
            let ever_ok = self.quoter_ever_ok.contains_key(&router_id);
            let mut streak = self.quoter_fail_streak.entry(router_id.clone()).or_insert(0);
            *streak += 1;
            if !ever_ok && *streak == QUOTER_NEVER_OK_WARN_BATCHES {
                warn!(
                    "[{}] QuoterV2 for router {} has never returned a quote ({} batches failed). \
                     Check its quoter_address/ABI (Uniswap QuoterV2, uint24 fee); if those are right, \
                     its pools may be too thin to quote at trade size.",
                    self.chain_id, router_id, QUOTER_NEVER_OK_WARN_BATCHES,
                );
            } else if ever_ok && *streak == QUOTER_BROKEN_WARN_BATCHES {
                warn!(
                    "[{}] QuoterV2 for router {} has failed {} batches in a row after working earlier \
                     — check RPC health and the quoter.",
                    self.chain_id, router_id, QUOTER_BROKEN_WARN_BATCHES,
                );
            } else {
                debug!("[{}] QuoterV2 batch failed for router {} (streak {})", self.chain_id, router_id, *streak);
            }
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

    // Run at most `concurrency` chunks at a time (smooths the Alchemy CU burst that firing
    // everything with join_all caused). `buffered`, NOT `buffer_unordered`: callers zip the
    // flattened results against their input order, so chunks must come back in order.
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
    .buffered(concurrency.max(1))
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
    match symbol {
        "USDC" | "USDT" | "DAI" | "WXDAI" | "USDC.E" | "USDBC" => float_amount,
        "WETH" | "ETH" | "WMATIC" | "MATIC" => float_amount * native_price,
        _ => float_amount,
    }
}
