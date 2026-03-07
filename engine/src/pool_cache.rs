use alloy::primitives::{Address, U256};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
}

/// Maximum age for V3 pool state before it is considered stale.
/// Set very high (24 h) so it never triggers in normal operation — V3 state is
/// deterministic (only Swap events change sqrtPriceX96/liquidity), so a pool
/// that has not swapped in hours still has exactly correct prices.
/// The only path that sets `last_updated` beyond this threshold is
/// `invalidate_v3_pools()`, called by the listener when its WebSocket drops.
/// This acts as a "down flag" sentinel: returns None until the first live Swap
/// event after reconnect refreshes `last_updated`.
pub const V3_STATE_MAX_AGE: Duration = Duration::from_secs(86_400); // 24 h

/// How long a V2/Solidly-volatile pool reserve is considered fresh.
/// 300s (5 min): covers pools that swap every few minutes (typical on Linea).
pub const VOLATILE_MAX_AGE: Duration = Duration::from_secs(300);

/// How long a Solidly-stable / SyncSwap-stable pool reserve is considered fresh.
pub const STABLE_MAX_AGE: Duration = Duration::from_secs(120);

/// Price impact factor per fee tier (numerator; denominator = 10_000).
/// Limits ΔsqrtP/sqrtP to stay within the active tick cluster without
/// needing tick boundary data. See plan for derivation.
pub fn v3_impact_factor(fee_ppm: u32) -> u64 {
    match fee_ppm {
        100  =>  6,   // 0.01% pool, 1bp tick spacing → 0.06% ΔP cap
        500  => 10,   // 0.05% pool, 10bp tick spacing → 0.10% ΔP cap
        2500 => 20,   // 0.25% pool, 50bp tick spacing → 0.20% ΔP cap
        3000 => 25,   // 0.30% pool, 60bp tick spacing → 0.25% ΔP cap
        _    => 50,   // 1.00%+ pool, wide spacing → 0.50% ΔP cap (default)
    }
}

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
#[derive(Clone, Default)]
pub struct PoolCache {
    pub by_address: Arc<DashMap<Address, PoolInfo>>,
    pub by_key: Arc<DashMap<String, Address>>,
    pub v3_by_address: Arc<DashMap<Address, V3PoolState>>,
    pub v3_by_key: Arc<DashMap<String, Address>>,
}

impl PoolCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a pool and register both directional keys.
    /// `last_sync` is set to a very old instant so startup reserves are
    /// treated as stale until the first live Sync event updates them.
    pub fn insert(&self, pool: Address, info: PoolInfo) {
        let t0 = format!("{:?}", info.token0).to_lowercase();
        let t1 = format!("{:?}", info.token1).to_lowercase();
        let rid = &info.router_id;

        self.by_key.insert(format!("{}:{}:{}", rid, t0, t1), pool);
        self.by_key.insert(format!("{}:{}:{}", rid, t1, t0), pool);
        self.by_address.insert(pool, info);
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

    /// Convenience: look up by directional key, then compute amount out.
    /// Does NOT check freshness — use `get_amount_out_by_key_fresh` for
    /// detection to avoid stale startup reserves.
    pub fn get_amount_out_by_key(
        &self,
        router_id: &str,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Option<U256> {
        let t_in  = format!("{:?}", token_in).to_lowercase();
        let t_out = format!("{:?}", token_out).to_lowercase();
        let key = format!("{}:{}:{}", router_id, t_in, t_out);
        let pool = *self.by_key.get(&key)?;
        self.get_amount_out(pool, token_in, amount_in)
    }

    /// Like `get_amount_out_by_key` but returns None if `last_sync` is older
    /// than `max_age`.  This prevents phantom profits from stale startup
    /// reserves that haven't yet been rebalanced on-chain.
    pub fn get_amount_out_by_key_fresh(
        &self,
        router_id: &str,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        max_age: Duration,
    ) -> Option<U256> {
        let t_in  = format!("{:?}", token_in).to_lowercase();
        let t_out = format!("{:?}", token_out).to_lowercase();
        let key = format!("{}:{}:{}", router_id, t_in, t_out);
        let pool = *self.by_key.get(&key)?;
        let info = self.by_address.get(&pool)?;
        if info.last_sync.elapsed() > max_age {
            return None; // stale — caller skips this DEX for this scan
        }
        compute_amount_out(&info, token_in, amount_in)
    }

    /// All pool addresses currently in the cache (used for Sync event subscription).
    pub fn pool_addresses(&self) -> Vec<Address> {
        self.by_address.iter().map(|e| *e.key()).collect()
    }

    /// Returns (token0, token1) for any watched pool — V2/Solidly/SyncSwap or V3.
    /// Used by the targeted-scan path to identify which tokens moved on a Swap event.
    pub fn get_pool_tokens(&self, pool: Address) -> Option<(Address, Address)> {
        if let Some(info) = self.by_address.get(&pool) {
            return Some((info.token0, info.token1));
        }
        if let Some(v3) = self.v3_by_address.get(&pool) {
            return Some((v3.token0, v3.token1));
        }
        None
    }

    // ── V3 methods ────────────────────────────────────────────────────────────

    /// Insert a V3 pool and register both directional keys.
    /// Key format: "router_id:token_in_lower:token_out_lower:fee"
    pub fn insert_v3(&self, pool: Address, state: V3PoolState) {
        let t0 = format!("{:?}", state.token0).to_lowercase();
        let t1 = format!("{:?}", state.token1).to_lowercase();
        let fee = state.fee;
        let rid = &state.router_id;

        self.v3_by_key.insert(format!("{}:{}:{}:{}", rid, t0, t1, fee), pool);
        self.v3_by_key.insert(format!("{}:{}:{}:{}", rid, t1, t0, fee), pool);
        self.v3_by_address.insert(pool, state);
    }

    /// Update sqrtPriceX96 and liquidity from a V3 Swap event.
    /// Also resets the staleness timer (`last_updated`).
    pub fn update_v3_state(&self, pool: Address, sqrt_price_x96: U256, liquidity: u128) {
        if let Some(mut e) = self.v3_by_address.get_mut(&pool) {
            e.sqrt_price_x96 = sqrt_price_x96;
            e.liquidity = liquidity;
            e.last_updated = Instant::now();
        }
    }

    /// All V3 pool addresses (used for Swap event subscription).
    pub fn v3_pool_addresses(&self) -> Vec<Address> {
        self.v3_by_address.iter().map(|e| *e.key()).collect()
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
    ) -> Option<(U256, u128)> {
        let t_in  = format!("{:?}", token_in).to_lowercase();
        let t_out = format!("{:?}", token_out).to_lowercase();
        let key = format!("{}:{}:{}:{}", router_id, t_in, t_out, fee);
        let pool_addr = *self.v3_by_key.get(&key)?;

        let state = self.v3_by_address.get(&pool_addr)?;

        // Guard against explicitly-invalidated pools (listener was down).
        // V3_STATE_MAX_AGE is 24 h — never reached in normal operation; only
        // triggered when invalidate_v3_pools() stamps last_updated far in the past.
        if state.last_updated.elapsed() > V3_STATE_MAX_AGE {
            return None;
        }

        let sqrtp = state.sqrt_price_x96;
        if sqrtp.is_zero() {
            return None;
        }

        let l = U256::from(state.liquidity);
        if l.is_zero() {
            return None;
        }

        // Virtual-reserve constant-product formula (exact within one tick).
        //
        // Within a single V3 tick, the pool behaves identically to a V2 xy=k pool
        // whose reserves are the "virtual reserves" derived from L and sqrtPriceX96:
        //   vr_token0 = L × 2^96 / sqrtP     (virtual reserve of token0)
        //   vr_token1 = L × sqrtP / 2^96     (virtual reserve of token1)
        //
        // Applying the standard xy=k fee formula to these virtual reserves gives the
        // same output as the V3 contract for single-tick trades — including the
        // price-impact term that the old marginal-rate formula completely missed.
        //
        // The old formula: amount_out = amount_in × price × fee_factor
        //   → ignores the denominator's amount_in term → overestimates for large trades
        //
        // This formula: amount_out = amount_in × (1M-fee) × vr_out
        //                           / (vr_in × 1M + amount_in × (1M-fee))
        //   → exact for single-tick; underestimates when trade spans multiple ticks
        //   → conservative: never produces phantom profits

        let q96 = U256::from(1u128) << 96u32;
        let fee_denom = U256::from(1_000_000u64);
        let fee_num  = U256::from(1_000_000u64 - state.fee as u64);

        // vr0 = L × Q96 / sqrtP,  vr1 = L × sqrtP >> 96
        let vr0 = l.saturating_mul(q96).checked_div(sqrtp)?;
        let vr1 = l.saturating_mul(sqrtp) >> 96u32;

        if vr0.is_zero() || vr1.is_zero() {
            return None;
        }

        let (vr_in, vr_out) = if token_in == state.token0 {
            (vr0, vr1)
        } else {
            (vr1, vr0)
        };

        // xy=k with ppm fee:  out = ai×(1M-fee)×vr_out / (vr_in×1M + ai×(1M-fee))
        let ai_fee = amount_in.checked_mul(fee_num)?;
        let num    = ai_fee.checked_mul(vr_out)?;
        let den    = vr_in.checked_mul(fee_denom)?.checked_add(ai_fee)?;
        if den.is_zero() {
            return None;
        }
        let amount_out = num / den;

        Some((amount_out, state.liquidity))
    }

    /// Estimate the maximum capital safely tradeable within the current tick.
    ///
    /// Uses the price-impact formula: capacity = virtual_reserve × impact_factor.
    /// Where virtual_reserve = L × Q96 / sqrtPriceX96 (for token0→token1).
    /// impact_factor is fee-tier-aware (see v3_impact_factor()).
    ///
    /// This is NOT the exact tick boundary — it's the amount that moves sqrtPrice
    /// by at most 0.05-0.50% (depending on fee tier), keeping the trade within the
    /// active tick cluster without needing tick bitmap data.
    pub fn estimate_safe_v3_capacity(&self, pool: Address, token_in: Address) -> U256 {
        let state = match self.v3_by_address.get(&pool) {
            Some(s) => s,
            None => return U256::ZERO,
        };
        let sqrtp = state.sqrt_price_x96;
        if sqrtp.is_zero() {
            return U256::ZERO;
        }

        let l = U256::from(state.liquidity);
        let q96 = U256::from(1u128) << 96u32;

        let virtual_reserve = if token_in == state.token0 {
            // token0→token1: virtual_reserve = L × Q96 / sqrtPriceX96
            l.saturating_mul(q96)
                .checked_div(sqrtp)
                .unwrap_or(U256::ZERO)
        } else {
            // token1→token0: virtual_reserve = L × sqrtPriceX96 / Q96
            l.saturating_mul(sqrtp) >> 96u32
        };

        let factor = U256::from(v3_impact_factor(state.fee));
        virtual_reserve * factor / U256::from(10_000u64)
    }

    /// Mark all V2/Solidly/SyncSwap pools as stale by stamping their
    /// `last_sync` to just past VOLATILE_MAX_AGE ago.  Called by the listener
    /// when its WebSocket subscription drops — prevents stale reserves from
    /// generating phantom arb opportunities during the reconnect window.
    /// Returns the number of pools invalidated.
    pub fn invalidate_v2_pools(&self) -> usize {
        let stale_at = Instant::now() - VOLATILE_MAX_AGE - Duration::from_secs(1);
        let mut count = 0usize;
        for mut e in self.by_address.iter_mut() {
            e.last_sync = stale_at;
            count += 1;
        }
        count
    }

    /// Stale a single V3 pool by router/token/fee key.
    /// `quote_v3_spot` will return None until the next live Swap event updates
    /// `last_updated`. Called after a pre-flight rejection to stop re-detection
    /// of the same phantom spread until on-chain prices actually move.
    pub fn invalidate_v3_pool_by_key(&self, router_id: &str, token_in: Address, token_out: Address, fee: u32) {
        let t_in  = format!("{:?}", token_in).to_lowercase();
        let t_out = format!("{:?}", token_out).to_lowercase();
        let key = format!("{}:{}:{}:{}", router_id, t_in, t_out, fee);
        if let Some(pool_addr) = self.v3_by_key.get(&key) {
            let stale_at = Instant::now() - V3_STATE_MAX_AGE - Duration::from_secs(1);
            if let Some(mut e) = self.v3_by_address.get_mut(&*pool_addr) {
                e.last_updated = stale_at;
            }
        }
    }

    /// Mark all V3 pools as stale by stamping their `last_updated` to just
    /// past V3_STATE_MAX_AGE ago.  Called by the listener when its WebSocket
    /// subscription drops.  `quote_v3_spot` will return None until the first
    /// live Swap event after reconnect refreshes `last_updated`.
    /// Returns the number of pools invalidated.
    pub fn invalidate_v3_pools(&self) -> usize {
        let stale_at = Instant::now() - V3_STATE_MAX_AGE - Duration::from_secs(1);
        let mut count = 0usize;
        for mut e in self.v3_by_address.iter_mut() {
            e.last_updated = stale_at;
            count += 1;
        }
        count
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
        amount_out_stable(amount_in, reserve_in, reserve_out, dec_in, dec_out, info.fee_bps)
    } else {
        Some(amount_out_v2(amount_in, reserve_in, reserve_out, info.fee_bps))
    }
}

/// Standard V2 xy=k constant-product output formula.
/// Also correct for Solidly-volatile and SyncSwap-classic pools.
///
/// fee_bps: fee in basis points (25 = 0.25%, 30 = 0.3%, 20 = 0.2%).
pub fn amount_out_v2(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
) -> U256 {
    let fee_num = U256::from(10_000u32 - fee_bps);
    let ai_fee  = amount_in * fee_num;
    let num     = ai_fee * reserve_out;
    let den     = reserve_in * U256::from(10_000u32) + ai_fee;
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
    fee_bps: u32,
) -> Option<U256> {
    let one = U256::from(10u64).pow(U256::from(18u32));

    // Normalise to 18 decimals so the curve math is token-agnostic.
    let scale_in  = U256::from(10u64).pow(U256::from((18u32).saturating_sub(decimals_in as u32)));
    let scale_out = U256::from(10u64).pow(U256::from((18u32).saturating_sub(decimals_out as u32)));

    let x = reserve_in.saturating_mul(scale_in);
    let y = reserve_out.saturating_mul(scale_out);

    // Apply fee to amount_in
    let dx = amount_in
        .saturating_mul(U256::from(10_000u32 - fee_bps))
        / U256::from(10_000u32);
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
        if dy.is_zero() { break; }
        if k0 < xy {
            y = y.saturating_add(dy);
        } else {
            y = y.saturating_sub(dy);
        }
    }
    y
}
