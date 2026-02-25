use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Result;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::abi::{PairSwapV2, PairSyncV2, PoolSwapV3};
use crate::config::PairConfig;
use crate::pool_cache::PoolCache;

// ─── Swap event (generic across V2 + V3) ─────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SwapEvent {
    pub chain_id: u64,
    pub pool: Address,
    pub block_number: u64,
}

// ─── Listener ─────────────────────────────────────────────────────────────────

pub struct Listener {
    pub chain_id: u64,
    pub chain_name: String,
    pub pairs: Vec<PairConfig>,
    pub min_swap_amount: u128,
}

impl Listener {
    pub fn new(chain_id: u64, chain_name: String, pairs: Vec<PairConfig>, min_swap_amount: u128) -> Self {
        Self { chain_id, chain_name, pairs, min_swap_amount }
    }

    /// Subscribe to events for all watched pools.
    ///
    /// For V2/Solidly pools (in `pool_cache`):
    ///   — Subscribes to `Sync(reserve0, reserve1)` and updates local reserves directly.
    ///   — Sends a `SwapEvent` to trigger opportunity evaluation.
    ///
    /// For V3 pools (in `watch_pools` config, not in pool_cache):
    ///   — Subscribes to `Swap` events and sends a `SwapEvent`.
    ///
    /// If no pools are configured, returns immediately (periodic polling is used instead).
    pub async fn subscribe<P: Provider + Clone + 'static>(
        &self,
        provider: P,
        tx: mpsc::Sender<SwapEvent>,
        pool_cache: Arc<PoolCache>,
    ) -> Result<()> {
        let chain_id = self.chain_id;
        let chain_name = self.chain_name.clone();

        // ── V2/Solidly pools: subscribe to Sync events ────────────────────────
        // These are the pools in pool_cache (discovered at startup).
        let v2_pool_addrs = pool_cache.pool_addresses();

        if !v2_pool_addrs.is_empty() {
            let sync_filter = Filter::new()
                .address(v2_pool_addrs.clone())
                .event_signature(PairSyncV2::SIGNATURE_HASH);

            let provider_s = provider.clone();
            let chain_name_s = chain_name.clone();
            let tx_s = tx.clone();
            let cache = pool_cache.clone();

            tokio::spawn(async move {
                let mut backoff = Duration::from_secs(1);
                loop {
                    match provider_s.subscribe_logs(&sync_filter).await {
                        Ok(sub) => {
                            backoff = Duration::from_secs(1);
                            let mut stream = sub.into_stream();
                            while let Some(log) = stream.next().await {
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

        // ── V3 / other pools: subscribe to Swap events ────────────────────────
        // These are watch_pools from config that are NOT in pool_cache
        // (V3 concentrated liquidity pools, SyncSwap pools, etc.).
        let v3_watch_pools: Vec<Address> = self
            .pairs
            .iter()
            .filter(|p| p.chain_id == chain_id)
            .flat_map(|p| p.watch_pools.iter())
            .filter_map(|addr| addr.parse::<Address>().ok())
            .filter(|addr| !pool_cache.by_address.contains_key(addr)) // only non-V2 pools
            .collect();

        if v3_watch_pools.is_empty() && v2_pool_addrs.is_empty() {
            debug!("[{}] No pools configured — using periodic polling", chain_name);
            return Ok(());
        }

        if !v3_watch_pools.is_empty() {
            let v2_filter = Filter::new()
                .address(v3_watch_pools.clone())
                .event_signature(PairSwapV2::SIGNATURE_HASH);
            let v3_filter = Filter::new()
                .address(v3_watch_pools)
                .event_signature(PoolSwapV3::SIGNATURE_HASH);

            let min_amount = self.min_swap_amount;

            // V2 Swap events on non-cache pools
            {
                let provider2 = provider.clone();
                let chain_name2 = chain_name.clone();
                let tx2 = tx.clone();
                let f2 = v2_filter;
                tokio::spawn(async move {
                    let mut backoff = Duration::from_secs(1);
                    loop {
                        match provider2.subscribe_logs(&f2).await {
                            Ok(sub) => {
                                backoff = Duration::from_secs(1);
                                let mut stream = sub.into_stream();
                                while let Some(log) = stream.next().await {
                                    let pool = log.address();
                                    let block = log.block_number.unwrap_or(0);
                                    if let Ok(decoded) = PairSwapV2::decode_log(log.as_ref()) {
                                        let max_amount = decoded.amount0In.max(decoded.amount1In)
                                            .max(decoded.amount0Out).max(decoded.amount1Out);
                                        if max_amount < alloy::primitives::U256::from(min_amount) {
                                            continue;
                                        }
                                    }
                                    let _ = tx2.send(SwapEvent { chain_id, pool, block_number: block }).await;
                                }
                                warn!("[{}] V2-Swap subscription ended, reconnecting in {:?}", chain_name2, backoff);
                            }
                            Err(e) => error!("[{}] V2-Swap error: {}", chain_name2, e),
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                });
            }

            // V3 Swap events
            {
                let provider3 = provider.clone();
                let chain_name3 = chain_name.clone();
                let tx3 = tx.clone();
                let f3 = v3_filter;
                tokio::spawn(async move {
                    let mut backoff = Duration::from_secs(1);
                    loop {
                        match provider3.subscribe_logs(&f3).await {
                            Ok(sub) => {
                                backoff = Duration::from_secs(1);
                                let mut stream = sub.into_stream();
                                while let Some(log) = stream.next().await {
                                    let pool = log.address();
                                    let block = log.block_number.unwrap_or(0);
                                    if let Ok(decoded) = PoolSwapV3::decode_log(log.as_ref()) {
                                        let abs0 = if decoded.amount0 < alloy::primitives::I256::ZERO {
                                            decoded.amount0.wrapping_neg().into_raw()
                                        } else { decoded.amount0.into_raw() };
                                        let abs1 = if decoded.amount1 < alloy::primitives::I256::ZERO {
                                            decoded.amount1.wrapping_neg().into_raw()
                                        } else { decoded.amount1.into_raw() };
                                        if abs0.max(abs1) < alloy::primitives::U256::from(min_amount) {
                                            continue;
                                        }
                                    }
                                    let _ = tx3.send(SwapEvent { chain_id, pool, block_number: block }).await;
                                }
                                warn!("[{}] V3-Swap subscription ended, reconnecting in {:?}", chain_name3, backoff);
                            }
                            Err(e) => error!("[{}] V3-Swap error: {}", chain_name3, e),
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                });
            }
        }

        Ok(())
    }
}
