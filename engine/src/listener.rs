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

use crate::abi::{IPancakeV3Pool, Swap, Sync};
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

/// Decode a single Swap/Sync log, apply it to the pool cache, and classify magnitude.
/// Returns `Some(magnitude)` when the log was recognized and applied.
/// Idempotent — calling twice with the same log is safe.
pub fn apply_log_to_cache(
    log: &alloy::rpc::types::Log,
    cache: &PoolCache,
    solidly_sync_hash: B256,
    large_swap_bps: u32,
    large_v3_bps: u32,
) -> Option<SwapMagnitude> {
    let pool = log.address();
    let topic0 = log.topics().first().copied()?;
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
        return Some(magnitude);
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
        return Some(magnitude);
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
        cache.update_v3_state(pool, sqrtp, liq);
        return Some(magnitude);
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
        cache.update_v3_state(pool, sqrtp, liq);
        return Some(magnitude);
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

        let event_sigs = vec![
            Sync::SIGNATURE_HASH,
            solidly_sync_hash,
            Swap::SIGNATURE_HASH,
            IPancakeV3Pool::Swap::SIGNATURE_HASH,
        ];

        let sync_count = Arc::new(AtomicU64::new(0));
        let v3_count = Arc::new(AtomicU64::new(0));

        let sync_count_log = sync_count.clone();
        let v3_count_log = v3_count.clone();
        let chain_name_c = chain_name.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
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

        if !poll_fallback {
            return Ok(());
        }

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
