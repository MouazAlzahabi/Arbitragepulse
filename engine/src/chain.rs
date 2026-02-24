use alloy::network::Ethereum;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::network::EthereumWallet;
use alloy::transports::ws::WsConnect;
use anyhow::Result;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::api::{broadcast_log, ChainStats, LogBroadcaster, SharedState};
use crate::config::{self, ChainConfig, PairConfig, RouterConfig};
use crate::db::Database;
use crate::executor::Executor;
use crate::listener::{Listener, SwapEvent};
use crate::metrics::Metrics;
use crate::strategy::{Opportunity, Strategy};

const COOLDOWN_SECS: u64 = 15;
/// Only count real execution failures (simulation reverts, send errors).
/// Gas-profitability rejects ("below threshold") do NOT count — they're pre-flight skips.
const MAX_CONSECUTIVE_FAILURES: u32 = 20;

// ─── Per-chain engine task ────────────────────────────────────────────────────

pub async fn run_chain(
    cfg: ChainConfig,
    routers: Vec<RouterConfig>,
    pairs: Vec<PairConfig>,
    private_key: String,
    shared_state: SharedState,
    log_tx: LogBroadcaster,
    db: Option<Arc<Database>>,
    config_path: String,
    metrics: Arc<Metrics>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    info!("[{}] Starting chain engine (id={})", cfg.name, cfg.id);

    // ── Build wallet provider (with fallback RPC list) ──
    let signer: PrivateKeySigner = private_key.parse()?;
    let signer_address = signer.address();
    let wallet = EthereumWallet::from(signer);

    let provider = connect_with_fallback(&cfg, wallet).await
        .map_err(|e| anyhow::anyhow!("[{}] All RPC endpoints failed: {}", cfg.name, e))?;

    let provider = Arc::new(provider);

    // ── Contract address ──
    let contract_addr: Address = cfg
        .contract_address
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid contract address: {}", cfg.contract_address))?;

    // ── Executor ──
    let executor = Arc::new(Mutex::new(Executor::new(
        cfg.id,
        cfg.name.clone(),
        contract_addr,
        signer_address,
        cfg.min_profit_usd,
        cfg.block_time_ms,
        db,
    )));

    // ── Strategy ──
    let chain_pairs: Vec<PairConfig> = pairs
        .iter()
        .filter(|p| p.chain_id == cfg.id)
        .cloned()
        .collect();

    let chain_routers: Vec<RouterConfig> = routers
        .iter()
        .filter(|r| r.chain_id == cfg.id)
        .cloned()
        .collect();

    let quoter_v2_address: Option<alloy::primitives::Address> = cfg
        .quoter_v2_address
        .as_deref()
        .and_then(|s| s.parse().ok());

    let strategy = Arc::new(RwLock::new(Strategy::new(
        cfg.id,
        chain_pairs.clone(),
        chain_routers.clone(),
        cfg.min_profit_usd,
        quoter_v2_address,
    )));

    // Populate SyncSwap pool cache at startup (no-op if no SyncSwap routers configured)
    {
        let mut strat = strategy.write().await;
        strat.populate_syncswap_pools(provider.as_ref()).await;
    }

    // ── Router health monitoring ──
    let router_monitor = Arc::new(crate::router_health::RouterHealthMonitor::new());

    // ── Register initial chain stats ──
    {
        let mut state = shared_state.write().await;
        if !state.chains.iter().any(|c| c.chain_id == cfg.id) {
            state.chains.push(ChainStats {
                chain_id: cfg.id,
                chain_name: cfg.name.clone(),
                total_scans: 0,
                total_attempts: 0,
                total_success: 0,
                total_profit_usd: 0.0,
                dry_run: true,
                paused: false,
                rpc_ok: true,   // we just connected successfully
                last_block: 0,
            });
        } else if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
            chain.rpc_ok = true;
        }
    }
    metrics.rpc_connected.with_label_values(&[&cfg.name]).set(1.0);

    // ── Swap event listener ──
    let listener = Listener::new(cfg.id, cfg.name.clone(), chain_pairs.clone(), cfg.min_swap_amount_filter);
    let (swap_tx, mut swap_rx) = mpsc::channel::<SwapEvent>(256);
    {
        let provider_clone = (*provider).clone();
        listener.subscribe(provider_clone, swap_tx).await?;
    }

    // ── Block-header subscription (drives immediate scanning on each new block) ──
    let (block_tx, mut block_rx) = mpsc::channel::<u64>(64);
    {
        let provider_for_blocks = (*provider).clone();
        let chain_name_b = cfg.name.clone();
        let block_tx_b = block_tx.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match provider_for_blocks.subscribe_blocks().await {
                    Ok(sub) => {
                        backoff = Duration::from_secs(1);
                        let mut stream = sub.into_stream();
                        while let Some(header) = stream.next().await {
                            let num = header.number;
                            debug!("[{}] New block #{}", chain_name_b, num);
                            let _ = block_tx_b.send(num).await;
                        }
                        warn!("[{}] Block subscription stream ended, reconnecting in {:?}", chain_name_b, backoff);
                    }
                    Err(e) => {
                        warn!("[{}] Block subscription error: {} — retrying in {:?}", chain_name_b, e, backoff);
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });
    }

    info!("[{}] Chain engine started", cfg.name);
    broadcast_log(
        &log_tx,
        "info",
        &format!("[{}] Chain engine started | contract={}", cfg.name, contract_addr),
        None,
    );
    // Log configured pairs at startup so the user can verify what's being scanned.
    let pair_ids: Vec<&str> = chain_pairs.iter().map(|p| p.id.as_str()).collect();
    broadcast_log(
        &log_tx,
        "info",
        &format!("[{}] Monitoring {} pairs: {}", cfg.name, pair_ids.len(), pair_ids.join(", ")),
        None,
    );

    // ── Safety / efficiency state ──
    let mut pending_pairs: HashSet<String> = HashSet::new();
    let mut cooldowns: HashMap<String, Instant> = HashMap::new();
    let mut consecutive_failures: u32 = 0;

    // ── Best-seen profit tracker (f64 stored as bits in AtomicU64) ────────────
    // Updated on every scan, reset after each 60s heartbeat log.
    let best_raw_profit: Arc<AtomicU64> = Arc::new(AtomicU64::new(0u64));

    // ── Best signed spread % (f64 bits). NEG_INFINITY when no quotes yet. ─────
    // Shows how close the market is to a profitable arb even when best_seen=$0.
    let best_spread_bits: Arc<AtomicU64> = Arc::new(AtomicU64::new(f64::NEG_INFINITY.to_bits()));

    // ── Quote diagnostic counters (last scan values) ──────────────────────────
    // fwd_count: how many non-zero forward quotes returned
    // multi_count: how many pairs had quotes from ≥2 distinct DEXes
    // active_count: how many pairs had ≥1 quote from any DEX
    let last_fwd_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0u64));
    let last_multi_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0u64));
    let last_active_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0u64));
    // Count of consecutive heartbeats with zero forward quotes.
    // Warning only fires at 3+ to suppress startup false-positives and transient RPC blips.
    let mut consecutive_zero_fwd: u32 = 0;

    // ── Main loop ──
    let poll_interval = Duration::from_millis(cfg.block_time_ms * 2);
    let mut poll_tick    = tokio::time::interval(poll_interval);
    let mut price_tick   = tokio::time::interval(Duration::from_secs(60));
    let mut config_tick  = tokio::time::interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            // ── Graceful shutdown ──────────────────────────────────────────────
            _ = shutdown.recv() => {
                info!("[{}] Shutdown signal received — stopping chain engine", cfg.name);
                metrics.rpc_connected.with_label_values(&[&cfg.name]).set(0.0);
                return Ok(());
            }

            // ── Periodic price update + cache cleanup + status log ────────────
            _ = price_tick.tick() => {
                update_native_price(&strategy, &executor, &provider, &chain_routers, &chain_pairs, &cfg).await;
                // Broadcast a heartbeat log so the Live Feed shows the engine is active
                // even when no opportunities are detected. Fires every ~60 seconds.
                let (scans, attempts, success) = {
                    let state = shared_state.read().await;
                    state.chains.iter()
                        .find(|c| c.chain_id == cfg.id)
                        .map(|c| (c.total_scans, c.total_attempts, c.total_success))
                        .unwrap_or_default()
                };
                let native_price = { strategy.read().await.native_price_usd };
                // Read and reset the best raw profit seen since last tick
                let best_seen_usd = f64::from_bits(best_raw_profit.swap(0u64, Ordering::Relaxed));
                let best_seen_str = if best_seen_usd > 0.0 {
                    format!("${:.4}", best_seen_usd)
                } else {
                    "$0".to_string()
                };
                // Read and reset the best signed spread % (shows market distance even when $0)
                let spread_pct = f64::from_bits(best_spread_bits.swap(f64::NEG_INFINITY.to_bits(), Ordering::Relaxed));
                let spread_str = if spread_pct.is_finite() {
                    format!("{:+.3}%", spread_pct * 100.0)
                } else {
                    "n/a".to_string()
                };
                // Read last-scan quote diagnostics
                let fwd_ok = last_fwd_count.load(Ordering::Relaxed);
                let multi_dex = last_multi_count.load(Ordering::Relaxed);
                let active_pairs = last_active_count.load(Ordering::Relaxed);
                let total_pairs = chain_pairs.len() as u64;
                broadcast_log(
                    &log_tx,
                    "info",
                    &format!(
                        "[{}] Scanning | scans={} executions={} success={} ETH=${:.0} | best_seen={} spread={} fwd_quotes={} active_pairs={}/{} cross_dex_pairs={}",
                        cfg.name, scans, attempts, success, native_price, best_seen_str, spread_str, fwd_ok, active_pairs, total_pairs, multi_dex,
                    ),
                    None,
                );
                // Broadcast a live-feed warn if no cross-DEX coverage (actionable).
                // Require 3 consecutive zero-fwd heartbeats before warning to suppress
                // startup false-positives and transient single-scan RPC blips.
                if fwd_ok == 0 {
                    consecutive_zero_fwd += 1;
                    if consecutive_zero_fwd >= 3 {
                        broadcast_log(
                            &log_tx,
                            "warn",
                            &format!(
                                "[{}] WARN: ALL forward quotes returned 0 for {}+ minutes — check QuoterV2 addresses and fee tiers in config.yaml",
                                cfg.name, consecutive_zero_fwd
                            ),
                            None,
                        );
                    }
                } else {
                    consecutive_zero_fwd = 0;
                }
                if fwd_ok > 0 && multi_dex == 0 {
                    broadcast_log(
                        &log_tx,
                        "warn",
                        &format!(
                            "[{}] WARN: 0 pairs with ≥2 DEX quotes (fwd_quotes={}) — V2 pool has no liquidity or only 1 DEX active. Add a second DEX with V2/V3 interface.",
                            cfg.name, fwd_ok
                        ),
                        None,
                    );
                }
            }

            // ── Config hot-reload ─────────────────────────────────────────────
            _ = config_tick.tick() => {
                if let Ok(new_cfg) = config::load_config(&config_path) {
                    let new_pairs: Vec<_> = new_cfg.pairs.iter()
                        .filter(|p| p.chain_id == cfg.id).cloned().collect();
                    let new_routers: Vec<_> = new_cfg.routers.iter()
                        .filter(|r| r.chain_id == cfg.id).cloned().collect();
                    let new_min_profit = new_cfg.chains.iter()
                        .find(|c| c.id == cfg.id)
                        .map(|c| c.min_profit_usd)
                        .unwrap_or(cfg.min_profit_usd);
                    {
                        let mut strat = strategy.write().await;
                        strat.pairs = new_pairs;
                        strat.routers = new_routers;
                        strat.min_profit_usd = new_min_profit;
                        // Refresh SyncSwap pool cache after router/pair changes
                        strat.populate_syncswap_pools(provider.as_ref()).await;
                    }
                    {
                        let mut exec = executor.lock().await;
                        exec.min_profit_usd = new_min_profit;
                    }
                    info!("[{}] Config hot-reloaded (min_profit=${:.2})", cfg.name, new_min_profit);
                }
            }

            // ── New block header → immediate scan ─────────────────────────────
            Some(block_num) = block_rx.recv() => {
                // Update RPC health + block number in shared state, count each block as a scan
                {
                    let mut state = shared_state.write().await;
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.rpc_ok = true;
                        chain.last_block = block_num;
                        chain.total_scans += 1;
                    }
                }
                metrics.last_block.with_label_values(&[&cfg.name]).set(block_num as f64);
                metrics.rpc_connected.with_label_values(&[&cfg.name]).set(1.0);

                let state = shared_state.read().await;
                let chain_paused = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                if state.paused || chain_paused { continue; }
                drop(state);

                evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &router_monitor, &best_raw_profit, &best_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                ).await;
            }

            // ── Periodic fallback scan (safety net if block subscription is down) ──
            _ = poll_tick.tick() => {
                let state = shared_state.read().await;
                let chain_paused = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                if state.paused || chain_paused { continue; }
                drop(state);

                evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &router_monitor, &best_raw_profit, &best_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                ).await;
            }

            // ── Swap event → immediate scan ───────────────────────────────────
            Some(event) = swap_rx.recv() => {
                debug!("[{}] Swap event on pool {:?}", cfg.name, event.pool);
                let state = shared_state.read().await;
                let chain_paused = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                if state.paused || chain_paused { continue; }
                drop(state);

                // Increment scan counter on swap-triggered evaluation too
                {
                    let mut state = shared_state.write().await;
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.total_scans += 1;
                    }
                }

                evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &router_monitor, &best_raw_profit, &best_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                ).await;
            }
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn evaluate_and_execute<P: Provider + Clone + 'static>(
    strategy: &Arc<RwLock<Strategy>>,
    executor: &Arc<Mutex<Executor>>,
    provider: &Arc<P>,
    shared_state: &SharedState,
    log_tx: &LogBroadcaster,
    cfg: &ChainConfig,
    metrics: &Arc<Metrics>,
    pending_pairs: &mut HashSet<String>,
    cooldowns: &mut HashMap<String, Instant>,
    consecutive_failures: &mut u32,
    router_monitor: &Arc<crate::router_health::RouterHealthMonitor>,
    best_raw_profit: &Arc<AtomicU64>,
    best_spread_bits: &Arc<AtomicU64>,
    last_fwd_count: &Arc<AtomicU64>,
    last_multi_count: &Arc<AtomicU64>,
    last_active_count: &Arc<AtomicU64>,
) {
    // ── Parallel detection: 2-hop + triangular ────────────────────────────────

    let all_opportunities = {
        let strat = strategy.read().await;
        let ((opps_2hop, best_2hop, fwd_ok, multi_dex, spread_2hop, active_pairs), (opps_tri, best_tri)) = tokio::join!(
            strat.evaluate(provider.as_ref()),
            strat.detect_triangular(provider.as_ref(), 5),
        );

        // Update quote diagnostic counters (last scan — overwrites on every call)
        last_fwd_count.store(fwd_ok as u64, Ordering::Relaxed);
        last_multi_count.store(multi_dex as u64, Ordering::Relaxed);
        last_active_count.store(active_pairs as u64, Ordering::Relaxed);

        // Update best-seen profit for the heartbeat log
        let best_raw = f64::max(best_2hop, best_tri);
        if best_raw > 0.0 {
            // Keep running maximum (load → compare → store)
            let current = f64::from_bits(best_raw_profit.load(Ordering::Relaxed));
            if best_raw > current {
                best_raw_profit.store(best_raw.to_bits(), Ordering::Relaxed);
            }
        }

        // Update best signed spread % (running maximum; reset each heartbeat)
        if spread_2hop.is_finite() {
            let current_spread = f64::from_bits(best_spread_bits.load(Ordering::Relaxed));
            if spread_2hop > current_spread {
                best_spread_bits.store(spread_2hop.to_bits(), Ordering::Relaxed);
            }
        }

        let mut merged = Vec::new();
        merged.extend(opps_2hop.into_iter().map(Opportunity::TwoHop));
        merged.extend(opps_tri.into_iter().map(Opportunity::Triangular));

        // Sort by profit_usd descending
        merged.sort_by(|a, b| {
            b.profit_usd()
                .partial_cmp(&a.profit_usd())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        merged
    };

    if all_opportunities.is_empty() {
        return;
    }

    metrics.opportunities.with_label_values(&[&cfg.name]).inc();

    let best_opp = &all_opportunities[0];
    let fingerprint = best_opp.fingerprint();
    let display_id = best_opp.pair_id();

    // Full opportunity cooldown (pair + routers + amount)
    if let Some(&cooled_at) = cooldowns.get(&fingerprint) {
        if cooled_at.elapsed().as_secs() < COOLDOWN_SECS {
            debug!("[{}] {} in cooldown, skipping", cfg.name, display_id);
            return;
        }
        cooldowns.remove(&fingerprint);
    }

    // Pending tx dedup (full fingerprint prevents duplicate identical opportunities)
    if pending_pairs.contains(&fingerprint) {
        debug!("[{}] {} tx already in-flight, skipping", cfg.name, display_id);
        return;
    }

    // ── SyncSwap detect-only check ────────────────────────────────────────────
    // SyncSwap opportunities are broadcast to the live feed but never sent to
    // the contract (no execution support yet). All other types proceed normally.

    if !best_opp.is_executable() {
        broadcast_log(
            log_tx,
            "opportunity",
            &format!(
                "[{}] [DETECT-ONLY/SyncSwap] {} | profit=${:.4}",
                cfg.name,
                display_id,
                best_opp.profit_usd(),
            ),
            Some(serde_json::json!({
                "chain":      cfg.name,
                "pair_id":    display_id,
                "profit_usd": best_opp.profit_usd(),
                "detect_only": true,
            })),
        );
        return;
    }

    // ── Dispatch based on opportunity type ────────────────────────────────────

    match best_opp {
        Opportunity::TwoHop(opp) => {
            // Two-round adaptive size optimizer (only for 2-hop)
            let optimized = {
                let strat = strategy.read().await;
                strat.optimize(opp, provider.as_ref()).await
            };

            broadcast_log(
                log_tx,
                "opportunity",
                &format!(
                    "[{}] 2-hop {} | profit=${:.4} | {}/{}",
                    cfg.name, optimized.pair_id, optimized.profit_usd, optimized.router_a_id, optimized.router_b_id
                ),
                Some(serde_json::json!({
                    "chain":      cfg.name,
                    "pair_id":    optimized.pair_id,
                    "profit_usd": optimized.profit_usd,
                    "router_a":   optimized.router_a_id,
                    "router_b":   optimized.router_b_id,
                })),
            );

            pending_pairs.insert(fingerprint.clone());

            let dry_run = shared_state.read().await.dry_run;
            let mut exec = executor.lock().await;
            exec.dry_run = dry_run;

            let exec_start = std::time::Instant::now();
            let router_ids = vec![optimized.router_a_id.clone(), optimized.router_b_id.clone()];

            match exec.execute(provider.as_ref(), &optimized).await {
                Ok(tx_hash) => {
                    let exec_time_ms = exec_start.elapsed().as_millis() as u64;
                    handle_execution_success(
                        &tx_hash,
                        &fingerprint,
                        optimized.profit_usd,
                        &optimized.pair_id,
                        &router_ids,
                        pending_pairs,
                        consecutive_failures,
                        dry_run,
                        cfg,
                        shared_state,
                        log_tx,
                        metrics,
                        &router_monitor,
                        exec_time_ms,
                    )
                    .await;
                }
                Err(e) => {
                    handle_execution_failure(
                        e,
                        &fingerprint,
                        &router_ids,
                        pending_pairs,
                        cooldowns,
                        consecutive_failures,
                        cfg,
                        shared_state,
                        metrics,
                        log_tx,
                        &router_monitor,
                    )
                    .await;
                }
            }
        }

        Opportunity::Triangular(opp) => {
            broadcast_log(
                log_tx,
                "opportunity",
                &format!(
                    "[{}] triangular {} | profit=${:.4} | {}/{}/{}",
                    cfg.name, opp.triplet_id, opp.profit_usd, opp.router_ab_id, opp.router_bc_id, opp.router_ca_id
                ),
                Some(serde_json::json!({
                    "chain":        cfg.name,
                    "triplet_id":   opp.triplet_id,
                    "profit_usd":   opp.profit_usd,
                    "router_ab":    opp.router_ab_id,
                    "router_bc":    opp.router_bc_id,
                    "router_ca":    opp.router_ca_id,
                })),
            );

            pending_pairs.insert(fingerprint.clone());

            let dry_run = shared_state.read().await.dry_run;
            let mut exec = executor.lock().await;
            exec.dry_run = dry_run;

            let exec_start = std::time::Instant::now();
            let router_ids = vec![
                opp.router_ab_id.clone(),
                opp.router_bc_id.clone(),
                opp.router_ca_id.clone(),
            ];

            match exec.execute_triangular(provider.as_ref(), opp).await {
                Ok(tx_hash) => {
                    let exec_time_ms = exec_start.elapsed().as_millis() as u64;
                    handle_execution_success(
                        &tx_hash,
                        &fingerprint,
                        opp.profit_usd,
                        &opp.triplet_id,
                        &router_ids,
                        pending_pairs,
                        consecutive_failures,
                        dry_run,
                        cfg,
                        shared_state,
                        log_tx,
                        metrics,
                        &router_monitor,
                        exec_time_ms,
                    )
                    .await;
                }
                Err(e) => {
                    handle_execution_failure(
                        e,
                        &fingerprint,
                        &router_ids,
                        pending_pairs,
                        cooldowns,
                        consecutive_failures,
                        cfg,
                        shared_state,
                        metrics,
                        log_tx,
                        &router_monitor,
                    )
                    .await;
                }
            }
        }
    }
}

/// Handle successful execution (both 2-hop and triangular).
async fn handle_execution_success(
    tx_hash: &str,
    pair_id: &str,
    profit_usd: f64,
    display_id: &str,
    router_ids: &[String],
    pending_pairs: &mut HashSet<String>,
    consecutive_failures: &mut u32,
    dry_run: bool,
    cfg: &ChainConfig,
    shared_state: &SharedState,
    log_tx: &LogBroadcaster,
    metrics: &Arc<Metrics>,
    router_monitor: &Arc<crate::router_health::RouterHealthMonitor>,
    execution_time_ms: u64,
) {
    *consecutive_failures = 0;
    pending_pairs.remove(pair_id);

    // Record router health (success for all routers involved)
    for router_id in router_ids {
        router_monitor.record_success(router_id, execution_time_ms);
    }

    let dry_label = if dry_run { "true" } else { "false" };
    metrics.executed.with_label_values(&[&cfg.name, dry_label]).inc();
    if !dry_run {
        metrics.profit_usd.with_label_values(&[&cfg.name]).add(profit_usd);
    }

    let mut state = shared_state.write().await;
    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
        chain.total_attempts += 1;
        chain.total_success += 1;
        chain.total_profit_usd += profit_usd;
    }

    broadcast_log(
        log_tx,
        "trade",
        &format!(
            "[{}] tx={} | pair={} | profit=${:.4}",
            cfg.name,
            &tx_hash[..10.min(tx_hash.len())],
            display_id,
            profit_usd
        ),
        Some(serde_json::json!({
            "chain":      cfg.name,
            "pair_id":    display_id,
            "profit_usd": profit_usd,
        })),
    );
}

/// Handle execution failure (both 2-hop and triangular).
async fn handle_execution_failure(
    error: anyhow::Error,
    pair_id: &str,
    router_ids: &[String],
    pending_pairs: &mut HashSet<String>,
    cooldowns: &mut HashMap<String, Instant>,
    consecutive_failures: &mut u32,
    cfg: &ChainConfig,
    shared_state: &SharedState,
    metrics: &Arc<Metrics>,
    log_tx: &LogBroadcaster,
    router_monitor: &Arc<crate::router_health::RouterHealthMonitor>,
) {
    pending_pairs.remove(pair_id);
    cooldowns.insert(pair_id.to_string(), Instant::now());

    // Gas-profitability rejects ("below threshold") are pre-execution profit checks —
    // not real execution failures. Skip circuit-breaker accounting entirely.
    let err_str = error.to_string();
    if err_str.contains("below threshold") {
        debug!("[{}] Skipped (unprofitable after gas): {}", cfg.name, err_str);
        return;
    }

    *consecutive_failures += 1;

    // Record router health (failure for all routers involved)
    for router_id in router_ids {
        router_monitor.record_failure(router_id);
    }

    metrics.failures.with_label_values(&[&cfg.name]).inc();

    let mut state = shared_state.write().await;
    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
        chain.total_attempts += 1;
    }

    // Circuit breaker
    if *consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
        warn!(
            "[{}] Circuit breaker: {} consecutive failures — pausing chain",
            cfg.name, consecutive_failures
        );
        if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
            chain.paused = true;
        }
        broadcast_log(
            log_tx,
            "error",
            &format!(
                "[{}] Circuit breaker triggered ({} failures) — chain paused. Resume via API.",
                cfg.name, consecutive_failures
            ),
            None,
        );
    }

    broadcast_log(
        log_tx,
        "error",
        &format!("[{}] Execute failed: {}", cfg.name, error),
        None,
    );
}

async fn update_native_price<P: Provider>(
    strategy: &Arc<RwLock<Strategy>>,
    executor: &Arc<Mutex<Executor>>,
    provider: &Arc<P>,
    routers: &[RouterConfig],
    pairs: &[PairConfig],
    cfg: &ChainConfig,
) {
    use crate::abi::{IUniswapV2Router02, IQuoterV2};
    use alloy::primitives::{U256, Uint};
    use crate::config::RouterType;

    // Special case: xDAI and other USD-pegged stablecoins
    if cfg.native_currency == "xDAI" {
        { let mut strat = strategy.write().await; strat.update_native_price(1.0); }
        { let mut exec = executor.lock().await; exec.update_native_price(1.0); }
        return;
    }

    // Parse wrapped native address from config (WETH, WMATIC, etc.)
    let wrapped_native: Address = match cfg.wrapped_native.parse() {
        Ok(a) => a,
        Err(_) => {
            warn!("[{}] Invalid wrapped_native address: {}", cfg.name, cfg.wrapped_native);
            return;
        }
    };

    // Find a pair: WrappedNative → Stablecoin (USDC, USDT, DAI)
    let stable_pair = pairs.iter().find(|p| {
        p.chain_id == cfg.id
            && p.token_in == cfg.wrapped_native
            && ["USDC", "USDT", "DAI"].contains(&p.token_out_symbol.as_str())
    });
    let stable_pair = match stable_pair { Some(p) => p, None => return };

    let token_out: Address = match stable_pair.token_out.parse() { Ok(a) => a, Err(_) => return };
    // Use 0.01 native token to minimise price impact on thin pools (result scaled ×100).
    // Quoting 1 full ETH on a low-TVL pool shifts the price significantly (~$200 off).
    let probe_amount = U256::from(10_000_000_000_000_000u128); // 0.01 ETH (1e16 wei)
    let probe_scale = 100_u128; // multiply back to get per-1-ETH price

    // ── Try V3 QuoterV2 first (more accurate on chains with thin V2 pools) ──
    // Iterate ALL V3 routers, each using its own quoter (per-router takes priority
    // over chain-level). Keep the HIGHEST price across all routers and fee tiers —
    // the deepest pool gives the most accurate market price.
    {
        let mut best_v3_price = 0.0;
        for router in routers.iter().filter(|r| r.chain_id == cfg.id && r.router_type == RouterType::V3) {
            // Effective quoter: per-router first, chain-level fallback
            let quoter_str = router.quoter_address.as_deref()
                .map(str::to_owned)
                .or_else(|| cfg.quoter_v2_address.clone());
            let quoter = match quoter_str.as_deref().and_then(|s| s.parse::<Address>().ok()) {
                Some(q) => q,
                None => continue,
            };
            let tiers: Vec<u32> = if router.fee_tiers.is_empty() { vec![500u32, 3000] } else { router.fee_tiers.clone() };
            for fee in &tiers {
                if let Ok(r) = IQuoterV2::new(quoter, provider.as_ref())
                    .quoteExactInputSingle(IQuoterV2::QuoteExactInputSingleParams {
                        tokenIn: wrapped_native,
                        tokenOut: token_out,
                        amountIn: probe_amount,
                        fee: Uint::from(*fee),
                        sqrtPriceLimitX96: Uint::ZERO,
                    })
                    .call()
                    .await
                {
                    if !r.amountOut.is_zero() {
                        let price = r.amountOut.to::<u128>() as f64 * probe_scale as f64 / 1_000_000.0;
                        debug!("[{}] {} price V3 router={} fee={} → ${:.2}", cfg.name, cfg.native_currency, router.id, fee, price);
                        if price > best_v3_price { best_v3_price = price; }
                    }
                }
            }
        }
        if best_v3_price > 0.0 {
            debug!("[{}] {} price (V3 best): ${:.2}", cfg.name, cfg.native_currency, best_v3_price);
            { let mut strat = strategy.write().await; strat.update_native_price(best_v3_price); }
            { let mut exec = executor.lock().await; exec.update_native_price(best_v3_price); }
            return;
        }
    }

    // ── Fall back to V2 getAmountsOut ──
    for router in routers.iter().filter(|r| r.chain_id == cfg.id && r.router_type == RouterType::V2) {
        let router_addr: Address = match router.address.parse() { Ok(a) => a, Err(_) => continue };

        if let Ok(amounts) = IUniswapV2Router02::new(router_addr, provider.as_ref())
            .getAmountsOut(probe_amount, vec![wrapped_native, token_out])
            .call()
            .await
        {
            if let Some(&out) = amounts.last() {
                if !out.is_zero() {
                    let price = out.to::<u128>() as f64 * probe_scale as f64 / 1_000_000.0;
                    debug!("[{}] {} price (V2 fallback) updated: ${:.2}", cfg.name, cfg.native_currency, price);
                    { let mut strat = strategy.write().await; strat.update_native_price(price); }
                    { let mut exec = executor.lock().await; exec.update_native_price(price); }
                    return;
                }
            }
        }
    }
}

// ─── RPC connection with fallback ─────────────────────────────────────────────

/// Try `cfg.ws_rpc` first, then each entry in `cfg.ws_rpc_fallbacks` in order.
/// All URLs should be WebSocket endpoints (wss:// or ws://).
/// HTTP fallback URLs in ws_rpc_fallbacks will connect successfully for eth_call
/// but subscription-based events will fail gracefully — periodic poll scanning continues.
async fn connect_with_fallback(
    cfg: &ChainConfig,
    wallet: EthereumWallet,
) -> Result<impl Provider + Clone + 'static> {
    let urls: Vec<&str> = std::iter::once(cfg.ws_rpc.as_str())
        .chain(cfg.ws_rpc_fallbacks.iter().map(String::as_str))
        .collect();

    let mut last_err = String::new();
    for (i, url) in urls.iter().enumerate() {
        match ProviderBuilder::<_, _, Ethereum>::new()
            .wallet(wallet.clone())
            .connect_ws(WsConnect::new(*url))
            .await
        {
            Ok(p) => {
                if i > 0 {
                    info!("[{}] Connected via fallback RPC #{}: {}", cfg.name, i, url);
                }
                return Ok(p);
            }
            Err(e) => {
                warn!("[{}] RPC endpoint {} failed: {}", cfg.name, url, e);
                last_err = e.to_string();
            }
        }
    }

    Err(anyhow::anyhow!(
        "all {} RPC endpoint(s) exhausted — last error: {}",
        urls.len(),
        last_err
    ))
}
