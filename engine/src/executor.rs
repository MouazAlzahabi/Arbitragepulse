use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Uint, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn, debug};

use crate::abi::{ArbitrageExecutor, IERC20};
use crate::db::Database;
use crate::strategy::{ArbOpportunity, TriangularOpportunity};

// Hardcoded gas limit — 500k covers any 2-hop arb (V2+V2, V2+V3, V3+V3).
// We skip eth_estimateGas to save one RPC round-trip per execution.
const GAS_LIMIT: u64 = 500_000;

// Triangular arb gas limit — 900k covers 3-hop paths (V2+V2+V2, V3+V3+V3, mixed).
const GAS_LIMIT_TRIANGULAR: u64 = 900_000;

// ─── TxPrep ───────────────────────────────────────────────────────────────────

/// Prepared transaction data returned by `prepare_2hop()` / `prepare_triangular()`
/// before the executor mutex is released.
///
/// The caller pattern:
/// 1. Acquire executor lock → call `prepare_*()` → release lock.
/// 2. Call `provider.send_transaction(prep.tx)` with NO lock held.
/// 3. Acquire executor lock briefly → call `record_sent()` or `record_failed()`.
/// 4. Spawn receipt task using the cloned `prep` fields.
pub struct TxPrep {
    /// Ready-to-send transaction (nonce already pre-allocated and incremented).
    pub tx: TransactionRequest,
    /// Pre-allocated nonce. If `record_failed()` is called, executor resets its
    /// nonce counter to `None` so it re-fetches from chain.
    pub nonce: u64,
    pub net_profit_usd: f64,
    pub gas_cost_usd: f64,
    // ── Fields cloned for the receipt background task ──
    pub chain_name: String,
    pub chain_id: u64,
    pub db: Option<Arc<Database>>,
    /// pair_id for 2-hop, triplet_id for triangular.
    pub opp_id: String,
    pub router_a: String,
    pub router_b: String,
    pub router_c: Option<String>,
    pub profit_usd: f64,
    pub dry_run: bool,
    pub confirmed_success: Arc<AtomicU64>,
    pub confirmed_failed: Arc<AtomicU64>,
    pub confirmed_profit_bits: Arc<AtomicU64>,
    pub contract_addr: Address,
    pub contract_balances: Option<Arc<RwLock<HashMap<Address, U256>>>>,
}

// ─── Stats ────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct ExecutorStats {
    pub total_attempts: u64,
    /// Txs sent successfully (NOT yet confirmed — confirmed count is tracked via atomics).
    pub total_sent: u64,
    pub total_failed: u64,
    pub total_simulated: u64,
    pub last_tx_hash: Option<String>,
    pub last_execution_ms: Option<u64>,
}

// ─── Executor ─────────────────────────────────────────────────────────────────

pub struct Executor {
    pub chain_id: u64,
    pub chain_name: String,
    pub contract_address: Address,
    /// Address of the signing wallet — used for nonce tracking.
    pub signer_address: Address,
    pub dry_run: bool,
    pub paused: bool,
    pub stats: ExecutorStats,
    /// ETH/USD price — updated from the price-tick handler each minute.
    pub native_price_usd: f64,
    /// Minimum NET profit (after gas) to execute. Set from chain config.
    pub min_profit_usd: f64,
    /// Tracked nonce. None = fetch from chain on first send. Incremented after
    /// each successful send_transaction so the mutex is released before the
    /// receipt arrives.
    nonce: Option<u64>,
    db: Option<Arc<Database>>,
    /// Gas price cache: (price_wei, timestamp). TTL = 2× block_time.
    gas_price_cache: Option<(u128, Instant)>,
    /// Gas price cache TTL (2× block_time for the chain).
    gas_price_cache_ttl: Duration,
    /// Atomics updated from receipt background tasks — safe to clone into spawns.
    pub confirmed_success: Arc<AtomicU64>,
    pub confirmed_failed: Arc<AtomicU64>,
    /// Confirmed profit in USD (stored as f64 bits in AtomicU64 for lock-free access).
    pub confirmed_profit_usd_bits: Arc<AtomicU64>,
    /// Sum of gross profit_usd from opportunities rejected by the "negative after gas" gate.
    /// Represents money left on the table due to gas cost outweighing gross profit.
    pub ghost_profit_usd_bits: Arc<AtomicU64>,
    /// Shared contract balance cache. Set by chain.rs after construction.
    /// After a confirmed trade, the receipt spawn refreshes this cache so
    /// the next optimize() call sees the updated balance without waiting for
    /// the 30s periodic timer.
    pub contract_balances: Option<Arc<RwLock<HashMap<Address, U256>>>>,
    /// Wallet used to sign transactions for manual broadcast to submission_rpcs.
    wallet: EthereumWallet,
    /// Extra HTTP RPC endpoints to broadcast signed txs to simultaneously.
    /// The same signed raw tx bytes are sent to all endpoints concurrently.
    /// Leave empty to broadcast to the primary provider only.
    submission_rpcs: Vec<url::Url>,
}

impl Executor {
    pub fn new(
        chain_id: u64,
        chain_name: String,
        contract_address: Address,
        signer_address: Address,
        wallet: EthereumWallet,
        submission_rpcs: Vec<url::Url>,
        min_profit_usd: f64,
        block_time_ms: u64,
        db: Option<Arc<Database>>,
    ) -> Self {
        // Gas price cache TTL = 2× block time (cache valid for 2 blocks)
        let gas_price_cache_ttl = Duration::from_millis(block_time_ms * 2);

        Self {
            chain_id,
            chain_name,
            contract_address,
            signer_address,
            dry_run: true,
            paused: false,
            stats: ExecutorStats::default(),
            native_price_usd: 2500.0,
            min_profit_usd,
            nonce: None,
            db,
            gas_price_cache: None,
            gas_price_cache_ttl,
            confirmed_success: Arc::new(AtomicU64::new(0)),
            confirmed_failed: Arc::new(AtomicU64::new(0)),
            confirmed_profit_usd_bits: Arc::new(AtomicU64::new(0u64)),
            ghost_profit_usd_bits: Arc::new(AtomicU64::new(0u64)),
            contract_balances: None,
            wallet,
            submission_rpcs,
        }
    }

    /// Called from the price-tick handler so gas cost estimates stay current.
    pub fn update_native_price(&mut self, price: f64) {
        self.native_price_usd = price;
    }

    /// Get gas price with caching. Checks cache first; if expired, fetches from chain.
    /// Called by chain.rs on each new block header to pre-populate the gas price
    /// cache from the block's baseFeePerGas. Eliminates `eth_gasPrice` RPC calls
    /// since the cache is refreshed before every scan.
    pub fn update_gas_price(&mut self, base_fee_wei: u128) {
        self.gas_price_cache = Some((base_fee_wei, Instant::now()));
    }

    async fn get_gas_price<P: Provider>(&mut self, provider: &P) -> u128 {
        // Check cache
        if let Some((cached_price, cached_at)) = self.gas_price_cache {
            if cached_at.elapsed() < self.gas_price_cache_ttl {
                debug!(
                    "[{}] Gas price from cache: {} wei (age: {:?})",
                    self.chain_name,
                    cached_price,
                    cached_at.elapsed()
                );
                return cached_price;
            }
        }

        // Cache miss or expired — fetch from chain
        let gas_price = provider.get_gas_price().await.unwrap_or(1_000_000_000u128);
        self.gas_price_cache = Some((gas_price, Instant::now()));
        debug!(
            "[{}] Gas price fetched: {} wei (cached for {:?})",
            self.chain_name, gas_price, self.gas_price_cache_ttl
        );
        gas_price
    }

    // ── Split-lock execution helpers ───────────────────────────────────────────
    // These three methods implement the "prepare / send / record" pattern that
    // releases the executor mutex before the slow send_transaction network call.

    /// Phase 1 of the split-lock pattern for 2-hop arbs.
    ///
    /// Call while holding the executor lock. Returns a `TxPrep` with the
    /// ready-to-send transaction and receipt-task data. Drop the lock before
    /// calling `provider.send_transaction(prep.tx)`.
    ///
    /// The nonce is pre-incremented inside this call so that back-to-back
    /// preparations allocate distinct nonces without a chain round-trip.
    pub async fn prepare_2hop<P: Provider>(
        &mut self,
        provider: &P,
        opp: &ArbOpportunity,
    ) -> Result<TxPrep> {
        if self.paused {
            return Err(anyhow!("Executor paused"));
        }
        let gas_price = self.get_gas_price(provider).await;
        // priority_fee mirrors what the tx builder uses below — must match exactly so the
        // "negative after gas" guard accounts for the full effective gas price (base + tip).
        let priority_fee = (gas_price / 10).max(100_000_000u128);
        let effective_gas_price = gas_price + priority_fee;
        let gas_cost_usd = {
            let cost_wei = effective_gas_price * GAS_LIMIT as u128;
            cost_wei as f64 / 1e18 * self.native_price_usd
        };
        let net_profit_usd = opp.profit_usd - gas_cost_usd;
        if net_profit_usd < 0.0 {
            return Err(anyhow!(
                "Net profit ${:.4} (gross ${:.4} - gas ${:.4}) is negative after gas — skipping",
                net_profit_usd, opp.profit_usd, gas_cost_usd,
            ));
        }
        let deadline = self.deadline();
        let min_profit = Self::gas_cost_to_token_floor(opp.expected_profit, opp.profit_usd, gas_cost_usd);
        let calldata = self.build_calldata(opp, deadline, min_profit);
        let mut tx_base = TransactionRequest::default()
            .to(self.contract_address)
            .input(calldata.into())
            .from(self.signer_address);
        tx_base.gas = Some(GAS_LIMIT);
        self.stats.total_attempts += 1;
        let nonce = match self.nonce {
            Some(n) => n,
            None => provider
                .get_transaction_count(self.signer_address)
                .await
                .map_err(|e| anyhow!("get_transaction_count failed: {}", e))?,
        };
        let tx = tx_base
            .nonce(nonce)
            .max_priority_fee_per_gas(priority_fee)
            .max_fee_per_gas((gas_price * 2) + priority_fee);
        // Pre-increment nonce BEFORE releasing the lock so concurrent prepares
        // allocate distinct nonces without a chain round-trip.
        self.nonce = Some(nonce + 1);
        Ok(TxPrep {
            tx,
            nonce,
            net_profit_usd,
            gas_cost_usd,
            chain_name: self.chain_name.clone(),
            chain_id: self.chain_id,
            db: self.db.clone(),
            opp_id: opp.pair_id.clone(),
            router_a: opp.router_a_id.clone(),
            router_b: opp.router_b_id.clone(),
            router_c: None,
            profit_usd: opp.profit_usd,
            dry_run: self.dry_run,
            confirmed_success: self.confirmed_success.clone(),
            confirmed_failed: self.confirmed_failed.clone(),
            confirmed_profit_bits: self.confirmed_profit_usd_bits.clone(),
            contract_addr: self.contract_address,
            contract_balances: self.contract_balances.clone(),
        })
    }

    /// Phase 1 of the split-lock pattern for triangular arbs.
    pub async fn prepare_triangular<P: Provider>(
        &mut self,
        provider: &P,
        opp: &TriangularOpportunity,
    ) -> Result<TxPrep> {
        if self.paused {
            return Err(anyhow!("Executor paused"));
        }
        let gas_price = self.get_gas_price(provider).await;
        let priority_fee = (gas_price / 10).max(100_000_000u128);
        let effective_gas_price = gas_price + priority_fee;
        let gas_cost_usd = {
            let cost_wei = effective_gas_price * GAS_LIMIT_TRIANGULAR as u128;
            cost_wei as f64 / 1e18 * self.native_price_usd
        };
        let net_profit_usd = opp.profit_usd - gas_cost_usd;
        if net_profit_usd < 0.0 {
            return Err(anyhow!(
                "Net profit ${:.4} (gross ${:.4} - gas ${:.4}) is negative after gas — skipping",
                net_profit_usd, opp.profit_usd, gas_cost_usd,
            ));
        }
        let deadline = self.deadline();
        let min_profit = Self::gas_cost_to_token_floor(opp.expected_profit, opp.profit_usd, gas_cost_usd);
        let calldata = self.build_triangular_calldata(opp, deadline, min_profit);
        let mut tx_base = TransactionRequest::default()
            .to(self.contract_address)
            .input(calldata.into())
            .from(self.signer_address);
        tx_base.gas = Some(GAS_LIMIT_TRIANGULAR);
        self.stats.total_attempts += 1;
        let nonce = match self.nonce {
            Some(n) => n,
            None => provider
                .get_transaction_count(self.signer_address)
                .await
                .map_err(|e| anyhow!("get_transaction_count failed: {}", e))?,
        };
        let tx = tx_base
            .nonce(nonce)
            .max_priority_fee_per_gas(priority_fee)
            .max_fee_per_gas((gas_price * 2) + priority_fee);
        self.nonce = Some(nonce + 1);
        Ok(TxPrep {
            tx,
            nonce,
            net_profit_usd,
            gas_cost_usd,
            chain_name: self.chain_name.clone(),
            chain_id: self.chain_id,
            db: self.db.clone(),
            opp_id: opp.triplet_id.clone(),
            router_a: opp.router_ab_id.clone(),
            router_b: opp.router_bc_id.clone(),
            router_c: Some(opp.router_ca_id.clone()),
            profit_usd: opp.profit_usd,
            dry_run: self.dry_run,
            confirmed_success: self.confirmed_success.clone(),
            confirmed_failed: self.confirmed_failed.clone(),
            confirmed_profit_bits: self.confirmed_profit_usd_bits.clone(),
            contract_addr: self.contract_address,
            contract_balances: self.contract_balances.clone(),
        })
    }

    /// Phase 3 (success): update executor stats after `send_transaction` succeeds.
    /// Call while briefly re-holding the executor lock (after the send completes).
    pub fn record_sent(&mut self, tx_hash: &str, elapsed_ms: u64) {
        self.stats.total_sent += 1;
        self.stats.last_tx_hash = Some(tx_hash.to_string());
        self.stats.last_execution_ms = Some(elapsed_ms);
    }

    /// Phase 3 (failure): reset nonce and update stats after `send_transaction` fails.
    /// The nonce was pre-incremented in prepare_*() but the send failed — it was
    /// NOT consumed. Reset to None so the next prepare re-fetches from chain.
    pub fn record_failed(&mut self) {
        self.nonce = None;
        self.stats.total_failed += 1;
    }

    /// Pre-fetch and cache the on-chain nonce so the first submission has zero
    /// RPC overhead. Called at startup and can be called after reverts to avoid
    /// the ~50ms nonce fetch on the next submission.
    pub async fn prefetch_nonce<P: Provider + Clone>(&mut self, provider: &P) {
        match provider.get_transaction_count(self.signer_address).await {
            Ok(n) => {
                self.nonce = Some(n);
                debug!("Executor: nonce pre-fetched = {}", n);
            }
            Err(e) => {
                warn!("Executor: nonce pre-fetch failed: {} — will fetch on first send", e);
            }
        }
    }

    /// Execute (or simulate) the arb. Returns the tx hash string.
    ///
    /// Live path: releases the executor mutex after send_transaction (before
    /// receipt) by spawning receipt-waiting as a background task. A nonce
    /// counter prevents collisions when back-to-back opportunities fire.
    pub async fn execute<P: Provider + Clone + 'static>(&mut self, provider: &P, opp: &ArbOpportunity) -> Result<String> {
        if self.paused {
            return Err(anyhow!("Executor paused"));
        }

        // ── Gas price (cached with TTL = 2× block_time, reused for cost check + fee tuning) ───
        let gas_price = self.get_gas_price(provider).await;

        // ── Gas profitability check (300k hardcoded — no eth_estimateGas) ─────
        let gas_cost_usd = {
            let cost_wei = gas_price * GAS_LIMIT as u128;
            cost_wei as f64 / 1e18 * self.native_price_usd
        };
        let net_profit_usd = opp.profit_usd - gas_cost_usd;
        if net_profit_usd < 0.0 {
            return Err(anyhow!(
                "Net profit ${:.4} (gross ${:.4} - gas ${:.4}) is negative after gas — skipping",
                net_profit_usd, opp.profit_usd, gas_cost_usd,
            ));
        }

        let deadline = self.deadline();
        let min_profit = Self::gas_cost_to_token_floor(opp.expected_profit, opp.profit_usd, gas_cost_usd);
        let calldata = self.build_calldata(opp, deadline, min_profit);
        let mut tx_base = TransactionRequest::default()
            .to(self.contract_address)
            .input(calldata.into())
            .from(self.signer_address);
        tx_base.gas = Some(GAS_LIMIT);

        self.stats.total_attempts += 1;

        // ── Dry-run: simulate only ─────────────────────────────────────────────
        if self.dry_run {
            match provider.call(tx_base).await {
                Ok(_) => {
                    self.stats.total_simulated += 1;
                    let tx_hash =
                        "0x0000000000000000000000000000000000000000000000000000000000000000"
                            .to_string();
                    info!(
                        "[{}] DRY-RUN ok | gross=${:.4} gas≈${:.4} net=${:.4}",
                        self.chain_name, opp.profit_usd, gas_cost_usd, net_profit_usd
                    );
                    self.persist_trade(opp, &tx_hash, true);
                    return Ok(tx_hash);
                }
                Err(e) => {
                    self.stats.total_failed += 1;
                    self.persist_trade(opp, "", false);
                    return Err(anyhow!("Simulation failed: {}", e));
                }
            }
        }

        // ── Nonce management ──────────────────────────────────────────────────
        // Fetch from chain on first use; afterwards increment locally so the
        // mutex can be released immediately after send (not after receipt).
        let nonce = match self.nonce {
            Some(n) => n,
            None => provider
                .get_transaction_count(self.signer_address)
                .await
                .map_err(|e| anyhow!("get_transaction_count failed: {}", e))?,
        };

        // ── Live execution with EIP-1559 tip tuning ────────────────────────────
        // priority_fee = 10% of base fee, minimum 0.1 gwei
        let priority_fee = (gas_price / 10).max(100_000_000u128);
        let tx = tx_base
            .nonce(nonce)
            .max_priority_fee_per_gas(priority_fee)
            .max_fee_per_gas(gas_price + priority_fee);

        let start = std::time::Instant::now();

        // ── Multi-RPC broadcast ────────────────────────────────────────────────
        // Fire the same tx to secondary HTTP endpoints concurrently (fire-and-forget).
        // Each secondary creates its own wallet provider — signing happens per-provider.
        // Primary WS provider is used for receipt tracking below.
        if !self.submission_rpcs.is_empty() {
            let tx_clone = tx.clone();
            let urls = self.submission_rpcs.clone();
            let wallet = self.wallet.clone();
            let cname = self.chain_name.clone();
            tokio::spawn(async move {
                let futs = urls.into_iter().map(|url| {
                    let tx = tx_clone.clone();
                    let wallet = wallet.clone();
                    let cname = cname.clone();
                    async move {
                        let p = ProviderBuilder::new().wallet(wallet).connect_http(url);
                        match p.send_transaction(tx).await {
                            Ok(pend) => debug!("[{}] secondary RPC accepted tx={:?}", cname, pend.tx_hash()),
                            Err(e) => debug!("[{}] secondary RPC rejected: {}", cname, e),
                        }
                    }
                });
                futures::future::join_all(futs).await;
            });
        }

        match provider.send_transaction(tx).await {
            Ok(pending) => {
                // Nonce consumed — update counter immediately so next tx can use nonce+1
                // even while this receipt is still pending.
                self.nonce = Some(nonce + 1);

                let tx_hash = format!("{:?}", pending.tx_hash());
                let elapsed_send = start.elapsed().as_millis() as u64;

                // total_sent = tx accepted by mempool (NOT yet confirmed).
                self.stats.total_sent += 1;
                self.stats.last_tx_hash = Some(tx_hash.clone());
                self.stats.last_execution_ms = Some(elapsed_send);

                info!(
                    "[{}] Arb sent ({}ms) | gross=${:.4} net=${:.4} | tx={}",
                    self.chain_name,
                    elapsed_send,
                    opp.profit_usd,
                    net_profit_usd,
                    &tx_hash[..10.min(tx_hash.len())],
                );

                // ── Fire-and-forget receipt: release mutex now ─────────────
                let chain_name = self.chain_name.clone();
                let db = self.db.clone();
                let chain_id = self.chain_id;
                let pair_id = opp.pair_id.clone();
                let router_a = opp.router_a_id.clone();
                let router_b = opp.router_b_id.clone();
                let profit_usd = opp.profit_usd;
                let dry_run = self.dry_run;
                let tx_hash_bg = tx_hash.clone();
                let provider_bg = provider.clone();
                let confirmed_success = self.confirmed_success.clone();
                let confirmed_failed = self.confirmed_failed.clone();
                let confirmed_profit_bits = self.confirmed_profit_usd_bits.clone();
                let contract_addr_bg = self.contract_address;
                let contract_balances_bg = self.contract_balances.clone();

                tokio::spawn(async move {
                    match pending.get_receipt().await {
                        Ok(receipt) => {
                            if receipt.status() {
                                confirmed_success.fetch_add(1, Ordering::Relaxed);
                                // Accumulate profit atomically (f64 add via fetch_update)
                                confirmed_profit_bits.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                                    Some((f64::from_bits(bits) + profit_usd).to_bits())
                                }).ok();
                                info!(
                                    "[{}] ✓ confirmed | gas={} | tx={}",
                                    chain_name,
                                    receipt.gas_used,
                                    &tx_hash_bg[..10.min(tx_hash_bg.len())],
                                );
                                // Immediate balance refresh so optimize() sees updated capital
                                if let Some(bals) = contract_balances_bg {
                                    let token_addrs: Vec<Address> = {
                                        let b = bals.read().await;
                                        b.keys().copied().collect()
                                    };
                                    for token in token_addrs {
                                        if let Ok(bal) = IERC20::new(token, &provider_bg).balanceOf(contract_addr_bg).call().await {
                                            let mut b = bals.write().await;
                                            b.insert(token, bal);
                                        }
                                    }
                                }
                            } else {
                                confirmed_failed.fetch_add(1, Ordering::Relaxed);
                                warn!("[{}] ✗ reverted | tx={}", chain_name, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                            }
                            if let Some(db) = db {
                                let success = receipt.status();
                                let _ = tokio::task::spawn_blocking(move || {
                                    let _ = db.insert_trade(chain_id, &chain_name, &pair_id, &router_a, &router_b, None, profit_usd, success, &tx_hash_bg, dry_run);
                                }).await;
                            }
                        }
                        Err(e) => {
                            confirmed_failed.fetch_add(1, Ordering::Relaxed);
                            warn!("[{}] Receipt error for {}: {}", chain_name, &tx_hash_bg[..10.min(tx_hash_bg.len())], e);
                        }
                    }
                    drop(provider_bg); // keep provider alive until receipt arrives
                });

                Ok(tx_hash)
            }
            Err(e) => {
                // Send failed — nonce was NOT consumed; reset so we refetch next time.
                self.nonce = None;
                self.stats.total_failed += 1;
                self.persist_trade(opp, "", false);
                Err(anyhow!("Send failed: {}", e))
            }
        }
    }

    /// Execute triangular arbitrage (A→B→C→A). Same flow as execute() but with
    /// 450k gas limit and different contract call.
    pub async fn execute_triangular<P: Provider + Clone + 'static>(
        &mut self,
        provider: &P,
        opp: &TriangularOpportunity,
    ) -> Result<String> {
        if self.paused {
            return Err(anyhow!("Executor paused"));
        }

        // Use cached gas price (TTL = 2× block_time)
        let gas_price = self.get_gas_price(provider).await;

        let gas_cost_usd = {
            let cost_wei = gas_price * GAS_LIMIT_TRIANGULAR as u128;
            cost_wei as f64 / 1e18 * self.native_price_usd
        };
        let net_profit_usd = opp.profit_usd - gas_cost_usd;
        if net_profit_usd < 0.0 {
            return Err(anyhow!(
                "Net profit ${:.4} (gross ${:.4} - gas ${:.4}) is negative after gas — skipping",
                net_profit_usd, opp.profit_usd, gas_cost_usd,
            ));
        }

        let deadline = self.deadline();
        let min_profit = Self::gas_cost_to_token_floor(opp.expected_profit, opp.profit_usd, gas_cost_usd);
        let calldata = self.build_triangular_calldata(opp, deadline, min_profit);
        let mut tx_base = TransactionRequest::default()
            .to(self.contract_address)
            .input(calldata.into())
            .from(self.signer_address);
        tx_base.gas = Some(GAS_LIMIT_TRIANGULAR);

        self.stats.total_attempts += 1;

        if self.dry_run {
            match provider.call(tx_base).await {
                Ok(_) => {
                    self.stats.total_simulated += 1;
                    let tx_hash =
                        "0x0000000000000000000000000000000000000000000000000000000000000000"
                            .to_string();
                    info!(
                        "[{}] DRY-RUN triangular ok | gross=${:.4} gas≈${:.4} net=${:.4}",
                        self.chain_name, opp.profit_usd, gas_cost_usd, net_profit_usd
                    );
                    return Ok(tx_hash);
                }
                Err(e) => {
                    self.stats.total_failed += 1;
                    return Err(anyhow!("Triangular simulation failed: {}", e));
                }
            }
        }

        let nonce = match self.nonce {
            Some(n) => n,
            None => provider
                .get_transaction_count(self.signer_address)
                .await
                .map_err(|e| anyhow!("get_transaction_count failed: {}", e))?,
        };

        let priority_fee = (gas_price / 10).max(100_000_000u128);
        let tx = tx_base
            .nonce(nonce)
            .max_priority_fee_per_gas(priority_fee)
            .max_fee_per_gas(gas_price + priority_fee);

        let start = std::time::Instant::now();

        // Multi-RPC broadcast (same pattern as execute())
        if !self.submission_rpcs.is_empty() {
            let tx_clone = tx.clone();
            let urls = self.submission_rpcs.clone();
            let wallet = self.wallet.clone();
            let cname = self.chain_name.clone();
            tokio::spawn(async move {
                let futs = urls.into_iter().map(|url| {
                    let tx = tx_clone.clone();
                    let wallet = wallet.clone();
                    let cname = cname.clone();
                    async move {
                        let p = ProviderBuilder::new().wallet(wallet).connect_http(url);
                        match p.send_transaction(tx).await {
                            Ok(pend) => debug!("[{}] secondary RPC accepted tx={:?}", cname, pend.tx_hash()),
                            Err(e) => debug!("[{}] secondary RPC rejected: {}", cname, e),
                        }
                    }
                });
                futures::future::join_all(futs).await;
            });
        }

        match provider.send_transaction(tx).await {
            Ok(pending) => {
                self.nonce = Some(nonce + 1);
                let tx_hash = format!("{:?}", pending.tx_hash());
                let elapsed_send = start.elapsed().as_millis() as u64;

                self.stats.total_sent += 1;
                self.stats.last_tx_hash = Some(tx_hash.clone());
                self.stats.last_execution_ms = Some(elapsed_send);

                info!(
                    "[{}] Triangular arb sent ({}ms) | gross=${:.4} net=${:.4} | tx={}",
                    self.chain_name,
                    elapsed_send,
                    opp.profit_usd,
                    net_profit_usd,
                    &tx_hash[..10.min(tx_hash.len())],
                );

                let chain_name = self.chain_name.clone();
                let db = self.db.clone();
                let chain_id = self.chain_id;
                let triplet_id = opp.triplet_id.clone();
                let router_ab = opp.router_ab_id.clone();
                let router_bc = opp.router_bc_id.clone();
                let router_ca = opp.router_ca_id.clone();
                let profit_usd = opp.profit_usd;
                let dry_run = self.dry_run;
                let tx_hash_bg = tx_hash.clone();
                let provider_bg = provider.clone();
                let confirmed_success = self.confirmed_success.clone();
                let confirmed_failed = self.confirmed_failed.clone();
                let confirmed_profit_bits = self.confirmed_profit_usd_bits.clone();
                let contract_addr_bg = self.contract_address;
                let contract_balances_bg = self.contract_balances.clone();

                tokio::spawn(async move {
                    match pending.get_receipt().await {
                        Ok(receipt) => {
                            if receipt.status() {
                                confirmed_success.fetch_add(1, Ordering::Relaxed);
                                confirmed_profit_bits.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                                    Some((f64::from_bits(bits) + profit_usd).to_bits())
                                }).ok();
                                info!(
                                    "[{}] ✓ triangular confirmed | gas={} | tx={}",
                                    chain_name,
                                    receipt.gas_used,
                                    &tx_hash_bg[..10.min(tx_hash_bg.len())],
                                );
                                // Immediate balance refresh so optimize() sees updated capital
                                if let Some(bals) = contract_balances_bg {
                                    let token_addrs: Vec<Address> = {
                                        let b = bals.read().await;
                                        b.keys().copied().collect()
                                    };
                                    for token in token_addrs {
                                        if let Ok(bal) = IERC20::new(token, &provider_bg).balanceOf(contract_addr_bg).call().await {
                                            let mut b = bals.write().await;
                                            b.insert(token, bal);
                                        }
                                    }
                                }
                            } else {
                                confirmed_failed.fetch_add(1, Ordering::Relaxed);
                                warn!("[{}] ✗ triangular reverted | tx={}", chain_name, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                            }
                            if let Some(db) = db {
                                let success = receipt.status();
                                let _ = tokio::task::spawn_blocking(move || {
                                    let _ = db.insert_trade(
                                        chain_id,
                                        &chain_name,
                                        &triplet_id,
                                        &router_ab,
                                        &router_bc,
                                        Some(&router_ca),
                                        profit_usd,
                                        success,
                                        &tx_hash_bg,
                                        dry_run,
                                    );
                                }).await;
                            }
                        }
                        Err(e) => {
                            confirmed_failed.fetch_add(1, Ordering::Relaxed);
                            warn!("[{}] Triangular receipt error for {}: {}", chain_name, &tx_hash_bg[..10.min(tx_hash_bg.len())], e);
                        }
                    }
                    drop(provider_bg);
                });

                Ok(tx_hash)
            }
            Err(e) => {
                self.nonce = None;
                self.stats.total_failed += 1;
                Err(anyhow!("Triangular send failed: {}", e))
            }
        }
    }

    fn deadline(&self) -> U256 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        U256::from(now + 120) // 2 min default
    }

    /// Convert gas cost from USD to token-in units using the same exchange rate
    /// as expected_profit → profit_usd. This gives a token-denominated floor
    /// for on-chain minProfit that covers gas regardless of token type or decimals.
    fn gas_cost_to_token_floor(expected_profit: U256, profit_usd: f64, gas_cost_usd: f64) -> U256 {
        if profit_usd <= 0.0 || expected_profit.is_zero() {
            return U256::from(1);
        }
        // tokens_per_usd = expected_profit (raw) / profit_usd
        let ep_f64 = expected_profit.to_string().parse::<f64>().unwrap_or(1.0);
        let tokens_per_usd = ep_f64 / profit_usd;
        let gas_tokens = (gas_cost_usd * tokens_per_usd) as u128;
        U256::from(gas_tokens).max(U256::from(1))
    }

    fn build_calldata(&self, opp: &ArbOpportunity, deadline: U256, min_profit: U256) -> Vec<u8> {
        ArbitrageExecutor::executeArbitrageCall {
            tokenIn: opp.token_in,
            tokenOut: opp.token_out,
            amountIn: opp.amount_in,
            routerA: opp.router_a,
            routerB: opp.router_b,
            feeA: Uint::<24, 1>::from(opp.fee_a),
            feeB: Uint::<24, 1>::from(opp.fee_b),
            minProfit: min_profit,
            deadline,
        }
        .abi_encode()
    }

    fn build_triangular_calldata(&self, opp: &TriangularOpportunity, deadline: U256, min_profit: U256) -> Vec<u8> {
        ArbitrageExecutor::executeTriangularArbitrageCall {
            tokenA: opp.token_a,
            tokenB: opp.token_b,
            tokenC: opp.token_c,
            amountIn: opp.amount_in,
            routerAB: opp.router_ab,
            routerBC: opp.router_bc,
            routerCA: opp.router_ca,
            feeAB: Uint::<24, 1>::from(opp.fee_ab),
            feeBC: Uint::<24, 1>::from(opp.fee_bc),
            feeCA: Uint::<24, 1>::from(opp.fee_ca),
            minProfit: min_profit,
            deadline,
        }
        .abi_encode()
    }

    /// Fire-and-forget: persist trade to SQLite off the async thread.
    fn persist_trade(&self, opp: &ArbOpportunity, tx_hash: &str, success: bool) {
        let Some(db) = self.db.clone() else { return };
        let chain_id = self.chain_id;
        let chain_name = self.chain_name.clone();
        let pair_id = opp.pair_id.clone();
        let router_a = opp.router_a_id.clone();
        let router_b = opp.router_b_id.clone();
        let profit_usd = opp.profit_usd;
        let dry_run = self.dry_run;
        let tx_hash = tx_hash.to_string();

        tokio::task::spawn_blocking(move || {
            if let Err(e) = db.insert_trade(
                chain_id,
                &chain_name,
                &pair_id,
                &router_a,
                &router_b,
                None, // No router_c for 2-hop arbs
                profit_usd,
                success,
                &tx_hash,
                dry_run,
            ) {
                tracing::warn!("DB write failed: {}", e);
            }
        });
    }
}
