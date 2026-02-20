use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Result;
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::abi::{PairSwapV2, PoolSwapV3};
use crate::config::PairConfig;

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
}

impl Listener {
    pub fn new(chain_id: u64, chain_name: String, pairs: Vec<PairConfig>) -> Self {
        Self {
            chain_id,
            chain_name,
            pairs,
        }
    }

    /// Subscribe to swap events for all watched pools.
    /// Sends a `SwapEvent` on each detected swap.
    /// If no watch_pools are configured, returns immediately (periodic strategy polling is used instead).
    pub async fn subscribe<P: Provider + Clone + 'static>(
        &self,
        provider: P,
        tx: mpsc::Sender<SwapEvent>,
    ) -> Result<()> {
        let watch_pools: Vec<Address> = self
            .pairs
            .iter()
            .filter(|p| p.chain_id == self.chain_id)
            .flat_map(|p| p.watch_pools.iter())
            .filter_map(|addr| addr.parse::<Address>().ok())
            .collect();

        if watch_pools.is_empty() {
            debug!(
                "[{}] No watch_pools configured — using periodic polling",
                self.chain_name
            );
            return Ok(());
        }

        // Subscribe to V2 Swap events
        let v2_filter = Filter::new()
            .address(watch_pools.clone())
            .event_signature(PairSwapV2::SIGNATURE_HASH);

        // Subscribe to V3 Swap events
        let v3_filter = Filter::new()
            .address(watch_pools.clone())
            .event_signature(PoolSwapV3::SIGNATURE_HASH);

        let chain_id = self.chain_id;
        let chain_name = self.chain_name.clone();
        let tx2 = tx.clone();

        // V2 subscription with reconnect
        let provider2 = provider.clone();
        let chain_name2 = chain_name.clone();
        let filter2 = v2_filter.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match provider2.subscribe_logs(&filter2).await {
                    Ok(sub) => {
                        backoff = Duration::from_secs(1); // reset on connect
                        let mut stream = sub.into_stream();
                        while let Some(log) = stream.next().await {
                            let pool = log.address();
                            let block = log.block_number.unwrap_or(0);
                            debug!("[{}] V2 Swap on {:?} block={}", chain_name2, pool, block);
                            let _ = tx2.send(SwapEvent { chain_id, pool, block_number: block }).await;
                        }
                        warn!("[{}] V2 subscription stream ended, reconnecting in {:?}", chain_name2, backoff);
                    }
                    Err(e) => {
                        error!("[{}] V2 subscription error: {} — retrying in {:?}", chain_name2, e, backoff);
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });

        // V3 subscription with reconnect
        let provider3 = provider.clone();
        let chain_name3 = chain_name.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match provider3.subscribe_logs(&v3_filter).await {
                    Ok(sub) => {
                        backoff = Duration::from_secs(1);
                        let mut stream = sub.into_stream();
                        while let Some(log) = stream.next().await {
                            let pool = log.address();
                            let block = log.block_number.unwrap_or(0);
                            debug!("[{}] V3 Swap on {:?} block={}", chain_name3, pool, block);
                            let _ = tx.send(SwapEvent { chain_id, pool, block_number: block }).await;
                        }
                        warn!("[{}] V3 subscription stream ended, reconnecting in {:?}", chain_name3, backoff);
                    }
                    Err(e) => {
                        error!("[{}] V3 subscription error: {} — retrying in {:?}", chain_name3, e, backoff);
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });

        Ok(())
    }
}
