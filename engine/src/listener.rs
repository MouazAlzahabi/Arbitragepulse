use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::abi::{IPancakeV3Pool, IUniswapV3Pool, Swap, Sync};
use crate::api::{broadcast_log, LogBroadcaster};
use crate::pool_cache::PoolCache;

fn reserve_delta_bps(old: U256, new: U256, threshold_bps: u32) -> bool {
    if old.is_zero() || threshold_bps == 0 {
        return false;
    }
    let delta = if new > old { new - old } else { old - new };
    delta * U256::from(10_000u32) >= old * U256::from(threshold_bps)
}

fn sqrtp_delta_bps(old: U256, new: U256, threshold_bps: u32) -> bool {
    if old.is_zero() || threshold_bps == 0 {
        return false;
    }
    let delta = if new > old { new - old } else { old - new };
    delta * U256::from(10_000u32) >= old * U256::from(threshold_bps)
}

/// topic0s of every pool log the engine applies: V2 Sync, Solidly Sync, V3 Swap (Uniswap,
/// Pancake), and V3 Mint/Burn (same signature on Uniswap V3, Pancake V3 and Slipstream).
/// Used by both log feeds (sealed + pendingLogs) and the poll fallback.
pub fn pool_log_topics(solidly_sync_hash: B256) -> Vec<B256> {
    vec![
        Sync::SIGNATURE_HASH,
        solidly_sync_hash,
        Swap::SIGNATURE_HASH,
        IPancakeV3Pool::Swap::SIGNATURE_HASH,
        IUniswapV3Pool::Mint::SIGNATURE_HASH,
        IUniswapV3Pool::Burn::SIGNATURE_HASH,
    ]
}

/// Decode a single Swap/Sync log, apply it to the pool cache, and classify magnitude.
/// Returns `Some(magnitude)` when the log was recognized, applied, and is new — the caller
/// should trigger a scan. A log older than the pool's last applied one is skipped; the
/// same log seen again (pre-confirmed, then sealed) is re-applied but returns `None` so it
/// doesn't trigger a second scan. Removed (reorged) logs are ignored.
/// V3 Mint/Burn are liquidity deltas: applied once (a repeat is a no-op), and they only
/// update state — liquidity alone doesn't move the price, so no scan is triggered; the
/// changed liquidity invalidates the pool's QuoteCache entries.
pub fn apply_log_to_cache(
    log: &alloy::rpc::types::Log,
    cache: &PoolCache,
    solidly_sync_hash: B256,
    large_swap_bps: u32,
    large_v3_bps: u32,
) -> Option<SwapMagnitude> {
    let pool = log.address();
    let topic0 = log.topics().first().copied()?;
    if log.removed {
        return None;
    }
    let repeat = match log.block_number.zip(log.log_index) {
        Some(pos) => match cache.advance_log_position(pool, pos) {
            std::cmp::Ordering::Less => return None,
            std::cmp::Ordering::Equal => true,
            std::cmp::Ordering::Greater => false,
        },
        None => false,
    };
    let mut magnitude = SwapMagnitude::Normal;

    if topic0 == Sync::SIGNATURE_HASH {
        let decoded = Sync::decode_log(log.as_ref()).ok()?;
        let r0 = U256::from(decoded.reserve0);
        let r1 = U256::from(decoded.reserve1);
        if let Some(old) = cache.by_address.get(&pool) {
            if reserve_delta_bps(old.reserve0, r0, large_swap_bps)
                || reserve_delta_bps(old.reserve1, r1, large_swap_bps)
            {
                magnitude = SwapMagnitude::Large;
            }
        }
        cache.update_reserves(pool, r0, r1);
        return (!repeat).then_some(magnitude);
    }

    if topic0 == solidly_sync_hash {
        let raw_data = log.data().data.as_ref();
        if raw_data.len() < 64 {
            return None;
        }
        let mut r0_bytes = [0u8; 32];
        let mut r1_bytes = [0u8; 32];
        r0_bytes.copy_from_slice(&raw_data[0..32]);
        r1_bytes.copy_from_slice(&raw_data[32..64]);
        let r0 = U256::from_be_bytes(r0_bytes);
        let r1 = U256::from_be_bytes(r1_bytes);
        if let Some(old) = cache.by_address.get(&pool) {
            if reserve_delta_bps(old.reserve0, r0, large_swap_bps)
                || reserve_delta_bps(old.reserve1, r1, large_swap_bps)
            {
                magnitude = SwapMagnitude::Large;
            }
        }
        cache.update_reserves(pool, r0, r1);
        return (!repeat).then_some(magnitude);
    }

    if topic0 == Swap::SIGNATURE_HASH {
        let decoded = Swap::decode_log(log.as_ref()).ok()?;
        let sqrtp = U256::from(decoded.sqrtPriceX96);
        let liq: u128 = decoded.liquidity.into();
        if let Some(old) = cache.v3_by_address.get(&pool) {
            if sqrtp_delta_bps(old.sqrt_price_x96, sqrtp, large_v3_bps) {
                magnitude = SwapMagnitude::Large;
            }
        }
        cache.update_v3_state(pool, sqrtp, liq, decoded.tick.as_i32());
        return (!repeat).then_some(magnitude);
    }

    if topic0 == IPancakeV3Pool::Swap::SIGNATURE_HASH {
        let decoded = IPancakeV3Pool::Swap::decode_log(log.as_ref()).ok()?;
        let sqrtp = U256::from(decoded.sqrtPriceX96);
        let liq: u128 = decoded.liquidity.into();
        if let Some(old) = cache.v3_by_address.get(&pool) {
            if sqrtp_delta_bps(old.sqrt_price_x96, sqrtp, large_v3_bps) {
                magnitude = SwapMagnitude::Large;
            }
        }
        cache.update_v3_state(pool, sqrtp, liq, decoded.tick.as_i32());
        return (!repeat).then_some(magnitude);
    }

    if topic0 == IUniswapV3Pool::Mint::SIGNATURE_HASH {
        if !repeat {
            let d = IUniswapV3Pool::Mint::decode_log(log.as_ref()).ok()?;
            cache.apply_v3_liquidity_delta(pool, d.tickLower.as_i32(), d.tickUpper.as_i32(), d.amount, true);
        }
        return None;
    }

    if topic0 == IUniswapV3Pool::Burn::SIGNATURE_HASH {
        if !repeat {
            let d = IUniswapV3Pool::Burn::decode_log(log.as_ref()).ok()?;
            cache.apply_v3_liquidity_delta(pool, d.tickLower.as_i32(), d.tickUpper.as_i32(), d.amount, false);
        }
        return None;
    }

    None
}

// ─── Swap event (generic across V2 + V3) ─────────────────────────────────────

/// Whether a swap moved the pool price significantly.
/// Large swaps create brief arbitrage windows and get fast-tracked in chain.rs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SwapMagnitude {
    Normal,
    Large,
}

#[derive(Debug, Clone)]
pub struct SwapEvent {
    pub chain_id: u64,
    pub pool: Address,
    pub block_number: u64,
    pub magnitude: SwapMagnitude,
}

// ─── Listener ─────────────────────────────────────────────────────────────────

pub struct Listener {
    pub chain_id: u64,
    pub chain_name: String,
    /// Reserve change threshold (basis points) to classify a V2/Solidly Sync as "large".
    /// 200 = 2% reserve shift.
    pub large_swap_bps: u32,
    /// sqrtPriceX96 change threshold (basis points) for V3 Swap events.
    /// 50 = 0.5% sqrtP change (~1% price impact).
    pub large_v3_bps: u32,
}

impl Listener {
    pub fn new(chain_id: u64, chain_name: String, large_swap_bps: u32, large_v3_bps: u32) -> Self {
        Self { chain_id, chain_name, large_swap_bps, large_v3_bps }
    }

    /// Poll on-chain events for all discovered pools using `eth_getLogs`.
    ///
    /// When `poll_fallback` is false (default on Base with working WS log subscription),
    /// only the stats ticker runs — avoids duplicate get_logs RPC load and double cache writes.
    pub async fn subscribe<P: Provider + Clone + 'static>(
        &self,
        provider: P,
        tx: mpsc::Sender<SwapEvent>,
        pool_cache: Arc<PoolCache>,
        log_tx: LogBroadcaster,
        poll_fallback: bool,
        block_time_ms: u64,
    ) -> Result<()> {
        let chain_id = self.chain_id;
        let chain_name = self.chain_name.clone();

        let all_addrs = pool_cache.watch_addresses();

        let msg = if poll_fallback {
            format!(
                "[{}] Listener: {} pools (poll fallback + WS)",
                chain_name, all_addrs.len()
            )
        } else {
            format!(
                "[{}] Listener: {} pools (WS-only — poll fallback off)",
                chain_name, all_addrs.len()
            )
        };
        info!("{}", msg);
        broadcast_log(&log_tx, "info", &msg, None);

        if all_addrs.is_empty() {
            let msg = format!("[{}] No pools in cache — event polling skipped", chain_name);
            warn!("{}", msg);
            broadcast_log(&log_tx, "warn", &msg, None);
            return Ok(());
        }

        use alloy::primitives::keccak256;
        let solidly_sync_hash = keccak256(b"Sync(uint256,uint256)");

        let event_sigs = pool_log_topics(solidly_sync_hash);

        if !poll_fallback {
            return Ok(());
        }

        let sync_count = Arc::new(AtomicU64::new(0));
        let v3_count = Arc::new(AtomicU64::new(0));

        let sync_count_log = sync_count.clone();
        let v3_count_log = v3_count.clone();
        let chain_name_c = chain_name.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(crate::config::tuning().price_refresh());
            let mut idle_ticks: u64 = 0;
            loop {
                interval.tick().await;
                let sc = sync_count_log.swap(0, Ordering::Relaxed);
                let vc = v3_count_log.swap(0, Ordering::Relaxed);
                if sc > 0 || vc > 0 {
                    idle_ticks = 0;
                    info!(
                        "[{}] Events polled: Sync={}, V3-Swap={} in last 60s",
                        chain_name_c, sc, vc
                    );
                } else {
                    idle_ticks += 1;
                    if idle_ticks % 5 == 0 {
                        info!("[{}] No events in last {}s", chain_name_c, idle_ticks * 60);
                    }
                }
            }
        });

        let cache = pool_cache.clone();
        let log_tx_poll = log_tx.clone();
        let large_swap_bps = self.large_swap_bps;
        let large_v3_bps = self.large_v3_bps;
        let base_filter = Filter::new()
            .address(all_addrs)
            .event_signature(event_sigs);
        let poll_interval = Duration::from_millis(block_time_ms.max(1000));
        tokio::spawn(async move {
            let mut last_block: u64 = 0;

            loop {
                let current_block = match provider.get_block_number().await {
                    Ok(b) => b,
                    Err(e) => {
                        debug!("[{}] get_block_number failed: {}", chain_name, e);
                        tokio::time::sleep(poll_interval).await;
                        continue;
                    }
                };

                if last_block == 0 {
                    last_block = current_block;
                    tokio::time::sleep(poll_interval).await;
                    continue;
                }

                if current_block <= last_block {
                    tokio::time::sleep(poll_interval).await;
                    continue;
                }

                const MAX_RANGE: u64 = 5;
                let from = if current_block - last_block > MAX_RANGE {
                    current_block - MAX_RANGE + 1
                } else {
                    last_block + 1
                };
                let to = current_block;

                let filter = base_filter.clone()
                    .from_block(from)
                    .to_block(to);

                match provider.get_logs(&filter).await {
                    Ok(logs) => {
                        for log in logs {
                            let pool = log.address();
                            let block = log.block_number.unwrap_or(current_block);
                            let topic0 = log.topics().first().copied();

                            if topic0 == Some(Sync::SIGNATURE_HASH) || topic0 == Some(solidly_sync_hash) {
                                sync_count.fetch_add(1, Ordering::Relaxed);
                            } else if topic0 == Some(Swap::SIGNATURE_HASH)
                                || topic0 == Some(IPancakeV3Pool::Swap::SIGNATURE_HASH)
                            {
                                v3_count.fetch_add(1, Ordering::Relaxed);
                            }

                            if let Some(magnitude) = apply_log_to_cache(
                                &log,
                                &cache,
                                solidly_sync_hash,
                                large_swap_bps,
                                large_v3_bps,
                            ) {
                                let _ = tx.try_send(SwapEvent {
                                    chain_id,
                                    pool,
                                    block_number: block,
                                    magnitude,
                                });
                            }
                        }
                        last_block = to;
                    }
                    Err(e) => {
                        let msg = format!(
                            "[{}] get_logs FAILED blocks {}..{}: {}",
                            chain_name, from, to, e
                        );
                        error!("{}", msg);
                        broadcast_log(&log_tx_poll, "error", &msg, None);
                        last_block = current_block;
                    }
                }

                tokio::time::sleep(poll_interval).await;
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool_cache::PoolInfo;
    use alloy::primitives::{address, keccak256};

    const POOL: Address = address!("00000000000000000000000000000000000000aa");

    fn cache_with_pool() -> PoolCache {
        let cache = PoolCache::new();
        cache.insert(POOL, PoolInfo {
            token0: address!("0000000000000000000000000000000000000001"),
            token1: address!("0000000000000000000000000000000000000002"),
            reserve0: U256::from(1_000u64),
            reserve1: U256::from(1_000u64),
            fee_bps: 30,
            router_id: "r".into(),
            is_stable: false,
            decimals0: 18,
            decimals1: 18,
            last_sync: std::time::Instant::now(),
            fee_num: U256::ZERO,
        });
        cache
    }

    /// A V2 Sync log for POOL at (block, index).
    fn sync_log(block: u64, index: u64, r0: u64, removed: bool) -> alloy::rpc::types::Log {
        let data = Sync { reserve0: alloy::primitives::Uint::from(r0), reserve1: alloy::primitives::Uint::from(1_000u64) }
            .encode_log_data();
        alloy::rpc::types::Log {
            inner: alloy::primitives::Log { address: POOL, data },
            block_number: Some(block),
            log_index: Some(index),
            removed,
            ..Default::default()
        }
    }

    fn apply(cache: &PoolCache, log: &alloy::rpc::types::Log) -> Option<SwapMagnitude> {
        apply_log_to_cache(log, cache, keccak256(b"Sync(uint256,uint256)"), 200, 50)
    }

    fn reserve0(cache: &PoolCache) -> U256 {
        cache.by_address.get(&POOL).unwrap().reserve0
    }

    #[test]
    fn preconfirmed_then_sealed_logs_stay_ordered_and_trigger_once() {
        let cache = cache_with_pool();
        // Pre-confirmed log of block 11 arrives first and triggers a scan.
        assert!(apply(&cache, &sync_log(11, 3, 2_000, false)).is_some());
        assert_eq!(reserve0(&cache), U256::from(2_000u64));
        // Its sealed copy: re-applied (same data), but no second scan.
        assert!(apply(&cache, &sync_log(11, 3, 2_000, false)).is_none());
        // A late sealed log of block 10 must not overwrite the newer state.
        assert!(apply(&cache, &sync_log(10, 9, 1_500, false)).is_none());
        assert_eq!(reserve0(&cache), U256::from(2_000u64));
        // A newer log applies and triggers.
        assert!(apply(&cache, &sync_log(11, 4, 2_100, false)).is_some());
        assert_eq!(reserve0(&cache), U256::from(2_100u64));
        // Removed (reorged) logs are ignored.
        assert!(apply(&cache, &sync_log(12, 0, 9_999, true)).is_none());
        assert_eq!(reserve0(&cache), U256::from(2_100u64));
    }

    const V3: Address = address!("00000000000000000000000000000000000000bb");

    /// V3 pool at tick 100 with liquidity 1_000_000.
    fn cache_with_v3_pool() -> PoolCache {
        let cache = PoolCache::new();
        cache.insert_v3(V3, crate::pool_cache::V3PoolState {
            token0: address!("0000000000000000000000000000000000000001"),
            token1: address!("0000000000000000000000000000000000000002"),
            fee: 500,
            sqrt_price_x96: U256::from(1u64) << 96,
            liquidity: 1_000_000,
            tick: 100,
            router_id: "r".into(),
            last_updated: std::time::Instant::now(),
            vr0: U256::ZERO,
            vr1: U256::ZERO,
            fee_num_v3: U256::ZERO,
        });
        cache
    }

    fn at(data: alloy::primitives::LogData, block: u64, index: u64) -> alloy::rpc::types::Log {
        alloy::rpc::types::Log {
            inner: alloy::primitives::Log { address: V3, data },
            block_number: Some(block),
            log_index: Some(index),
            ..Default::default()
        }
    }

    fn i24(v: i32) -> alloy::primitives::aliases::I24 {
        alloy::primitives::aliases::I24::try_from(v).unwrap()
    }

    fn mint(lo: i32, hi: i32, amount: u128, block: u64, index: u64) -> alloy::rpc::types::Log {
        at(IUniswapV3Pool::Mint {
            sender: Address::ZERO, owner: Address::ZERO, tickLower: i24(lo), tickUpper: i24(hi),
            amount, amount0: U256::ZERO, amount1: U256::ZERO,
        }.encode_log_data(), block, index)
    }

    fn burn(lo: i32, hi: i32, amount: u128, block: u64, index: u64) -> alloy::rpc::types::Log {
        at(IUniswapV3Pool::Burn {
            owner: Address::ZERO, tickLower: i24(lo), tickUpper: i24(hi),
            amount, amount0: U256::ZERO, amount1: U256::ZERO,
        }.encode_log_data(), block, index)
    }

    fn liq(cache: &PoolCache) -> u128 {
        cache.v3_by_address.get(&V3).unwrap().liquidity
    }

    #[test]
    fn v3_mint_burn_update_in_range_liquidity_once() {
        let cache = cache_with_v3_pool();
        // In range (lower inclusive): liquidity grows, no scan.
        assert!(apply(&cache, &mint(100, 200, 500, 10, 1)).is_none());
        assert_eq!(liq(&cache), 1_000_500);
        // Its sealed copy (same position) must not double-apply.
        apply(&cache, &mint(100, 200, 500, 10, 1));
        assert_eq!(liq(&cache), 1_000_500);
        // Out of range (upper exclusive / below): unchanged.
        apply(&cache, &mint(0, 100, 7, 10, 2));
        apply(&cache, &mint(150, 300, 7, 10, 3));
        assert_eq!(liq(&cache), 1_000_500);
        // In-range Burn decreases; a repeat is a no-op.
        apply(&cache, &burn(-50, 150, 300, 10, 4));
        apply(&cache, &burn(-50, 150, 300, 10, 4));
        assert_eq!(liq(&cache), 1_000_200);
        // An older log (already covered by newer state) is skipped.
        apply(&cache, &mint(-50, 150, 999, 9, 0));
        assert_eq!(liq(&cache), 1_000_200);
    }

    #[test]
    fn v3_swap_sets_tick_used_by_mint() {
        let cache = cache_with_v3_pool();
        let swap = Swap {
            sender: Address::ZERO, recipient: Address::ZERO,
            amount0: alloy::primitives::I256::ZERO, amount1: alloy::primitives::I256::ZERO,
            sqrtPriceX96: alloy::primitives::aliases::U160::from(1u64) << 96,
            liquidity: 2_000_000, tick: i24(-500),
        }.encode_log_data();
        assert!(apply(&cache, &at(swap, 10, 0)).is_some());
        assert_eq!(liq(&cache), 2_000_000);
        // The old range [100, 200) no longer holds the tick; [-600, -400) does.
        apply(&cache, &mint(100, 200, 5, 10, 1));
        assert_eq!(liq(&cache), 2_000_000);
        apply(&cache, &mint(-600, -400, 5, 10, 2));
        assert_eq!(liq(&cache), 2_000_005);
    }

    #[test]
    fn logs_without_position_still_apply() {
        let cache = cache_with_pool();
        let mut log = sync_log(0, 0, 3_000, false);
        log.block_number = None;
        log.log_index = None;
        assert!(apply(&cache, &log).is_some());
        assert_eq!(reserve0(&cache), U256::from(3_000u64));
    }
}
