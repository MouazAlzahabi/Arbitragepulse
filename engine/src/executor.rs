use alloy::primitives::{Address, Uint, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn, debug};

use crate::abi::ArbitrageExecutor;
use crate::db::Database;
use crate::strategy::{ArbOpportunity, TriangularOpportunity};

// Hardcoded gas limit — 300k covers any 2-hop arb (V2+V2, V2+V3, V3+V3).
// We skip eth_estimateGas to save one RPC round-trip per execution.
const GAS_LIMIT: u64 = 300_000;

// Triangular arb gas limit — 450k covers 3-hop paths (V2+V2+V2, V3+V3+V3, mixed).
const GAS_LIMIT_TRIANGULAR: u64 = 450_000;

// ─── Stats ────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct ExecutorStats {
    pub total_attempts: u64,
    pub total_success: u64,
    pub total_failed: u64,
    pub total_simulated: u64,
    pub total_profit_wei: U256,
    pub last_tx_hash: Option<String>,
    pub last_execution_ms: Option<u64>,
}

// ─── Executor ─────────────────────────────────────────────────────────────────

#[allow(dead_code)]
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
}

impl Executor {
    pub fn new(
        chain_id: u64,
        chain_name: String,
        contract_address: Address,
        signer_address: Address,
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
        }
    }

    /// Called from the price-tick handler so gas cost estimates stay current.
    pub fn update_native_price(&mut self, price: f64) {
        self.native_price_usd = price;
    }

    /// Get gas price with caching. Checks cache first; if expired, fetches from chain.
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
        if net_profit_usd < self.min_profit_usd {
            return Err(anyhow!(
                "Net profit ${:.4} (gross ${:.4} - gas ${:.4}) below threshold ${:.2}",
                net_profit_usd, opp.profit_usd, gas_cost_usd, self.min_profit_usd,
            ));
        }

        let deadline = self.deadline();
        let calldata = self.build_calldata(opp, deadline);
        let mut tx_base = TransactionRequest::default()
            .to(self.contract_address)
            .input(calldata.into());
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

        match provider.send_transaction(tx).await {
            Ok(pending) => {
                // Nonce consumed — update counter immediately so next tx can use nonce+1
                // even while this receipt is still pending.
                self.nonce = Some(nonce + 1);

                let tx_hash = format!("{:?}", pending.tx_hash());
                let elapsed_send = start.elapsed().as_millis() as u64;

                self.stats.total_success += 1;
                self.stats.total_profit_wei += opp.expected_profit;
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

                tokio::spawn(async move {
                    match pending.get_receipt().await {
                        Ok(receipt) => {
                            if receipt.status() {
                                info!(
                                    "[{}] ✓ confirmed | gas={} | tx={}",
                                    chain_name,
                                    receipt.gas_used,
                                    &tx_hash_bg[..10.min(tx_hash_bg.len())],
                                );
                            } else {
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
        if net_profit_usd < self.min_profit_usd {
            return Err(anyhow!(
                "Net profit ${:.4} (gross ${:.4} - gas ${:.4}) below threshold ${:.2}",
                net_profit_usd, opp.profit_usd, gas_cost_usd, self.min_profit_usd,
            ));
        }

        let deadline = self.deadline();
        let calldata = self.build_triangular_calldata(opp, deadline);
        let mut tx_base = TransactionRequest::default()
            .to(self.contract_address)
            .input(calldata.into());
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

        match provider.send_transaction(tx).await {
            Ok(pending) => {
                self.nonce = Some(nonce + 1);
                let tx_hash = format!("{:?}", pending.tx_hash());
                let elapsed_send = start.elapsed().as_millis() as u64;

                self.stats.total_success += 1;
                self.stats.total_profit_wei += opp.expected_profit;
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

                tokio::spawn(async move {
                    match pending.get_receipt().await {
                        Ok(receipt) => {
                            if receipt.status() {
                                info!(
                                    "[{}] ✓ triangular confirmed | gas={} | tx={}",
                                    chain_name,
                                    receipt.gas_used,
                                    &tx_hash_bg[..10.min(tx_hash_bg.len())],
                                );
                            } else {
                                warn!("[{}] ✗ triangular reverted | tx={}", chain_name, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                            }
                            // Persist triangular trade to database
                            if let Some(db) = db {
                                let success = receipt.status();
                                let _ = tokio::task::spawn_blocking(move || {
                                    let _ = db.insert_trade(
                                        chain_id,
                                        &chain_name,
                                        &triplet_id,
                                        &router_ab,
                                        &router_bc,
                                        Some(&router_ca), // Include router_c for triangular
                                        profit_usd,
                                        success,
                                        &tx_hash_bg,
                                        dry_run,
                                    );
                                }).await;
                            }
                        }
                        Err(e) => {
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

    /// Whitelist a router on the contract (onlyOwner call).
    #[allow(dead_code)]
    pub async fn whitelist_router<P: Provider>(
        &self,
        provider: &P,
        router: Address,
        is_v3: bool,
    ) -> Result<()> {
        // setAllowedRouter
        let calldata = ArbitrageExecutor::setAllowedRouterCall {
            router,
            approved: true,
        }
        .abi_encode();

        let tx = TransactionRequest::default()
            .to(self.contract_address)
            .input(calldata.into());

        provider
            .send_transaction(tx)
            .await?
            .get_receipt()
            .await?;

        if is_v3 {
            // setRouterType(router, 1) — 1 = RouterType.V3
            let calldata = ArbitrageExecutor::setRouterTypeCall {
                router,
                rtype: 1,
            }
            .abi_encode();

            let tx = TransactionRequest::default()
                .to(self.contract_address)
                .input(calldata.into());

            provider
                .send_transaction(tx)
                .await?
                .get_receipt()
                .await?;
        }

        info!(
            "[{}] Whitelisted router {:?} (v3={})",
            self.chain_name, router, is_v3
        );
        Ok(())
    }

    #[allow(dead_code)]
    pub fn pause(&mut self) {
        self.paused = true;
        warn!("[{}] Executor paused", self.chain_name);
    }

    #[allow(dead_code)]
    pub fn resume(&mut self) {
        self.paused = false;
        info!("[{}] Executor resumed", self.chain_name);
    }

    #[allow(dead_code)]
    pub fn set_dry_run(&mut self, dry_run: bool) {
        self.dry_run = dry_run;
        info!("[{}] Dry-run = {}", self.chain_name, dry_run);
    }

    fn deadline(&self) -> U256 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        U256::from(now + 120) // 2 min default
    }

    fn build_calldata(&self, opp: &ArbOpportunity, deadline: U256) -> Vec<u8> {
        ArbitrageExecutor::executeArbitrageCall {
            tokenIn: opp.token_in,
            tokenOut: opp.token_out,
            amountIn: opp.amount_in,
            routerA: opp.router_a,
            routerB: opp.router_b,
            feeA: Uint::<24, 1>::from(opp.fee_a),
            feeB: Uint::<24, 1>::from(opp.fee_b),
            minProfit: opp.expected_profit / U256::from(2u32), // 50% slippage buffer
            deadline,
        }
        .abi_encode()
    }

    fn build_triangular_calldata(&self, opp: &TriangularOpportunity, deadline: U256) -> Vec<u8> {
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
            minProfit: opp.expected_profit / U256::from(2u32), // 50% slippage buffer
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
