use alloy::primitives::{Address, U256};
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

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
/// SyncSwap pool, kept fresh from `Sync(reserve0, reserve1)` events.
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
    /// True for Solidly-stable / SyncSwap-stable pools (x³y+xy³=k curve).
    /// False for V2 / Solidly-volatile / SyncSwap-classic pools (xy=k curve).
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
/// so one event handler updates both with zero extra RPC calls.
#[derive(Debug, Clone)]
pub struct V3PoolState {
    /// token0 = lower address (EVM convention).
    pub token0: Address,
    pub token1: Address,
    /// Fee in ppm: 100, 500, 2500, 3000, 10000.
    pub fee: u32,
    /// Current sqrt price as Q64.96 fixed-point (from slot0 / Swap event).
    pub sqrt_price_x96: U256,
    /// Active-tick liquidity (only valid for the current tick — not total TVL).
    pub liquidity: u128,
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
        self.fee_num_v3 = FEE_DENOM_V3.saturating_sub(U256::from(self.fee));
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
pub const VOLATILE_MAX_AGE: Duration = Duration::from_secs(120);

/// How long a Solidly-stable / SyncSwap-stable pool reserve is considered fresh.
/// 30s: If a stable pool gets one-sided arb'd to near-zero, it stops receiving Sync events
/// (no one trades terrible rates). A missed Sync event would keep the cache stale forever
/// at 120s. 30s ensures the pool falls out of scope within one scan cycle if events stop.
pub const STABLE_MAX_AGE: Duration = Duration::from_secs(30);

/// Q96 = 2^96, used for V3 sqrtPriceX96 ↔ virtual-reserve conversion.
/// Stored as a const to avoid `U256::from(1u128) << 96` on every quote call.
/// Limb layout (little-endian u64): bit 96 = limb[1] bit 32 → limb[1] = 2^32 = 4_294_967_296.
const Q96: U256 = U256::from_limbs([0, 4_294_967_296u64, 0, 0]);

/// V3 fee denominator: 1_000_000 ppm.
const FEE_DENOM_V3: U256 = U256::from_limbs([1_000_000u64, 0, 0, 0]);

/// Max fraction of input-side virtual reserve for V3 spot quotes (single-tick safe).
/// Trades larger than this fraction of `vr_in` can cross ticks where L=0 and overestimate output.
const V3_SPOT_MAX_VR_FRACTION_BPS: u64 = 200; // 2% of vr_in

fn cap_v3_spot_amount_in(vr_in: U256, amount_in: U256) -> U256 {
    if vr_in.is_zero() {
        return amount_in;
    }
    let max_in =
        vr_in.saturating_mul(U256::from(V3_SPOT_MAX_VR_FRACTION_BPS)) / U256::from(10_000u64);
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


// ─── Pool cache ───────────────────────────────────────────────────────────────

/// Thread-safe pool reserve cache.
///
/// **V2/Solidly/SyncSwap** (xy=k or x³y+xy³=k):
/// * `by_address` — pool_address → PoolInfo (reserves updated live from Sync events)
/// * `by_key`     — "router_id:token_in_lower:token_out_lower" → pool_address
///
/// Key conventions:
///   - V2:               `"router_id:0x...:0x..."`
///   - Solidly volatile: `"router_id::volatile:0x...:0x..."`
///   - Solidly stable:   `"router_id::stable:0x...:0x..."`
///   - SyncSwap:         `"router_id:0x...:0x..."`
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
    /// Heartbeat v3 fresh/total — recomputed when `v3_metrics_dirty` is set.
    v3_metrics_dirty: Arc<AtomicBool>,
    cached_v3_fresh: Arc<AtomicUsize>,
    cached_v3_total: Arc<AtomicUsize>,
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
            v3_metrics_dirty: Arc::new(AtomicBool::new(true)),
            cached_v3_fresh: Arc::new(AtomicUsize::new(0)),
            cached_v3_total: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl PoolCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn mark_watch_addrs_dirty(&self) {
        self.watch_addrs_dirty.store(true, Ordering::Release);
    }

    fn mark_v3_metrics_dirty(&self) {
        self.v3_metrics_dirty.store(true, Ordering::Release);
    }

    fn rebuild_watch_addrs_if_dirty(&self) {
        if !self.watch_addrs_dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut addrs: Vec<Address> = self.by_address.iter().map(|e| *e.key()).collect();
        addrs.extend(self.v3_by_address.iter().map(|e| *e.key()));
        if let Ok(mut guard) = self.watch_addrs.write() {
            *guard = addrs;
        }
    }

    /// All pool addresses (V2 + V3) for eth_subscribe / getLogs filters.
    /// Rebuilt only when pools are inserted or pruned — avoids O(n) collect per poll.
    pub fn watch_addresses(&self) -> Vec<Address> {
        self.rebuild_watch_addrs_if_dirty();
        self.watch_addrs
            .read()
            .map(|v| v.clone())
            .unwrap_or_default()
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
        let info = self.by_address.get(&pool)?;
        compute_amount_out(&info, token_in, amount_in)
    }

    /// Look up by directional key and compute amount out.
    ///
    /// `max_age`: if `Some(d)`, returns `None` when the pool's reserves were last
    /// synced more than `d` ago — prevents phantom profits from stale startup state.
    /// Pass `None` for V2 pools (startup reserves are trustworthy for xy=k).
    /// Pass `Some(VOLATILE_MAX_AGE)` / `Some(STABLE_MAX_AGE)` for Solidly/SyncSwap.
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
            let info = self.by_address.get(&pool)?;
            if info.last_sync.elapsed() > age {
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
            let info = self.by_address.get(&pool)?;
            if info.last_sync.elapsed() > age {
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

    /// All V2/Solidly/SyncSwap pool addresses currently in the cache.
    pub fn pool_addresses(&self) -> Vec<Address> {
        self.by_address.iter().map(|e| *e.key()).collect()
    }

    /// All V3 pool addresses currently in the cache.
    pub fn v3_pool_addresses(&self) -> Vec<Address> {
        self.v3_by_address.iter().map(|e| *e.key()).collect()
    }

    /// Count V3 pools updated within `window`. Returns (fresh, total).
    /// fresh = pools that received at least one Swap event within the window.
    pub fn count_fresh_v3_pools(&self, window: Duration) -> (usize, usize) {
        let total_live = self.v3_by_address.len();
        let cached_total = self.cached_v3_total.load(Ordering::Relaxed);
        if self.v3_metrics_dirty.swap(false, Ordering::AcqRel) || cached_total != total_live {
            let fresh = self
                .v3_by_address
                .iter()
                .filter(|e| e.last_updated.elapsed() <= window)
                .count();
            self.cached_v3_fresh.store(fresh, Ordering::Relaxed);
            self.cached_v3_total.store(total_live, Ordering::Relaxed);
            return (fresh, total_live);
        }
        (
            self.cached_v3_fresh.load(Ordering::Relaxed),
            self.cached_v3_total.load(Ordering::Relaxed),
        )
    }

    /// Returns (token0, token1) for any watched pool — V2/Solidly/SyncSwap or V3.
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
        self.mark_v3_metrics_dirty();
    }

    /// Update sqrtPriceX96 and liquidity from a V3 Swap event.
    /// Also resets the staleness timer and recomputes cached virtual reserves
    /// so quote_v3_spot() reads pre-built values without any per-call maths.
    pub fn update_v3_state(&self, pool: Address, sqrt_price_x96: U256, liquidity: u128) {
        if let Some(mut e) = self.v3_by_address.get_mut(&pool) {
            e.sqrt_price_x96 = sqrt_price_x96;
            e.liquidity = liquidity;
            e.last_updated = Instant::now();
            e.refresh_vr();
            self.mark_v3_metrics_dirty();
        }
    }

    /// Evict V2/Solidly and V3 pools whose reserves/state have not been refreshed
    /// by an on-chain event for longer than `v2_ttl` or `v3_ttl` respectively.
    ///
    /// Called periodically (e.g. every 10 min) to reclaim memory from pools that
    /// became inactive or were removed from a DEX. Both `by_key` indexes are cleaned
    /// up by reconstructing the keys from the evicted pool's metadata.
    pub fn prune_stale(&self, v2_ttl: Duration, v3_ttl: Duration) -> (usize, usize) {
        // ── V2 / Solidly / SyncSwap ──────────────────────────────────────────
        let stale_v2: Vec<(Address, PoolInfo)> = self
            .by_address
            .iter()
            .filter(|e| e.last_sync.elapsed() > v2_ttl)
            .map(|e| (*e.key(), e.value().clone()))
            .collect();

        for (pool, info) in &stale_v2 {
            self.by_address.remove(pool);
            let rid = info.router_id.as_str();
            self.by_key
                .remove(&V2PoolKey::new(rid, info.token0, info.token1));
            self.by_key
                .remove(&V2PoolKey::new(rid, info.token1, info.token0));
        }

        // ── V3 ───────────────────────────────────────────────────────────────
        let stale_v3: Vec<(Address, V3PoolState)> = self
            .v3_by_address
            .iter()
            .filter(|e| e.last_updated.elapsed() > v3_ttl)
            .map(|e| (*e.key(), e.value().clone()))
            .collect();

        for (pool, state) in &stale_v3 {
            self.v3_by_address.remove(pool);
            let rid = state.router_id.as_str();
            let fee = state.fee;
            self.v3_by_key
                .remove(&V3PoolKey::new(rid, state.token0, state.token1, fee));
            self.v3_by_key
                .remove(&V3PoolKey::new(rid, state.token1, state.token0, fee));
        }

        if !stale_v2.is_empty() || !stale_v3.is_empty() {
            self.mark_watch_addrs_dirty();
            self.mark_v3_metrics_dirty();
        }

        (stale_v2.len(), stale_v3.len())
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
/// Also correct for Solidly-volatile and SyncSwap-classic pools.
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
/// Both Solidly-stable and SyncSwap-stable pools use this curve.
/// Reserves are normalised to 18 decimals before solving, then de-normalised.
///
/// fee_bps: typically 4 (0.04%) for Solidly stable, 10 (0.10%) for SyncSwap stable.
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
