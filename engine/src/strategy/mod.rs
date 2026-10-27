use alloy::primitives::{Address, Uint, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::eips::BlockId;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use dashmap::DashMap;
use futures::StreamExt;
use std::collections::{hash_map::Entry, HashMap};
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use crate::abi::{IMulticall3, IQuoterV2};
use crate::config::{PairConfig, RouterConfig, RouterType};
use crate::pool_cache::{PoolCache, V2PoolKey, V3PoolKey};
pub use crate::types::*;
use crate::util::u256_to_f64;

pub mod evaluate;
pub mod triangular;
pub mod optimize;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Minimum active-tick liquidity for a V3 pool to be considered tradeable.
/// Real pools: 10^15 – 10^20. Dead/empty pools: near zero.
/// This filters out uninitialized pools and ghost liquidity without needing USD valuation.
pub(crate) fn min_v3_liquidity() -> u128 { crate::config::tuning().min_v3_liquidity }

/// Max calls per Multicall3 batch.
/// 10 keeps each batch well under ~30M gas (Alchemy eth_call limit) even with expensive
/// V3 quoteExactInputSingle calls that can reach 5M gas/call when tracing ticks.
pub(crate) fn multicall_chunk_size() -> usize { crate::config::tuning().multicall_chunk_size }

/// QuoterV2 post-buffer (bps): removed from `amount_back` before profit vs `min_profit_usd`.
/// Used in Phase 1.5/1.5b/1.5c and optimize.rs — keep all paths on this single constant.
pub(crate) fn quoter_haircut_bps() -> u64 { crate::config::tuning().quoter_haircut_bps }

/// Detection / execution buffer applied to quoted output before profit checks (Phase 1.1).
pub(crate) fn apply_exec_quote_haircut(amount_out: U256) -> U256 {
    (amount_out * U256::from(10_000 - quoter_haircut_bps())) / U256::from(10_000u32)
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

/// Slipstream quoter calldata: `int24 tickSpacing` in place of the fee tier (`fee` carries
/// the tick spacing for Slipstream routers). Returns the same 4 values as QuoterV2.
pub(crate) fn encode_slipstream_quote(token_in: Address, token_out: Address, tick_spacing: u32, amount_in: U256) -> Vec<u8> {
    use crate::abi::ISlipstreamQuoter;
    ISlipstreamQuoter::quoteExactInputSingleCall {
        params: ISlipstreamQuoter::QuoteExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount_in,
            tickSpacing: alloy::primitives::aliases::I24::try_from(tick_spacing as i32).unwrap_or_default(),
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

/// Warn when a router's QuoterV2 has never returned a quote and every batch for it has
/// failed for this long (and at least [`quoter_warn_min_batches()`] batches) — most likely a
/// wrong quoter address or ABI. Time-based: one scan runs several batches (1.5, 1.5b,
/// 1.5c, triangular), so a batch count alone trips within ~2 s.
fn quoter_never_ok_warn_after() -> std::time::Duration { crate::config::tuning().quoter_never_ok_warn_after() }

/// Same, for a router that HAS returned quotes before. Longer: thin pools quoted at large
/// inputs revert in QuoterV2 legitimately, and RPC blips fail whole batches.
fn quoter_broken_warn_after() -> std::time::Duration { crate::config::tuning().quoter_broken_warn_after() }

/// Minimum consecutive failed batches before either warning.
fn quoter_warn_min_batches() -> u32 { crate::config::tuning().quoter_warn_min_batches }

/// Max age of a cached 2-hop QuoterV2 result ([`QuoteCache`]). Cached quotes are also
/// invalidated when the pool's local sqrtPriceX96 or liquidity changes; the age bound
/// limits the damage when a Swap/Mint/Burn event is missed (WS reconnect, dropped log)
/// and the local state goes stale. ~3 blocks on Base.
pub(crate) fn quote_cache_max_age() -> std::time::Duration { crate::config::tuning().quote_cache_max_age() }

/// Pool cap for [`QuoteCache`]; cleared wholesale when exceeded.
fn quote_cache_max_pools() -> usize { crate::config::tuning().quote_cache_max_pools }

/// Per-pool amount cap for [`QuoteCache`]: the oldest entries go first when exceeded.
/// 1.5b/1.5c key on amounts that change with every reserve move.
const QUOTE_CACHE_MAX_AMOUNTS: usize = 64;

/// Minimum age of a pool's local state before a quote for it is cached — see
/// [`QuoteCache::insert`].
fn quote_cache_settle() -> std::time::Duration { crate::config::tuning().quote_cache_settle() }

/// Timeout for one Multicall3 chunk in [`run_multicall`]. Without it a hung RPC request
/// blocks the chain's scan loop forever (seen on a cold Anvil fork). QuoterV2 batches
/// normally take ~80–160 ms; on timeout the chunk's calls count as failed.
fn multicall_chunk_timeout() -> std::time::Duration { crate::config::tuning().multicall_chunk_timeout() }

/// One V3 leg to quote via QuoterV2 — see [`Strategy::quote_v3_legs`]. Router, tokens and
/// fee all come from `key`, so dedup and calldata can't disagree.
pub(crate) struct V3LegQuote<'a> {
    pub key: &'a V3PoolKey,
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

/// QuoterV2 result cache for the 2-hop Phase 1.5/1.5b paths, keyed on pool then amount_in.
/// One invalidation rule for every user: an entry is served only while the pool's local
/// (sqrtPriceX96, liquidity) equals the state captured when the quote was REQUESTED and
/// it is younger than [`quote_cache_max_age()`].
#[derive(Default)]
pub struct QuoteCache {
    pools: DashMap<V3PoolKey, HashMap<U256, CachedQuote>>,
}

/// (sqrtPriceX96 and liquidity when requested, amountOut, stored at).
type CachedQuote = (U256, u128, U256, std::time::Instant);

impl QuoteCache {
    pub(crate) fn get(&self, pools: &PoolCache, key: &V3PoolKey, amount_in: U256) -> Option<U256> {
        let (sqrtp, liq) = pools.v3_price_state(key)?;
        let by_amount = self.pools.get(key)?;
        let e = by_amount.get(&amount_in)?;
        (e.0 == sqrtp && e.1 == liq && e.3.elapsed() <= quote_cache_max_age()).then_some(e.2)
    }

    /// Store a result (ZERO = the quoter reverted / quoted nothing, cached so it isn't
    /// re-sent). `state_at_send` is the pool's (sqrtPriceX96, liquidity) read when the
    /// request was built: if the pool moves while the call is in flight, the entry no
    /// longer matches and is never served.
    ///
    /// Skipped while the pool's local state is younger than [`quote_cache_settle()`]: a
    /// quote sent right after a pre-confirmed log can price the state before it (the
    /// quote RPC's `pending` lags the pendingLogs feed), and caching it under the new
    /// state would keep serving that stale result. The quote itself is still used.
    pub(crate) fn insert(&self, pools: &PoolCache, key: V3PoolKey, amount_in: U256, state_at_send: (U256, u128), amount_out: U256) {
        self.insert_settled(pools, key, amount_in, state_at_send, amount_out, quote_cache_settle());
    }

    fn insert_settled(&self, pools: &PoolCache, key: V3PoolKey, amount_in: U256, state_at_send: (U256, u128), amount_out: U256, settle: std::time::Duration) {
        if pools.v3_state_age(&key).is_none_or(|age| age < settle) {
            return;
        }
        if self.pools.len() > quote_cache_max_pools() {
            self.pools.clear();
        }
        // Entries for a state other than the pool's current one (or past max age) can never
        // be served again ([`Self::get`]). Compared to the current state, not state_at_send,
        // so a late result from an older state doesn't evict valid entries.
        let current = pools.v3_price_state(&key);
        let max_age = quote_cache_max_age();
        let mut by_amount = self.pools.entry(key).or_default();
        by_amount.retain(|_, e| Some((e.0, e.1)) == current && e.3.elapsed() <= max_age);
        by_amount.insert(amount_in, (state_at_send.0, state_at_send.1, amount_out, std::time::Instant::now()));
        while by_amount.len() > QUOTE_CACHE_MAX_AMOUNTS {
            let Some(oldest) = by_amount.iter().min_by_key(|(_, e)| e.3).map(|(a, _)| *a) else { break };
            by_amount.remove(&oldest);
        }
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
    /// Block tag for quote reads: `pending` when `http_provider` is the preconf endpoint
    /// (`chain.preconf_http_rpc`), else `latest`.
    pub quote_block: BlockId,
    /// Pre-parsed router addresses keyed by router ID.
    /// Populated once at construction — avoids Address::from_str() O(pairs × routers)
    /// times per scan (e.g. 50 pairs × 6 routers = 300 parses per block).
    pub router_addr_map: HashMap<String, Address>,
    /// Per-router QuoterV2 address (router.quoter_address ?? chain quoter_v2_address).
    pub quoter_by_router: HashMap<String, Address>,
    /// Quoters of Slipstream routers — they take a tick spacing, see [`Strategy::encode_quote`].
    pub slipstream_quoters: std::collections::HashSet<Address>,
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
    /// Router ID → QuoterV2 batches in which every call for it failed:
    /// (consecutive failed batches, first failure time, already warned) per router.
    quoter_fail_streak: DashMap<String, (u32, std::time::Instant, bool)>,
    /// Routers whose QuoterV2 has returned at least one quote since startup.
    quoter_ever_ok: DashMap<String, ()>,
    /// Round-trip timings of the last scan, for the execution log: see [`ScanTimings`].
    pub timings: ScanTimings,
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

        let slipstream_quoters = slipstream_quoters(&routers);
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
            .filter(|r| matches!(r.router_type, RouterType::Aerodrome))
            .map(|r| {
                (
                    r.id.clone(),
                    (format!("{}::volatile", r.id), format!("{}::stable", r.id)),
                )
            })
            .collect();

        if optimistic_submission
            && routers.iter().any(|r| r.router_type == RouterType::V3)
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
            pool_cache,
            rpc_concurrency,
            pair_scan: Mutex::new(Vec::new()),
            tri_scan: Mutex::new(Vec::new()),
            opp_session_counts: Mutex::new(HashMap::new()),
            quote_cache: Arc::new(QuoteCache::default()),
            optimistic_submission,
            http_provider,
            quote_block: BlockId::latest(),
            router_addr_map,
            slipstream_quoters,
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
            timings: ScanTimings::default(),
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
            .filter(|p| disabled_triplets.contains(&p.id))
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
            .map(|(i, _)| i)
            .collect();
        self.chain_router_indices = self
            .routers
            .iter()
            .enumerate()
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
        self.slipstream_quoters = slipstream_quoters(&self.routers);
        self.solidly_pool_keys = self
            .routers
            .iter()
            .filter(|r| matches!(r.router_type, RouterType::Aerodrome))
            .map(|r| {
                (
                    r.id.clone(),
                    (format!("{}::volatile", r.id), format!("{}::stable", r.id)),
                )
            })
            .collect();
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
            let k = leg.key;
            let Some(&quoter) = self.quoter_by_router.get(&*k.router_id) else { continue };
            match dedup.entry((k, leg.amount_in)) {
                Entry::Occupied(e) => sent[*e.get()].1.push(i),
                Entry::Vacant(e) => {
                    e.insert(sent.len());
                    mc.push((quoter, self.encode_quote(quoter, k.token_in, k.token_out, k.fee, leg.amount_in)));
                    sent.push((&k.router_id, vec![i]));
                }
            }
        }
        if mc.is_empty() {
            return out;
        }

        let raw = match self.http_provider {
            Some(ref hp) => run_multicall(hp, mc, self.rpc_concurrency, self.quote_block).await,
            None => run_multicall(provider, mc, self.rpc_concurrency, self.quote_block).await,
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

    /// Update per-router QuoterV2 health after a batch and warn once on persistent failure:
    /// - never returned a quote, failing for [`quoter_never_ok_warn_after()`] → likely wrong
    ///   quoter address/ABI (or every pool too thin to quote at trade size);
    /// - worked before, failing for [`quoter_broken_warn_after()`] → persistent breakage.
    ///
    /// Both also need [`quoter_warn_min_batches()`] failed batches in a row. Any success
    /// resets the streak, so a later breakage is warned again.
    pub(crate) fn note_quoter_health(&self, health: QuoterBatchHealth) {
        for (router_id, ok) in health.0 {
            if ok {
                self.quoter_fail_streak.remove(&router_id);
                self.quoter_ever_ok.insert(router_id, ());
                continue;
            }
            let ever_ok = self.quoter_ever_ok.contains_key(&router_id);
            let mut streak = self.quoter_fail_streak.entry(router_id.clone())
                .or_insert((0, std::time::Instant::now(), false));
            let (batches, since, warned) = &mut *streak;
            *batches += 1;
            let threshold = if ever_ok { quoter_broken_warn_after() } else { quoter_never_ok_warn_after() };
            if !*warned && *batches >= quoter_warn_min_batches() && since.elapsed() >= threshold {
                *warned = true;
                if ever_ok {
                    warn!(
                        "[{}] QuoterV2 for router {} has failed every call for {}s ({} batches) after \
                         working earlier — check RPC health and the quoter.",
                        self.chain_id, router_id, since.elapsed().as_secs(), batches,
                    );
                } else {
                    warn!(
                        "[{}] QuoterV2 for router {} has never returned a quote ({}s, {} batches). \
                         Check its quoter_address/ABI (Uniswap QuoterV2, uint24 fee); if those are right, \
                         its pools may be too thin to quote at trade size.",
                        self.chain_id, router_id, since.elapsed().as_secs(), batches,
                    );
                }
            } else {
                debug!("[{}] QuoterV2 batch failed for router {} (streak {})", self.chain_id, router_id, batches);
            }
        }
    }

    /// The preconf quote provider (`chain.preconf_http_rpc`), when set — for callers that
    /// would otherwise quote on the WS provider (the optimizer).
    pub fn preconf_quote_provider(&self) -> Option<DynProvider> {
        if self.quote_block == BlockId::pending() { self.http_provider.clone() } else { None }
    }

    /// Quote calldata for `quoter`: Slipstream's ABI for Slipstream quoters, QuoterV2's otherwise.
    pub(crate) fn encode_quote(&self, quoter: Address, token_in: Address, token_out: Address, fee: u32, amount_in: U256) -> Vec<u8> {
        if self.slipstream_quoters.contains(&quoter) {
            encode_slipstream_quote(token_in, token_out, fee, amount_in)
        } else {
            encode_v3_quote(token_in, token_out, fee, amount_in)
        }
    }

    pub fn update_native_price(&mut self, price: f64) {
        self.native_price_usd = price;
    }
}

fn slipstream_quoters(routers: &[RouterConfig]) -> std::collections::HashSet<Address> {
    routers.iter()
        .filter(|r| r.slipstream)
        .filter_map(|r| r.quoter_address.as_deref()?.parse::<Address>().ok())
        .collect()
}

// ─── Multicall3 batch helper ───────────────────────────────────────────────────

/// True when a failed Multicall3 `eth_call` was answered by the node with a revert or
/// out-of-gas (the aggregate itself failed — per-call replay can still succeed). False for
/// transport/RPC errors (rate limit, 5xx, reset): replaying 10× to the same endpoint would
/// only amplify the load that caused them.
fn multicall_error_is_revert(err: &alloy::transports::TransportError) -> bool {
    match err {
        alloy::transports::RpcError::ErrorResp(p) if !p.is_retry_err() => {
            let msg = p.message.to_lowercase();
            p.code == 3 || msg.contains("revert") || msg.contains("gas")
        }
        _ => false,
    }
}

/// Wall time of the last scan's quote round trips (ms; 0 = the phase made no RPC call).
/// One scan runs at a time per chain; evaluate and detect_triangular write disjoint fields.
#[derive(Default)]
pub struct ScanTimings {
    /// 2-hop Phase 1.5 (V3 forward QuoterV2), overlapped with 1.5c.
    pub p15_ms: std::sync::atomic::AtomicU64,
    /// 2-hop Phase 1.5b / remaining 1.5c (after Phase 1.5).
    pub p15bc_ms: std::sync::atomic::AtomicU64,
    /// Triangular QuoterV2 verification, all rounds.
    pub tri_verify_ms: std::sync::atomic::AtomicU64,
    /// Triangular verification rounds run.
    pub tri_verify_rounds: std::sync::atomic::AtomicU64,
}

impl ScanTimings {
    /// `p15=…ms p15bc=…ms tri_verify=…ms×N` for the execution log lines.
    pub fn summary(&self) -> String {
        use std::sync::atomic::Ordering::Relaxed;
        format!(
            "p15={}ms p15bc={}ms tri_verify={}ms×{}",
            self.p15_ms.load(Relaxed), self.p15bc_ms.load(Relaxed),
            self.tri_verify_ms.load(Relaxed), self.tri_verify_rounds.load(Relaxed),
        )
    }
}

/// Batch all calls into a single `eth_call` to Multicall3.
/// Returns `Some(Vec<u8>)` for each call that succeeded, `None` for failures.
/// Batch calls via Multicall3, chunking into groups of multicall_chunk_size().
/// All chunks are fired concurrently so total latency ≈ max(slowest chunk), not sum.
/// Falls back to sequential individual eth_calls only for a chunk whose aggregate reverted
/// or returned undecodable bytes; a transport/RPC error or timeout marks the chunk failed.
pub(crate) async fn run_multicall<P: Provider + Clone>(
    provider: &P,
    calls: Vec<(Address, Vec<u8>)>,
    concurrency: usize,
    block: BlockId,
) -> Vec<Option<Vec<u8>>> {
    run_multicall_answered(provider, calls, concurrency, block).await
        .into_iter().map(|(r, _)| r).collect()
}

/// [`run_multicall`], plus per call whether the node actually answered it: true for a
/// result or an on-chain failure (inner revert, reverted fallback call), false when its
/// chunk (or its fallback call) hit a timeout or transport/RPC error. Only an answered
/// `None` is a real revert — callers caching failures must check this per call, since
/// chunks fail independently.
pub(crate) async fn run_multicall_answered<P: Provider + Clone>(
    provider: &P,
    calls: Vec<(Address, Vec<u8>)>,
    concurrency: usize,
    block: BlockId,
) -> Vec<(Option<Vec<u8>>, bool)> {
    if calls.is_empty() {
        return vec![];
    }

    let mc3: Address = "0xcA11bde05977b3631167028862bE2a173976CA11"
        .parse()
        .expect("hardcoded multicall3 address");

    // Materialise chunks into owned Vecs so the stream doesn't hold a borrow across awaits
    // (borrowed slice refs across await → future !Send → tokio::spawn compile error).
    let owned_chunks: Vec<Vec<(Address, Vec<u8>)>> = calls
        .chunks(multicall_chunk_size())
        .map(|c| c.to_vec())
        .collect();

    // Run at most `concurrency` chunks at a time (smooths the Alchemy CU burst that firing
    // everything with join_all caused). `buffered`, NOT `buffer_unordered`: callers zip the
    // flattened results against their input order, so chunks must come back in order.
    // Per chunk: Ok(Ok(raw)) = answered, Ok(Err(true)) = aggregate reverted,
    // Ok(Err(false)) = transport/RPC error, Err(()) = timed out.
    let chunk_raw: Vec<Result<Result<_, bool>, ()>> = futures::stream::iter(owned_chunks.into_iter().map(|chunk| {
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
        async move {
            tokio::time::timeout(multicall_chunk_timeout(), p.call(tx).block(block)).await
                .map(|r| r.map_err(|e| {
                    let revert = multicall_error_is_revert(&e);
                    if !revert {
                        warn!("Multicall3 chunk RPC error — calls counted as failed, no sequential fallback: {e}");
                    }
                    revert
                }))
                .map_err(|_| ())
        }
    }))
    .buffered(concurrency.max(1))
    .collect()
    .await;

    // Flatten results in order; fall back per-chunk if Multicall3 decode fails.
    let mut out: Vec<(Option<Vec<u8>>, bool)> = Vec::with_capacity(calls.len());
    let mut call_offset = 0usize;
    for raw_res in chunk_raw {
        let chunk_len = multicall_chunk_size().min(calls.len() - call_offset);
        let Ok(raw_res) = raw_res else {
            // Timed out: count the chunk's calls as failed. No sequential fallback — the
            // RPC is evidently stuck, and more calls would stall the scan loop further.
            warn!("Multicall3 chunk timed out after {:?} — {} call(s) counted as failed", multicall_chunk_timeout(), chunk_len);
            out.extend(std::iter::repeat_n((None, false), chunk_len));
            call_offset += chunk_len;
            continue;
        };
        let raw_opt = match raw_res {
            Ok(raw) => Some(raw),
            Err(true) => None,
            Err(false) => {
                // Transport/RPC error (rate limit, 5xx): same as a timeout — replaying the
                // chunk call by call would multiply load on an endpoint already refusing.
                out.extend(std::iter::repeat_n((None, false), chunk_len));
                call_offset += chunk_len;
                continue;
            }
        };
        match raw_opt.and_then(|raw| IMulticall3::aggregate3Call::abi_decode_returns(&raw).ok()) {
            Some(decoded) => {
                out.extend(decoded.into_iter().map(|r| {
                    (r.success.then(|| r.returnData.to_vec()), true)
                }));
            }
            None => {
                // Aggregate reverted or returned undecodable bytes (Multicall3 unavailable or
                // gas exceeded) — fall back sequentially.
                warn!("Multicall3 chunk failed (gas limit exceeded?) — falling back to {} sequential eth_calls; reduce multicall_chunk_size() if this persists", chunk_len);
                for (target, data) in calls[call_offset..call_offset + chunk_len].iter() {
                    let tx = TransactionRequest::default()
                        .to(*target)
                        .input(data.clone().into());
                    let r = tokio::time::timeout(multicall_chunk_timeout(), provider.call(tx).block(block)).await;
                    out.push(match r {
                        Ok(Ok(b)) => (Some(b.to_vec()), true),
                        Ok(Err(e)) => (None, multicall_error_is_revert(&e)),
                        Err(_) => (None, false),
                    });
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

/// `token_in` symbols priced at $1. Config validation requires every pair's `token_in`
/// to be one of these or the wrapped native token (case-insensitive).
pub(crate) const USD_STABLE_SYMBOLS: [&str; 6] = ["USDC", "USDT", "DAI", "WXDAI", "USDC.E", "USDBC"];
/// `token_in` symbols priced at the live native price.
pub(crate) const NATIVE_SYMBOLS: [&str; 4] = ["WETH", "ETH", "WMATIC", "MATIC"];

pub(crate) fn token_amount_to_usd(amount: U256, decimals: u8, symbol: &str, native_price: f64) -> f64 {
    let scale = 10_f64.powi(decimals as i32);
    let float_amount = u256_to_f64(amount) / scale;
    if USD_STABLE_SYMBOLS.iter().any(|s| s.eq_ignore_ascii_case(symbol)) {
        float_amount
    } else if NATIVE_SYMBOLS.iter().any(|s| s.eq_ignore_ascii_case(symbol)) {
        float_amount * native_price
    } else {
        // Unreachable for token_in: config validation rejects other symbols.
        float_amount
    }
}

#[cfg(test)]
mod quote_encoding_tests {
    use super::*;

    #[test]
    fn slipstream_quote_uses_int24_tick_spacing_selector_and_same_decoder() {
        use alloy::sol_types::SolValue;
        let (a, b) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let uni = encode_v3_quote(a, b, 100, U256::from(5u64));
        let slip = encode_slipstream_quote(a, b, 100, U256::from(5u64));
        // quoteExactInputSingle((address,address,uint256,uint24,uint160)) vs (…,int24,…)
        assert_eq!(&uni[..4], &[0xc6, 0xa5, 0x02, 0x6a]);
        assert_eq!(&slip[..4], &[0x9e, 0x7d, 0xef, 0xe6]);
        // A positive int24 encodes like the uint24 word: only the selector differs.
        assert_eq!(&uni[4..], &slip[4..]);
        // Both quoters return (uint256 amountOut, uint160, uint32, uint256).
        let ret = (U256::from(42u64), U256::ZERO, U256::ZERO, U256::ZERO).abi_encode_params();
        assert_eq!(decode_v3_quote(Some(&ret)), Some(U256::from(42u64)));
    }
}

#[cfg(test)]
mod multicall_tests {
    use super::*;
    use alloy::transports::mock::Asserter;

    // ErrorPayload (alloy's json-rpc feature isn't enabled, so build it from JSON).
    fn err<T: serde::de::DeserializeOwned>(code: i64, msg: &str) -> T {
        serde_json::from_value(serde_json::json!({ "code": code, "message": msg })).unwrap()
    }

    fn calls(n: u8) -> Vec<(Address, Vec<u8>)> {
        (0..n).map(|i| (Address::repeat_byte(i + 1), vec![0u8; 4])).collect()
    }

    /// A rate-limited multicall is one request: the chunk is failed, not replayed per call.
    #[tokio::test]
    async fn rpc_error_does_not_replay_chunk() {
        let asserter = Asserter::new();
        asserter.push_failure(err(-32011, "request limit reached"));
        // Sentinels: any sequential replay would consume these.
        for _ in 0..3 {
            asserter.push_failure(err(3, "execution reverted"));
        }
        let provider = alloy::providers::ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let out = run_multicall(&provider, calls(3), 1, BlockId::latest()).await;
        assert_eq!(out, vec![None; 3]);
        assert_eq!(asserter.read_q().len(), 3);
    }

    /// A reverted aggregate (e.g. out of gas) still falls back to per-call eth_calls.
    #[tokio::test]
    async fn reverted_aggregate_falls_back_per_call() {
        let asserter = Asserter::new();
        asserter.push_failure(err(3, "execution reverted"));
        for _ in 0..3 {
            asserter.push_failure(err(3, "execution reverted"));
        }
        let provider = alloy::providers::ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let out = run_multicall(&provider, calls(3), 1, BlockId::latest()).await;
        assert_eq!(out, vec![None; 3]);
        assert_eq!(asserter.read_q().len(), 0);
    }

    /// Two chunks, the second rate-limited: only the first chunk's calls are answered, so a
    /// caller never caches the second chunk's `None`s as real reverts.
    #[tokio::test]
    async fn failed_chunk_calls_are_not_answered() {
        let n = multicall_chunk_size() + 1;
        let results: Vec<IMulticall3::McResult> = (0..multicall_chunk_size())
            .map(|i| IMulticall3::McResult { success: i != 0, returnData: vec![7u8; 32].into() })
            .collect();
        let ret = IMulticall3::aggregate3Call::abi_encode_returns(&results);
        let asserter = Asserter::new();
        asserter.push_success(&alloy::primitives::Bytes::from(ret));
        asserter.push_failure(err(-32011, "request limit reached"));
        let provider = alloy::providers::ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let out = run_multicall_answered(&provider, calls(n as u8), 1, BlockId::latest()).await;
        assert_eq!(out.len(), n);
        // Chunk 1: an inner revert is answered (cacheable as ZERO), the rest returned data.
        assert_eq!(out[0], (None, true));
        assert!(out[1..n - 1].iter().all(|(r, a)| r.is_some() && *a));
        // Chunk 2: failed in transport — not answered.
        assert_eq!(out[n - 1], (None, false));
        assert!(asserter.read_q().is_empty());
    }
}

#[cfg(test)]
mod haircut_tests {
    use super::*;

    #[test]
    fn default_haircut_keeps_a_ten_bps_exact_edge() {
        // +0.10% exact quote on 10k USDC: 15 bps used to turn it into a loss.
        let amount_in = U256::from(10_000_000_000u64);
        let back = amount_in * U256::from(10_010u64) / U256::from(10_000u64);
        let after = apply_exec_quote_haircut(back);
        assert!(after > amount_in);
        // 2 bps of 10.01k = $2.002 off a $10.00 edge.
        assert_eq!(after - amount_in, U256::from(7_998_000u64));
    }
}

#[cfg(test)]
mod quote_cache_tests {
    use super::*;
    use std::time::{Duration, Instant};

    const POOL: Address = Address::repeat_byte(0xbb);

    fn pools_updated_ago(ago: Duration) -> (PoolCache, V3PoolKey) {
        let (t0, t1) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let pools = PoolCache::new();
        pools.insert_v3(POOL, crate::pool_cache::V3PoolState {
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
        (pools, V3PoolKey::new("r", t0, t1, 500))
    }

    #[test]
    fn quote_for_a_just_moved_pool_is_not_cached() {
        let settle = Duration::from_millis(500);
        let amount = U256::from(1_000u64);
        let out = U256::from(990u64);

        // Pool moved 50 ms ago (pre-confirmed log): `pending` may still be the old state.
        let (pools, key) = pools_updated_ago(Duration::from_millis(50));
        let state = pools.v3_price_state(&key).unwrap();
        let cache = QuoteCache::default();
        cache.insert_settled(&pools, key.clone(), amount, state, out, settle);
        assert_eq!(cache.get(&pools, &key, amount), None);

        // Settled pool: cached and served as before.
        let (pools, key) = pools_updated_ago(Duration::from_secs(1));
        let state = pools.v3_price_state(&key).unwrap();
        cache.insert_settled(&pools, key.clone(), amount, state, out, settle);
        assert_eq!(cache.get(&pools, &key, amount), Some(out));

        // settle = 0 keeps the old always-cache behaviour.
        let (pools, key) = pools_updated_ago(Duration::ZERO);
        let state = pools.v3_price_state(&key).unwrap();
        let cache = QuoteCache::default();
        cache.insert_settled(&pools, key.clone(), amount, state, out, Duration::ZERO);
        assert_eq!(cache.get(&pools, &key, amount), Some(out));
    }

    #[test]
    fn insert_evicts_other_states_and_caps_amounts_per_pool() {
        let (pools, key) = pools_updated_ago(Duration::from_secs(1));
        let state = pools.v3_price_state(&key).unwrap();
        let cache = QuoteCache::default();
        let len = || cache.pools.get(&key).unwrap().len();

        // An entry stored under an older pool state is dropped by the next insert.
        let old_state = (state.0 + U256::from(1u64), state.1);
        cache.insert_settled(&pools, key.clone(), U256::from(1u64), old_state, U256::from(1u64), Duration::ZERO);
        cache.insert_settled(&pools, key.clone(), U256::from(2u64), state, U256::from(2u64), Duration::ZERO);
        assert_eq!(len(), 1);
        assert_eq!(cache.get(&pools, &key, U256::from(2u64)), Some(U256::from(2u64)));

        // Same state, ever-changing amounts: bounded, newest kept.
        let n = QUOTE_CACHE_MAX_AMOUNTS as u64 + 10;
        for a in 100..100 + n {
            cache.insert_settled(&pools, key.clone(), U256::from(a), state, U256::from(a), Duration::ZERO);
        }
        assert_eq!(len(), QUOTE_CACHE_MAX_AMOUNTS);
        let last = U256::from(100 + n - 1);
        assert_eq!(cache.get(&pools, &key, last), Some(last));
    }
}
