use alloy::primitives::{Address, U256};
use dashmap::DashMap;
use std::sync::Arc;

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

// ─── Pool cache ───────────────────────────────────────────────────────────────

/// Thread-safe pool reserve cache.
/// * `by_address` — pool_address → PoolInfo (reserves updated live from Sync events)
/// * `by_key`     — "router_id:token_in_lower:token_out_lower" → pool_address
///
/// Both directions (A→B and B→A) are stored under separate keys; they map to the
/// same pool address since V2/Solidly pools are symmetric.
#[derive(Clone, Default)]
pub struct PoolCache {
    pub by_address: Arc<DashMap<Address, PoolInfo>>,
    pub by_key: Arc<DashMap<String, Address>>,
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
