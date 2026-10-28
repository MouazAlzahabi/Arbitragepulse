use alloy::network::Ethereum;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::network::EthereumWallet;
use alloy::transports::ws::WsConnect;
use anyhow::Result;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::abi::{IERC20, IRouterWithFactory, IUniswapV2Factory, IAerodromeFactory, IUniswapV2Pair, IUniswapV3Factory, IUniswapV3Pool};
use crate::api::{broadcast_log, ChainStats, LogBroadcaster, SharedState};
use crate::config::{self, ChainConfig, PairConfig, RouterConfig, RouterType};
use crate::db::Database;
use crate::util::{rpc_timeout, rpc_read_timeout};
use crate::executor::{Executor, TxPrep};
use crate::listener::{Listener, SwapEvent, SwapMagnitude};
use crate::metrics::Metrics;
use crate::pool_cache::{PoolCache, PoolInfo, V3PoolState};
use crate::strategy::{Opportunity, Strategy};

pub mod scan;
pub mod state;
pub mod discover;
pub mod pending;

/// Cooldown after a dry-run failure or balance-guard skip (cheap check, no RPC spent).
pub(crate) fn cooldown_secs() -> u64 { crate::config::tuning().cooldown_secs }
/// Cooldown after a tx is sent to chain. Short window so the pool can emit Sync events
/// and the local cache catches up before the next attempt.
pub(crate) fn send_cooldown_secs() -> u64 { crate::config::tuning().send_cooldown_secs }
/// Cooldown for routes rejected as unprofitable after gas. These routes have a real gross
/// spread but the profit doesn't cover gas. Gas prices on L2s are stable minute-to-minute,
/// so there is no value in retrying the same route every 15s — it wastes log space and
/// prevents the engine from properly surfacing other opportunities.
pub(crate) fn gas_reject_cooldown_secs() -> u64 { crate::config::tuning().gas_reject_cooldown_secs }
/// Only count real execution failures (simulation reverts, send errors).
/// Gas-profitability rejects ("below threshold") do NOT count — they're pre-flight skips.
pub(crate) fn max_consecutive_failures() -> u32 { crate::config::tuning().max_consecutive_failures }

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

    let rpc_counter = crate::rpc_count::RpcCounter::default();
    let provider = connect_with_fallback(&cfg, wallet.clone(), rpc_counter.clone()).await
        .map_err(|e| anyhow::anyhow!("[{}] All RPC endpoints failed: {}", cfg.name, e))?;

    let provider = Arc::new(provider);

    // ── Chain-id guard: the RPC must serve the chain this config describes ──
    let rpc_chain_id = crate::util::rpc_timeout(crate::util::rpc_read_timeout(), provider.get_chain_id())
        .await
        .map_err(|e| anyhow::anyhow!("[{}] eth_chainId failed: {}", cfg.name, e))?;
    if let Err(e) = config::check_chain_id(cfg.id, rpc_chain_id, &cfg.name) {
        // Not retryable: the caller's restart loop would just hit it again.
        tracing::error!("{}", e);
        std::process::exit(2);
    }

    // ── Contract address ──
    let contract_addr: Address = cfg
        .contract_address
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid contract address: {}", cfg.contract_address))?;
    if contract_addr.is_zero() {
        return Err(anyhow::anyhow!("[{}] contract_address is zero — check config", cfg.name));
    }

    // ── Executor setup check: owner = signer (exit 2), router allowlist (exit 2 live), paused ──
    // Runs before pool discovery so a wrong key fails within seconds. Startup only.
    let dry_run_at_start = shared_state.read().await.dry_run;
    let start_paused = state::check_contract_setup(
        provider.as_ref(), contract_addr, signer_address, &routers, &cfg.name, dry_run_at_start, &log_tx,
    ).await?;

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
        cfg.priority_fee_wei,
        cfg.deadline_seconds,
        db,
    )));
    // Shared RPC health — its backoff is cleared on pause so a resume executes at once.
    let rpc_health = executor.lock().await.rpc_health.clone();

    // ── Strategy ──
    let chain_pairs: Vec<PairConfig> = pairs
        .iter()
        .cloned()
        .collect();

    let chain_routers: Vec<RouterConfig> = routers
        .iter()
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

    // ── Pool discovery: populate V2/Aerodrome reserve cache ───────────────────
    // Eliminates per-block eth_calls for xy=k routers — reserves stay fresh via
    // on-chain Sync events (listener.rs subscribes and updates the cache live).
    let pool_cache = Arc::new(PoolCache::new());
    let lost_v2 = discover::discover_pools(&chain_routers, &chain_pairs, provider.as_ref(), &pool_cache).await;
    info!("[{}] Pool cache: {} V2/Aerodrome pools discovered", cfg.name, pool_cache.by_address.len());

    // ── V3 pool discovery: seed sqrtPriceX96 + liquidity from slot0() ─────────
    // Subscribes to V3 Swap events after startup to keep state fresh with zero RPC cost.
    let lost_v3 = discover::discover_v3_pools(&chain_routers, &chain_pairs, provider.as_ref(), &pool_cache).await;
    info!("[{}] V3 pool cache: {} pools seeded", cfg.name, pool_cache.v3_by_address.len());
    // Discovery calls lost to RPC failure (after retries) leave pools silently untracked
    // until the next restart — say so where the operator looks, but keep running.
    if lost_v2 + lost_v3 > 0 {
        let msg = format!(
            "[{}] Pool discovery INCOMPLETE: {} call(s) lost to RPC failure after retries \
             ({} V2/Aerodrome + {} V3 pools tracked). Missing pools are not watched until a \
             restart — check RPC health and restart the engine.",
            cfg.name, lost_v2 + lost_v3, pool_cache.by_address.len(), pool_cache.v3_by_address.len());
        warn!("{}", msg);
        broadcast_log(&log_tx, "warn", &msg, None);
    }

    // Dedicated HTTP provider for QuoterV2 reads — HTTP/2 connection pooling is
    // faster than WS for single request-response calls (~20-40ms improvement).
    // With `preconf_http_rpc` it is the Flashblocks endpoint instead, read at `pending`.
    // (URL validated at config load.)
    let preconf_http: Option<alloy::providers::DynProvider> = cfg.preconf_http_rpc.as_deref()
        .and_then(|raw| {
            let url = raw.parse::<url::Url>().ok()?;
            info!("[{}] Quotes and simulations at `pending` on {}", cfg.name, crate::util::redact_url(raw));
            Some(ProviderBuilder::new().connect_http(url).erased())
        });
    let http_provider: Option<alloy::providers::DynProvider> = preconf_http.clone().or_else(|| cfg.http_rpc
        .parse::<url::Url>()
        .ok()
        .map(|url| ProviderBuilder::new().connect_http(url).erased()));
    executor.lock().await.sim_provider = preconf_http.clone();
    // Background reads (contract/signer balances, ETH price) don't need the main provider's
    // speed or subscriptions: they go to the HTTP endpoint, keeping the main (paid) RPC for
    // subscriptions, sends, receipts and nonces. Falls back to the main provider without one.
    let reads: Arc<alloy::providers::DynProvider> = Arc::new(
        http_provider.clone().unwrap_or_else(|| provider.as_ref().clone().erased()),
    );

    let min_triangular_profit_usd = cfg
        .min_triangular_profit_usd
        .unwrap_or(cfg.min_profit_usd);

    let strategy = Arc::new(RwLock::new(Strategy::new(
        cfg.id,
        chain_pairs.clone(),
        chain_routers.clone(),
        cfg.min_profit_usd,
        min_triangular_profit_usd,
        quoter_v2_address,
        pool_cache.clone(),
        cfg.rpc_concurrency,
        cfg.optimistic_submission,
        http_provider,
    )));
    if preconf_http.is_some() {
        strategy.write().await.quote_block = alloy::eips::BlockId::pending();
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
    state::refresh_contract_balances(reads.as_ref(), contract_addr, &contract_balances).await;
    // Signer gas check (chain.min_native_balance): warn once per low period.
    let gas_low_warned = Arc::new(AtomicBool::new(false));
    state::check_signer_gas(reads.as_ref(), signer_address, cfg.min_native_balance, &gas_low_warned, &log_tx, &cfg.name).await;
    // Startup funding check: warn for any underfunded pair.
    state::check_contract_funding(&cfg.name, &chain_pairs, contract_addr, &contract_balances, &log_tx).await;
    // Balance slots for dry-run's assumed balance (simulate unfunded trades instead of
    // skipping them). On the simulation endpoint, which must accept eth_call state overrides.
    {
        let sim: &alloy::providers::DynProvider = preconf_http.as_ref().unwrap_or(reads.as_ref());
        let tokens: Vec<Address> = contract_balances.read().await.keys().copied().collect();
        let slots = state::discover_balance_slots(sim, &tokens, contract_addr).await;
        let found: Vec<String> = chain_pairs.iter()
            .filter_map(|p| p.token_in.parse::<Address>().ok().map(|a| (a, &p.token_in_symbol)))
            .filter_map(|(a, sym)| slots.get(&a).map(|s| format!("{sym} slot {s}")))
            .collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        info!("[{}] Dry-run assumed balance: {}", cfg.name,
              if found.is_empty() { "unavailable (no balance slot found) — unfunded trades are skipped".to_string() } else { found.join(", ") });
        executor.lock().await.balance_slots = Arc::new(slots);
    }
    // Wire balances into executor for post-trade immediate refresh.
    { executor.lock().await.contract_balances = Some(contract_balances.clone()); }
    // Pre-fetch nonce at startup so first submission has zero RPC overhead.
    { executor.lock().await.prefetch_nonce(provider.as_ref()).await; }

    // ── Register initial chain stats ──
    // Totals already in shared state (seeded from the DB, or from a previous run of this
    // chain after a reconnect). The executor's atomics count from 0 for this run, so the
    // heartbeat reports baseline + atomic — never the atomic alone.
    let (mut base_success, mut base_failed, mut base_profit) = {
        let state = shared_state.read().await;
        state.chains.iter().find(|c| c.chain_id == cfg.id)
            .map(|c| (c.total_success, c.total_failed, c.total_profit_usd))
            .unwrap_or((0, 0, 0.0))
    };
    {
        let mut state = shared_state.write().await;
        if !state.chains.iter().any(|c| c.chain_id == cfg.id) {
            state.chains.push(ChainStats {
                chain_id: cfg.id,
                chain_name: cfg.name.clone(),
                total_scans: 0,
                total_opportunities: 0,
                total_attempts: 0,
                total_success: 0,
                total_failed: 0,
                total_dry_run_ok: 0,
                total_exec_failed: 0,
                total_profit_usd: 0.0,
                ghost_profit_usd: 0.0,
                base_fee_gwei: 0.0,
                dry_run: true,
                paused: start_paused,
                rpc_ok: true,   // we just connected successfully
                last_block: 0,
                last_block_at: 0,
                rpc_latency_ms: 0.0,
                rpc_latency_max_ms: 0.0,
            });
        } else if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
            chain.rpc_ok = true;
            if start_paused {
                chain.paused = true;
            }
        }
        // Register this run's RPC health (replacing a previous run's after a reconnect).
        state.rpc_health.insert(cfg.id, rpc_health.clone());
    }
    spawn_rpc_monitor(&provider, &rpc_health, cfg.name.clone());
    metrics.rpc_connected.with_label_values(&[&cfg.name]).set(1.0);

    // ── Swap event listener ──
    let listener = Listener::new(cfg.id, cfg.name.clone(), cfg.large_swap_threshold_bps, cfg.large_v3_threshold_bps);
    let (swap_tx, mut swap_rx) = mpsc::channel::<SwapEvent>(1024);
    let swap_tx_for_sub = swap_tx.clone();
    {
        let provider_clone = (*provider).clone();
        listener.subscribe(
            provider_clone,
            swap_tx,
            pool_cache.clone(),
            log_tx.clone(),
            cfg.poll_logs_fallback,
            cfg.block_time_ms,
        ).await?;
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
                backoff = (backoff * 2).min(crate::config::tuning().ws_reconnect_max_backoff());
            }
        });
    }

    // ── Real-time log subscriptions for instant pool state updates ──────────
    // Sealed-block logs (primary RPC) are the source of truth. With `preconf_ws_rpc`, the
    // same filter is also subscribed as `pendingLogs` on a Flashblocks-aware endpoint:
    // events from 200 ms pre-confirmed sub-blocks, ~1.2 s earlier. Both feed the same
    // cache + scan trigger; PoolCache's log-position guard keeps them ordered and stops a
    // log's sealed copy from triggering a second scan.
    let solidly_sync_hash_sub = alloy::primitives::keccak256(b"Sync(uint256,uint256)");
    let preconf_logs = Arc::new(AtomicU64::new(0));
    {
        let log_filter = alloy::rpc::types::Filter::new()
            .address(pool_cache.watch_addresses())
            .event_signature(crate::listener::pool_log_topics(solidly_sync_hash_sub));
        let feed = LogFeed {
            cache: pool_cache.clone(),
            swap_tx: swap_tx_for_sub.clone(),
            chain_id: cfg.id,
            chain_name: cfg.name.clone(),
            solidly_sync_hash: solidly_sync_hash_sub,
            large_swap_bps: cfg.large_swap_threshold_bps,
            large_v3_bps: cfg.large_v3_threshold_bps,
        };
        spawn_log_subscription((*provider).clone(), log_filter.clone(), feed.clone(), None, None);
        if let Some(url) = cfg.preconf_ws_rpc.clone() {
            // Connected in the background: an optional feed must never hold up startup.
            spawn_preconf_subscription(url, log_filter, feed, preconf_logs.clone());
        }
    }

    // ── Pending TX monitor (optional, requires mempool-enabled WS endpoint) ──
    // Fires a targeted arb scan with matching tip when a large swap is detected
    // before it confirms — enables same-block execution on FCFS chains like Base.
    let (pending_tx_chan, mut pending_rx) = mpsc::channel::<pending::PendingSwapEvent>(256);
    if cfg.pending_tx_monitoring {
        // Build the pending-monitor URL list: primary ws_rpc first (Alchemy — has mempool access),
        // then fallbacks. Public nodes (wss://mainnet.base.org) don't expose the mempool on OP
        // Stack and will always fail with 405; they sit in fallbacks as a last resort for block
        // subscriptions only.
        let mut pending_ws_urls: Vec<String> = vec![cfg.ws_rpc.clone()];
        pending_ws_urls.extend_from_slice(&cfg.ws_rpc_fallbacks);
        pending::spawn_pending_monitor(
            cfg.name.clone(),
            pending_ws_urls,
            // 0 = no size filter: profitability is decided in evaluate_and_execute().
            0u128,
            pending_tx_chan,
        );
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
    // opps_found: detected opportunities — each route fingerprint counted at most once per
    // cooldown_secs, whether or not it was then skipped (route/pair cooldown, in-flight),
    // traded or failed; routes recently rejected after gas are not counted. Cumulative; the
    // heartbeat shows the per-beat delta and /stats the total — the dashboard's OPPORTUNITIES tile.
    let opps_found: Arc<AtomicU64> = Arc::new(AtomicU64::new(0u64));
    // recheck_drops: candidates dropped by the quiet-pool on-chain recheck (cached reserves
    // moved or unreadable). Cumulative; shown in the heartbeat only when > 0.
    let recheck_drops: Arc<AtomicU64> = Arc::new(AtomicU64::new(0u64));
    // Count of consecutive heartbeats with zero forward quotes.
    // Warning only fires at 3+ to suppress startup false-positives and transient RPC blips.
    let mut consecutive_zero_fwd: u32 = 0;
    // Scans at the previous heartbeat — used to show per-beat delta instead of cumulative.
    let mut last_hb_scans: u64 = 0;
    let mut last_hb_opps: u64 = 0;
    // Lock-free scan counter — incremented by poll_tick and swap handlers (no write lock needed).
    // Synced to shared_state.total_scans during heartbeat's existing write() acquisition.
    let scan_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

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
    let mut price_tick    = tokio::time::interval(crate::config::tuning().price_refresh());
    let mut config_tick   = tokio::time::interval(crate::config::tuning().config_reload());
    let mut balance_tick  = tokio::time::interval(crate::config::tuning().balance_refresh());
    // Native-price and balance refreshes run as background tasks so their RPC calls never
    // delay a scan. Each flag stops a slow refresh from overlapping with the next tick.
    let price_refresh_busy   = Arc::new(AtomicBool::new(false));
    // Main-RPC round trip (ms, f64 bits; 0 = not yet measured): median and worst of the last
    // 6 eth_blockNumber pings, taken every `tuning.rpc_latency_sample_ms` (60 s → a 6-minute
    // window) by a background task (never on the select loop). Display only; each ping is a
    // request on the WS provider's quota, so the window is wide rather than the ping frequent.
    let rpc_latency_bits = Arc::new(AtomicU64::new(0f64.to_bits()));
    let rpc_latency_max_bits = Arc::new(AtomicU64::new(0f64.to_bits()));
    {
        let (provider, median, worst) = (Arc::downgrade(&provider), rpc_latency_bits.clone(), rpc_latency_max_bits.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(crate::config::tuning().rpc_latency_sample());
            let mut samples: std::collections::VecDeque<f64> = std::collections::VecDeque::with_capacity(6);
            loop {
                tick.tick().await;
                let Some(provider) = provider.upgrade() else { return };
                let t = Instant::now();
                if rpc_timeout(rpc_read_timeout(), provider.get_block_number()).await.is_ok() {
                    if samples.len() == 6 { samples.pop_front(); }
                    samples.push_back(t.elapsed().as_secs_f64() * 1000.0);
                }
                let (m, w) = latency_median_max(&samples);
                median.store(m.to_bits(), Ordering::Relaxed);
                worst.store(w.to_bits(), Ordering::Relaxed);
            }
        });
    }
    let balance_refresh_busy = Arc::new(AtomicBool::new(false));
    // When the last block header arrived: a long gap means the WS backend is dead (E1-3).
    let mut last_block_seen = Instant::now();
    // Last config-reload error, so a persistent one is reported once, not every tick.
    let mut last_reload_err: Option<String> = None;

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
                if last_block_seen.elapsed() > crate::config::tuning().block_stall_exit() {
                    tracing::error!("[{}] No new block for {:.0?} — RPC / WebSocket dead; exiting (code 1) so the service manager restarts the engine and walks the RPC fallback list", cfg.name, last_block_seen.elapsed());
                    std::process::exit(1);
                }
                if !price_refresh_busy.swap(true, Ordering::AcqRel) {
                    let (strategy, executor, reads) = (strategy.clone(), executor.clone(), reads.clone());
                    let (routers, pairs, cfg) = (chain_routers.clone(), chain_pairs.clone(), cfg.clone());
                    let busy = price_refresh_busy.clone();
                    tokio::spawn(async move {
                        state::update_native_price(&strategy, &executor, &reads, &routers, &pairs, &cfg).await;
                        busy.store(false, Ordering::Release);
                    });
                }

                // Prune expired cooldown entries. Entries are cleaned up lazily when
                // the same fingerprint is looked up, but one-off fingerprints (different
                // amount_in each time) never get re-seen and accumulate indefinitely.
                let now = Instant::now();
                cooldowns.retain(|_, expire_at| *expire_at > now);

                // Latest ping from the price-refresh task (the previous tick's on this one).
                let rpc_latency_ms = f64::from_bits(rpc_latency_bits.load(Ordering::Relaxed));

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
                            (base_success, base_failed, base_profit) = (0, 0, 0.0);
                            exec.confirmed_success.store(0, Ordering::Relaxed);
                            exec.confirmed_failed.store(0, Ordering::Relaxed);
                            exec.confirmed_profit_usd_bits.store(0, Ordering::Relaxed);
                            exec.ghost_profit_usd_bits.store(0, Ordering::Relaxed);
                            scan_count.store(0, Ordering::Relaxed);
                            opps_found.store(0, Ordering::Relaxed);
                            (last_hb_scans, last_hb_opps) = (0, 0);
                            state.stats_reset_requested = false;
                        }
                    }
                    let ok = exec.confirmed_success.load(Ordering::Relaxed);
                    let fail = exec.confirmed_failed.load(Ordering::Relaxed);
                    let profit = f64::from_bits(exec.confirmed_profit_usd_bits.load(Ordering::Relaxed));
                    let ghost = f64::from_bits(exec.ghost_profit_usd_bits.load(Ordering::Relaxed));
                    drop(exec);
                    // Sync executor atomics + lock-free scan_count → shared_state.
                    let mut state = shared_state.write().await;
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.total_success = base_success + ok;
                        chain.total_failed = base_failed + fail;
                        chain.total_profit_usd = base_profit + profit;
                        chain.rpc_latency_ms = rpc_latency_ms;
                        chain.rpc_latency_max_ms = f64::from_bits(rpc_latency_max_bits.load(Ordering::Relaxed));
                        chain.ghost_profit_usd = ghost;
                        chain.total_scans = scan_count.load(Ordering::Relaxed);
                        chain.total_opportunities = opps_found.load(Ordering::Relaxed);
                    }
                    base_success + ok
                };
                let (attempts, success, paused) = {
                    let state = shared_state.read().await;
                    let chain = state.chains.iter().find(|c| c.chain_id == cfg.id);
                    let (a, s) = chain.map(|c| (c.total_attempts, c.total_success)).unwrap_or((0, confirmed_ok));
                    (a, s, state.paused || chain.is_some_and(|c| c.paused))
                };
                let total_scans = scan_count.load(Ordering::Relaxed);
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
                let spread_str = crate::strategy::fmt_spread(f64::from_bits(
                    best_spread_bits.swap(f64::NEG_INFINITY.to_bits(), Ordering::Relaxed)));
                // Read and reset Phase 1.5 verified spread (QuoterV2-confirmed, can be negative).
                // bestV distinguishes real spread from V3 spot formula phantoms.
                let verified_str = crate::strategy::fmt_spread(f64::from_bits(
                    best_verified_spread_bits.swap(f64::NEG_INFINITY.to_bits(), Ordering::Relaxed)));
                // Best spreads incl. routes that best=/spread= skip. spot2 (2-hop) and tri
                // (triangular) are unverified spot upper bounds; triV is QuoterV2-verified.
                let (spot2_str, tri_str, tri_v_str) = {
                    use crate::strategy::{fmt_spread, BeatSpreads};
                    let s = strategy.read().await;
                    (fmt_spread(BeatSpreads::take(&s.beat.spot2)), fmt_spread(BeatSpreads::take(&s.beat.tri)),
                     fmt_spread(BeatSpreads::take(&s.beat.tri_v)))
                };
                // Read last-scan quote diagnostics
                let fwd_ok = last_fwd_count.load(Ordering::Relaxed);
                let multi_dex = last_multi_count.load(Ordering::Relaxed);
                let active_pairs = last_active_count.load(Ordering::Relaxed);
                let total_pairs = chain_pairs.len() as u64;
                let total_opps = opps_found.load(Ordering::Relaxed);
                let opp_count = total_opps.saturating_sub(last_hb_opps);
                last_hb_opps = total_opps;
                let (v3_fresh, v3_total) =
                    pool_cache.count_fresh_v3_pools(crate::config::tuning().v3_fresh_window());
                let preconf_n = preconf_logs.swap(0, Ordering::Relaxed);
                let recheck_drop_n = recheck_drops.load(Ordering::Relaxed);
                // Pairs the executor can't fund (cached balances, no RPC): they detect but never trade.
                let unfunded = state::underfunded_pairs(&chain_pairs, &*contract_balances.read().await).len();
                let heartbeat_msg = format!(
                    "[{}] ♥ scans={} execs={} ok={} | last_scan={}s ago | best={} bestV={} spread={} spot2={} tri={} triV={} | quotes={} active={}/{} cross={} | opps={}{} | v2={} v3={}/{}{}{}",
                    cfg.name, scans, attempts, success, last_scan_at.elapsed().as_secs(), best_seen_str, verified_str, spread_str, spot2_str, tri_str, tri_v_str, fwd_ok, active_pairs, total_pairs, multi_dex, opp_count,
                    if recheck_drop_n > 0 { format!(" recheck_drop={}", recheck_drop_n) } else { String::new() },
                    pool_cache.by_address.len(), v3_fresh, v3_total,
                    if cfg.preconf_ws_rpc.is_some() { format!(" | preconf_logs={}", preconf_n) } else { String::new() },
                    if unfunded > 0 { format!(" | unfunded={}/{}", unfunded, total_pairs) } else { String::new() },
                );
                // Requests on the main provider since the last beat, by method; then Multicall3
                // chunks on the quote endpoint (ok/timeout/err) since the last beat.
                let qmc = strategy.read().await.quote_gate.take_summary();
                let heartbeat_msg = format!("{} | rpc={} | qmc={}", heartbeat_msg, rpc_counter.take_summary(), qmc);
                // Mirror to server terminal so it's visible even when WS is disconnected.
                // A full scan normally runs every block (~2 s); a long gap means scanning has
                // stalled (e.g. RPC outage), which the counters alone don't show.
                // Paused chains don't scan, so a long gap there is expected, not a stall.
                if !paused && total_scans > 0 && last_scan_at.elapsed() > crate::config::tuning().heartbeat_stall_warn() {
                    warn!("{}", heartbeat_msg);
                } else {
                    info!("{}", heartbeat_msg);
                }
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
                if !balance_refresh_busy.swap(true, Ordering::AcqRel) {
                    let (reads, balances) = (reads.clone(), contract_balances.clone());
                    let busy = balance_refresh_busy.clone();
                    let (log_tx, chain_name, min_eth, warned) =
                        (log_tx.clone(), cfg.name.clone(), cfg.min_native_balance, gas_low_warned.clone());
                    tokio::spawn(async move {
                        state::refresh_contract_balances(reads.as_ref(), contract_addr, &balances).await;
                        state::check_signer_gas(reads.as_ref(), signer_address, min_eth, &warned, &log_tx, &chain_name).await;
                        busy.store(false, Ordering::Release);
                    });
                }
            }


            // ── Config hot-reload ─────────────────────────────────────────────
            _ = config_tick.tick() => {
                let loaded = config::load_config(&config_path).and_then(|c| {
                    if c.chain.id == cfg.id {
                        Ok(c)
                    } else {
                        Err(anyhow::anyhow!("chain.id changed to {} — the reload is ignored; restart to switch chains", c.chain.id))
                    }
                });
                match loaded {
                    Ok(new_cfg) => {
                        last_reload_err = None;
                        let new_pairs: Vec<_> = new_cfg.pairs.clone();
                        let new_routers: Vec<_> = new_cfg.routers.clone();
                        // Same instance, same chain: a reload that switched chains is ignored.
                        let new_chain_cfg = Some(&new_cfg.chain).filter(|c| c.id == cfg.id);
                        let new_min_profit = new_chain_cfg
                            .map(|c| c.min_profit_usd)
                            .unwrap_or(cfg.min_profit_usd);
                        let new_min_tri_profit = new_chain_cfg
                            .and_then(|c| c.min_triangular_profit_usd)
                            .unwrap_or(new_min_profit);

                        {
                            let mut strat = strategy.write().await;

                            let profit_changed = strat.min_profit_usd != new_min_profit
                                || strat.min_triangular_profit_usd != new_min_tri_profit;

                            strat.pairs = new_pairs;
                            strat.routers = new_routers;
                            strat.min_profit_usd = new_min_profit;
                            strat.min_triangular_profit_usd = new_min_tri_profit;
                            // Single rebuild with updated pairs, routers, and profit floors.
                            strat.rebuild_token_pair_index();
                            strat.rebuild_router_maps();

                            if profit_changed {
                                info!(
                                    "[{}] Config hot-reloaded (min_profit=${:.2}, min_triangular_profit=${:.2})",
                                    cfg.name, new_min_profit, new_min_tri_profit
                                );
                            }
                        } // strategy write lock released here — before acquiring executor lock

                        {
                            let mut exec = executor.lock().await;
                            exec.min_profit_usd = new_min_profit;
                        }
                    }
                    Err(e) => {
                        let s = e.to_string();
                        if last_reload_err.as_deref() != Some(s.as_str()) {
                            let msg = format!("[{}] Config reload failed — still running the previous config: {}", cfg.name, s);
                            warn!("{}", msg);
                            broadcast_log(&log_tx, "error", &msg, None);
                            last_reload_err = Some(s);
                        }
                    }
                }
            }

            // ── New block header → immediate scan ─────────────────────────────
            Some((block_num, base_fee)) = block_rx.recv() => {
                // Pre-populate gas price cache from block header — eliminates eth_gasPrice RPC calls.
                executor.lock().await.update_gas_price(base_fee);
                last_block_seen = Instant::now();

                // Update RPC health + block number in shared state, count each block as a scan
                {
                    scan_count.fetch_add(1, Ordering::Relaxed);
                    let base_fee_gwei = base_fee as f64 / 1e9;
                    let mut state = shared_state.write().await;
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.rpc_ok = true;
                        chain.last_block = block_num;
                        chain.last_block_at = crate::api::now_secs();
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
                if reset_breaker_on_resume(chain_paused, &mut consecutive_failures) {
                    info!("[{}] Circuit breaker cleared by resume", cfg.name);
                }
                if global_paused || chain_paused {
                    on_paused(&mut last_scan_at, &rpc_health);
                    continue;
                }

                scan::evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &best_raw_profit, &best_spread_bits, &best_verified_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                    &opps_found, &recheck_drops, &contract_balances, None, disabled_set, None,
                    "block",
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
                scan_count.fetch_add(1, Ordering::Relaxed);
                let (global_paused, chain_paused, disabled_set) = {
                    let state = shared_state.read().await;
                    let cp = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                    (state.paused, cp, state.disabled_pairs.clone())
                };
                if reset_breaker_on_resume(chain_paused, &mut consecutive_failures) {
                    info!("[{}] Circuit breaker cleared by resume", cfg.name);
                }
                if global_paused || chain_paused {
                    on_paused(&mut last_scan_at, &rpc_health);
                    continue;
                }

                scan::evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &best_raw_profit, &best_spread_bits, &best_verified_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                    &opps_found, &recheck_drops, &contract_balances, None, disabled_set, None,
                    "poll",
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

                // Single strategy read: pool tokens + affected pair indices.
                let (pair_mask, token_filter, affected_pair_ids) = {
                    let strat = strategy.read().await;
                    let (tok_a, tok_b) = match strat.pool_cache.get_pool_tokens(event.pool) {
                        Some(t) => t,
                        None => continue,
                    };
                    let indices = strat.pairs_for_tokens(tok_a, tok_b);
                    if indices.is_empty() {
                        continue;
                    }
                    let ids: Vec<String> = if is_large {
                        indices.iter().map(|&i| strat.pairs[i].id.clone()).collect()
                    } else {
                        vec![]
                    };
                    (
                        indices.into_iter().collect::<HashSet<usize>>(),
                        vec![tok_a, tok_b],
                        ids,
                    )
                };

                // Clear cooldowns for affected pairs. Fingerprints contain pair_id as a prefix
                // so a simple contains() check works without any allocations per entry.
                if is_large && !affected_pair_ids.is_empty() {
                    cooldowns.retain(|fingerprint, _| {
                        !affected_pair_ids.iter().any(|id| fingerprint.contains(id.as_str()))
                    });
                }

                scan_count.fetch_add(1, Ordering::Relaxed);
                let (global_paused, chain_paused, disabled_set) = {
                    let state = shared_state.read().await;
                    let cp = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                    (state.paused, cp, state.disabled_pairs.clone())
                };
                if reset_breaker_on_resume(chain_paused, &mut consecutive_failures) {
                    info!("[{}] Circuit breaker cleared by resume", cfg.name);
                }
                if global_paused || chain_paused {
                    on_paused(&mut last_scan_at, &rpc_health);
                    continue;
                }

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
                    &opps_found, &recheck_drops, &contract_balances, Some((pair_mask, token_filter)), disabled_set,
                    None,
                    "swap",
                ).await;
                // NOTE: last_scan_at intentionally NOT updated here.
            }

            // ── Pending TX detected → same-block arb submission ──────────────
            // Fires when a large pending swap is seen in the mempool before it
            // confirms. We run a targeted scan for pairs involving the swapped
            // tokens and override the tip to match the pending TX's tip, landing
            // our TX in the SAME block as the triggering swap.
            Some(pending_event) = pending_rx.recv() => {
                let (global_paused, chain_paused, disabled_set) = {
                    let state = shared_state.read().await;
                    let cp = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                    (state.paused, cp, state.disabled_pairs.clone())
                };
                if reset_breaker_on_resume(chain_paused, &mut consecutive_failures) {
                    info!("[{}] Circuit breaker cleared by resume", cfg.name);
                }
                if global_paused || chain_paused {
                    on_paused(&mut last_scan_at, &rpc_health);
                    continue;
                }

                // Build targeted mask for pairs involving the pending swap's tokens
                let (pair_mask, token_filter) = {
                    let strat = strategy.read().await;
                    let indices = strat.pairs_for_tokens(pending_event.token_in, pending_event.token_out);
                    (indices.into_iter().collect::<HashSet<usize>>(), vec![pending_event.token_in, pending_event.token_out])
                };
                if pair_mask.is_empty() { continue; }

                // tip = pending TX tip (we match to land in same block).
                // Slightly higher than the trigger so we're competitive for same-block ordering.
                let tip_override = pending_event.tip_wei.max(cfg.priority_fee_wei);

                let pair_count = pair_mask.len();
                // Log AFTER evaluate_and_execute to avoid a format!+String alloc on the hot path
                // before the scan fires. The log still appears immediately after return.
                scan::evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &best_raw_profit, &best_spread_bits, &best_verified_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                    &opps_found, &recheck_drops, &contract_balances, Some((pair_mask, token_filter)), disabled_set,
                    Some(tip_override),
                    "pending",
                ).await;
                info!(
                    "[{}] Pending swap processed: {:?}→{:?} amt={} tip={}wei ({} pairs)",
                    cfg.name, pending_event.token_in, pending_event.token_out,
                    pending_event.amount_in, pending_event.tip_wei, pair_count
                );
                // NOTE: last_scan_at intentionally NOT updated here.
            }
        }
    }
}

// ─── Pool log subscriptions ───────────────────────────────────────────────────

/// What a log subscription feeds: the pool cache and the scan trigger.
#[derive(Clone)]
struct LogFeed {
    cache: Arc<PoolCache>,
    swap_tx: mpsc::Sender<SwapEvent>,
    chain_id: u64,
    chain_name: String,
    solidly_sync_hash: alloy::primitives::B256,
    large_swap_bps: u32,
    large_v3_bps: u32,
}

/// Subscribe to pool logs and keep re-subscribing. `method`: `None` = standard `logs`
/// (sealed blocks), `Some("pendingLogs")` = Flashblocks pre-confirmed logs. Each new log
/// updates the cache and triggers a targeted scan (see `apply_log_to_cache`).
fn spawn_log_subscription<P: Provider + Clone + 'static>(
    provider: P,
    filter: alloy::rpc::types::Filter,
    feed: LogFeed,
    method: Option<&'static str>,
    counter: Option<Arc<AtomicU64>>,
) {
    tokio::spawn(run_log_subscription(provider, filter, feed, method, counter));
}

/// Bound on one WebSocket handshake. alloy's `connect_ws` has none, so a stalled
/// endpoint (accepts TCP, never answers) would otherwise hang the caller forever.
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Flashblocks `pendingLogs` feed: connect to `url` in a background task, retrying
/// with backoff (each attempt bounded by [`WS_CONNECT_TIMEOUT`]), then run the
/// subscription. Until it connects, detection runs on sealed-block logs (~1.2 s later).
fn spawn_preconf_subscription(
    url: String,
    filter: alloy::rpc::types::Filter,
    feed: LogFeed,
    counter: Arc<AtomicU64>,
) {
    tokio::spawn(async move {
        let shown = crate::util::redact_url(&url);
        let mut backoff = Duration::from_secs(1);
        let mut warned = false;
        let preconf = loop {
            let conn = ProviderBuilder::new()
                .connect_ws(WsConnect::new(url.clone()).with_max_retries(u32::MAX));
            match rpc_timeout(WS_CONNECT_TIMEOUT, conn).await {
                Ok(p) => break p,
                Err(e) => {
                    if !warned {
                        warn!("[{}] Flashblocks endpoint {} unreachable ({}) — sealed-block logs only, retrying in background",
                              feed.chain_name, shown, e);
                        warned = true;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(crate::config::tuning().ws_reconnect_max_backoff());
                }
            }
        };
        info!("[{}] Flashblocks pre-confirmed logs: subscribing on {}", feed.chain_name, shown);
        run_log_subscription(preconf, filter, feed, Some("pendingLogs"), Some(counter)).await;
    });
}

async fn run_log_subscription<P: Provider + Clone + 'static>(
    provider: P,
    filter: alloy::rpc::types::Filter,
    feed: LogFeed,
    method: Option<&'static str>,
    counter: Option<Arc<AtomicU64>>,
) {
    let label = method.unwrap_or("logs");
    let mut backoff = Duration::from_secs(1);
    let mut last_err = String::new();
    loop {
        let sub = match method {
            None => provider.subscribe_logs(&filter).await,
            Some(m) => provider.subscribe::<_, alloy::rpc::types::Log>((m, filter.clone())).await,
        };
        match sub {
            Ok(sub) => {
                backoff = Duration::from_secs(1);
                last_err.clear();
                let mut stream = sub.into_stream();
                while let Some(log) = stream.next().await {
                    if let Some(c) = &counter {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(magnitude) = crate::listener::apply_log_to_cache(
                        &log, &feed.cache, feed.solidly_sync_hash, feed.large_swap_bps, feed.large_v3_bps,
                    ) {
                        let _ = feed.swap_tx.try_send(SwapEvent {
                            chain_id: feed.chain_id,
                            pool: log.address(),
                            block_number: log.block_number.unwrap_or(0),
                            magnitude,
                        });
                    }
                }
                debug!("[{}] {} subscription ended, reconnecting...", feed.chain_name, label);
            }
            Err(e) => {
                // Once per distinct error: an endpoint without the method would
                // otherwise log every backoff step.
                let msg = e.to_string();
                if msg != last_err {
                    warn!("[{}] {} subscription failed: {} — retrying with backoff", feed.chain_name, label, msg);
                    last_err = msg;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(crate::config::tuning().ws_reconnect_max_backoff());
            }
        }
    }
}

// ─── RPC connection with fallback ─────────────────────────────────────────────

/// The chain's RPC monitor: while degraded and past the hold, probe every
/// [`crate::util::rpc_monitor_every()`] with a plain eth_call (Multicall3.getBlockNumber,
/// the method simulations use) and clear the degraded state once it succeeds within
/// [`crate::util::rpc_probe_healthy()`]. The only writer of "recovered", and independent of
/// scans, so a quiet market can't leave a chain stuck. Holds weak references: it exits
/// when this chain run's provider or health object is dropped (shutdown / reconnect).
fn spawn_rpc_monitor<P: Provider + Clone + 'static>(
    provider: &Arc<P>,
    rpc_health: &crate::util::SharedRpcHealth,
    chain: String,
) {
    use crate::util::{rpc_timeout, classify_rpc_failure, RpcFailure, rpc_monitor_every, rpc_probe_healthy};
    const MULTICALL3: Address = alloy::primitives::address!("cA11bde05977b3631167028862bE2a173976CA11");
    const GET_BLOCK_NUMBER: [u8; 4] = [0x42, 0xcb, 0xb1, 0x5c]; // getBlockNumber()
    let (provider, health) = (Arc::downgrade(provider), Arc::downgrade(rpc_health));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(rpc_monitor_every());
        let mut slow_logged = false;
        loop {
            tick.tick().await;
            let (Some(provider), Some(health)) = (provider.upgrade(), health.upgrade()) else { return };
            if !health.probe_due() {
                slow_logged = false;
                continue;
            }
            let tx = alloy::rpc::types::TransactionRequest::default()
                .to(MULTICALL3)
                .input(alloy::primitives::Bytes::from_static(&GET_BLOCK_NUMBER).into());
            let t = Instant::now();
            let result = rpc_timeout(rpc_probe_healthy(), provider.call(tx)).await;
            let took = t.elapsed();
            let rate_limited = result.as_ref().err().is_some_and(|e| classify_rpc_failure(e) == RpcFailure::RateLimited);
            if result.is_ok() || rate_limited {
                if let Some(was) = health.recover() {
                    info!("[{}] RPC healthy again ({:.2}s{}) — execution back on after {:.0?}",
                          chain, took.as_secs_f64(), if rate_limited { ", rate limited" } else { "" }, was);
                }
            } else if !slow_logged {
                // Once per degraded period: the probe itself keeps running every 2 s.
                warn!("[{}] RPC not healthy yet ({}) — execution stays off, probing every {:?}",
                      chain, result.err().unwrap_or_default(), rpc_monitor_every());
                slow_logged = true;
            }
        }
    });
}

/// Median and maximum of the latency samples (ms); (0, 0) when empty.
fn latency_median_max(samples: &std::collections::VecDeque<f64>) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut v: Vec<f64> = samples.iter().copied().collect();
    v.sort_by(|a, b| a.total_cmp(b));
    let mid = v.len() / 2;
    let median = if v.len() % 2 == 0 { (v[mid - 1] + v[mid]) / 2.0 } else { v[mid] };
    (median, v[v.len() - 1])
}

/// A breaker pause that was just cleared by /engine/resume left the failure count at the
/// threshold; reset it so the next single failure doesn't re-trip at once.
fn reset_breaker_on_resume(chain_paused: bool, consecutive_failures: &mut u32) -> bool {
    if !chain_paused && *consecutive_failures >= max_consecutive_failures() {
        *consecutive_failures = 0;
        true
    } else {
        false
    }
}

/// A scan trigger fired while the chain is paused: no scan is expected (keeps the
/// heartbeat's stall check quiet, also right after resume), and any RPC backoff from
/// before the pause is stale.
fn on_paused(last_scan_at: &mut Instant, rpc_health: &crate::util::RpcHealth) {
    *last_scan_at = Instant::now();
    rpc_health.clear();
}

/// Try `cfg.ws_rpc` first, then each entry in `cfg.ws_rpc_fallbacks` in order.
/// All URLs should be WebSocket endpoints (wss:// or ws://).
/// HTTP fallback URLs in ws_rpc_fallbacks will connect successfully for eth_call
/// but subscription-based events will fail gracefully — periodic poll scanning continues.
async fn connect_with_fallback(
    cfg: &ChainConfig,
    wallet: EthereumWallet,
    counter: crate::rpc_count::RpcCounter,
) -> Result<impl Provider + Clone + 'static> {
    let urls: Vec<&str> = std::iter::once(cfg.ws_rpc.as_str())
        .chain(cfg.ws_rpc_fallbacks.iter().map(String::as_str))
        .collect();
    connect_first(&cfg.name, &urls, wallet, WS_CONNECT_TIMEOUT, counter).await
}

/// First URL whose WS handshake completes within `limit` (a stalled endpoint falls
/// through to the next one instead of hanging startup).
async fn connect_first(
    name: &str,
    urls: &[&str],
    wallet: EthereumWallet,
    limit: Duration,
    counter: crate::rpc_count::RpcCounter,
) -> Result<impl Provider + Clone + 'static> {
    let mut last_err = String::new();
    for (i, url) in urls.iter().enumerate() {
        // Every request on this provider is counted per method (heartbeat `rpc=`).
        let client = alloy::rpc::client::ClientBuilder::default()
            .layer(crate::rpc_count::CountLayer(counter.clone()))
            .ws(WsConnect::new(*url).with_max_retries(u32::MAX));
        match rpc_timeout(limit, client).await {
            Ok(client) => {
                let p = ProviderBuilder::<_, _, Ethereum>::new()
                    .wallet(wallet.clone())
                    .connect_client(client);
                if i > 0 {
                    info!("[{}] Connected via fallback RPC #{}: {}", name, i, crate::util::redact_url(url));
                }
                return Ok(p);
            }
            Err(e) => {
                warn!("[{}] RPC endpoint {} failed: {}", name, crate::util::redact_url(url), e);
                last_err = e;
            }
        }
    }

    Err(anyhow::anyhow!(
        "all {} RPC endpoint(s) exhausted — last error: {}",
        urls.len(),
        last_err
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_median_max_cases() {
        use std::collections::VecDeque;
        assert_eq!(latency_median_max(&VecDeque::new()), (0.0, 0.0));
        // One 21 ms spike among 6–9 ms samples moves the max, not the median.
        let v: VecDeque<f64> = [6.0, 9.0, 21.0, 7.0, 8.0, 6.0].into_iter().collect();
        assert_eq!(latency_median_max(&v), (7.5, 21.0));
        let v: VecDeque<f64> = [5.0, 30.0, 6.0].into_iter().collect();
        assert_eq!(latency_median_max(&v), (6.0, 30.0));
    }

    /// A WS endpoint that accepts TCP but never answers the handshake must not hang
    /// startup: each attempt times out and the next URL is tried.
    #[tokio::test]
    async fn stalled_ws_handshake_times_out_and_falls_through() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock); // keep open, never reply
            }
        });
        let url = format!("ws://{}", addr);
        let wallet = EthereumWallet::from(PrivateKeySigner::random());
        let t0 = Instant::now();
        let res = connect_first("test", &[url.as_str(), url.as_str()], wallet, Duration::from_millis(300), Default::default()).await;
        let err = match res { Ok(_) => panic!("stalled endpoint connected"), Err(e) => e.to_string() };
        assert!(err.contains("timed out"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(3), "took {:?}", t0.elapsed());
    }

    #[test]
    fn reset_breaker_on_resume_cases() {
        // Default tuning: threshold 20.
        let mut n = 20;
        assert!(reset_breaker_on_resume(false, &mut n));
        assert_eq!(n, 0);
        let mut n = 20;
        assert!(!reset_breaker_on_resume(true, &mut n));
        assert_eq!(n, 20);
        let mut n = 3;
        assert!(!reset_breaker_on_resume(false, &mut n));
        assert_eq!(n, 3);
    }
}
