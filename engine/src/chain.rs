use alloy::network::Ethereum;
use alloy::primitives::{Address, U256};
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

use crate::abi::{IERC20, IRouterWithFactory, IUniswapV2Factory, ISolidlyFactory, IUniswapV2Pair, IUniswapV3Factory, IUniswapV3Pool};
use crate::api::{broadcast_log, ChainStats, LogBroadcaster, SharedState};
use crate::config::{self, ChainConfig, PairConfig, RouterConfig, RouterType};
use crate::db::Database;
use crate::executor::{Executor, TxPrep};
use crate::listener::{Listener, SwapEvent};
use crate::metrics::Metrics;
use crate::pool_cache::{PoolCache, PoolInfo, V3PoolState};
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

    // ── Pool discovery: populate V2/Solidly-volatile reserve cache ────────────
    // Eliminates per-block eth_calls for xy=k routers — reserves stay fresh via
    // on-chain Sync events (listener.rs subscribes and updates the cache live).
    let pool_cache = Arc::new(PoolCache::new());
    discover_pools(&chain_routers, &chain_pairs, provider.as_ref(), &pool_cache).await;
    info!("[{}] Pool cache: {} V2/Solidly pools discovered", cfg.name, pool_cache.by_address.len());

    // ── V3 pool discovery: seed sqrtPriceX96 + liquidity from slot0() ─────────
    // Subscribes to V3 Swap events after startup to keep state fresh with zero RPC cost.
    discover_v3_pools(&chain_routers, &chain_pairs, provider.as_ref(), &pool_cache).await;
    info!("[{}] V3 pool cache: {} pools seeded", cfg.name, pool_cache.v3_by_address.len());

    let strategy = Arc::new(RwLock::new(Strategy::new(
        cfg.id,
        chain_pairs.clone(),
        chain_routers.clone(),
        cfg.min_profit_usd,
        quoter_v2_address,
        pool_cache.clone(),
        cfg.rpc_concurrency,
    )));

    // Populate SyncSwap pool cache at startup (no-op if no SyncSwap routers configured)
    {
        let mut strat = strategy.write().await;
        strat.populate_syncswap_pools(provider.as_ref()).await;
    }

    // ── Contract balance cache ─────────────────────────────────────────────────
    // Caches the ArbitrageExecutor's token_in balances. Refreshed every 30s and
    // immediately after each confirmed trade. Used by optimize() to cap the probe
    // range — prevents querying amounts the contract cannot cover.
    //
    // Only token_in addresses matter: token_out balances are not held by the contract
    // (the arb is atomic: in → profit → back to token_in).
    let contract_balances: Arc<RwLock<HashMap<Address, U256>>> = {
        let mut initial: HashMap<Address, U256> = HashMap::new();
        for pair in &chain_pairs {
            if let Ok(addr) = pair.token_in.parse::<Address>() {
                initial.insert(addr, U256::ZERO);
            }
        }
        Arc::new(RwLock::new(initial))
    };
    // Fetch initial balances so the optimizer has data from the first scan.
    refresh_contract_balances(provider.as_ref(), contract_addr, &contract_balances).await;
    // Startup funding check: warn for any underfunded pair.
    check_contract_funding(&cfg.name, &chain_pairs, contract_addr, &contract_balances).await;
    // Wire balances into executor for post-trade immediate refresh.
    { executor.lock().await.contract_balances = Some(contract_balances.clone()); }

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
                total_failed: 0,
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
    let listener = Listener::new(cfg.id, cfg.name.clone());
    let (swap_tx, mut swap_rx) = mpsc::channel::<SwapEvent>(256);
    {
        let provider_clone = (*provider).clone();
        listener.subscribe(provider_clone, swap_tx, pool_cache.clone()).await?;
    }

    // ── Block-header subscription (drives immediate scanning on each new block) ──
    let (block_tx, mut block_rx) = mpsc::channel::<(u64, u128)>(64);
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
                            let base_fee = header.base_fee_per_gas.unwrap_or(1_000_000_000) as u128;
                            debug!("[{}] New block #{}", chain_name_b, num);
                            let _ = block_tx_b.send((num, base_fee)).await;
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
    // Tracks when the last full evaluate ran — used to gate the poll_tick fallback.
    let mut last_scan_at = Instant::now() - Duration::from_secs(60);
    // Per-pool burst guard for targeted Swap-triggered scans (50ms cooldown per pool).
    let mut pool_last_scan: HashMap<Address, Instant> = HashMap::new();
    // scan_interval_ms controls how often the poll_tick can fire a scan.
    // Decoupled from block_time_ms: set lower to scan more often between blocks.
    // Default 1000ms gives ~60 scans/min on a 2s-block chain (alternates block/poll).
    let poll_interval = Duration::from_millis(cfg.scan_interval_ms);
    let mut poll_tick     = tokio::time::interval(poll_interval);
    let mut price_tick    = tokio::time::interval(Duration::from_secs(60));
    let mut config_tick   = tokio::time::interval(Duration::from_secs(30));
    let mut balance_tick  = tokio::time::interval(Duration::from_secs(30));

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
                // Sync confirmed_success from executor atomics into shared_state
                // (receipt background tasks update the atomics, not shared_state directly).
                let confirmed_ok = {
                    let exec = executor.lock().await;
                    let ok = exec.confirmed_success.load(Ordering::Relaxed);
                    let fail = exec.confirmed_failed.load(Ordering::Relaxed);
                    let profit = f64::from_bits(exec.confirmed_profit_usd_bits.load(Ordering::Relaxed));
                    drop(exec);
                    // Sync confirmed counts from executor atomics → shared_state
                    let mut state = shared_state.write().await;
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.total_success = ok;
                        chain.total_failed = fail;
                        chain.total_profit_usd = profit;
                    }
                    ok
                };
                let (scans, attempts, success) = {
                    let state = shared_state.read().await;
                    state.chains.iter()
                        .find(|c| c.chain_id == cfg.id)
                        .map(|c| (c.total_scans, c.total_attempts, c.total_success))
                        .unwrap_or((0, 0, confirmed_ok))
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

            // ── Periodic contract balance refresh ─────────────────────────────
            _ = balance_tick.tick() => {
                refresh_contract_balances(provider.as_ref(), contract_addr, &contract_balances).await;
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

                        // Only trigger the network call if pairs/routers actually changed
                        let structure_changed = strat.pairs.len() != new_pairs.len()
                            || strat.routers.len() != new_routers.len();

                        strat.pairs = new_pairs;
                        strat.routers = new_routers;

                        if structure_changed {
                            info!("[{}] Config structure changed — refreshing SyncSwap cache", cfg.name);
                            strat.populate_syncswap_pools(provider.as_ref()).await;
                        }

                        if strat.min_profit_usd != new_min_profit {
                            strat.min_profit_usd = new_min_profit;
                            info!("[{}] Config hot-reloaded (min_profit=${:.2})", cfg.name, new_min_profit);
                        }
                    } // strategy write lock released here — before acquiring executor lock

                    {
                        let mut exec = executor.lock().await;
                        exec.min_profit_usd = new_min_profit;
                    }
                }
            }

            // ── New block header → immediate scan ─────────────────────────────
            Some((block_num, base_fee)) = block_rx.recv() => {
                // Pre-populate gas price cache from block header — eliminates eth_gasPrice RPC calls.
                executor.lock().await.update_gas_price(base_fee);

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
                    &contract_balances, None,
                ).await;
                last_scan_at = Instant::now();
            }

            // ── Periodic fallback scan (safety net if block subscription is down) ──
            _ = poll_tick.tick() => {
                // True fallback: skip if the block subscription fired a scan recently.
                // Without this guard, poll_tick fires every 4s independently, doubling
                // the scan rate to 45/min (30 block + 15 poll) instead of the intended 30.
                if last_scan_at.elapsed() < poll_interval {
                    debug!("[{}] poll_tick skipped — block scan {}ms ago", cfg.name,
                           last_scan_at.elapsed().as_millis());
                    continue;
                }
                let state = shared_state.read().await;
                let chain_paused = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                if state.paused || chain_paused { continue; }
                drop(state);

                evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &router_monitor, &best_raw_profit, &best_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                    &contract_balances, None,
                ).await;
                last_scan_at = Instant::now();
            }

            // ── Swap event → instant targeted scan ───────────────────────────────
            // Reacts to intra-block price moves with zero rate limit.
            // Only evaluates pairs that contain the tokens from the swapped pool,
            // so the scan takes <0.1ms (pure local math, 0 RPC).
            // 50ms per-pool burst guard prevents CPU saturation from event floods.
            // last_scan_at is NOT updated — targeted scans must not suppress the
            // block-triggered full scan or the poll_tick fallback.
            Some(event) = swap_rx.recv() => {
                // Per-pool burst protection
                if let Some(&fired_at) = pool_last_scan.get(&event.pool) {
                    if fired_at.elapsed() < Duration::from_millis(50) {
                        continue;
                    }
                }
                pool_last_scan.insert(event.pool, Instant::now());

                // Identify which tokens moved in this pool (V2/Solidly/SyncSwap + V3)
                let tokens = {
                    let strat = strategy.read().await;
                    strat.pool_cache.get_pool_tokens(event.pool)
                };
                let (tok_a, tok_b) = match tokens {
                    Some(t) => t,
                    None => continue, // Pool not in cache — block scan covers it
                };

                // Find which configured pairs involve these tokens
                let (pair_mask, token_filter) = {
                    let strat = strategy.read().await;
                    let indices = strat.pairs_for_tokens(tok_a, tok_b);
                    (indices.into_iter().collect::<HashSet<usize>>(), vec![tok_a, tok_b])
                };
                if pair_mask.is_empty() { continue; }

                let state = shared_state.read().await;
                let chain_paused = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                if state.paused || chain_paused { continue; }
                drop(state);

                {
                    let mut state = shared_state.write().await;
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.total_scans += 1;
                    }
                }

                debug!(
                    "[{}] Swap on {:?} — targeted scan ({} pairs)",
                    cfg.name, event.pool, pair_mask.len()
                );

                evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &router_monitor, &best_raw_profit, &best_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                    &contract_balances, Some((pair_mask, token_filter)),
                ).await;
                // NOTE: last_scan_at intentionally NOT updated here.
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
    contract_balances: &Arc<RwLock<HashMap<Address, U256>>>,
    // None = full scan; Some((pair_mask, token_filter)) = targeted scan from a Swap event.
    targeted: Option<(HashSet<usize>, Vec<Address>)>,
) {
    // ── Parallel detection: 2-hop + triangular ────────────────────────────────

    let all_opportunities = {
        let strat = strategy.read().await;
        let ((opps_2hop, best_2hop, fwd_ok, multi_dex, spread_2hop, active_pairs), (opps_tri, best_tri)) = tokio::join!(
            strat.evaluate(provider.as_ref(), targeted.as_ref().map(|(m, _)| m)),
            strat.detect_triangular(provider.as_ref(), 5, targeted.as_ref().map(|(_, t)| t.as_slice())),
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
            let max_bal = {
                let bals = contract_balances.read().await;
                bals.get(&opp.token_in).copied()
            };
            let optimized = {
                let strat = strategy.read().await;
                strat.optimize(opp, provider.as_ref(), max_bal).await
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
            let exec_start = std::time::Instant::now();
            let router_ids = vec![optimized.router_a_id.clone(), optimized.router_b_id.clone()];

            if dry_run {
                // Dry-run: simulate with lock held (test mode, latency not critical)
                let mut exec = executor.lock().await;
                exec.dry_run = true;
                match exec.execute(provider.as_ref(), &optimized).await {
                    Ok(tx_hash) => {
                        let exec_time_ms = exec_start.elapsed().as_millis() as u64;
                        drop(exec);
                        cooldowns.insert(fingerprint.clone(), Instant::now());
                        handle_execution_success(
                            &tx_hash, &fingerprint, optimized.profit_usd, &optimized.pair_id,
                            &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                            shared_state, log_tx, metrics, &router_monitor, exec_time_ms,
                        ).await;
                    }
                    Err(e) => {
                        drop(exec);
                        handle_execution_failure(
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns,
                            consecutive_failures, cfg, shared_state, metrics, log_tx, &router_monitor,
                        ).await;
                    }
                }
            } else {
                // Live: prepare (nonce + tx build) with brief lock, release, then send
                // without holding the mutex. Prevents blocking the select! loop for
                // the 500ms–2s it takes send_transaction to complete on Linea.
                let prep_result: Result<TxPrep> = {
                    let mut exec = executor.lock().await;
                    exec.dry_run = false;
                    exec.prepare_2hop(provider.as_ref(), &optimized).await
                    // lock drops here
                };
                match prep_result {
                    Err(e) => {
                        handle_execution_failure(
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns,
                            consecutive_failures, cfg, shared_state, metrics, log_tx, &router_monitor,
                        ).await;
                    }
                    Ok(prep) => {
                        // Pre-flight simulation: verifies profitability on-chain before
                        // spending Linea gas. Catches phantom arbs from V3 virtual-reserve
                        // approximation. Does NOT hold the executor mutex.
                        if let Err(e) = provider.call(prep.tx.clone()).await {
                            { let mut exec = executor.lock().await; exec.record_failed(); }
                            pending_pairs.remove(&fingerprint);
                            cooldowns.insert(fingerprint.clone(), Instant::now());
                            warn!("[{}] Pre-flight rejected phantom arb: {}", cfg.name, e);
                            return;
                        }
                        let send_start = std::time::Instant::now();
                        match provider.send_transaction(prep.tx.clone()).await {
                            Ok(pending) => {
                                let tx_hash = format!("{:?}", pending.tx_hash());
                                let elapsed_send = send_start.elapsed().as_millis() as u64;
                                info!(
                                    "[{}] Arb sent ({}ms) | gross=${:.4} net=${:.4} | tx={}",
                                    cfg.name, elapsed_send, prep.profit_usd, prep.net_profit_usd,
                                    &tx_hash[..10.min(tx_hash.len())],
                                );
                                { let mut exec = executor.lock().await; exec.record_sent(&tx_hash, elapsed_send); }
                                cooldowns.insert(fingerprint.clone(), Instant::now());

                                // Fire-and-forget receipt task using cloned prep fields
                                let tx_hash_bg = tx_hash.clone();
                                let provider_bg = provider.clone();
                                let confirmed_success_bg = prep.confirmed_success.clone();
                                let confirmed_failed_bg = prep.confirmed_failed.clone();
                                let confirmed_profit_bg = prep.confirmed_profit_bits.clone();
                                let chain_name_bg = prep.chain_name.clone();
                                let db_bg = prep.db.clone();
                                let chain_id_bg = prep.chain_id;
                                let opp_id_bg = prep.opp_id.clone();
                                let router_a_bg = prep.router_a.clone();
                                let router_b_bg = prep.router_b.clone();
                                let profit_bg = prep.profit_usd;
                                let contract_addr_bg = prep.contract_addr;
                                let contract_balances_bg = prep.contract_balances.clone();
                                tokio::spawn(async move {
                                    match pending.get_receipt().await {
                                        Ok(receipt) => {
                                            if receipt.status() {
                                                confirmed_success_bg.fetch_add(1, Ordering::Relaxed);
                                                confirmed_profit_bg.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                                                    Some((f64::from_bits(bits) + profit_bg).to_bits())
                                                }).ok();
                                                info!("[{}] ✓ confirmed | gas={} | tx={}", chain_name_bg, receipt.gas_used, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                                                if let Some(bals) = contract_balances_bg {
                                                    let token_addrs: Vec<Address> = { let b = bals.read().await; b.keys().copied().collect() };
                                                    for token in token_addrs {
                                                        if let Ok(bal) = IERC20::new(token, &provider_bg).balanceOf(contract_addr_bg).call().await {
                                                            let mut b = bals.write().await;
                                                            b.insert(token, bal);
                                                        }
                                                    }
                                                }
                                            } else {
                                                confirmed_failed_bg.fetch_add(1, Ordering::Relaxed);
                                                warn!("[{}] ✗ reverted | tx={}", chain_name_bg, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                                            }
                                            if let Some(db) = db_bg {
                                                let success = receipt.status();
                                                let _ = tokio::task::spawn_blocking(move || {
                                                    let _ = db.insert_trade(chain_id_bg, &chain_name_bg, &opp_id_bg, &router_a_bg, &router_b_bg, None, profit_bg, success, &tx_hash_bg, false);
                                                }).await;
                                            }
                                        }
                                        Err(e) => {
                                            confirmed_failed_bg.fetch_add(1, Ordering::Relaxed);
                                            warn!("[{}] Receipt error for {}: {}", chain_name_bg, &tx_hash_bg[..10.min(tx_hash_bg.len())], e);
                                        }
                                    }
                                    drop(provider_bg);
                                });

                                let exec_time_ms = exec_start.elapsed().as_millis() as u64;
                                handle_execution_success(
                                    &tx_hash, &fingerprint, optimized.profit_usd, &optimized.pair_id,
                                    &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                                    shared_state, log_tx, metrics, &router_monitor, exec_time_ms,
                                ).await;
                            }
                            Err(e) => {
                                { let mut exec = executor.lock().await; exec.record_failed(); }
                                if let Some(db) = prep.db.clone() {
                                    let cn = prep.chain_name.clone();
                                    let cid = prep.chain_id;
                                    let oid = prep.opp_id.clone();
                                    let ra = prep.router_a.clone();
                                    let rb = prep.router_b.clone();
                                    let p = prep.profit_usd;
                                    tokio::task::spawn_blocking(move || {
                                        let _ = db.insert_trade(cid, &cn, &oid, &ra, &rb, None, p, false, "", false);
                                    });
                                }
                                handle_execution_failure(
                                    anyhow::anyhow!("Send failed: {}", e),
                                    &fingerprint, &router_ids, pending_pairs, cooldowns,
                                    consecutive_failures, cfg, shared_state, metrics, log_tx, &router_monitor,
                                ).await;
                            }
                        }
                    }
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
            let exec_start = std::time::Instant::now();
            let router_ids = vec![
                opp.router_ab_id.clone(),
                opp.router_bc_id.clone(),
                opp.router_ca_id.clone(),
            ];

            if dry_run {
                // Dry-run: simulate with lock held (test mode, latency not critical)
                let mut exec = executor.lock().await;
                exec.dry_run = true;
                match exec.execute_triangular(provider.as_ref(), opp).await {
                    Ok(tx_hash) => {
                        let exec_time_ms = exec_start.elapsed().as_millis() as u64;
                        drop(exec);
                        cooldowns.insert(fingerprint.clone(), Instant::now());
                        handle_execution_success(
                            &tx_hash, &fingerprint, opp.profit_usd, &opp.triplet_id,
                            &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                            shared_state, log_tx, metrics, &router_monitor, exec_time_ms,
                        ).await;
                    }
                    Err(e) => {
                        drop(exec);
                        handle_execution_failure(
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns,
                            consecutive_failures, cfg, shared_state, metrics, log_tx, &router_monitor,
                        ).await;
                    }
                }
            } else {
                // Live: prepare with brief lock, release, then send without holding mutex
                let prep_result: Result<TxPrep> = {
                    let mut exec = executor.lock().await;
                    exec.dry_run = false;
                    exec.prepare_triangular(provider.as_ref(), opp).await
                    // lock drops here
                };
                match prep_result {
                    Err(e) => {
                        handle_execution_failure(
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns,
                            consecutive_failures, cfg, shared_state, metrics, log_tx, &router_monitor,
                        ).await;
                    }
                    Ok(prep) => {
                        // Pre-flight simulation: same gate as 2-hop path.
                        if let Err(e) = provider.call(prep.tx.clone()).await {
                            { let mut exec = executor.lock().await; exec.record_failed(); }
                            pending_pairs.remove(&fingerprint);
                            cooldowns.insert(fingerprint.clone(), Instant::now());
                            warn!("[{}] Pre-flight rejected triangular phantom arb: {}", cfg.name, e);
                            return;
                        }
                        let send_start = std::time::Instant::now();
                        match provider.send_transaction(prep.tx.clone()).await {
                            Ok(pending) => {
                                let tx_hash = format!("{:?}", pending.tx_hash());
                                let elapsed_send = send_start.elapsed().as_millis() as u64;
                                info!(
                                    "[{}] Triangular arb sent ({}ms) | gross=${:.4} net=${:.4} | tx={}",
                                    cfg.name, elapsed_send, prep.profit_usd, prep.net_profit_usd,
                                    &tx_hash[..10.min(tx_hash.len())],
                                );
                                { let mut exec = executor.lock().await; exec.record_sent(&tx_hash, elapsed_send); }
                                cooldowns.insert(fingerprint.clone(), Instant::now());

                                // Fire-and-forget receipt task
                                let tx_hash_bg = tx_hash.clone();
                                let provider_bg = provider.clone();
                                let confirmed_success_bg = prep.confirmed_success.clone();
                                let confirmed_failed_bg = prep.confirmed_failed.clone();
                                let confirmed_profit_bg = prep.confirmed_profit_bits.clone();
                                let chain_name_bg = prep.chain_name.clone();
                                let db_bg = prep.db.clone();
                                let chain_id_bg = prep.chain_id;
                                let opp_id_bg = prep.opp_id.clone();
                                let router_a_bg = prep.router_a.clone();
                                let router_b_bg = prep.router_b.clone();
                                let router_c_bg = prep.router_c.clone();
                                let profit_bg = prep.profit_usd;
                                let contract_addr_bg = prep.contract_addr;
                                let contract_balances_bg = prep.contract_balances.clone();
                                tokio::spawn(async move {
                                    match pending.get_receipt().await {
                                        Ok(receipt) => {
                                            if receipt.status() {
                                                confirmed_success_bg.fetch_add(1, Ordering::Relaxed);
                                                confirmed_profit_bg.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                                                    Some((f64::from_bits(bits) + profit_bg).to_bits())
                                                }).ok();
                                                info!("[{}] ✓ triangular confirmed | gas={} | tx={}", chain_name_bg, receipt.gas_used, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                                                if let Some(bals) = contract_balances_bg {
                                                    let token_addrs: Vec<Address> = { let b = bals.read().await; b.keys().copied().collect() };
                                                    for token in token_addrs {
                                                        if let Ok(bal) = IERC20::new(token, &provider_bg).balanceOf(contract_addr_bg).call().await {
                                                            let mut b = bals.write().await;
                                                            b.insert(token, bal);
                                                        }
                                                    }
                                                }
                                            } else {
                                                confirmed_failed_bg.fetch_add(1, Ordering::Relaxed);
                                                warn!("[{}] ✗ triangular reverted | tx={}", chain_name_bg, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                                            }
                                            if let Some(db) = db_bg {
                                                let success = receipt.status();
                                                let _ = tokio::task::spawn_blocking(move || {
                                                    let _ = db.insert_trade(chain_id_bg, &chain_name_bg, &opp_id_bg, &router_a_bg, &router_b_bg, router_c_bg.as_deref(), profit_bg, success, &tx_hash_bg, false);
                                                }).await;
                                            }
                                        }
                                        Err(e) => {
                                            confirmed_failed_bg.fetch_add(1, Ordering::Relaxed);
                                            warn!("[{}] Triangular receipt error for {}: {}", chain_name_bg, &tx_hash_bg[..10.min(tx_hash_bg.len())], e);
                                        }
                                    }
                                    drop(provider_bg);
                                });

                                let exec_time_ms = exec_start.elapsed().as_millis() as u64;
                                handle_execution_success(
                                    &tx_hash, &fingerprint, opp.profit_usd, &opp.triplet_id,
                                    &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                                    shared_state, log_tx, metrics, &router_monitor, exec_time_ms,
                                ).await;
                            }
                            Err(e) => {
                                { let mut exec = executor.lock().await; exec.record_failed(); }
                                if let Some(db) = prep.db.clone() {
                                    let cn = prep.chain_name.clone();
                                    let cid = prep.chain_id;
                                    let oid = prep.opp_id.clone();
                                    let ra = prep.router_a.clone();
                                    let rb = prep.router_b.clone();
                                    let rc = prep.router_c.clone();
                                    let p = prep.profit_usd;
                                    tokio::task::spawn_blocking(move || {
                                        let _ = db.insert_trade(cid, &cn, &oid, &ra, &rb, rc.as_deref(), p, false, "", false);
                                    });
                                }
                                handle_execution_failure(
                                    anyhow::anyhow!("Triangular send failed: {}", e),
                                    &fingerprint, &router_ids, pending_pairs, cooldowns,
                                    consecutive_failures, cfg, shared_state, metrics, log_tx, &router_monitor,
                                ).await;
                            }
                        }
                    }
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

    // total_attempts = tx sent; total_success is updated in the heartbeat
    // from executor.confirmed_success (set after receipt confirms on-chain).
    let mut state = shared_state.write().await;
    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
        chain.total_attempts += 1;
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

    // RPC rate-limit (429) is an infra issue, not a strategy failure.
    // Don't increment the circuit-breaker counter — just log and cool down.
    if err_str.contains("429")
        || err_str.contains("compute units")
        || err_str.contains("rate limit")
    {
        broadcast_log(
            log_tx,
            "error",
            &format!("[{}] Execute failed: {}", cfg.name, error),
            None,
        );
        return;
    }

    // Pre-flight simulation failures are expected — the opportunity closed between
    // detection and execution. Don't count toward circuit breaker; this is working
    // as intended. Only tx SEND failures indicate systemic problems (nonce, wallet).
    if err_str.contains("simulation failed") || err_str.contains("Pre-flight") {
        broadcast_log(
            log_tx,
            "warn",
            &format!("[{}] Pre-flight failed (skipping): {}", cfg.name, error),
            None,
        );
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
    let decimal_scale = 10_f64.powi(stable_pair.token_out_decimals as i32);

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
                        let price = r.amountOut.to::<u128>() as f64 * probe_scale as f64 / decimal_scale;
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
                    let price = out.to::<u128>() as f64 * probe_scale as f64 / decimal_scale;
                    debug!("[{}] {} price (V2 fallback) updated: ${:.2}", cfg.name, cfg.native_currency, price);
                    { let mut strat = strategy.write().await; strat.update_native_price(price); }
                    { let mut exec = executor.lock().await; exec.update_native_price(price); }
                    return;
                }
            }
        }
    }
}

// ─── Contract balance helpers ─────────────────────────────────────────────────

/// Refresh the cached token_in balances held by the ArbitrageExecutor contract.
///
/// Called at startup and every 30s by the balance_tick timer. The balance map
/// is keyed by token address and initialised to U256::ZERO at startup; this
/// function overwrites each entry with the live on-chain value.
async fn refresh_contract_balances<P: Provider>(
    provider: &P,
    contract_addr: Address,
    balances: &Arc<RwLock<HashMap<Address, U256>>>,
) {
    let token_addrs: Vec<Address> = {
        let b = balances.read().await;
        b.keys().copied().collect()
    };
    for token in token_addrs {
        match IERC20::new(token, provider).balanceOf(contract_addr).call().await {
            Ok(bal) => {
                let mut b = balances.write().await;
                b.insert(token, bal);
            }
            Err(e) => {
                debug!("Failed to fetch balance for {:?}: {}", token, e);
            }
        }
    }
}

/// Startup check: log a warning for any pair whose token_in balance in the
/// ArbitrageExecutor contract is below the configured trade_amount.
/// The engine continues running — detection is still valid; only execution
/// will fail (caught by pre-flight simulation) until the contract is funded.
async fn check_contract_funding(
    chain_name: &str,
    pairs: &[crate::config::PairConfig],
    contract_addr: Address,
    balances: &Arc<RwLock<HashMap<Address, U256>>>,
) {
    let b = balances.read().await;
    for pair in pairs {
        let token_addr = match pair.token_in.parse::<Address>() {
            Ok(a) => a,
            Err(_) => continue,
        };
        let balance = b.get(&token_addr).copied().unwrap_or(U256::ZERO);
        let required = match crate::strategy::parse_amount_capped(
            &pair.trade_amount,
            pair.max_trade.as_deref(),
            pair.token_in_decimals,
        ) {
            Some(a) => a,
            None => continue,
        };
        if balance < required {
            warn!(
                "[{}] ⚠ Contract underfunded for {} | balance={} trade_amount={} {} | fund {}",
                chain_name,
                pair.id,
                balance,
                required,
                pair.token_in_symbol,
                contract_addr,
            );
        }
    }
}

// ─── Pool discovery (startup) ─────────────────────────────────────────────────

/// Discover V2 and Solidly-volatile pools for all configured pairs and routers.
/// Populates `pool_cache` with initial reserves so strategy can quote locally.
///
/// Three multicall rounds:
///   1. router.factory()  per V2/Solidly router
///   2. factory.getPair() per (router × pair)
///   3. pair.token0() + pair.getReserves() per discovered pool
async fn discover_pools<P: Provider>(
    routers: &[RouterConfig],
    pairs: &[PairConfig],
    provider: &P,
    pool_cache: &PoolCache,
) {
    use alloy::primitives::U256;
    use alloy::sol_types::SolCall;

    // ── Collect V2 and Solidly routers ────────────────────────────────────────
    struct RouterMeta { id: String, addr: Address, rtype: RouterType, fee_bps: u32 }
    let target_routers: Vec<RouterMeta> = routers
        .iter()
        .filter(|r| r.router_type == RouterType::V2 || r.router_type == RouterType::Solidly)
        .filter_map(|r| {
            let addr: Address = r.address.parse().ok()?;
            Some(RouterMeta { id: r.id.clone(), addr, rtype: r.router_type.clone(), fee_bps: r.fee_bps })
        })
        .collect();

    if target_routers.is_empty() { return; }

    // ── Round 1: factory address per router ───────────────────────────────────
    let factory_calls: Vec<(Address, Vec<u8>)> = target_routers
        .iter()
        .map(|r| (r.addr, IRouterWithFactory::factoryCall {}.abi_encode()))
        .collect();
    let factory_raw = discover_mc(factory_calls, provider).await;
    let factories: Vec<Option<Address>> = factory_raw
        .iter()
        .map(|r| {
            r.as_ref()
                .and_then(|raw| IRouterWithFactory::factoryCall::abi_decode_returns(raw).ok())
                .filter(|a: &Address| !a.is_zero())
        })
        .collect();

    // ── Round 2: getPair per (router × pair) ─────────────────────────────────
    // Solidly routers produce TWO calls per pair: volatile (stable:false) and
    // stable (stable:true). Both pools are discovered and cached independently.
    struct PairMeta { router_id: String, rtype: RouterType, fee_bps: u32, ta: Address, tb: Address, is_stable: bool }
    let mut pair_calls: Vec<(Address, Vec<u8>)> = Vec::new();
    let mut pair_metas: Vec<PairMeta> = Vec::new();

    for (ri, rm) in target_routers.iter().enumerate() {
        let factory = match factories[ri] { Some(f) => f, None => continue };
        for pair in pairs {
            let ta: Address = match pair.token_in.parse() { Ok(a) => a, Err(_) => continue };
            let tb: Address = match pair.token_out.parse() { Ok(a) => a, Err(_) => continue };
            match rm.rtype {
                RouterType::V2 => {
                    let cd = IUniswapV2Factory::getPairCall { tokenA: ta, tokenB: tb }.abi_encode();
                    pair_calls.push((factory, cd));
                    pair_metas.push(PairMeta { router_id: rm.id.clone(), rtype: rm.rtype.clone(), fee_bps: rm.fee_bps, ta, tb, is_stable: false });
                }
                RouterType::Solidly => {
                    // Volatile pool (xy=k)
                    let cd_vol = ISolidlyFactory::getPairCall { tokenA: ta, tokenB: tb, stable: false }.abi_encode();
                    pair_calls.push((factory, cd_vol));
                    pair_metas.push(PairMeta { router_id: rm.id.clone(), rtype: rm.rtype.clone(), fee_bps: rm.fee_bps, ta, tb, is_stable: false });
                    // Stable pool (x³y+xy³=k) — Solidly deploys a separate pool address
                    let cd_sta = ISolidlyFactory::getPairCall { tokenA: ta, tokenB: tb, stable: true }.abi_encode();
                    pair_calls.push((factory, cd_sta));
                    pair_metas.push(PairMeta { router_id: rm.id.clone(), rtype: rm.rtype.clone(), fee_bps: rm.fee_bps, ta, tb, is_stable: true });
                }
                _ => continue,
            }
        }
    }

    if pair_calls.is_empty() { return; }
    let pair_raw = discover_mc(pair_calls, provider).await;

    // ── Round 3: token0 + getReserves + decimals(ta) + decimals(tb) per pool ──
    // 4 calls per pool. The pool address from the stable=true call is a different
    // contract from stable=false, so `seen` naturally allows both.
    struct PoolMeta { pool: Address, router_id: String, rtype: RouterType, fee_bps: u32, ta: Address, tb: Address, is_stable: bool }
    let mut rv_calls: Vec<(Address, Vec<u8>)> = Vec::new();
    let mut pool_metas: Vec<PoolMeta> = Vec::new();
    let mut seen: std::collections::HashSet<Address> = std::collections::HashSet::new();

    fn decode_addr(raw: &[u8]) -> Option<Address> {
        if raw.len() >= 32 {
            let bytes: [u8; 20] = raw[12..32].try_into().ok()?;
            let a = Address::from(bytes);
            if !a.is_zero() { Some(a) } else { None }
        } else { None }
    }

    for (pm, raw_opt) in pair_metas.iter().zip(pair_raw.iter()) {
        let pool = match raw_opt.as_ref().and_then(|r| decode_addr(r)) { Some(a) => a, None => continue };
        if !seen.insert(pool) { continue; }
        rv_calls.push((pool, IUniswapV2Pair::token0Call {}.abi_encode()));
        rv_calls.push((pool, IUniswapV2Pair::getReservesCall {}.abi_encode()));
        rv_calls.push((pm.ta, IERC20::decimalsCall {}.abi_encode()));
        rv_calls.push((pm.tb, IERC20::decimalsCall {}.abi_encode()));
        pool_metas.push(PoolMeta {
            pool,
            router_id: pm.router_id.clone(),
            rtype: pm.rtype.clone(),
            fee_bps: pm.fee_bps,
            ta: pm.ta,
            tb: pm.tb,
            is_stable: pm.is_stable,
        });
    }

    if pool_metas.is_empty() { return; }
    let rv_raw = discover_mc(rv_calls, provider).await;

    // ── Insert into pool_cache ────────────────────────────────────────────────
    for (i, pm) in pool_metas.iter().enumerate() {
        let t0_raw  = match rv_raw.get(4 * i)     { Some(Some(r)) => r, _ => continue };
        let res_raw = match rv_raw.get(4 * i + 1) { Some(Some(r)) => r, _ => continue };
        // Decimals are best-effort — fall back to 18 if call reverted (e.g. non-standard tokens)
        let dec_ta: u8 = rv_raw.get(4 * i + 2).and_then(|r| r.as_ref())
            .and_then(|r| IERC20::decimalsCall::abi_decode_returns(r).ok())
            .unwrap_or(18);
        let dec_tb: u8 = rv_raw.get(4 * i + 3).and_then(|r| r.as_ref())
            .and_then(|r| IERC20::decimalsCall::abi_decode_returns(r).ok())
            .unwrap_or(18);

        let token0   = match IUniswapV2Pair::token0Call::abi_decode_returns(t0_raw).ok()    { Some(t) => t, None => continue };
        let reserves = match IUniswapV2Pair::getReservesCall::abi_decode_returns(res_raw).ok() { Some(r) => r, None => continue };

        // token1 = whichever of (ta, tb) is not token0
        let token1 = if pm.ta == token0 { pm.tb } else { pm.ta };

        // Map ta/tb decimals to token0/token1 order
        let (decimals0, decimals1) = if pm.ta == token0 { (dec_ta, dec_tb) } else { (dec_tb, dec_ta) };

        // Key suffix: Solidly volatile → "::volatile", Solidly stable → "::stable", V2 → unchanged
        let effective_id = match pm.rtype {
            RouterType::Solidly => if pm.is_stable {
                format!("{}::stable", pm.router_id)
            } else {
                format!("{}::volatile", pm.router_id)
            },
            _ => pm.router_id.clone(),
        };

        // last_sync = Instant::now(): startup reserves were just fetched from chain via
        // multicall — they ARE the current on-chain state. Use them immediately.
        // The Sync event listener will keep them fresh from here on.
        pool_cache.insert(pm.pool, PoolInfo {
            token0,
            token1,
            reserve0: U256::from(reserves.reserve0),
            reserve1: U256::from(reserves.reserve1),
            fee_bps: pm.fee_bps,
            router_id: effective_id,
            is_stable: pm.is_stable,
            decimals0,
            decimals1,
            last_sync: Instant::now(),
        });
    }
}

// ─── V3 pool discovery (startup) ──────────────────────────────────────────────

/// Discover Uniswap V3 pools for all configured pairs and V3 routers.
/// Populates `pool_cache.v3_by_address` with initial sqrtPriceX96 + liquidity
/// so that the strategy can run spot-price screens without RPC calls.
///
/// After startup, the listener subscribes to V3 Swap events on discovered pools
/// and keeps state fresh via `pool_cache.update_v3_state()`.
///
/// Two multicall rounds:
///   1. factory.getPool(A, B, fee)  per (V3 router × pair × fee_tier)
///   2. pool.slot0() + pool.liquidity() per discovered pool
async fn discover_v3_pools<P: Provider>(
    routers: &[RouterConfig],
    pairs: &[PairConfig],
    provider: &P,
    pool_cache: &PoolCache,
) {
    use alloy::primitives::U256;
    use alloy::sol_types::SolCall;
    use std::collections::HashSet;
    use std::time::Instant;

    // ── Collect V3 routers with factory_address ───────────────────────────────
    struct V3Router { id: String, factory: Address, fee_tiers: Vec<u32> }
    let v3_routers: Vec<V3Router> = routers
        .iter()
        .filter(|r| r.router_type == RouterType::V3)
        .filter_map(|r| {
            let factory = r.factory_address.as_deref()?.parse::<Address>().ok()?;
            let tiers = if r.fee_tiers.is_empty() { vec![500u32, 3000, 10000] } else { r.fee_tiers.clone() };
            Some(V3Router { id: r.id.clone(), factory, fee_tiers: tiers })
        })
        .collect();

    if v3_routers.is_empty() {
        return;
    }

    // ── Round 1: getPool(A, B, fee) per (router × pair × fee_tier) ───────────
    struct PoolQuery { router_id: String, ta: Address, tb: Address, fee: u32 }
    let mut pool_calls: Vec<(Address, Vec<u8>)> = Vec::new();
    let mut pool_queries: Vec<PoolQuery> = Vec::new();

    for vr in &v3_routers {
        for pair in pairs {
            let ta: Address = match pair.token_in.parse() { Ok(a) => a, Err(_) => continue };
            let tb: Address = match pair.token_out.parse() { Ok(a) => a, Err(_) => continue };
            for &fee in &vr.fee_tiers {
                use alloy::primitives::Uint;
                let cd = IUniswapV3Factory::getPoolCall {
                    tokenA: ta,
                    tokenB: tb,
                    fee: Uint::from(fee),
                }.abi_encode();
                pool_calls.push((vr.factory, cd));
                pool_queries.push(PoolQuery { router_id: vr.id.clone(), ta, tb, fee });
            }
        }
    }

    if pool_calls.is_empty() {
        return;
    }
    let pool_raw = discover_mc(pool_calls, provider).await;

    // ── Round 2: slot0() + liquidity() per discovered pool ────────────────────
    struct StateQuery { pool: Address, router_id: String, ta: Address, tb: Address, fee: u32 }
    let mut state_calls: Vec<(Address, Vec<u8>)> = Vec::new();
    let mut state_queries: Vec<StateQuery> = Vec::new();
    let mut seen: HashSet<Address> = HashSet::new();

    for (pq, raw_opt) in pool_queries.iter().zip(pool_raw.iter()) {
        let pool: Address = raw_opt
            .as_ref()
            .and_then(|raw| IUniswapV3Factory::getPoolCall::abi_decode_returns(raw).ok())
            .filter(|a: &Address| !a.is_zero())
            .unwrap_or(Address::ZERO);

        if pool.is_zero() || !seen.insert(pool) {
            continue;
        }

        // Two calls per pool: slot0() and liquidity()
        state_calls.push((pool, IUniswapV3Pool::slot0Call {}.abi_encode()));
        state_calls.push((pool, IUniswapV3Pool::liquidityCall {}.abi_encode()));
        state_queries.push(StateQuery {
            pool,
            router_id: pq.router_id.clone(),
            ta: pq.ta,
            tb: pq.tb,
            fee: pq.fee,
        });
    }

    if state_queries.is_empty() {
        return;
    }
    let state_raw = discover_mc(state_calls, provider).await;

    // ── Insert into pool_cache ────────────────────────────────────────────────
    // Also need token0 to know direction. We derive it: token0 = min(ta, tb) by address.
    for (i, sq) in state_queries.iter().enumerate() {
        let slot0_raw = match state_raw.get(2 * i) { Some(Some(r)) => r, _ => continue };
        let liq_raw   = match state_raw.get(2 * i + 1) { Some(Some(r)) => r, _ => continue };

        let slot0 = match IUniswapV3Pool::slot0Call::abi_decode_returns(slot0_raw).ok() {
            Some(s) => s,
            None => continue,
        };
        let liquidity = match IUniswapV3Pool::liquidityCall::abi_decode_returns(liq_raw).ok() {
            Some(l) => l,
            None => continue,
        };

        let sqrt_price_x96 = U256::from(slot0.sqrtPriceX96);
        if sqrt_price_x96.is_zero() {
            continue; // pool not initialised
        }

        // In V3 pools, token0 is always the lower address
        let (token0, token1) = if sq.ta < sq.tb { (sq.ta, sq.tb) } else { (sq.tb, sq.ta) };

        // slot0 data was just fetched from chain — it IS the current on-chain state.
        // Mark as fresh so quote_v3_spot() works immediately at startup without
        // waiting for the first Swap event. The same-router V3 filter in strategy.rs
        // already prevents cross-fee-tier phantom arbs that motivated stale init.
        pool_cache.insert_v3(sq.pool, V3PoolState {
            token0,
            token1,
            fee: sq.fee,
            sqrt_price_x96,
            liquidity: liquidity.into(),
            router_id: sq.router_id.clone(),
            last_updated: Instant::now(),
        });
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

// ─── Chunked multicall for pool discovery ─────────────────────────────────────

/// Multicall3 helper used at startup by discover_pools / discover_v3_pools.
///
/// Splits `calls` into chunks of `DISCOVER_CHUNK` items, issuing one Multicall3
/// call per chunk sequentially. This smooths the startup CU burst:
///   • Old: 90 sub-calls in one Multicall3 (or worse, 90 sequential eth_calls on fallback)
///   • New: 5 × 20 = 5 Multicall3 calls, ~5 × 26 CU = 130 CU spread over ~150ms
const DISCOVER_CHUNK: usize = 20;

async fn discover_mc<P: Provider>(
    calls: Vec<(Address, Vec<u8>)>,
    provider: &P,
) -> Vec<Option<Vec<u8>>> {
    use alloy::rpc::types::TransactionRequest;
    use alloy::sol_types::SolCall;

    if calls.is_empty() {
        return vec![];
    }

    let mc3: Address = "0xcA11bde05977b3631167028862bE2a173976CA11"
        .parse()
        .expect("hardcoded Multicall3");

    let mut results: Vec<Option<Vec<u8>>> = Vec::with_capacity(calls.len());

    for chunk in calls.chunks(DISCOVER_CHUNK) {
        let mc_calls: Vec<crate::abi::IMulticall3::Call3> = chunk
            .iter()
            .map(|(t, d)| crate::abi::IMulticall3::Call3 {
                target: *t,
                allowFailure: true,
                callData: d.clone().into(),
            })
            .collect();
        let calldata = crate::abi::IMulticall3::aggregate3Call { calls: mc_calls }.abi_encode();
        let tx = TransactionRequest::default().to(mc3).input(calldata.into());

        if let Ok(raw) = provider.call(tx).await {
            if let Ok(ret) = crate::abi::IMulticall3::aggregate3Call::abi_decode_returns(&raw) {
                results.extend(
                    ret.into_iter()
                        .map(|r| if r.success { Some(r.returnData.to_vec()) } else { None }),
                );
                continue;
            }
        }
        // Fallback: sequential calls for this chunk
        for (target, data) in chunk {
            let tx2 = TransactionRequest::default()
                .to(*target)
                .input(data.clone().into());
            results.push(provider.call(tx2).await.ok().map(|b| b.to_vec()));
        }
    }

    results
}
