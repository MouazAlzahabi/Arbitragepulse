use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::abi::{PairSyncV2, PoolSwapV3};
use crate::api::{broadcast_log, LogBroadcaster};
use crate::pool_cache::PoolCache;

// ─── Swap event (generic across V2 + V3) ─────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SwapEvent {
    pub chain_id: u64,
    pub pool: Address,
    pub block_number: u64,
}

// ─── Listener ─────────────────────────────────────────────────────────────────

pub struct Listener {
    pub chain_id: u64,
    pub chain_name: String,
}

impl Listener {
    pub fn new(chain_id: u64, chain_name: String) -> Self {
        Self { chain_id, chain_name }
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

        let event_sigs = vec![
            PairSyncV2::SIGNATURE_HASH,
            PoolSwapV3::SIGNATURE_HASH,
        ];

        let sync_count = Arc::new(AtomicU64::new(0));
        let v3_count = Arc::new(AtomicU64::new(0));

        // Periodic counter log (every 60s)
        let sync_count_log = sync_count.clone();
        let v3_count_log = v3_count.clone();
        let chain_name_c = chain_name.clone();
        let log_tx_counter = log_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let sc = sync_count_log.swap(0, Ordering::Relaxed);
                let vc = v3_count_log.swap(0, Ordering::Relaxed);
                let msg = format!(
                    "[{}] Events polled: Sync={}, V3-Swap={} in last 60s",
                    chain_name_c, sc, vc
                );
                info!("{}", msg);
                broadcast_log(&log_tx_counter, "info", &msg, None);
            }
        });

        // Main polling loop
        let cache = pool_cache.clone();
        let log_tx_poll = log_tx.clone();
        tokio::spawn(async move {
            let mut last_block: u64 = 0;
            let mut poll_count: u64 = 0;
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
                    // First poll: start from current block (don't replay history)
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
                        // Log every poll to dashboard for diagnosis (temporary)
                        if !logs.is_empty() || poll_count % 15 == 0 {
                            let msg = format!(
                                "[{}] get_logs blocks {}..{} → {} events ({} addrs watched)",
                                chain_name, from, to, logs.len(), all_addrs.len()
                            );
                            info!("{}", msg);
                            broadcast_log(&log_tx_poll, "info", &msg, None);
                        }
                        for log in logs {
                            let pool = log.address();
                            let block = log.block_number.unwrap_or(current_block);
                            let topic0 = log.topics().first().copied();

                            if topic0 == Some(PairSyncV2::SIGNATURE_HASH) {
                                sync_count.fetch_add(1, Ordering::Relaxed);
                                if let Ok(decoded) = PairSyncV2::decode_log(log.as_ref()) {
                                    let r0 = U256::from(decoded.reserve0);
                                    let r1 = U256::from(decoded.reserve1);
                                    cache.update_reserves(pool, r0, r1);
                                    debug!(
                                        "[{}] Sync pool={:?} r0={} r1={} block={}",
                                        chain_name, pool, r0, r1, block
                                    );
                                }
                            } else if topic0 == Some(PoolSwapV3::SIGNATURE_HASH) {
                                v3_count.fetch_add(1, Ordering::Relaxed);
                                if let Ok(decoded) = PoolSwapV3::decode_log(log.as_ref()) {
                                    let sqrtp = U256::from(decoded.sqrtPriceX96);
                                    let liq: u128 = decoded.liquidity.into();
                                    cache.update_v3_state(pool, sqrtp, liq);
                                    debug!(
                                        "[{}] V3 state pool={:?} sqrtp={} liq={} block={}",
                                        chain_name, pool, sqrtp, liq, block
                                    );
                                }
                            }

                            let _ = tx.send(SwapEvent { chain_id, pool, block_number: block }).await;
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
                poll_count += 1;

                tokio::time::sleep(poll_interval).await;
            }
        });

        Ok(())
    }
}
