use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Result;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use std::sync::atomic::{AtomicU64, Ordering};
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

    /// Subscribe to on-chain events for all discovered pools.
    ///
    /// **V2/Solidly pools** (pool_cache.by_address — seeded by discover_pools at startup):
    ///   — Subscribes to `Sync(reserve0, reserve1)` events.
    ///   — Updates local reserves immediately (zero RPC cost).
    ///   — Sends SwapEvent to trigger opportunity evaluation.
    ///
    /// **V3 pools** (pool_cache.v3_by_address — seeded by discover_v3_pools at startup):
    ///   — Subscribes to `Swap(sqrtPriceX96, liquidity, ...)` events.
    ///   — Updates sqrtPriceX96 + liquidity in cache immediately (zero RPC cost).
    ///   — Sends SwapEvent to trigger opportunity evaluation.
    ///
    /// If no pools are in the cache, returns immediately (periodic polling continues).
    pub async fn subscribe<P: Provider + Clone + 'static>(
        &self,
        provider: P,
        tx: mpsc::Sender<SwapEvent>,
        pool_cache: Arc<PoolCache>,
        log_tx: LogBroadcaster,
    ) -> Result<()> {
        let chain_id = self.chain_id;
        let chain_name = self.chain_name.clone();

        // ── V2/Solidly pools: subscribe to Sync events ────────────────────────
        // These are the pools in pool_cache (discovered at startup).
        let v2_pool_addrs = pool_cache.pool_addresses();
        let v3_count_at_entry = pool_cache.v3_pool_addresses().len();
        let msg = format!(
            "[{}] Listener: V2 pools={}, V3 pools={}",
            chain_name, v2_pool_addrs.len(), v3_count_at_entry
        );
        info!("{}", msg);
        broadcast_log(&log_tx, "info", &msg, None);

        if !v2_pool_addrs.is_empty() {
            let sync_filter = Filter::new()
                .address(v2_pool_addrs.clone())
                .event_signature(PairSyncV2::SIGNATURE_HASH);

            let provider_s = provider.clone();
            let chain_name_s = chain_name.clone();
            let tx_s = tx.clone();
            let cache = pool_cache.clone();

            let sync_count = Arc::new(AtomicU64::new(0));
            let sync_count_log = sync_count.clone();
            let chain_name_sc = chain_name.clone();
            let log_tx_sync = log_tx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    let c = sync_count_log.swap(0, Ordering::Relaxed);
                    let msg = format!("[{}] Sync events: {} in last 60s", chain_name_sc, c);
                    info!("{}", msg);
                    broadcast_log(&log_tx_sync, "info", &msg, None);
                }
            });
            tokio::spawn(async move {
                let mut backoff = Duration::from_secs(1);
                loop {
                    match provider_s.subscribe_logs(&sync_filter).await {
                        Ok(sub) => {
                            backoff = Duration::from_secs(1);
                            let mut stream = sub.into_stream();
                            while let Some(log) = stream.next().await {
                                sync_count.fetch_add(1, Ordering::Relaxed);
                                let pool = log.address();
                                let block = log.block_number.unwrap_or(0);

                                if let Ok(decoded) = PairSyncV2::decode_log(log.as_ref()) {
                                    // Update local reserves immediately — zero RPC cost.
                                    let r0 = U256::from(decoded.reserve0);
                                    let r1 = U256::from(decoded.reserve1);
                                    cache.update_reserves(pool, r0, r1);
                                    debug!(
                                        "[{}] Sync pool={:?} r0={} r1={} block={}",
                                        chain_name_s, pool, r0, r1, block
                                    );
                                }

                                // Always send SwapEvent to trigger arb evaluation
                                let _ = tx_s.send(SwapEvent { chain_id, pool, block_number: block }).await;
                            }
                            warn!("[{}] Sync subscription stream ended, reconnecting in {:?}", chain_name_s, backoff);
                        }
                        Err(e) => {
                            error!("[{}] Sync subscription error: {} — retrying in {:?}", chain_name_s, e, backoff);
                        }
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            });
        }

        // ── V3 pools: subscribe to Swap events and update local state cache ──────
        // Pool addresses come from pool_cache.v3_by_address (populated by discover_v3_pools
        // at startup). No manual watch_pools config needed — auto-discovered.
        //
        // On each Swap event:
        //   1. Update sqrtPriceX96 + liquidity in pool_cache (keeps state fresh, zero RPC)
        //   2. Send SwapEvent to trigger evaluate_and_execute()
        let v3_pool_addrs = pool_cache.v3_pool_addresses();
        let v3_pool_addrs_empty = v3_pool_addrs.is_empty();

        if !v3_pool_addrs_empty {
            let v3_filter = Filter::new()
                .address(v3_pool_addrs)
                .event_signature(PoolSwapV3::SIGNATURE_HASH);

            let provider_v3 = provider.clone();
            let chain_name_v3 = chain_name.clone();
            let tx_v3 = tx.clone();
            let cache_v3 = pool_cache.clone();

            let v3_count = Arc::new(AtomicU64::new(0));
            let v3_count_log = v3_count.clone();
            let chain_name_v3c = chain_name.clone();
            let log_tx_v3 = log_tx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    let c = v3_count_log.swap(0, Ordering::Relaxed);
                    let msg = format!("[{}] V3-Swap events: {} in last 60s", chain_name_v3c, c);
                    info!("{}", msg);
                    broadcast_log(&log_tx_v3, "info", &msg, None);
                }
            });
            tokio::spawn(async move {
                let mut backoff = Duration::from_secs(1);
                loop {
                    match provider_v3.subscribe_logs(&v3_filter).await {
                        Ok(sub) => {
                            backoff = Duration::from_secs(1);
                            let mut stream = sub.into_stream();
                            while let Some(log) = stream.next().await {
                                v3_count.fetch_add(1, Ordering::Relaxed);
                                let pool = log.address();
                                let block = log.block_number.unwrap_or(0);

                                if let Ok(decoded) = PoolSwapV3::decode_log(log.as_ref()) {
                                    // Update both sqrtPriceX96 and liquidity atomically.
                                    // The V3 Swap event always emits the post-swap values for both.
                                    let sqrtp = alloy::primitives::U256::from(decoded.sqrtPriceX96);
                                    let liq: u128 = decoded.liquidity.into();
                                    cache_v3.update_v3_state(pool, sqrtp, liq);
                                    debug!(
                                        "[{}] V3 state updated pool={:?} sqrtp={} liq={} block={}",
                                        chain_name_v3, pool, sqrtp, liq, block
                                    );
                                }

                                // Trigger opportunity evaluation regardless of decode result
                                let _ = tx_v3.send(SwapEvent { chain_id, pool, block_number: block }).await;
                            }
                            warn!("[{}] V3-Swap subscription ended, reconnecting in {:?}", chain_name_v3, backoff);
                        }
                        Err(e) => {
                            error!("[{}] V3-Swap subscription error: {} — retrying in {:?}", chain_name_v3, e, backoff);
                        }
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            });
        }

        if v3_pool_addrs_empty && v2_pool_addrs.is_empty() {
            let msg = format!("[{}] No pools in cache — event subscriptions skipped", chain_name);
            warn!("{}", msg);
            broadcast_log(&log_tx, "warn", &msg, None);
        }

        Ok(())
    }
}
