use alloy::primitives::{Address, U256};
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

// ─── Uniswap V4 pool keys ─────────────────────────────────────────────────────

/// A Uniswap V4 PoolKey (hookless). `currency0` is address(0) for native ETH; the cache
/// stores such a pool under WETH so routes join it with every other WETH pool, and the
/// executor wraps / unwraps on the leg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V4PoolKey {
    pub currency0: Address,
    pub currency1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
}

impl V4PoolKey {
    /// PoolId = keccak256(abi.encode(currency0, currency1, fee, tickSpacing, hooks = 0)).
    pub fn id(&self) -> alloy::primitives::B256 {
        let word = |a: Address| alloy::primitives::B256::left_padding_from(a.as_slice());
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(word(self.currency0).as_slice());
        buf.extend_from_slice(word(self.currency1).as_slice());
        buf.extend_from_slice(&U256::from(self.fee).to_be_bytes::<32>());
        buf.extend_from_slice(&alloy::primitives::I256::try_from(self.tick_spacing as i64)
            .unwrap_or_default().to_be_bytes::<32>());
        buf.extend_from_slice(&[0u8; 32]); // hooks
        alloy::primitives::keccak256(buf)
    }

    /// The address the cache and the executor know this pool by: the low 160 bits of its
    /// PoolId (executor `v4PoolAddress`).
    pub fn pool_address(&self) -> Address {
        v4_pool_address(self.id())
    }

    /// A native ETH pool whose partner sorts below WETH (e.g. VIRTUAL, tBTC): in the cache,
    /// where WETH stands for ETH, the partner is token0, so the pool's price is stored
    /// inverted ([`V4PoolKey::to_cache_price`]).
    pub fn inverted(&self) -> bool {
        self.currency0.is_zero() && self.currency1 < crate::util::WETH
    }

    /// The pool's (sqrtPriceX96, tick) as the cache stores them: unchanged, or for an
    /// [`inverted`](Self::inverted) pool 2^192 / sqrtP and -tick - 1. The -1 keeps tick
    /// ranges exact: the pool's [lower, upper) holds tick t iff [-upper, -lower) holds -t-1.
    pub fn to_cache_price(&self, sqrt_price_x96: U256, tick: i32) -> (U256, i32) {
        if !self.inverted() || sqrt_price_x96.is_zero() {
            return (sqrt_price_x96, tick);
        }
        ((U256::from(1u8) << 192) / sqrt_price_x96, -tick - 1)
    }

    /// A position's tick range as the cache sees it (mirrored for an inverted pool).
    pub fn to_cache_range(&self, lower: i32, upper: i32) -> (i32, i32) {
        if self.inverted() { (-upper, -lower) } else { (lower, upper) }
    }
}

/// Low 160 bits of a V4 PoolId (a Swap / ModifyLiquidity log's topic1).
pub fn v4_pool_address(id: alloy::primitives::B256) -> Address {
    Address::from_slice(&id.as_slice()[12..])
}

// ─── Structured pool lookup keys (no per-quote format! / string hashing) ────────

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct V2PoolKey {
    pub router_id: Arc<str>,
    pub token_in: Address,
    pub token_out: Address,
}

impl V2PoolKey {
    pub fn new(router_id: &str, token_in: Address, token_out: Address) -> Self {
        Self {
            router_id: Arc::from(router_id),
            token_in,
            token_out,
        }
    }
}

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct V3PoolKey {
    pub router_id: Arc<str>,
    pub token_in: Address,
    pub token_out: Address,
    pub fee: u32,
}

impl V3PoolKey {
    pub fn new(router_id: &str, token_in: Address, token_out: Address, fee: u32) -> Self {
        Self {
            router_id: Arc::from(router_id),
            token_in,
            token_out,
            fee,
        }
    }
}

// ─── Pool state ───────────────────────────────────────────────────────────────

/// Current on-chain state of one V2 / Solidly-volatile / Solidly-stable /
/// pool, kept fresh from `Sync(reserve0, reserve1)` events.
#[derive(Debug, Clone)]
pub struct PoolInfo {
    /// token0 = lower address (EVM convention).
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    /// Fee in basis-points (e.g. 25 = 0.25%, 30 = 0.3%).
    pub fee_bps: u32,
    /// The router ID that routes through this pool (for key lookup).
    pub router_id: String,
    /// True for Aerodrome-stable pools (x³y+xy³=k curve).
    /// False for V2 / Aerodrome-volatile pools (xy=k curve).
    pub is_stable: bool,
    /// Token decimals — needed to normalise reserves for the stable curve.
    pub decimals0: u8,
    pub decimals1: u8,
    /// When reserves were last updated by a live Sync event.
    /// Initialised to a deliberately old instant so startup reserves are
    /// treated as stale until the first on-chain Sync event arrives.
    pub last_sync: Instant,
    /// Precomputed fee numerator: 10_000 − fee_bps  (e.g. fee_bps=30 → fee_num=9_970).
    /// Stored as U256 so amount_out_v2/amount_out_stable avoid U256::from() per call.
    /// Populated by PoolCache::insert(); set to U256::ZERO in struct literals (overwritten).
    pub fee_num: U256,
}

// ─── V3 pool state ────────────────────────────────────────────────────────────

/// Concentrated-liquidity pool state, kept fresh from `Swap` events.
/// Both `sqrt_price_x96` and `liquidity` are emitted by every V3 Swap event —
/// so one event handler updates both with zero extra RPC calls. In-range Mint/Burn
/// change `liquidity` between Swaps; they are applied as deltas (see `apply_v3_liquidity_delta`).
#[derive(Debug, Clone)]
pub struct V3PoolState {
    /// token0 = lower address (EVM convention).
    pub token0: Address,
    pub token1: Address,
    /// Pool id within its router — the `fee` of the pool key and of trade calldata: the fee
    /// tier in ppm (100, 500, 2500, 3000, 10000), or the tick spacing for Aerodrome Slipstream.
    pub fee: u32,
    /// Swap fee in ppm used by the local quote. Equals `fee` for Uniswap/Pancake V3; read
    /// per pool for Slipstream (its fee is not implied by the tick spacing).
    pub swap_fee: u32,
    /// Current sqrt price as Q64.96 fixed-point (from slot0 / Swap event).
    pub sqrt_price_x96: U256,
    /// Active-tick liquidity (only valid for the current tick — not total TVL).
    pub liquidity: u128,
    /// Current tick (slot0 / Swap event). Decides whether a Mint/Burn is in range.
    pub tick: i32,
    /// Router ID that routes through this pool.
    pub router_id: String,
    /// When this entry was last updated (used for staleness check).
    pub last_updated: Instant,

    // ── Precomputed per Swap event — reused on every quote call ──────────────
    /// Virtual reserve of token0: L × Q96 / sqrtPriceX96.
    /// Zero when sqrtPrice or liquidity is zero.
    pub vr0: U256,
    /// Virtual reserve of token1: L × sqrtPriceX96 >> 96.
    pub vr1: U256,
    /// Fee numerator in ppm: 1_000_000 − fee.  (e.g. fee=500 → fee_num=999_500)
    pub fee_num_v3: U256,
}

impl V3PoolState {
    /// Recompute derived fields from current sqrtPriceX96 and liquidity.
    /// Called once at insert and once per Swap event — amortises 2 U256 muls +
    /// 1 U256 div across all quote_v3_spot() calls until the next price change.
    pub fn refresh_vr(&mut self) {
        self.fee_num_v3 = FEE_DENOM_V3.saturating_sub(U256::from(self.swap_fee));
        let l = U256::from(self.liquidity);
        if self.sqrt_price_x96.is_zero() || l.is_zero() {
            self.vr0 = U256::ZERO;
            self.vr1 = U256::ZERO;
        } else {
            self.vr0 = l.saturating_mul(Q96)
                .checked_div(self.sqrt_price_x96)
                .unwrap_or(U256::ZERO);
            self.vr1 = l.saturating_mul(self.sqrt_price_x96) >> 96u32;
        }
    }
}

/// How long a V2/Solidly-volatile pool reserve is considered fresh.
/// 120s: volatile pools self-correct via arbitrage — they cannot become critically
/// imbalanced like stable pools can. Lower-volume pairs (BRETT, DEGEN, doginme) may
/// not trade every 60s; a 60s TTL makes their pools go stale and disappear from Phase 1.
/// 120s keeps them in scope for detection without adding phantom-spread risk.
pub fn volatile_max_age() -> Duration { crate::config::tuning().volatile_pool_max_age() }

/// How long an Aerodrome-stable pool reserve is considered fresh.
/// 30s: If a stable pool gets one-sided arb'd to near-zero, it stops receiving Sync events
/// (no one trades terrible rates). A missed Sync event would keep the cache stale forever
/// at 120s. 30s ensures the pool falls out of scope within one scan cycle if events stop.
pub fn stable_max_age() -> Duration { crate::config::tuning().stable_pool_max_age() }

/// Q96 = 2^96, used for V3 sqrtPriceX96 ↔ virtual-reserve conversion.
/// Stored as a const to avoid `U256::from(1u128) << 96` on every quote call.
/// Limb layout (little-endian u64): bit 96 = limb[1] bit 32 → limb[1] = 2^32 = 4_294_967_296.
const Q96: U256 = U256::from_limbs([0, 4_294_967_296u64, 0, 0]);

/// V3 fee denominator: 1_000_000 ppm.
const FEE_DENOM_V3: U256 = U256::from_limbs([1_000_000u64, 0, 0, 0]);

/// Max fraction of input-side virtual reserve for V3 spot quotes (single-tick safe).
/// Trades larger than this fraction of `vr_in` can cross ticks where L=0 and overestimate output.
fn v3_spot_max_vr_fraction_bps() -> u64 { crate::config::tuning().v3_spot_max_vr_fraction_bps }

fn cap_v3_spot_amount_in(vr_in: U256, amount_in: U256) -> U256 {
    if vr_in.is_zero() {
        return amount_in;
    }
    let max_in =
        vr_in.saturating_mul(U256::from(v3_spot_max_vr_fraction_bps())) / U256::from(10_000u64);
    if max_in.is_zero() {
        return amount_in;
    }
    amount_in.min(max_in)
}

/// V2/Solidly fee denominator: 10_000 bps.
const FEE_DENOM_V2: U256 = U256::from_limbs([10_000u64, 0, 0, 0]);

/// Lookup table: SCALE[n] = 10^n  (n = 0..=18, all fit in u64).
/// Avoids U256::pow() in the stable-AMM hot path (~6 multiplications per call → 1 array index).
const SCALE: [U256; 19] = [
    U256::from_limbs([                           1, 0, 0, 0]), // 10^0
    U256::from_limbs([                          10, 0, 0, 0]), // 10^1
    U256::from_limbs([                         100, 0, 0, 0]), // 10^2
    U256::from_limbs([                       1_000, 0, 0, 0]), // 10^3
    U256::from_limbs([                      10_000, 0, 0, 0]), // 10^4
    U256::from_limbs([                     100_000, 0, 0, 0]), // 10^5
    U256::from_limbs([                   1_000_000, 0, 0, 0]), // 10^6
    U256::from_limbs([                  10_000_000, 0, 0, 0]), // 10^7
    U256::from_limbs([                 100_000_000, 0, 0, 0]), // 10^8
    U256::from_limbs([               1_000_000_000, 0, 0, 0]), // 10^9
    U256::from_limbs([              10_000_000_000, 0, 0, 0]), // 10^10
    U256::from_limbs([             100_000_000_000, 0, 0, 0]), // 10^11
    U256::from_limbs([           1_000_000_000_000, 0, 0, 0]), // 10^12
    U256::from_limbs([          10_000_000_000_000, 0, 0, 0]), // 10^13
    U256::from_limbs([         100_000_000_000_000, 0, 0, 0]), // 10^14
    U256::from_limbs([       1_000_000_000_000_000, 0, 0, 0]), // 10^15
    U256::from_limbs([      10_000_000_000_000_000, 0, 0, 0]), // 10^16
    U256::from_limbs([     100_000_000_000_000_000, 0, 0, 0]), // 10^17
    U256::from_limbs([   1_000_000_000_000_000_000, 0, 0, 0]), // 10^18
];


/// USD per whole token: `known` as given, then each other token from its deepest pool
/// against an already-priced token (by that side's USD value), in two passes.
fn price_tokens(
    sides: &[(Address, Address, f64, Address, f64)],
    known: &HashMap<Address, (f64, u8)>,
    decimals: &HashMap<Address, u8>,
) -> HashMap<Address, f64> {
    let mut prices: HashMap<Address, f64> = known.iter().map(|(t, (p, _))| (*t, *p)).collect();
    let dec = |t: &Address| decimals.get(t).copied().or_else(|| known.get(t).map(|k| k.1));
    for _ in 0..2 {
        // token -> (depth of the priced side in USD, price)
        let mut best: HashMap<Address, (f64, f64)> = HashMap::new();
        for (_, t0, r0, t1, r1) in sides {
            for (a, ra, b, rb) in [(t0, r0, t1, r1), (t1, r1, t0, r0)] {
                // a is priced, b is not
                let (Some(&pa), false) = (prices.get(a), prices.contains_key(b)) else { continue };
                let (Some(da), Some(db)) = (dec(a), dec(b)) else { continue };
                if *ra <= 0.0 || *rb <= 0.0 { continue; }
                let side_usd = ra / 10f64.powi(da as i32) * pa;
                let price_b = side_usd / (rb / 10f64.powi(db as i32));
                if best.get(b).is_none_or(|(d, _)| side_usd > *d) {
                    best.insert(*b, (side_usd, price_b));
                }
            }
        }
        if best.is_empty() { break; }
        prices.extend(best.into_iter().map(|(t, (_, p))| (t, p)));
    }
    prices
}

// ─── Pool cache ───────────────────────────────────────────────────────────────

/// Thread-safe pool reserve cache.
///
/// **V2/Aerodrome** (xy=k or x³y+xy³=k):
/// * `by_address` — pool_address → PoolInfo (reserves updated live from Sync events)
/// * `by_key`     — "router_id:token_in_lower:token_out_lower" → pool_address
///
/// Key conventions:
///   - V2:               `"router_id:0x...:0x..."`
///   - Solidly volatile: `"router_id::volatile:0x...:0x..."`
///   - Solidly stable:   `"router_id::stable:0x...:0x..."`
///
/// **V3** (concentrated liquidity):
/// * `v3_by_address` — pool_address → V3PoolState
/// * `v3_by_key`     — "router_id:token_in_lower:token_out_lower:fee" → pool_address
///
/// Both directions are stored under separate keys (same pool address).
#[derive(Clone)]
pub struct PoolCache {
    pub by_address: Arc<DashMap<Address, PoolInfo>>,
    pub by_key: Arc<DashMap<V2PoolKey, Address>>,
    pub v3_by_address: Arc<DashMap<Address, V3PoolState>>,
    pub v3_by_key: Arc<DashMap<V3PoolKey, Address>>,
    /// Cached union of V2 + V3 pool addresses for log filters / listeners.
    watch_addrs: Arc<RwLock<Vec<Address>>>,
    watch_addrs_dirty: Arc<AtomicBool>,
    /// Last applied (block number, log index) per pool. Two log feeds update the cache —
    /// pre-confirmed (flashblocks, ~1.2 s earlier) and sealed — so a late sealed log must
    /// never overwrite newer pre-confirmed state.
    log_pos: Arc<DashMap<Address, (u64, u64)>>,
    /// When any pool log was last applied (ms since `created`). While recent, the feed is
    /// live and a quiet pool's reserves are current — see [`PoolCache::feed_live`].
    last_log_ms: Arc<AtomicU64>,
    created: Instant,
    /// Pools below `tuning.min_pool_depth_usd`: every local quote treats them as missing.
    /// Recomputed from cached state by [`PoolCache::recompute_shallow`]; they stay
    /// subscribed, so a pool that fills is quoted again after the next recompute.
    shallow: Arc<dashmap::DashSet<Address>>,
    /// Pools of direct venues the executor does not allowlist (live mode): never quoted.
    disabled: Arc<dashmap::DashSet<Address>>,
    /// Uniswap V4 pools (by their pool address, see [`V4PoolKey::pool_address`]): kept in
    /// `v3_by_address` like any CL pool; this holds the PoolKey for quotes and the
    /// executor. Their logs come from the PoolManager, so they are not in `watch_addresses`.
    pub v4_keys: Arc<DashMap<Address, V4PoolKey>>,
}

impl Default for PoolCache {
    fn default() -> Self {
        Self {
            by_address: Arc::new(DashMap::new()),
            by_key: Arc::new(DashMap::new()),
            v3_by_address: Arc::new(DashMap::new()),
            v3_by_key: Arc::new(DashMap::new()),
            watch_addrs: Arc::new(RwLock::new(Vec::new())),
            watch_addrs_dirty: Arc::new(AtomicBool::new(true)),
            log_pos: Arc::new(DashMap::new()),
            last_log_ms: Arc::new(AtomicU64::new(u64::MAX)),
            created: Instant::now(),
            shallow: Arc::new(dashmap::DashSet::new()),
            disabled: Arc::new(dashmap::DashSet::new()),
            v4_keys: Arc::new(DashMap::new()),
        }
    }
}

impl PoolCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// A pool log was applied (either feed): the feed is live.
    pub fn note_log(&self) {
        self.last_log_ms.store(self.created.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// True while a pool log arrived within `tuning.feed_silence_ms`. Every Sync/Swap is
    /// applied, so a pool without events then is quiet, not stale: its price is unchanged.
    pub fn feed_live(&self) -> bool {
        let last = self.last_log_ms.load(Ordering::Relaxed);
        last != u64::MAX
            && (self.created.elapsed().as_millis() as u64).saturating_sub(last)
                <= crate::config::tuning().feed_silence_ms
    }

    /// The pool's max age is exceeded (no Sync for `volatile/stable_pool_max_age`). While the
    /// feed is live such a pool is still quoted, but its reserves are re-read on-chain before
    /// a trade uses it (`scan::recheck_quiet_pools`).
    pub fn is_quiet(&self, pool: Address) -> bool {
        self.by_address.get(&pool).is_some_and(|i| {
            let max = if i.is_stable { stable_max_age() } else { volatile_max_age() };
            i.last_sync.elapsed() > max
        })
    }

    /// Set a V3-type pool's swap fee (ppm) — Slipstream fees are dynamic. Recomputes the
    /// derived quote fields. Returns true when the fee changed.
    pub fn set_v3_swap_fee(&self, pool: Address, swap_fee: u32) -> bool {
        let Some(mut s) = self.v3_by_address.get_mut(&pool) else { return false };
        if s.swap_fee == swap_fee { return false; }
        s.swap_fee = swap_fee;
        s.refresh_vr();
        true
    }

    /// Set a V2/Aerodrome pool's fee (bps). Returns true when the fee changed.
    pub fn set_v2_fee_bps(&self, pool: Address, fee_bps: u32) -> bool {
        let Some(mut i) = self.by_address.get_mut(&pool) else { return false };
        if i.fee_bps == fee_bps { return false; }
        i.fee_bps = fee_bps;
        i.fee_num = FEE_DENOM_V2.saturating_sub(U256::from(fee_bps));
        true
    }

    /// True when local quotes must skip the pool: below `tuning.min_pool_depth_usd` (see
    /// `recompute_shallow`) or disabled (`disable_pool`).
    pub fn is_skipped(&self, pool: Address) -> bool {
        (!self.shallow.is_empty() && self.shallow.contains(&pool))
            || (!self.disabled.is_empty() && self.disabled.contains(&pool))
    }

    /// Never quote this pool again (e.g. a direct-venue pool the executor doesn't allow).
    pub fn disable_pool(&self, pool: Address) {
        self.disabled.insert(pool);
    }

    /// Recompute the shallow-pool set from cached state. `known` holds USD per whole token
    /// and decimals for tokens priced from outside the cache (stables, wrapped native);
    /// every other token is priced from its deepest cached pool against a priced token
    /// (two passes, so e.g. X/cbBTC works once cbBTC is priced via USDC). Depth = USD value
    /// of both sides (V2: reserves, V3: in-range virtual reserves); with one side unpriced it
    /// is twice the priced side, and a pool with neither priced is kept (never hidden blind).
    /// Returns (shallow, total) pool counts. No RPC.
    pub fn recompute_shallow(&self, known: &HashMap<Address, (f64, u8)>, decimals: &HashMap<Address, u8>, min_usd: f64) -> (usize, usize) {
        let sides = self.pool_sides();
        let total = sides.len();
        if min_usd <= 0.0 {
            self.shallow.clear();
            return (0, total);
        }
        let prices = price_tokens(&sides, known, decimals);
        let usd = |t: &Address, raw: f64| -> Option<f64> {
            let p = prices.get(t)?;
            let d = decimals.get(t).copied().or_else(|| known.get(t).map(|k| k.1))?;
            Some(raw / 10f64.powi(d as i32) * p)
        };
        let mut n = 0;
        for (pool, t0, r0, t1, r1) in &sides {
            let depth = match (usd(t0, *r0), usd(t1, *r1)) {
                (Some(a), Some(b)) => Some(a + b),
                (Some(a), None) | (None, Some(a)) => Some(2.0 * a),
                (None, None) => None,
            };
            if depth.is_some_and(|d| d < min_usd) {
                self.shallow.insert(*pool);
                n += 1;
            } else {
                self.shallow.remove(pool);
            }
        }
        (n, total)
    }

    /// (pool, token0, raw amount0, token1, raw amount1) for every pool: V2 reserves, V3
    /// in-range virtual reserves.
    fn pool_sides(&self) -> Vec<(Address, Address, f64, Address, f64)> {
        let f = crate::util::u256_to_f64;
        let mut v: Vec<_> = self.by_address.iter()
            .map(|e| (*e.key(), e.token0, f(e.reserve0), e.token1, f(e.reserve1)))
            .collect();
        v.extend(self.v3_by_address.iter()
            .map(|e| (*e.key(), e.token0, f(e.vr0), e.token1, f(e.vr1))));
        v
    }

    fn mark_watch_addrs_dirty(&self) {
        self.watch_addrs_dirty.store(true, Ordering::Release);
    }

    fn rebuild_watch_addrs_if_dirty(&self) {
        if !self.watch_addrs_dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut addrs: Vec<Address> = self.by_address.iter().map(|e| *e.key()).collect();
        addrs.extend(self.v3_by_address.iter().map(|e| *e.key()).filter(|a| !self.v4_keys.contains_key(a)));
        if let Ok(mut guard) = self.watch_addrs.write() {
            *guard = addrs;
        }
    }

    /// V4 pool ids (log topic1) for the PoolManager log filter.
    pub fn v4_pool_ids(&self) -> Vec<alloy::primitives::B256> {
        self.v4_keys.iter().map(|e| e.value().id()).collect()
    }

    /// All pool addresses (V2 + V3, not V4) for eth_subscribe / getLogs filters.
    /// Rebuilt only when pools are inserted or pruned — avoids O(n) collect per poll.
    pub fn watch_addresses(&self) -> Vec<Address> {
        self.rebuild_watch_addrs_if_dirty();
        self.watch_addrs
            .read()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Record that the log at `pos` (block number, log index) is being applied to `pool`.
    /// `Less`: older than what was applied — skip it. `Equal`: the same log again (e.g.
    /// its sealed copy after the pre-confirmed one) — safe to re-apply, no new event.
    /// `Greater`: new — recorded.
    pub fn advance_log_position(&self, pool: Address, pos: (u64, u64)) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let mut entry = self.log_pos.entry(pool).or_insert((0, 0));
        match pos.cmp(&*entry) {
            Ordering::Greater => { *entry = pos; Ordering::Greater }
            other => other,
        }
    }

    /// Insert a pool and register both directional keys.
    /// Precomputes `fee_num = 10_000 − fee_bps` as U256 so hot-path AMM math
    /// never calls U256::from() per quote call.
    pub fn insert(&self, pool: Address, mut info: PoolInfo) {
        info.fee_num = FEE_DENOM_V2.saturating_sub(U256::from(info.fee_bps));
        let rid = info.router_id.as_str();
        self.by_key.insert(V2PoolKey::new(rid, info.token0, info.token1), pool);
        self.by_key.insert(V2PoolKey::new(rid, info.token1, info.token0), pool);
        self.by_address.insert(pool, info);
        self.mark_watch_addrs_dirty();
    }

    /// Update reserves from a Sync event and stamp freshness.
    /// No-op if pool not in cache.
    pub fn update_reserves(&self, pool: Address, reserve0: U256, reserve1: U256) {
        if let Some(mut e) = self.by_address.get_mut(&pool) {
            e.reserve0 = reserve0;
            e.reserve1 = reserve1;
            e.last_sync = Instant::now();
        }
    }

    /// Get output amount for tokenIn → tokenOut using the pool's local reserves.
    /// Routes to xy=k or x³y+xy³=k formula based on `PoolInfo.is_stable`.
    /// Returns None if pool not cached or reserves are zero.
    pub fn get_amount_out(&self, pool: Address, token_in: Address, amount_in: U256) -> Option<U256> {
        if self.is_skipped(pool) { return None; }
        let info = self.by_address.get(&pool)?;
        compute_amount_out(&info, token_in, amount_in)
    }

    /// Look up by directional key and compute amount out.
    ///
    /// `max_age`: if `Some(d)`, returns `None` when the pool's reserves were last
    /// synced more than `d` ago — prevents phantom profits from stale startup state.
    /// Pass `None` for V2 pools (startup reserves are trustworthy for xy=k).
    /// Pass `Some(volatile_max_age())` / `Some(stable_max_age())` for Aerodrome.
    pub fn get_amount_out_by_key(
        &self,
        router_id: &str,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        max_age: Option<Duration>,
    ) -> Option<U256> {
        let key = V2PoolKey::new(router_id, token_in, token_out);
        let pool = *self.by_key.get(&key)?;
        if let Some(age) = max_age {
            if self.is_skipped(pool) { return None; }
            let info = self.by_address.get(&pool)?;
            if info.last_sync.elapsed() > age && !self.feed_live() {
                return None; // stale — caller skips this DEX for this scan
            }
            compute_amount_out(&info, token_in, amount_in)
        } else {
            self.get_amount_out(pool, token_in, amount_in)
        }
    }

    /// Look up by a pre-built directional key string and compute amount out.
    ///
    /// Same as `get_amount_out_by_key` but accepts a caller-supplied key to avoid
    /// the `format!("{}:{}:{}", ...)` allocation inside the hot scan loop.
    /// Use when the key has already been built once per pair (not once per router).
    pub fn get_amount_out_by_v2_key(
        &self,
        key: &V2PoolKey,
        token_in: Address,
        amount_in: U256,
        max_age: Option<Duration>,
    ) -> Option<U256> {
        let pool = *self.by_key.get(key)?;
        if let Some(age) = max_age {
            if self.is_skipped(pool) { return None; }
            let info = self.by_address.get(&pool)?;
            if info.last_sync.elapsed() > age && !self.feed_live() {
                return None;
            }
            compute_amount_out(&info, token_in, amount_in)
        } else {
            self.get_amount_out(pool, token_in, amount_in)
        }
    }

    /// Legacy string-key lookup — builds a [`V2PoolKey`] (router id must not need parsing).
    pub fn get_amount_out_by_key_str(
        &self,
        router_id: &str,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        max_age: Option<Duration>,
    ) -> Option<U256> {
        self.get_amount_out_by_v2_key(
            &V2PoolKey::new(router_id, token_in, token_out),
            token_in,
            amount_in,
            max_age,
        )
    }

    /// All V2/Aerodrome pool addresses currently in the cache.
    pub fn pool_addresses(&self) -> Vec<Address> {
        self.by_address.iter().map(|e| *e.key()).collect()
    }

    /// All V3 pool addresses currently in the cache.
    pub fn v3_pool_addresses(&self) -> Vec<Address> {
        self.v3_by_address.iter().map(|e| *e.key()).collect()
    }

    /// Count V3 pools updated within `window`. Returns (fresh, total).
    /// fresh = pools that received at least one Swap event within the window.
    /// Recomputed on every call (heartbeat only, O(pools)) so pools that age out of the
    /// window without a new event are reflected immediately.
    pub fn count_fresh_v3_pools(&self, window: Duration) -> (usize, usize) {
        let fresh = self
            .v3_by_address
            .iter()
            .filter(|e| e.last_updated.elapsed() <= window)
            .count();
        (fresh, self.v3_by_address.len())
    }

    /// Returns (token0, token1) for any watched pool — V2/Aerodrome or V3.
    /// Used by the targeted-scan path to identify which tokens moved on a Swap event.
    pub fn get_pool_tokens(&self, pool: Address) -> Option<(Address, Address)> {
        if let Some(info) = self.by_address.get(&pool) {
            Some((info.token0, info.token1))
        } else if let Some(v3) = self.v3_by_address.get(&pool) {
            Some((v3.token0, v3.token1))
        } else {
            None
        }
    }

    // ── V3 methods ────────────────────────────────────────────────────────────

    /// Insert a V3 pool and register both directional keys.
    /// Key format: "router_id:token_in_lower:token_out_lower:fee"
    pub fn insert_v3(&self, pool: Address, mut state: V3PoolState) {
        state.refresh_vr(); // precompute vr0, vr1, fee_num_v3 once at insert
        let fee = state.fee;
        let rid = state.router_id.as_str();

        self.v3_by_key
            .insert(V3PoolKey::new(rid, state.token0, state.token1, fee), pool);
        self.v3_by_key
            .insert(V3PoolKey::new(rid, state.token1, state.token0, fee), pool);
        self.v3_by_address.insert(pool, state);
        self.mark_watch_addrs_dirty();
    }

    /// Update sqrtPriceX96 and liquidity from a V3 Swap event.
    /// Also resets the staleness timer and recomputes cached virtual reserves
    /// so quote_v3_spot() reads pre-built values without any per-call maths.
    pub fn update_v3_state(&self, pool: Address, sqrt_price_x96: U256, liquidity: u128, tick: i32) {
        if let Some(mut e) = self.v3_by_address.get_mut(&pool) {
            e.sqrt_price_x96 = sqrt_price_x96;
            e.liquidity = liquidity;
            e.tick = tick;
            e.last_updated = Instant::now();
            e.refresh_vr();
        }
    }

    /// Apply a V3 Mint (`add`) or Burn (`!add`) of `amount` liquidity on `[tick_lower, tick_upper)`.
    /// Only a position containing the current tick changes active liquidity (the pool's
    /// own rule: `tickLower <= tick < tickUpper`). Not idempotent — the caller must apply
    /// each log once. Returns whether the active liquidity changed.
    pub fn apply_v3_liquidity_delta(&self, pool: Address, tick_lower: i32, tick_upper: i32, amount: u128, add: bool) -> bool {
        let Some(mut e) = self.v3_by_address.get_mut(&pool) else { return false };
        if amount == 0 || !(tick_lower <= e.tick && e.tick < tick_upper) {
            return false;
        }
        e.liquidity = if add { e.liquidity.saturating_add(amount) } else { e.liquidity.saturating_sub(amount) };
        e.last_updated = Instant::now();
        e.refresh_vr();
        true
    }

    /// Spot-price screen for a V3 pool.
    ///
    /// Returns `Some((estimated_amount_out, current_liquidity))` when:
    ///   - Pool is in cache
    ///   - sqrtPriceX96 is non-zero
    ///
    /// The amount_out is an *approximation* valid within the current tick only.
    /// Use it as a fast directional screen; confirm with QuoterV2 before execution.
    ///
    /// **No time-based staleness check**: V3 sqrtPriceX96 and liquidity are
    /// deterministic — they only change when a Swap event fires. Once we have a
    /// valid state (from startup slot0 discovery or a Swap event), the values
    /// remain exactly correct until the next Swap event updates them. A time-based
    /// TTL causes false blindness on quiet pools and serves no accuracy purpose.
    pub fn quote_v3_spot(
        &self,
        router_id: &str,
        token_in: Address,
        token_out: Address,
        fee: u32,
        amount_in: U256,
    ) -> Option<(U256, u128, U256)> {
        let key = V3PoolKey::new(router_id, token_in, token_out, fee);
        self.quote_v3_spot_key(&key, token_in, amount_in)
    }

    /// Same as `quote_v3_spot` but accepts a pre-built [`V3PoolKey`].
    pub fn quote_v3_spot_key(
        &self,
        key: &V3PoolKey,
        token_in: Address,
        amount_in: U256,
    ) -> Option<(U256, u128, U256)> {
        let pool_addr = *self.v3_by_key.get(key)?;
        if self.is_skipped(pool_addr) { return None; }

        let state = self.v3_by_address.get(&pool_addr)?;

        // Virtual reserves are precomputed by refresh_vr() at insert / Swap event time.
        // vr0 = L×Q96/sqrtP (token0 virtual reserve), vr1 = L×sqrtP>>96 (token1).
        // Both are zero when sqrtPrice or liquidity is zero — single guard covers all cases.
        let vr0 = state.vr0;
        let vr1 = state.vr1;
        if vr0.is_zero() || vr1.is_zero() {
            return None;
        }

        let (vr_in, vr_out) = if token_in == state.token0 {
            (vr0, vr1)
        } else {
            (vr1, vr0)
        };

        let effective_in = cap_v3_spot_amount_in(vr_in, amount_in);

        // xy=k with ppm fee:  out = ai×fee_num×vr_out / (vr_in×FEE_DENOM + ai×fee_num)
        // fee_num = 1_000_000 − fee, precomputed in refresh_vr().
        let ai_fee = effective_in.checked_mul(state.fee_num_v3)?;
        let num    = ai_fee.checked_mul(vr_out)?;
        let den    = vr_in.checked_mul(FEE_DENOM_V3)?.checked_add(ai_fee)?;
        if den.is_zero() {
            return None;
        }

        Some((num / den, state.liquidity, effective_in))
    }

    /// Output at the pool's current marginal price (after fee), with no tick cap and no
    /// price impact: `amount_in × vr_out / vr_in × (1 − fee)`. Swap output is increasing
    /// and concave in input, so this is an **upper bound** on the real output for any
    /// `amount_in`.
    pub fn quote_v3_marginal(
        &self,
        key: &V3PoolKey,
        token_in: Address,
        amount_in: U256,
    ) -> Option<U256> {
        let pool_addr = *self.v3_by_key.get(key)?;
        if self.is_skipped(pool_addr) { return None; }
        let state = self.v3_by_address.get(&pool_addr)?;
        let (vr_in, vr_out) = if token_in == state.token0 {
            (state.vr0, state.vr1)
        } else {
            (state.vr1, state.vr0)
        };
        if vr_in.is_zero() || vr_out.is_zero() {
            return None;
        }
        let num = amount_in.checked_mul(state.fee_num_v3)?.checked_mul(vr_out)?;
        let den = vr_in.checked_mul(FEE_DENOM_V3)?;
        Some(num / den)
    }

    /// Current (sqrtPriceX96, active liquidity) of the pool behind `key`, if cached.
    /// Changes whenever the pool trades or liquidity is added/removed in range.
    pub fn v3_price_state(&self, key: &V3PoolKey) -> Option<(U256, u128)> {
        let pool_addr = *self.v3_by_key.get(key)?;
        self.v3_by_address.get(&pool_addr).map(|s| (s.sqrt_price_x96, s.liquidity))
    }

    /// Time since the pool's local state last changed (Swap, Mint/Burn or seed).
    pub fn v3_state_age(&self, key: &V3PoolKey) -> Option<Duration> {
        let pool_addr = *self.v3_by_key.get(key)?;
        self.v3_by_address.get(&pool_addr).map(|s| s.last_updated.elapsed())
    }

}

// ─── AMM math ─────────────────────────────────────────────────────────────────

/// Routes to the correct AMM formula based on `PoolInfo.is_stable`.
fn compute_amount_out(info: &PoolInfo, token_in: Address, amount_in: U256) -> Option<U256> {
    let (reserve_in, reserve_out, dec_in, dec_out) = if token_in == info.token0 {
        (info.reserve0, info.reserve1, info.decimals0, info.decimals1)
    } else if token_in == info.token1 {
        (info.reserve1, info.reserve0, info.decimals1, info.decimals0)
    } else {
        return None;
    };
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return None;
    }
    if info.is_stable {
        amount_out_stable(amount_in, reserve_in, reserve_out, dec_in, dec_out, info.fee_num)
    } else {
        Some(amount_out_v2(amount_in, reserve_in, reserve_out, info.fee_num))
    }
}

/// Standard V2 xy=k constant-product output formula.
/// Also correct for Aerodrome-volatile pools.
///
/// fee_num: precomputed 10_000 − fee_bps (from PoolInfo.fee_num).
pub fn amount_out_v2(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_num: U256,
) -> U256 {
    let ai_fee = amount_in * fee_num;
    let num    = ai_fee * reserve_out;
    let den    = reserve_in * FEE_DENOM_V2 + ai_fee;
    if den.is_zero() { U256::ZERO } else { num / den }
}

/// Solidly stable AMM: f(x,y) = x³y + xy³ = k  (Newton-Raphson solver).
///
/// Aerodrome-stable pools use this curve.
/// Reserves are normalised to 18 decimals before solving, then de-normalised.
///
/// fee_bps: per pool from factory.getFee() — Aerodrome's default stable fee is 5 (0.05%).
pub fn amount_out_stable(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    decimals_in: u8,
    decimals_out: u8,
    fee_num: U256,
) -> Option<U256> {
    let one = SCALE[18];

    // Normalise to 18 decimals so the curve math is token-agnostic.
    let scale_in  = SCALE[(18usize).saturating_sub(decimals_in  as usize)];
    let scale_out = SCALE[(18usize).saturating_sub(decimals_out as usize)];

    let x = reserve_in.saturating_mul(scale_in);
    let y = reserve_out.saturating_mul(scale_out);

    // Apply fee to amount_in; fee_num = 10_000 − fee_bps (precomputed on PoolInfo).
    let dx = amount_in.saturating_mul(fee_num) / FEE_DENOM_V2;
    let dx_norm = dx.saturating_mul(scale_in);

    // k = x³y + xy³  (invariant before the swap)
    let k = stable_k(x, y, one);

    // Solve for y_after: (x + dx)³·y_after + (x + dx)·y_after³ = k
    let x_after = x.saturating_add(dx_norm);
    let y_after = stable_get_y(x_after, k, y, one);

    if y_after >= y {
        return None; // numerical issue or zero-liquidity
    }
    let dy_norm = y - y_after;

    // De-normalise: divide by scale_out to get token units
    Some(dy_norm / scale_out)
}

// ── Stable curve helpers ──────────────────────────────────────────────────────

/// Invariant: k = x³y + xy³ = xy(x²+y²), normalised to 1e18.
fn stable_k(x: U256, y: U256, one: U256) -> U256 {
    // x*y*(x²+y²) / one³
    let x2 = x.checked_mul(x).and_then(|v| v.checked_div(one)).unwrap_or(U256::ZERO);
    let y2 = y.checked_mul(y).and_then(|v| v.checked_div(one)).unwrap_or(U256::ZERO);
    let xy = x.checked_mul(y).and_then(|v| v.checked_div(one)).unwrap_or(U256::ZERO);
    xy.checked_mul(x2.saturating_add(y2)).and_then(|v| v.checked_div(one))
        .unwrap_or(U256::ZERO)
}

/// f(x0, y) = x0³·y + x0·y³,  normalised to 1e18.
fn stable_f(x0: U256, y: U256, one: U256) -> U256 {
    let x3 = x0.checked_mul(x0)
        .and_then(|v| v.checked_div(one))
        .and_then(|v| v.checked_mul(x0))
        .and_then(|v| v.checked_div(one))
        .unwrap_or(U256::ZERO);
    let y3 = y.checked_mul(y)
        .and_then(|v| v.checked_div(one))
        .and_then(|v| v.checked_mul(y))
        .and_then(|v| v.checked_div(one))
        .unwrap_or(U256::ZERO);
    let a = x3.checked_mul(y).and_then(|v| v.checked_div(one)).unwrap_or(U256::ZERO);
    let b = x0.checked_mul(y3).and_then(|v| v.checked_div(one)).unwrap_or(U256::ZERO);
    a.saturating_add(b)
}

/// d/dy of f(x0, y) = x0³ + 3·x0·y²,  normalised to 1e18.
fn stable_d(x0: U256, y: U256, one: U256) -> U256 {
    let x3 = x0.checked_mul(x0)
        .and_then(|v| v.checked_div(one))
        .and_then(|v| v.checked_mul(x0))
        .and_then(|v| v.checked_div(one))
        .unwrap_or(U256::ZERO);
    let y2 = y.checked_mul(y).and_then(|v| v.checked_div(one)).unwrap_or(U256::ZERO);
    let b = U256::from(3u64)
        .checked_mul(x0).and_then(|v| v.checked_mul(y2))
        .and_then(|v| v.checked_div(one))
        .unwrap_or(U256::ZERO);
    x3.saturating_add(b)
}

/// Newton-Raphson solver: find y such that f(x0, y) = xy (the invariant k).
/// Converges in < 10 iterations for typical reserve ratios.
fn stable_get_y(x0: U256, xy: U256, y_init: U256, one: U256) -> U256 {
    let mut y = y_init;
    for _ in 0..255 {
        let k0 = stable_f(x0, y, one);
        let d  = stable_d(x0, y, one);
        if d.is_zero() { break; }
        let dy = if k0 < xy {
            (xy - k0).checked_mul(one).and_then(|v| v.checked_div(d)).unwrap_or(U256::ZERO)
        } else {
            (k0 - xy).checked_mul(one).and_then(|v| v.checked_div(d)).unwrap_or(U256::ZERO)
        };
        if dy <= U256::ONE { break; }
        if k0 < xy {
            y = y.saturating_add(dy);
        } else {
            y = y.saturating_sub(dy);
        }
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_native_pool_below_weth_is_inverted_in_the_cache() {
        let virtual_: Address = "0x0b3e328455c4059EEb9e3f84b5543F74E24e7E1b".parse().unwrap();
        let usdc: Address = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".parse().unwrap();
        let inv = V4PoolKey { currency0: Address::ZERO, currency1: virtual_, fee: 3000, tick_spacing: 60 };
        assert!(inv.inverted());
        assert!(!V4PoolKey { currency1: usdc, ..inv }.inverted(), "USDC sorts above WETH");
        assert!(!V4PoolKey { currency0: virtual_, currency1: crate::util::WETH, ..inv }.inverted(), "ERC-20 pool");
        // Price VIRTUAL per ETH = 4 → sqrtP = 2·2^96; cached (WETH per VIRTUAL) = 1/4 → 2^96 / 2.
        let (s, t) = inv.to_cache_price(U256::from(2u8) << 96, 13_863);
        assert_eq!(s, (U256::from(1u8) << 96) / U256::from(2u8));
        assert_eq!(t, -13_864);
        // A tick is in a position's range in the pool iff the mirrored tick is in the mirrored range.
        for (lo, hi) in [(-120, 60), (0, 60), (60, 120)] {
            for tick in -130..130 {
                let (_, ct) = inv.to_cache_price(U256::from(1u8) << 96, tick);
                let (clo, chi) = inv.to_cache_range(lo, hi);
                assert_eq!(lo <= tick && tick < hi, clo <= ct && ct < chi, "tick {tick} range [{lo},{hi})");
            }
        }
    }

    #[test]
    fn v4_pool_id_matches_solidity_abi_encode() {
        // Base native ETH/USDC 0.05 % (cast keccak $(cast abi-encode "f(address,address,uint24,int24,address)" …)).
        let usdc: Address = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".parse().unwrap();
        let key = V4PoolKey { currency0: Address::ZERO, currency1: usdc, fee: 500, tick_spacing: 10 };
        let id: alloy::primitives::B256 = "0x96d4b53a38337a5733179751781178a2613306063c511b78cd02684739288c0a".parse().unwrap();
        assert_eq!(key.id(), id);
        assert_eq!(key.pool_address(), "0x781178a2613306063c511b78cd02684739288c0a".parse::<Address>().unwrap());
        assert_eq!(v4_pool_address(id), key.pool_address());
        // Negative tick spacing sign-extends (int24 in the ABI word).
        let neg = V4PoolKey { tick_spacing: -10, ..key };
        assert_eq!(neg.id(), "0x472b306de6ffbe2b8f7d3eb7ea822428181097a61d3afc3c0d7c604d15879477".parse::<alloy::primitives::B256>().unwrap());
    }

    #[test]
    fn v4_pools_are_not_watch_addresses() {
        let cache = PoolCache::new();
        let state = |router: &str| V3PoolState {
            token0: Address::repeat_byte(1), token1: Address::repeat_byte(2), fee: 500, swap_fee: 500,
            sqrt_price_x96: U256::from(1u128 << 96), liquidity: 1_000_000, tick: 0,
            router_id: router.into(), last_updated: Instant::now(),
            vr0: U256::ZERO, vr1: U256::ZERO, fee_num_v3: U256::ZERO,
        };
        let key = V4PoolKey { currency0: Address::repeat_byte(1), currency1: Address::repeat_byte(2), fee: 500, tick_spacing: 10 };
        cache.v4_keys.insert(key.pool_address(), key);
        cache.insert_v3(key.pool_address(), state("v4"));
        cache.insert_v3(Address::repeat_byte(0xaa), state("v3"));
        assert_eq!(cache.watch_addresses(), vec![Address::repeat_byte(0xaa)]);
        assert_eq!(cache.v4_pool_ids(), vec![key.id()]);
    }

    #[test]
    fn fresh_v3_count_reflects_pools_aging_out_without_updates() {
        let cache = PoolCache::new();
        cache.insert_v3(Address::repeat_byte(0xaa), V3PoolState {
            token0: Address::repeat_byte(1),
            token1: Address::repeat_byte(2),
            fee: 500,
            swap_fee: 500,
            sqrt_price_x96: U256::from(1u64) << 96,
            liquidity: 1_000_000,
            tick: 0,
            router_id: "r".into(),
            last_updated: Instant::now(),
            vr0: U256::ZERO,
            vr1: U256::ZERO,
            fee_num_v3: U256::ZERO,
        });
        assert_eq!(cache.count_fresh_v3_pools(Duration::from_secs(30)), (1, 1));
        // The pool ages past the window with no new event.
        cache.v3_by_address.get_mut(&Address::repeat_byte(0xaa)).unwrap().last_updated =
            Instant::now() - Duration::from_secs(60);
        assert_eq!(cache.count_fresh_v3_pools(Duration::from_secs(30)), (0, 1));
    }

    fn v2(t0: Address, r0: u128, t1: Address, r1: u128) -> PoolInfo {
        PoolInfo {
            token0: t0, token1: t1, reserve0: U256::from(r0), reserve1: U256::from(r1),
            fee_bps: 30, router_id: "r".into(), is_stable: false, decimals0: 18, decimals1: 18,
            last_sync: Instant::now(), fee_num: U256::ZERO,
        }
    }

    /// The depth floor prices tokens through the cache (VVV via a deep VVV/WETH pool, then a
    /// dust pool is measured in dollars), hides only pools below it, and un-hides them once
    /// they fill. A pool with no priced token is never hidden.
    #[test]
    fn shallow_set_uses_usd_depth_and_follows_refills() {
        let e18 = 10u128.pow(18);
        let (usdc, weth, vvv, mystery, other) = (
            Address::repeat_byte(1), Address::repeat_byte(2), Address::repeat_byte(3),
            Address::repeat_byte(4), Address::repeat_byte(5),
        );
        let (deep_usdc, deep_vvv, dust_vvv, mystery_pool) =
            (Address::repeat_byte(0x10), Address::repeat_byte(0x11), Address::repeat_byte(0x12), Address::repeat_byte(0x13));
        let cache = PoolCache::new();
        // 1M USDC / 250 WETH, and 100 WETH ($400k) / 200k VVV → VVV = $2.
        cache.insert(deep_usdc, v2(usdc, 1_000_000 * 10u128.pow(6), weth, 250 * e18));
        cache.insert(deep_vvv, v2(weth, 100 * e18, vvv, 200_000 * e18));
        // Dust: 0.001 WETH / 2 VVV ≈ $8. Its raw L-style size would pass a units check.
        cache.insert(dust_vvv, v2(weth, e18 / 1000, vvv, 2 * e18));
        cache.insert(mystery_pool, v2(mystery, 5 * e18, other, 5 * e18));

        let known: HashMap<Address, (f64, u8)> = [(usdc, (1.0, 6)), (weth, (4000.0, 18))].into_iter().collect();
        let decimals: HashMap<Address, u8> =
            [(usdc, 6), (weth, 18), (vvv, 18), (mystery, 18), (other, 18)].into_iter().collect();

        assert_eq!(cache.recompute_shallow(&known, &decimals, 20_000.0), (1, 4));
        assert!(cache.is_skipped(dust_vvv));
        assert!(!cache.is_skipped(deep_vvv) && !cache.is_skipped(mystery_pool));
        assert_eq!(cache.get_amount_out(dust_vvv, weth, U256::from(e18 / 10_000)), None);
        assert!(cache.get_amount_out(deep_vvv, weth, U256::from(e18)).is_some());

        // Liquidity arrives: 10 WETH / 20k VVV ($80k) → quoted again after the next recompute.
        cache.update_reserves(dust_vvv, U256::from(10 * e18), U256::from(20_000 * e18));
        assert_eq!(cache.recompute_shallow(&known, &decimals, 20_000.0), (0, 4));
        assert!(cache.get_amount_out(dust_vvv, weth, U256::from(e18 / 10)).is_some());

        // 0 turns the floor off.
        cache.update_reserves(dust_vvv, U256::from(1u64), U256::from(1u64));
        assert_eq!(cache.recompute_shallow(&known, &decimals, 0.0), (0, 4));
    }
}
