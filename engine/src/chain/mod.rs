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

use alloy::sol_types::SolEvent;
use crate::abi::{IERC20, IRouterWithFactory, IUniswapV2Factory, ISolidlyFactory, IAerodromeFactory, IUniswapV2Pair, IUniswapV3Factory, IUniswapV3Pool, IPancakeV3Pool};
use crate::api::{broadcast_log, ChainStats, LogBroadcaster, SharedState};
use crate::config::{self, ChainConfig, PairConfig, RouterConfig, RouterType};
use crate::db::Database;
use crate::executor::{Executor, TxPrep};
use crate::listener::{Listener, SwapEvent, SwapMagnitude};
use crate::metrics::Metrics;
use crate::pool_cache::{PoolCache, PoolInfo, V3PoolState};
use crate::strategy::{Opportunity, Strategy};

pub mod scan;
pub mod state;
pub mod discover;

/// Cooldown after a dry-run failure or balance-guard skip (cheap check, no RPC spent).
pub(crate) const COOLDOWN_SECS: u64 = 15;
/// Cooldown after a tx is sent to chain. Short window so the pool can emit Sync events
/// and the local cache catches up before the next attempt.
pub(crate) const SEND_COOLDOWN_SECS: u64 = 15;
/// Cooldown for routes rejected as unprofitable after gas. These routes have a real gross
/// spread but the profit doesn't cover gas. Gas prices on L2s are stable minute-to-minute,
/// so there is no value in retrying the same route every 15s — it wastes log space and
/// prevents the engine from properly surfacing other opportunities.
pub(crate) const GAS_REJECT_COOLDOWN_SECS: u64 = 60;
/// Only count real execution failures (simulation reverts, send errors).
/// Gas-profitability rejects ("below threshold") do NOT count — they're pre-flight skips.
pub(crate) const MAX_CONSECUTIVE_FAILURES: u32 = 20;

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

    let provider = connect_with_fallback(&cfg, wallet.clone()).await
        .map_err(|e| anyhow::anyhow!("[{}] All RPC endpoints failed: {}", cfg.name, e))?;

    let provider = Arc::new(provider);

    // ── Contract address ──
    let contract_addr: Address = cfg
        .contract_address
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid contract address: {}", cfg.contract_address))?;

    // ── Executor ──
    let submission_rpc_urls: Vec<url::Url> = cfg.submission_rpcs.iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    let executor = Arc::new(Mutex::new(Executor::new(
        cfg.id,
        cfg.name.clone(),
        contract_addr,
        signer_address,
        wallet.clone(),
        submission_rpc_urls,
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

    // Announce startup immediately so the dashboard switches out of "connecting" state
    // before the slow pool-discovery multicalls begin.
    broadcast_log(
        &log_tx,
        "info",
        &format!("[{}] Starting up — discovering pools for {} pairs across {} routers…",
            cfg.name, chain_pairs.len(), chain_routers.len()),
        None,
    );

    // ── Pool discovery: populate V2/Solidly-volatile reserve cache ────────────
    // Eliminates per-block eth_calls for xy=k routers — reserves stay fresh via
    // on-chain Sync events (listener.rs subscribes and updates the cache live).
    let pool_cache = Arc::new(PoolCache::new());
    discover::discover_pools(&chain_routers, &chain_pairs, provider.as_ref(), &pool_cache).await;
    info!("[{}] Pool cache: {} V2/Solidly pools discovered", cfg.name, pool_cache.by_address.len());

    // ── V3 pool discovery: seed sqrtPriceX96 + liquidity from slot0() ─────────
    // Subscribes to V3 Swap events after startup to keep state fresh with zero RPC cost.
    discover::discover_v3_pools(&chain_routers, &chain_pairs, provider.as_ref(), &pool_cache).await;
    info!("[{}] V3 pool cache: {} pools seeded", cfg.name, pool_cache.v3_by_address.len());

    // Dedicated HTTP provider for QuoterV2 reads — HTTP/2 connection pooling is
    // faster than WS for single request-response calls (~20-40ms improvement).
    // Dedicated HTTP provider for QuoterV2 reads — HTTP/2 connection pooling is
    // faster than WS for single request-response calls (~20-40ms improvement).
    let http_provider: Option<alloy::providers::DynProvider> = cfg.http_rpc
        .parse::<url::Url>()
        .ok()
        .map(|url| ProviderBuilder::new().connect_http(url).erased());

    let strategy = Arc::new(RwLock::new(Strategy::new(
        cfg.id,
        chain_pairs.clone(),
        chain_routers.clone(),
        cfg.min_profit_usd,
        quoter_v2_address,
        pool_cache.clone(),
        cfg.rpc_concurrency,
        cfg.optimistic_submission,
        http_provider,
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
    state::refresh_contract_balances(provider.as_ref(), contract_addr, &contract_balances).await;
    // Startup funding check: warn for any underfunded pair.
    state::check_contract_funding(&cfg.name, &chain_pairs, contract_addr, &contract_balances).await;
    // Wire balances into executor for post-trade immediate refresh.
    { executor.lock().await.contract_balances = Some(contract_balances.clone()); }
    // Pre-fetch nonce at startup so first submission has zero RPC overhead.
    { executor.lock().await.prefetch_nonce(provider.as_ref()).await; }

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
                ghost_profit_usd: 0.0,
                base_fee_gwei: 0.0,
                dry_run: true,
                paused: false,
                rpc_ok: true,   // we just connected successfully
                last_block: 0,
                rpc_latency_ms: 0.0,
            });
        } else if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
            chain.rpc_ok = true;
        }
    }
    metrics.rpc_connected.with_label_values(&[&cfg.name]).set(1.0);

    // ── Swap event listener ──
    let listener = Listener::new(cfg.id, cfg.name.clone(), cfg.large_swap_threshold_bps, cfg.large_v3_threshold_bps);
    let (swap_tx, mut swap_rx) = mpsc::channel::<SwapEvent>(256);
    let swap_tx_for_sub = swap_tx.clone();
    {
        let provider_clone = (*provider).clone();
        listener.subscribe(provider_clone, swap_tx, pool_cache.clone(), log_tx.clone()).await?;
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

    // ── Real-time log subscription for instant pool state updates ──────────
    // Logs arrive on the WS connection before the next newHeads event,
    // so pool state is current when block_rx fires the scanner.
    let solidly_sync_hash_sub = alloy::primitives::keccak256(b"Sync(uint256,uint256)");
    let (log_tx_sub, mut log_rx) = mpsc::channel::<alloy::rpc::types::Log>(1024);
    {
        let mut log_addrs = pool_cache.pool_addresses();
        log_addrs.extend(pool_cache.v3_pool_addresses());
        let log_filter = alloy::rpc::types::Filter::new()
            .address(log_addrs)
            .event_signature(vec![
                crate::abi::Sync::SIGNATURE_HASH,
                solidly_sync_hash_sub,
                crate::abi::Swap::SIGNATURE_HASH,
                IPancakeV3Pool::Swap::SIGNATURE_HASH,
            ]);
        let provider_logs = (*provider).clone();
        let log_tx_sub2 = log_tx_sub.clone();
        let swap_tx_sub = swap_tx_for_sub;
        let cache_for_sub = pool_cache.clone();
        let chain_id_sub = cfg.id;
        let cname = cfg.name.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match provider_logs.subscribe_logs(&log_filter).await {
                    Ok(sub) => {
                        backoff = Duration::from_secs(1);
                        let mut stream = sub.into_stream();
                        while let Some(log) = stream.next().await {
                            // Apply to cache immediately so pool state is fresh
                            let applied = crate::listener::apply_log_to_cache(
                                &log, &cache_for_sub, solidly_sync_hash_sub,
                            );
                            // Trigger a targeted scan immediately — no waiting for the 2s poll cycle
                            if applied {
                                let pool = log.address();
                                let block_number = log.block_number.unwrap_or(0);
                                let _ = swap_tx_sub.try_send(crate::listener::SwapEvent {
                                    chain_id: chain_id_sub,
                                    pool,
                                    block_number,
                                    magnitude: crate::listener::SwapMagnitude::Normal,
                                });
                            }
                            // Forward raw log to log_rx for pre-scan drain (idempotent safety net)
                            let _ = log_tx_sub2.send(log).await;
                        }
                        debug!("[{}] log subscription ended, reconnecting...", cname);
                    }
                    Err(e) => {
                        debug!("[{}] log subscription failed: {} — retrying in {:?}", cname, e, backoff);
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
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
    // ── Best QuoterV2-verified spread % (f64 bits). NEG_INFINITY = Phase 1.5 not yet run. ─
    // Distinguishes V3 spot formula phantoms (best=$0.29 raw) from real profit (bestV=+0.01%).
    let best_verified_spread_bits: Arc<AtomicU64> = Arc::new(AtomicU64::new(f64::NEG_INFINITY.to_bits()));

    // ── Quote diagnostic counters (last scan values) ──────────────────────────
    // fwd_count: how many non-zero forward quotes returned
    // multi_count: how many pairs had quotes from ≥2 distinct DEXes
    // active_count: how many pairs had ≥1 quote from any DEX
    let last_fwd_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0u64));
    let last_multi_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0u64));
    let last_active_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0u64));
    // last_opp_count: how many profitable opportunities were found in the last scan
    let last_opp_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0u64));
    // Count of consecutive heartbeats with zero forward quotes.
    // Warning only fires at 3+ to suppress startup false-positives and transient RPC blips.
    let mut consecutive_zero_fwd: u32 = 0;
    // Scans at the previous heartbeat — used to show per-beat delta instead of cumulative.
    let mut last_hb_scans: u64 = 0;

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
    let mut prune_tick    = tokio::time::interval(Duration::from_secs(600)); // every 10 min

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
                state::update_native_price(&strategy, &executor, &provider, &chain_routers, &chain_pairs, &cfg).await;

                // Measure RPC round-trip latency via a lightweight eth_blockNumber call.
                let rpc_ping_start = Instant::now();
                let rpc_latency_ms = match provider.get_block_number().await {
                    Ok(_) => rpc_ping_start.elapsed().as_millis() as f64,
                    Err(_) => 0.0,
                };

                // Broadcast a heartbeat log so the Live Feed shows the engine is active
                // even when no opportunities are detected. Fires every ~60 seconds.
                // Sync confirmed_success from executor atomics into shared_state
                // (receipt background tasks update the atomics, not shared_state directly).
                let confirmed_ok = {
                    let exec = executor.lock().await;
                    // If a stats reset was requested via POST /stats/reset, zero the
                    // executor atomics NOW (before reading them into shared_state).
                    // This prevents the old cumulative values from bouncing back after
                    // stats_reset already zeroed the shared_state chain counters.
                    {
                        let mut state = shared_state.write().await;
                        if state.stats_reset_requested {
                            exec.confirmed_success.store(0, Ordering::Relaxed);
                            exec.confirmed_failed.store(0, Ordering::Relaxed);
                            exec.confirmed_profit_usd_bits.store(0, Ordering::Relaxed);
                            exec.ghost_profit_usd_bits.store(0, Ordering::Relaxed);
                            state.stats_reset_requested = false;
                        }
                    }
                    let ok = exec.confirmed_success.load(Ordering::Relaxed);
                    let fail = exec.confirmed_failed.load(Ordering::Relaxed);
                    let profit = f64::from_bits(exec.confirmed_profit_usd_bits.load(Ordering::Relaxed));
                    let ghost = f64::from_bits(exec.ghost_profit_usd_bits.load(Ordering::Relaxed));
                    drop(exec);
                    // Sync confirmed counts from executor atomics → shared_state
                    let mut state = shared_state.write().await;
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.total_success = ok;
                        chain.total_failed = fail;
                        chain.total_profit_usd = profit;
                        chain.rpc_latency_ms = rpc_latency_ms;
                        chain.ghost_profit_usd = ghost;
                    }
                    ok
                };
                let (total_scans, attempts, success) = {
                    let state = shared_state.read().await;
                    state.chains.iter()
                        .find(|c| c.chain_id == cfg.id)
                        .map(|c| (c.total_scans, c.total_attempts, c.total_success))
                        .unwrap_or((0, 0, confirmed_ok))
                };
                let scans = total_scans.saturating_sub(last_hb_scans);
                last_hb_scans = total_scans;
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
                // Read and reset Phase 1.5 verified spread (QuoterV2-confirmed, can be negative).
                // bestV distinguishes real spread from V3 spot formula phantoms.
                let verified_spread = f64::from_bits(best_verified_spread_bits.swap(f64::NEG_INFINITY.to_bits(), Ordering::Relaxed));
                let verified_str = if verified_spread.is_finite() {
                    format!("{:+.3}%", verified_spread * 100.0)
                } else {
                    "n/a".to_string()
                };
                // Read last-scan quote diagnostics
                let fwd_ok = last_fwd_count.load(Ordering::Relaxed);
                let multi_dex = last_multi_count.load(Ordering::Relaxed);
                let active_pairs = last_active_count.load(Ordering::Relaxed);
                let total_pairs = chain_pairs.len() as u64;
                let opp_count = last_opp_count.load(Ordering::Relaxed);
                let (v3_fresh, v3_total) = {
                    let strat = strategy.read().await;
                    strat.pool_cache.count_fresh_v3_pools(Duration::from_secs(30))
                };
                let heartbeat_msg = format!(
                    "[{}] ♥ scans={} execs={} ok={} | best={} bestV={} spread={} | quotes={} active={}/{} cross={} | opps={} | v3={}/{}",
                    cfg.name, scans, attempts, success, best_seen_str, verified_str, spread_str, fwd_ok, active_pairs, total_pairs, multi_dex, opp_count, v3_fresh, v3_total,
                );
                // Mirror to server terminal so it's visible even when WS is disconnected.
                info!("{}", heartbeat_msg);
                broadcast_log(&log_tx, "heartbeat", &heartbeat_msg, None);

                // Publish per-pair scan snapshot to shared_state (read by dashboard /pair-scan).
                // Merges 2-hop pairs (from evaluate) and triangular triplets (from detect_triangular).
                {
                    let disabled_set = {
                        let st = shared_state.read().await;
                        st.disabled_pairs.clone()
                    };
                    let strat = strategy.read().await;
                    let mut scan_info = strat.pair_scan.lock().unwrap().clone();
                    let tri_info = strat.tri_scan.lock().unwrap().clone();
                    drop(strat);
                    // Append triangular entries (they have no config-level disable toggle yet)
                    scan_info.extend(tri_info);
                    for info in &mut scan_info {
                        info.disabled = disabled_set.contains(&info.pair_id);
                    }
                    let mut st = shared_state.write().await;
                    st.pair_scan.insert(cfg.id, scan_info);
                }

                // Broadcast a live-feed warn if no cross-DEX coverage (actionable).
                // Require 3 consecutive zero-fwd heartbeats before warning to suppress
                // startup false-positives and transient single-scan RPC blips.
                if fwd_ok == 0 {
                    consecutive_zero_fwd += 1;
                    if consecutive_zero_fwd >= 3 {
                        let msg = format!(
                            "[{}] WARN: ALL forward quotes returned 0 for {}+ minutes — check QuoterV2 addresses and fee tiers in config.yaml",
                            cfg.name, consecutive_zero_fwd
                        );
                        warn!("{}", msg);
                        broadcast_log(&log_tx, "warn", &msg, None);
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
                state::refresh_contract_balances(provider.as_ref(), contract_addr, &contract_balances).await;
            }

            // ── Pool cache staleness pruning (every 10 min) ───────────────────
            _ = prune_tick.tick() => {
                let pool_cache = strategy.read().await.pool_cache.clone();
                let (evicted_v2, evicted_v3) = pool_cache.prune_stale(
                    Duration::from_secs(600),   // 10 min TTL for V2/Solidly
                    Duration::from_secs(1800),  // 30 min TTL for V3 (quieter pools ok)
                );
                if evicted_v2 + evicted_v3 > 0 {
                    debug!("[{}] pool cache pruned: {} V2/Solidly, {} V3 stale entries evicted",
                        cfg.name, evicted_v2, evicted_v3);
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
                    let base_fee_gwei = base_fee as f64 / 1e9;
                    let mut state = shared_state.write().await;
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.rpc_ok = true;
                        chain.last_block = block_num;
                        chain.total_scans += 1;
                        chain.base_fee_gwei = base_fee_gwei;
                    }
                }
                metrics.last_block.with_label_values(&[&cfg.name]).set(block_num as f64);
                metrics.rpc_connected.with_label_values(&[&cfg.name]).set(1.0);

                // Read paused state and disabled pairs in one lock acquisition.
                let (global_paused, chain_paused, disabled_set) = {
                    let state = shared_state.read().await;
                    let cp = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                    (state.paused, cp, state.disabled_pairs.clone())
                };
                if global_paused || chain_paused { continue; }

                // ── Drain pending log events before scanning ──────────────────
                // Logs from the just-mined block arrive on the WS subscription
                // before newHeads — flush any queued updates so the scanner sees
                // fresh pool state. try_recv() is non-blocking.
                {
                    let pc = strategy.read().await.pool_cache.clone();
                    while let Ok(log) = log_rx.try_recv() {
                        crate::listener::apply_log_to_cache(&log, &pc, solidly_sync_hash_sub);
                    }
                }
                // ─────────────────────────────────────────────────────────────

                scan::evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &best_raw_profit, &best_spread_bits, &best_verified_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                    &last_opp_count, &contract_balances, None, disabled_set,
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
                let (global_paused, chain_paused, disabled_set) = {
                    let mut state = shared_state.write().await;
                    // Count poll_tick scans too — block_rx misses during WS reconnects
                    // are covered by poll_tick, so total_scans should include them.
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.total_scans += 1;
                    }
                    let cp = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                    (state.paused, cp, state.disabled_pairs.clone())
                };
                if global_paused || chain_paused { continue; }

                scan::evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &best_raw_profit, &best_spread_bits, &best_verified_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                    &last_opp_count, &contract_balances, None, disabled_set,
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
                let is_large = event.magnitude == SwapMagnitude::Large;

                // Per-pool burst protection — skipped for large swaps
                if !is_large {
                    if let Some(&fired_at) = pool_last_scan.get(&event.pool) {
                        if fired_at.elapsed() < Duration::from_millis(50) {
                            continue;
                        }
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

                // Large swap: clear cooldowns for all fingerprints involving these tokens.
                // A whale trade fundamentally changes pool state — previous pre-flight
                // rejections are no longer valid at the new price.
                if is_large {
                    let ta_lower = format!("{:?}", tok_a).to_lowercase();
                    let tb_lower = format!("{:?}", tok_b).to_lowercase();
                    cooldowns.retain(|fingerprint, _| {
                        let fp_lower = fingerprint.to_lowercase();
                        !fp_lower.contains(&ta_lower) && !fp_lower.contains(&tb_lower)
                    });
                }

                // Find which configured pairs involve these tokens
                let (pair_mask, token_filter) = {
                    let strat = strategy.read().await;
                    let indices = strat.pairs_for_tokens(tok_a, tok_b);
                    (indices.into_iter().collect::<HashSet<usize>>(), vec![tok_a, tok_b])
                };
                if pair_mask.is_empty() { continue; }

                // Read paused + disabled in one lock; also increment total_scans.
                let (global_paused, chain_paused, disabled_set) = {
                    let mut state = shared_state.write().await;
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.total_scans += 1;
                    }
                    let cp = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                    (state.paused, cp, state.disabled_pairs.clone())
                };
                if global_paused || chain_paused { continue; }

                if is_large {
                    let msg = format!(
                        "[{}] LARGE swap on {:?} — fast-track scan ({} pairs, cooldowns cleared)",
                        cfg.name, event.pool, pair_mask.len()
                    );
                    info!("{}", msg);
                    broadcast_log(&log_tx, "info", &msg, None);
                } else {
                    debug!(
                        "[{}] Swap on {:?} — targeted scan ({} pairs)",
                        cfg.name, event.pool, pair_mask.len()
                    );
                }

                scan::evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &best_raw_profit, &best_spread_bits, &best_verified_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                    &last_opp_count, &contract_balances, Some((pair_mask, token_filter)), disabled_set,
                ).await;
                // NOTE: last_scan_at intentionally NOT updated here.
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
