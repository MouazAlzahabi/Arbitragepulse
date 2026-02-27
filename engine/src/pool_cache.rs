use alloy::primitives::{Address, U256};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ─── Pool state ───────────────────────────────────────────────────────────────

/// Current on-chain state of one V2/Solidly-volatile pool, kept fresh from
/// `Sync(reserve0, reserve1)` events.
#[derive(Debug, Clone)]
pub struct PoolInfo {
    /// token0 = lower address (EVM convention, same for Uniswap V2 and Solidly forks).
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    /// Fee in basis-points (e.g. 25 = 0.25%, 30 = 0.3%, 20 = 0.2%).
    pub fee_bps: u32,
    /// The router ID that routes through this pool (for key lookup).
    pub router_id: String,
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

/// How long a V3 pool state is considered fresh.
/// If no Swap event arrives within this window, the cached L and sqrtP may have
/// drifted — skip trading that pool to avoid stale-capacity reverts.
pub const V3_STATE_MAX_AGE: Duration = Duration::from_secs(120);

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
/// **V2/Solidly** (xy=k):
/// * `by_address` — pool_address → PoolInfo (reserves updated live from Sync events)
/// * `by_key`     — "router_id:token_in_lower:token_out_lower" → pool_address
///
/// **V3** (concentrated liquidity):
/// * `v3_by_address` — pool_address → V3PoolState (sqrtPriceX96 + liquidity, updated from Swap events)
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
    pub fn insert(&self, pool: Address, info: PoolInfo) {
        let t0 = format!("{:?}", info.token0).to_lowercase();
        let t1 = format!("{:?}", info.token1).to_lowercase();
        let rid = &info.router_id;

        self.by_key.insert(format!("{}:{}:{}", rid, t0, t1), pool);
        self.by_key.insert(format!("{}:{}:{}", rid, t1, t0), pool);
        self.by_address.insert(pool, info);
    }

    /// Update reserves from a Sync event.  No-op if pool not in cache.
    pub fn update_reserves(&self, pool: Address, reserve0: U256, reserve1: U256) {
        if let Some(mut e) = self.by_address.get_mut(&pool) {
            e.reserve0 = reserve0;
            e.reserve1 = reserve1;
        }
    }

    /// Get output amount for tokenIn → tokenOut using local xy=k reserves.
    /// Returns None if pool not cached or reserves are zero.
    pub fn get_amount_out(&self, pool: Address, token_in: Address, amount_in: U256) -> Option<U256> {
        let info = self.by_address.get(&pool)?;
        let (reserve_in, reserve_out) = if token_in == info.token0 {
            (info.reserve0, info.reserve1)
        } else if token_in == info.token1 {
            (info.reserve1, info.reserve0)
        } else {
            return None;
        };
        if reserve_in.is_zero() || reserve_out.is_zero() {
            return None;
        }
        Some(amount_out_v2(amount_in, reserve_in, reserve_out, info.fee_bps))
    }

    /// Convenience: look up by directional key, then compute amount out.
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

    /// All pool addresses currently in the cache (used for Sync event subscription).
    pub fn pool_addresses(&self) -> Vec<Address> {
        self.by_address.iter().map(|e| *e.key()).collect()
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
    ///   - Cache is fresh (< V3_STATE_MAX_AGE since last Swap event)
    ///   - sqrtPriceX96 is non-zero
    ///
    /// The amount_out is an *approximation* valid within the current tick only.
    /// Use it as a fast directional screen; confirm with QuoterV2 before execution.
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

        // Staleness check: skip if no Swap event recently
        if state.last_updated.elapsed() > V3_STATE_MAX_AGE {
            return None;
        }

        let sqrtp = state.sqrt_price_x96;
        if sqrtp.is_zero() {
            return None;
        }

        // Spot-price formula (single-tick approximation, integer arithmetic):
        //   price = (sqrtPriceX96 / 2^96)^2
        //   token0→token1: amount_out ≈ amount_in × sqrtp² / 2^192 × (1_000_000 - fee) / 1_000_000
        //   token1→token0: amount_out ≈ amount_in × 2^192 / sqrtp² × (1_000_000 - fee) / 1_000_000
        let fee_num = U256::from(1_000_000u64 - fee as u64);
        let q192 = U256::from(1u8) << 192u32;

        let amount_out = if token_in == state.token0 {
            let sqrtp_sq = sqrtp.checked_mul(sqrtp)?; // may overflow for extreme prices
            amount_in
                .checked_mul(sqrtp_sq)?
                .checked_div(q192)?
                .checked_mul(fee_num)?
                / U256::from(1_000_000u64)
        } else {
            // token1 → token0: invert price
            let sqrtp_sq = sqrtp.checked_mul(sqrtp)?;
            amount_in
                .checked_mul(q192)?
                .checked_div(sqrtp_sq)?
                .checked_mul(fee_num)?
                / U256::from(1_000_000u64)
        };

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
}

// ─── AMM math ─────────────────────────────────────────────────────────────────

/// Standard V2 xy=k constant-product output formula.
/// Also correct for Solidly-volatile pools (same curve, different fee).
///
/// fee_bps: fee in basis points (25 = 0.25%, 30 = 0.3%, 20 = 0.2%).
pub fn amount_out_v2(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
) -> U256 {
    // amount_in_with_fee = amount_in * (10000 - fee_bps)
    // amount_out = amount_in_with_fee * reserve_out
    //            / (reserve_in * 10000 + amount_in_with_fee)
    let fee_num = U256::from(10_000u32 - fee_bps);
    let ai_fee  = amount_in * fee_num;
    let num     = ai_fee * reserve_out;
    let den     = reserve_in * U256::from(10_000u32) + ai_fee;
    if den.is_zero() { U256::ZERO } else { num / den }
}
