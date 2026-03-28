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

use crate::abi::{PancakeV3Swap, Swap, Sync};
use crate::api::{broadcast_log, LogBroadcaster};
use crate::pool_cache::PoolCache;

/// Decode a single Swap/Sync log and apply it to the pool cache.
/// Returns `true` if the log was recognized and applied.
/// Idempotent — calling twice with the same log is safe (just overwrites with same values).
/// Used by both the polling listener and the real-time log subscription in chain/mod.rs.
pub fn apply_log_to_cache(
    log: &alloy::rpc::types::Log,
    cache: &PoolCache,
    solidly_sync_hash: B256,
) -> bool {
    let pool = log.address();
    let topic0 = log.topics().first().copied();

    if topic0 == Some(Sync::SIGNATURE_HASH) {
        if let Ok(decoded) = Sync::decode_log(log.as_ref()) {
            cache.update_reserves(pool, U256::from(decoded.reserve0), U256::from(decoded.reserve1));
            return true;
        }
    } else if topic0 == Some(solidly_sync_hash) {
        let raw_data = log.data().data.as_ref();
        if raw_data.len() >= 64 {
            let mut r0_bytes = [0u8; 32];
            let mut r1_bytes = [0u8; 32];
            r0_bytes.copy_from_slice(&raw_data[0..32]);
            r1_bytes.copy_from_slice(&raw_data[32..64]);
            cache.update_reserves(pool, U256::from_be_bytes(r0_bytes), U256::from_be_bytes(r1_bytes));
            return true;
        }
    } else if topic0 == Some(Swap::SIGNATURE_HASH) {
        if let Ok(decoded) = Swap::decode_log(log.as_ref()) {
            cache.update_v3_state(pool, U256::from(decoded.sqrtPriceX96), decoded.liquidity.into());
            return true;
        }
    } else if topic0 == Some(PancakeV3Swap::SIGNATURE_HASH) {
        if let Ok(decoded) = PancakeV3Swap::decode_log(log.as_ref()) {
            cache.update_v3_state(pool, U256::from(decoded.sqrtPriceX96), decoded.liquidity.into());
            return true;
        }
    }
    false
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
    /// Many providers (Alchemy on Linea) silently drop `eth_subscribe("logs")`
    /// while `eth_subscribe("newHeads")` works fine. Polling `get_logs` per block
    /// is universally supported and costs 1 RPC call per poll interval.
    ///
    /// **V2/Solidly pools**: Polls `Sync(reserve0, reserve1)` events.
    /// **V3 pools**: Polls `Swap(sqrtPriceX96, liquidity, ...)` events.
    pub async fn subscribe<P: Provider + Clone + 'static>(
        &self,
        provider: P,
        tx: mpsc::Sender<SwapEvent>,
        pool_cache: Arc<PoolCache>,
        log_tx: LogBroadcaster,
    ) -> Result<()> {
        let chain_id = self.chain_id;
        let chain_name = self.chain_name.clone();

        let v2_pool_addrs = pool_cache.pool_addresses();
        let v3_pool_addrs = pool_cache.v3_pool_addresses();

        let msg = format!(
            "[{}] Listener: V2 pools={}, V3 pools={} (polling mode)",
            chain_name, v2_pool_addrs.len(), v3_pool_addrs.len()
        );
        info!("{}", msg);
        broadcast_log(&log_tx, "info", &msg, None);

        if v2_pool_addrs.is_empty() && v3_pool_addrs.is_empty() {
            let msg = format!("[{}] No pools in cache — event polling skipped", chain_name);
            warn!("{}", msg);
            broadcast_log(&log_tx, "warn", &msg, None);
            return Ok(());
        }

        // Build combined filter for both Sync + Swap events in a single get_logs call.
        let mut all_addrs = v2_pool_addrs.clone();
        all_addrs.extend_from_slice(&v3_pool_addrs);

        // Solidly/Aerodrome pools emit Sync(uint256,uint256) — different topic hash
        // from Uniswap V2's Sync(uint112,uint112). Both must be in the filter.
        use alloy::primitives::keccak256;
        let solidly_sync_hash = keccak256(b"Sync(uint256,uint256)");

        let event_sigs = vec![
            Sync::SIGNATURE_HASH,           // Uniswap V2: Sync(uint112,uint112)
            solidly_sync_hash,              // Aerodrome/Solidly: Sync(uint256,uint256)
            Swap::SIGNATURE_HASH,           // Uniswap V3: Swap(7 fields)
            PancakeV3Swap::SIGNATURE_HASH,  // PancakeSwap V3: Swap(9 fields, with protocol fees)
        ];

        let sync_count = Arc::new(AtomicU64::new(0));
        let v3_count = Arc::new(AtomicU64::new(0));

        // Periodic counter log: every 60s if events > 0, every 5 minutes if idle.
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
                    // Log every 5 minutes (5 ticks * 60s) when idle
                    if idle_ticks % 5 == 0 {
                        info!("[{}] No events in last {}s", chain_name_c, idle_ticks * 60);
                    }
                }
            }
        });

        // Main polling loop
        let cache = pool_cache.clone();
        let log_tx_poll = log_tx.clone();
        let large_swap_bps = self.large_swap_bps;
        let large_v3_bps = self.large_v3_bps;
        tokio::spawn(async move {
            let mut last_block: u64 = 0;
            let poll_interval = Duration::from_secs(2); // Linea ~2s block time

            loop {
                // Get current block number
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
                    // No new blocks yet
                    tokio::time::sleep(poll_interval).await;
                    continue;
                }

                // Cap range to 5 blocks (Alchemy free tier allows max 10).
                // If we've fallen behind, skip to recent blocks — stale events
                // aren't useful; we just need fresh pool state.
                const MAX_RANGE: u64 = 5;
                let from = if current_block - last_block > MAX_RANGE {
                    current_block - MAX_RANGE + 1
                } else {
                    last_block + 1
                };
                let to = current_block;

                let filter = Filter::new()
                    .address(all_addrs.clone())
                    .event_signature(event_sigs.clone())
                    .from_block(from)
                    .to_block(to);

                match provider.get_logs(&filter).await {
                    Ok(logs) => {
                        for log in logs {
                            let pool = log.address();
                            let block = log.block_number.unwrap_or(current_block);
                            let topic0 = log.topics().first().copied();

                            let mut magnitude = SwapMagnitude::Normal;

                            if topic0 == Some(Sync::SIGNATURE_HASH) {
                                sync_count.fetch_add(1, Ordering::Relaxed);
                                if let Ok(decoded) = Sync::decode_log(log.as_ref()) {
                                    let r0 = U256::from(decoded.reserve0);
                                    let r1 = U256::from(decoded.reserve1);

                                    // Detect large reserve shift BEFORE updating cache
                                    if large_swap_bps > 0 {
                                        if let Some(old) = cache.by_address.get(&pool) {
                                            let threshold = U256::from(large_swap_bps);
                                            let bps_10k = U256::from(10_000u32);
                                            // Check reserve0 delta
                                            if !old.reserve0.is_zero() {
                                                let delta0 = if r0 > old.reserve0 { r0 - old.reserve0 } else { old.reserve0 - r0 };
                                                if delta0 * bps_10k >= old.reserve0 * threshold {
                                                    magnitude = SwapMagnitude::Large;
                                                }
                                            }
                                            // Check reserve1 delta (take the max)
                                            if magnitude == SwapMagnitude::Normal && !old.reserve1.is_zero() {
                                                let delta1 = if r1 > old.reserve1 { r1 - old.reserve1 } else { old.reserve1 - r1 };
                                                if delta1 * bps_10k >= old.reserve1 * threshold {
                                                    magnitude = SwapMagnitude::Large;
                                                }
                                            }
                                        }
                                    }

                                    cache.update_reserves(pool, r0, r1);
                                    if magnitude == SwapMagnitude::Large {
                                        info!(
                                            "[{}] LARGE Sync pool={:?} r0={} r1={} block={}",
                                            chain_name, pool, r0, r1, block
                                        );
                                    } else {
                                        debug!(
                                            "[{}] Sync pool={:?} r0={} r1={} block={}",
                                            chain_name, pool, r0, r1, block
                                        );
                                    }
                                }
                            } else if topic0 == Some(solidly_sync_hash) {
                                // Aerodrome/Solidly pools emit Sync(uint256,uint256).
                                // Decode manually: 2×uint256 packed in log data (no indexed params).
                                sync_count.fetch_add(1, Ordering::Relaxed);
                                let raw_data = log.data().data.as_ref();
                                if raw_data.len() >= 64 {
                                    let mut r0_bytes = [0u8; 32];
                                    let mut r1_bytes = [0u8; 32];
                                    r0_bytes.copy_from_slice(&raw_data[0..32]);
                                    r1_bytes.copy_from_slice(&raw_data[32..64]);
                                    let r0 = U256::from_be_bytes(r0_bytes);
                                    let r1 = U256::from_be_bytes(r1_bytes);

                                    if large_swap_bps > 0 {
                                        if let Some(old) = cache.by_address.get(&pool) {
                                            let threshold = U256::from(large_swap_bps);
                                            let bps_10k = U256::from(10_000u32);
                                            if !old.reserve0.is_zero() {
                                                let delta0 = if r0 > old.reserve0 { r0 - old.reserve0 } else { old.reserve0 - r0 };
                                                if delta0 * bps_10k >= old.reserve0 * threshold {
                                                    magnitude = SwapMagnitude::Large;
                                                }
                                            }
                                            if magnitude == SwapMagnitude::Normal && !old.reserve1.is_zero() {
                                                let delta1 = if r1 > old.reserve1 { r1 - old.reserve1 } else { old.reserve1 - r1 };
                                                if delta1 * bps_10k >= old.reserve1 * threshold {
                                                    magnitude = SwapMagnitude::Large;
                                                }
                                            }
                                        }
                                    }

                                    cache.update_reserves(pool, r0, r1);
                                    if magnitude == SwapMagnitude::Large {
                                        info!(
                                            "[{}] LARGE Solidly Sync pool={:?} r0={} r1={} block={}",
                                            chain_name, pool, r0, r1, block
                                        );
                                    } else {
                                        debug!(
                                            "[{}] Solidly Sync pool={:?} r0={} r1={} block={}",
                                            chain_name, pool, r0, r1, block
                                        );
                                    }
                                }
                            } else if topic0 == Some(Swap::SIGNATURE_HASH) || topic0 == Some(PancakeV3Swap::SIGNATURE_HASH) {
                                v3_count.fetch_add(1, Ordering::Relaxed);
                                // Decode sqrtPriceX96 and liquidity from whichever V3 Swap variant fired.
                                let v3_decoded = if topic0 == Some(Swap::SIGNATURE_HASH) {
                                    Swap::decode_log(log.as_ref())
                                        .ok()
                                        .map(|d| (U256::from(d.sqrtPriceX96), d.liquidity.into()))
                                } else {
                                    PancakeV3Swap::decode_log(log.as_ref())
                                        .ok()
                                        .map(|d| (U256::from(d.sqrtPriceX96), d.liquidity.into()))
                                };
                                if let Some((sqrtp, liq)) = v3_decoded {
                                    let sqrtp: U256 = sqrtp;
                                    let liq: u128 = liq;

                                    // Detect large sqrtPriceX96 shift BEFORE updating cache
                                    if large_v3_bps > 0 {
                                        if let Some(old) = cache.v3_by_address.get(&pool) {
                                            let old_sqrtp = old.sqrt_price_x96;
                                            if !old_sqrtp.is_zero() {
                                                let threshold = U256::from(large_v3_bps);
                                                let bps_10k = U256::from(10_000u32);
                                                let delta = if sqrtp > old_sqrtp { sqrtp - old_sqrtp } else { old_sqrtp - sqrtp };
                                                if delta * bps_10k >= old_sqrtp * threshold {
                                                    magnitude = SwapMagnitude::Large;
                                                }
                                            }
                                        }
                                    }

                                    cache.update_v3_state(pool, sqrtp, liq);
                                    if magnitude == SwapMagnitude::Large {
                                        info!(
                                            "[{}] LARGE V3 swap pool={:?} sqrtp={} liq={} block={}",
                                            chain_name, pool, sqrtp, liq, block
                                        );
                                    } else {
                                        debug!(
                                            "[{}] V3 state pool={:?} sqrtp={} liq={} block={}",
                                            chain_name, pool, sqrtp, liq, block
                                        );
                                    }
                                }
                            }

                            let _ = tx.send(SwapEvent { chain_id, pool, block_number: block, magnitude }).await;
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
                        // On error, skip ahead to avoid stuck range growing
                        last_block = current_block;
                    }
                }

                tokio::time::sleep(poll_interval).await;
            }
        });

        Ok(())
    }
}
