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
pub mod maintain;

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

    // ── Startup reads: public http_rpc first, main (paid) WS provider as fallback ──
    // The setup check and pool discovery are ~70 eth_calls per (re)start; on http_rpc they
    // cost the main RPC nothing. A step that fails or loses calls there is redone once on
    // the main provider. The chain-id guard above and the nonce stay on the main provider.
    let http_rpc_provider: Option<alloy::providers::DynProvider> = cfg.http_rpc
        .parse::<url::Url>()
        .ok()
        .map(|url| ProviderBuilder::new().connect_http(url).erased());
    let main_reads = provider.as_ref().clone().erased();
    let startup_reads = http_rpc_provider.as_ref().unwrap_or(&main_reads);
    let has_startup_fallback = http_rpc_provider.is_some();
    // Periodic pool rediscovery reads on the same endpoint as startup discovery.
    let rediscover_reads = startup_reads.clone();

    // ── Executor setup check: owner = signer (exit 2), router allowlist (exit 2 live), paused ──
    // Runs before pool discovery so a wrong key fails within seconds. Startup only.
    let dry_run_at_start = shared_state.read().await.dry_run;
    let setup = |p| state::check_contract_setup(
        p, contract_addr, signer_address, &routers, &cfg.name, dry_run_at_start, cfg.borrow, &log_tx,
    );
    let start_paused = match setup(startup_reads).await {
        Err(e) if has_startup_fallback => {
            warn!("{} (http_rpc) — retrying the setup check on the main RPC", e);
            setup(&main_reads).await?
        }
        r => r?,
    };

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
    let mut lost_v2 = discover::discover_pools(&chain_routers, &chain_pairs, startup_reads, &pool_cache).await;
    if lost_v2 > 0 && has_startup_fallback {
        warn!("[{}] V2/Aerodrome discovery lost {} call(s) on http_rpc — retrying on the main RPC", cfg.name, lost_v2);
        lost_v2 = discover::discover_pools(&chain_routers, &chain_pairs, &main_reads, &pool_cache).await;
    }
    info!("[{}] Pool cache: {} V2/Aerodrome pools discovered", cfg.name, pool_cache.by_address.len());

    // ── V3 pool discovery: seed sqrtPriceX96 + liquidity from slot0() ─────────
    // Subscribes to V3 Swap events after startup to keep state fresh with zero RPC cost.
    let mut lost_v3 = discover::discover_v3_pools(&chain_routers, &chain_pairs, startup_reads, &pool_cache).await;
    if lost_v3 > 0 && has_startup_fallback {
        warn!("[{}] V3 discovery lost {} call(s) on http_rpc — retrying on the main RPC", cfg.name, lost_v3);
        lost_v3 = discover::discover_v3_pools(&chain_routers, &chain_pairs, &main_reads, &pool_cache).await;
    }
    let mut lost_v4 = discover::discover_v4_pools(&chain_routers, &chain_pairs, startup_reads, &pool_cache).await;
    if lost_v4 > 0 && has_startup_fallback {
        warn!("[{}] V4 discovery lost {} call(s) on http_rpc — retrying on main RPC", cfg.name, lost_v4);
        lost_v4 = discover::discover_v4_pools(&chain_routers, &chain_pairs, &main_reads, &pool_cache).await;
    }
    lost_v3 += lost_v4;
    if !pool_cache.v4_keys.is_empty() {
        info!("[{}] V4 pool cache: {} hookless pools seeded", cfg.name, pool_cache.v4_keys.len());
    }
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
    // With `quote_http_rpc`, quotes and simulations use that endpoint (at `latest`).
    // (URL validated at config load.)
    let quote_http: Option<alloy::providers::DynProvider> = cfg.quote_http_rpc.as_deref()
        .and_then(|raw| {
            let url = raw.parse::<url::Url>().ok()?;
            info!("[{}] Quotes and simulations on {}", cfg.name, crate::util::redact_url(raw));
            Some(ProviderBuilder::new().connect_http(url).erased())
        });
    let http_provider: Option<alloy::providers::DynProvider> = quote_http.clone().or(http_rpc_provider);
    executor.lock().await.sim_provider = quote_http.clone();
    // Background reads (contract/signer balances, ETH price) don't need the main provider's
    // speed or subscriptions: they go to the HTTP endpoint, keeping the main (paid) RPC for
    // subscriptions, sends, receipts and nonces. Falls back to the main provider without one.
    let reads: Arc<alloy::providers::DynProvider> = Arc::new(
        http_provider.clone().unwrap_or(main_reads),
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
    maintain::spawn_depth_refresh(strategy.clone(), pool_cache.clone(), chain_pairs.clone(), cfg.name.clone());

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
    // Startup funding check: warn for any underfunded pair (borrowed trades need no balance).
    if !cfg.borrow {
        state::check_contract_funding(&cfg.name, &chain_pairs, contract_addr, &contract_balances, &log_tx).await;
    }
    // Balance slots for dry-run's assumed balance (simulate unfunded trades instead of
    // skipping them). On the simulation endpoint, which must accept eth_call state overrides.
    {
        let sim: &alloy::providers::DynProvider = quote_http.as_ref().unwrap_or(reads.as_ref());
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
    // Direct venues (legs sent to pools) and V4 borrowing.
    {
        let mut exec = executor.lock().await;
        exec.direct = Arc::new(crate::executor::DirectVenues::from_routers(&chain_routers, pool_cache.clone()));
        exec.borrow = cfg.borrow;
    }
    if cfg.borrow {
        info!("[{}] Borrowing: every trade takes its amountIn from the Uniswap V4 PoolManager (no fee, no interest) — contract balance not used", cfg.name);
    }
    // Direct venues trade only on pools the executor allowlists: report the missing ones.
    state::check_direct_pools(reads.as_ref(), contract_addr, &chain_routers, &pool_cache, dry_run_at_start, &cfg.name, &log_tx).await;
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
                blind: None,
                last_block: 0,
                last_block_at: 0,
                rpc_latency_ms: 0.0,
                rpc_latency_max_ms: 0.0,
            });
        } else if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
            chain.rpc_ok = true;
            chain.blind = None; // a previous run's verdict; the next heartbeat re-judges
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

    // ── Block-header subscription (gas price, head position, stall watchdog) ──
    let (block_tx, mut block_rx) = mpsc::channel::<(u64, u128)>(64);
    {
        let provider_for_blocks = (*provider).clone();
        let chain_name_b = cfg.name.clone();
        let block_tx_b = block_tx.clone();
        crate::util::set_slot_ms(cfg.block_time_ms);
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            let mut clock_warned: Option<Instant> = None;
            loop {
                match provider_for_blocks.subscribe_blocks().await {
                    Ok(sub) => {
                        backoff = Duration::from_secs(1);
                        let mut stream = sub.into_stream();
                        while let Some(header) = stream.next().await {
                            let num = header.number;
                            let base_fee = header.base_fee_per_gas.unwrap_or(1_000_000_000) as u128;
                            // Block positions in the logs count from the head's arrival. A server
                            // clock far off the chain also skews trade deadlines, so say so.
                            let lead = crate::util::note_head(header.number, header.timestamp);
                            if !(-2..=4).contains(&lead) && clock_warned.is_none_or(|t| t.elapsed() > Duration::from_secs(600)) {
                                warn!("[{}] Server clock is {}s {} the chain head — sync it (NTP): trade deadlines use the host clock", chain_name_b, lead.abs(), if lead < 0 { "behind" } else { "ahead of" });
                                clock_warned = Some(Instant::now());
                            }
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
    // Canonical-block logs on the primary RPC: every pool, one batch per 200 ms block. They
    // update the cache and trigger the targeted scan. (Base retired Flashblocks and their
    // `pendingLogs` feed with the Denim upgrade; a sealed block is now as early as it gets.)
    let solidly_sync_hash_sub = alloy::primitives::keccak256(b"Sync(uint256,uint256)");
    let log_feed = LogFeed {
        cache: pool_cache.clone(),
        swap_tx: swap_tx_for_sub.clone(),
        chain_id: cfg.id,
        chain_name: cfg.name.clone(),
        solidly_sync_hash: solidly_sync_hash_sub,
        large_swap_bps: cfg.large_swap_threshold_bps,
        large_v3_bps: cfg.large_v3_threshold_bps,
    };
    // Starts the log feeds for the cache's current pools. Re-run by pool rediscovery
    // (maintain.rs) when new pools appear: the new feeds start before the old ones are
    // aborted, so no log is missed; the overlap's duplicates are dropped by the cache's
    // log-position guard.
    // V4 pools log through their PoolManager (the `v4: true` router's address).
    let v4_managers: Vec<Address> = chain_routers.iter().filter(|r| r.v4)
        .filter_map(|r| r.address.parse().ok()).collect();
    let start_log_feeds: Arc<dyn Fn() -> Vec<tokio::task::JoinHandle<()>> + Send + Sync> = {
        let (cache, provider, feed) = (pool_cache.clone(), provider.clone(), log_feed.clone());
        Arc::new(move || {
            let addrs = cache.watch_addresses();
            let mut handles = Vec::new();
            // An address-less filter would match every log on the chain: skip it instead.
            if !addrs.is_empty() {
                let filter = alloy::rpc::types::Filter::new()
                    .address(addrs)
                    .event_signature(crate::listener::pool_log_topics(solidly_sync_hash_sub));
                handles.push(spawn_log_subscription((*provider).clone(), filter, feed.clone()));
            }
            // V4: the PoolManager's Swap / ModifyLiquidity for the cached pool ids only (it
            // logs for every V4 pool on the chain).
            let v4_ids = cache.v4_pool_ids();
            if !v4_managers.is_empty() && !v4_ids.is_empty() {
                let v4_filter = alloy::rpc::types::Filter::new()
                    .address(v4_managers.clone())
                    .event_signature(crate::listener::v4_log_topics())
                    .topic1(v4_ids);
                handles.push(spawn_log_subscription((*provider).clone(), v4_filter, feed.clone()));
            }
            handles
        })
    };
    let log_feed_handles = start_log_feeds();
    maintain::spawn_fee_refresh(rediscover_reads.clone(), chain_routers.clone(), pool_cache.clone(), cfg.name.clone());
    maintain::spawn_rediscovery(
        rediscover_reads, chain_routers.clone(), chain_pairs.clone(), pool_cache.clone(),
        start_log_feeds.clone(), log_feed_handles, cfg.name.clone(),
        (contract_addr, dry_run_at_start, log_tx.clone()),
    );

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
    // Detection runs on pool logs: each block's swaps on our pools trigger one targeted
    // scan of the pairs they touch (swap arm). A block with no such swap needs no scan —
    // nothing we price moved. The full scan (every pair and triangle) runs every
    // `scan_interval_ms` (10 s) as the safety net for state that moved without a log we saw.
    // Tracks when the last full scan ran.
    let mut last_scan_at = Instant::now() - Duration::from_secs(60);
    let poll_interval = Duration::from_millis(cfg.scan_interval_ms);
    let mut poll_tick     = tokio::time::interval(poll_interval);
    let mut price_tick    = tokio::time::interval(crate::config::tuning().price_refresh());
    let mut config_tick   = tokio::time::interval(crate::config::tuning().config_reload());
    let mut balance_tick  = tokio::time::interval(crate::config::tuning().balance_refresh());
    // After a stall (a slow scan), skip missed ticks instead of firing them back to back.
    for t in [&mut poll_tick, &mut price_tick, &mut config_tick, &mut balance_tick] {
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    }
    // When this run's loop started: the feed-silence clock before the first pool log.
    let loop_started = Instant::now();
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
    // Quote and submission endpoint round trips for the heartbeat's `lat rtt=[…]` (hosts
    // only — URLs can carry API keys): one request each per `rpc_latency_sample_ms` on a
    // reused connection (warm) and on a new one (cold = what a send pays after the send
    // pool dropped its idle connection). Own clients, so the send pool isn't warmed.
    // Quote endpoints get eth_chainId. Send endpoints get eth_sendRawTransaction with an
    // undecodable tx ("0x00"): the sequencer refuses every other method (403), and any
    // JSON-RPC answer, an error included, means the send path is up.
    {
        let endpoints: Vec<(&'static str, url::Url)> = cfg.quote_http_rpc.iter()
            .filter_map(|u| u.parse().ok()).map(|u| ("quote", u))
            .chain(cfg.submission_rpcs.iter().filter_map(|u| u.parse().ok()).map(|u| ("send", u)))
            .collect();
        let warm = reqwest::Client::builder().timeout(rpc_read_timeout()).build();
        let cold = reqwest::Client::builder().timeout(rpc_read_timeout()).pool_max_idle_per_host(0).build();
        if let (false, Ok(warm), Ok(cold)) = (endpoints.is_empty(), warm, cold) {
            tokio::spawn(async move {
                let quote_body = serde_json::json!({"jsonrpc": "2.0", "method": "eth_chainId", "params": [], "id": 1});
                let send_body = serde_json::json!({"jsonrpc": "2.0", "method": "eth_sendRawTransaction", "params": ["0x00"], "id": 1});
                let rtt = |c: &reqwest::Client, role: &str, url: &url::Url| {
                    let body = if role == "send" { &send_body } else { &quote_body };
                    let (req, start) = (c.post(url.as_str()).json(body).send(), Instant::now());
                    async move {
                        let answered = match req.await {
                            Ok(r) => r.json::<serde_json::Value>().await
                                .is_ok_and(|v| v.get("result").is_some() || v.get("error").is_some()),
                            Err(_) => false,
                        };
                        let ms = start.elapsed().as_millis();
                        if answered { format!("{}ms", ms) } else { "err".to_string() }
                    }
                };
                let mut tick = tokio::time::interval(crate::config::tuning().rpc_latency_sample());
                loop {
                    tick.tick().await;
                    let mut parts = Vec::with_capacity(endpoints.len());
                    for (role, url) in &endpoints {
                        let w = rtt(&warm, role, url).await;
                        let c = rtt(&cold, role, url).await;
                        parts.push(format!("{}:{}={}/cold={}", role, url.host_str().unwrap_or("?"), w, c));
                    }
                    if let Ok(mut s) = crate::util::ENDPOINT_RTT.lock() {
                        *s = parts.join(" ");
                    }
                }
            });
        }
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
                let recheck_drop_n = recheck_drops.load(Ordering::Relaxed);
                // Pairs the executor can't fund (cached balances, no RPC): they detect but never trade.
                let unfunded = if cfg.borrow { 0 } else { state::underfunded_pairs(&chain_pairs, &*contract_balances.read().await).len() };
                let heartbeat_msg = format!(
                    "[{}] ♥ scans={} execs={} ok={} | last_scan={}s ago | best={} bestV={} spread={} spot2={} tri={} triV={} | quotes={} active={}/{} cross={} | opps={}{} | v2={} v3={}/{}{}",
                    cfg.name, scans, attempts, success, last_scan_at.elapsed().as_secs(), best_seen_str, verified_str, spread_str, spot2_str, tri_str, tri_v_str, fwd_ok, active_pairs, total_pairs, multi_dex, opp_count,
                    if recheck_drop_n > 0 { format!(" recheck_drop={}", recheck_drop_n) } else { String::new() },
                    pool_cache.by_address.len(), v3_fresh, v3_total,
                    if unfunded > 0 { format!(" | unfunded={}/{}", unfunded, total_pairs) } else { String::new() },
                );
                // Requests on the main provider since the last beat, by method; then Multicall3
                // chunks on the quote endpoint (ok/timeout/err) since the last beat.
                let (qmc, tri_cal, p2_cal) = {
                    let s = strategy.read().await;
                    (s.quote_gate.take_counts(), s.tri_cal.take_summary(), s.p2_cal.take_summary())
                };
                let heartbeat_msg = format!("{} | rpc={} | qmc={}", heartbeat_msg, rpc_counter.take_summary(), qmc.summary());
                // Triangles verified per local-spread band (verified/positive) and memo skips.
                let heartbeat_msg = match tri_cal {
                    Some(c) => format!("{heartbeat_msg} | tri_cal={c}"),
                    None => heartbeat_msg,
                };
                // 2-hop routes verified, split by local profit below/above the USD floor.
                let heartbeat_msg = match p2_cal {
                    Some(c) => format!("{heartbeat_msg} | p2_cal={c}"),
                    None => heartbeat_msg,
                };
                // Blind = running but unable to detect: say so loudly (ERROR + /health 503)
                // instead of a normal-looking heartbeat. Paused chains are expected to be quiet.
                let blind = {
                    let mut why = Vec::new();
                    if qmc.mostly_failing() {
                        why.push(format!("quotes failing ({} of {} chunks)", qmc.timeout + qmc.err, qmc.ok + qmc.timeout + qmc.err));
                    }
                    let blind_after = Duration::from_millis(crate::config::tuning().feed_blind_ms);
                    let silent = pool_cache.since_last_log().unwrap_or_else(|| loop_started.elapsed());
                    if !blind_after.is_zero() && silent >= blind_after {
                        why.push(format!("no pool log for {}s", silent.as_secs()));
                    }
                    (!paused && !why.is_empty()).then(|| why.join(", "))
                };
                if let Some(why) = &blind {
                    let msg = format!("[{}] BLIND: {} — no opportunity can be detected or verified", cfg.name, why);
                    tracing::error!("{}", msg);
                    broadcast_log(&log_tx, "error", &msg, None);
                }
                if let Some(chain) = shared_state.write().await.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                    chain.blind = blind;
                }
                // Latency since the last beat (p50/p95/max(n) ms): see util::take_latency_summary.
                let heartbeat_msg = format!("{} | lat {}", heartbeat_msg, crate::util::take_latency_summary());
                // Mirror to server terminal so it's visible even when WS is disconnected.
                // A full scan normally runs every `scan_interval_ms`; a long gap means scanning
                // has stalled (e.g. RPC outage), which the counters alone don't show.
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

            // ── New block header: gas price, head, liveness (no scan) ──────────
            // At 200 ms blocks a full scan per block was 300/min and held the loop for most
            // of every slot, so swap events queued for seconds. Prices move only through
            // pool logs, which the swap arm scans; the full scan runs on poll_tick.
            Some((block_num, base_fee)) = block_rx.recv() => {
                // Pre-populate gas price cache from block header — eliminates eth_gasPrice RPC calls.
                executor.lock().await.update_gas_price(base_fee);
                last_block_seen = Instant::now();
                {
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
            }

            // ── Periodic full scan (safety net: state that moved without a log we saw) ──
            _ = poll_tick.tick() => {
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
                    &opps_found, &recheck_drops, &contract_balances, None, disabled_set,
                    "poll",
                    None,
                ).await;
                last_scan_at = Instant::now();
            }

            // ── Block's swaps → one targeted scan ────────────────────────────────
            // A block's pool logs arrive together: collect them (see collect_swap_batch) and
            // scan only the pairs and triangles they touch. Logs that came in while the
            // previous scan ran are merged into this one, so the loop is never more than one
            // scan behind the chain. last_scan_at is NOT updated — it times the full scan.
            Some(event) = swap_rx.recv() => {
                let batch = collect_swap_batch(event, &mut swap_rx).await;
                for ev in &batch {
                    crate::util::LAT_QUEUE.record(ev.received_at.elapsed().as_millis() as u64);
                }
                if batch.iter().any(|ev| ev.block_number != batch[0].block_number) {
                    crate::util::SCANS_MERGED.fetch_add(1, Ordering::Relaxed);
                }
                let is_large = batch.iter().any(|ev| ev.magnitude == SwapMagnitude::Large);
                let merged = batch.len() - 1;
                let event = batch.iter().find(|ev| ev.magnitude == SwapMagnitude::Large).unwrap_or(&batch[0]).clone();
                // The longest-waiting event this scan answers: the reaction time in the
                // exec_lat line starts at its arrival.
                let trigger_at = oldest_event(&batch);

                // Single strategy read: pool tokens + affected pair indices, merged over the batch.
                let (pair_mask, token_filter, affected_pair_ids) = {
                    let strat = strategy.read().await;
                    merge_swap_batch(&batch, |p| strat.pool_cache.get_pool_tokens(p), |a, b| strat.pairs_for_tokens(a, b),
                                     |i| strat.pairs[i].id.clone())
                };
                if pair_mask.is_empty() {
                    continue;
                }

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
                        "[{}] Swap on {:?} — targeted scan ({} pairs, {} queued swap(s) merged)",
                        cfg.name, event.pool, pair_mask.len(), merged
                    );
                }

                scan::evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
                    &best_raw_profit, &best_spread_bits, &best_verified_spread_bits, &last_fwd_count, &last_multi_count, &last_active_count,
                    &opps_found, &recheck_drops, &contract_balances, Some((pair_mask, token_filter)), disabled_set,
                    "swap",
                    trigger_at,
                ).await;
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

/// Subscribe to pool logs (canonical blocks) and keep re-subscribing. Each new log updates
/// the cache and triggers a targeted scan (see `apply_log_to_cache`).
fn spawn_log_subscription<P: Provider + Clone + 'static>(
    provider: P,
    filter: alloy::rpc::types::Filter,
    feed: LogFeed,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_log_subscription(provider, filter, feed))
}

/// Bound on one WebSocket handshake. alloy's `connect_ws` has none, so a stalled
/// endpoint (accepts TCP, never answers) would otherwise hang the caller forever.
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

async fn run_log_subscription<P: Provider + Clone + 'static>(
    provider: P,
    filter: alloy::rpc::types::Filter,
    feed: LogFeed,
) {
    let mut backoff = Duration::from_secs(1);
    let mut last_err = String::new();
    loop {
        match provider.subscribe_logs(&filter).await {
            Ok(sub) => {
                backoff = Duration::from_secs(1);
                last_err.clear();
                let mut stream = sub.into_stream();
                while let Some(log) = stream.next().await {
                    crate::util::LOGS_RECEIVED.fetch_add(1, Ordering::Relaxed);
                    let received_at = Instant::now();
                    let seen_pos = crate::util::block_position_now();
                    let block_number = log.block_number.unwrap_or(0);
                    if let Some(magnitude) = crate::listener::apply_log_to_cache(
                        &log, &feed.cache, feed.solidly_sync_hash, feed.large_swap_bps, feed.large_v3_bps,
                    ) {
                        if feed.swap_tx.try_send(SwapEvent {
                            chain_id: feed.chain_id,
                            pool: crate::listener::log_pool(&log),
                            block_number,
                            magnitude,
                            received_at,
                            seen_pos,
                        }).is_err() {
                            crate::util::SWAP_DROPS.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                debug!("[{}] logs subscription ended, reconnecting...", feed.chain_name);
            }
            Err(e) => {
                // Once per distinct error: a failing endpoint would otherwise log every
                // backoff step.
                let msg = e.to_string();
                if msg != last_err {
                    warn!("[{}] logs subscription failed: {} — retrying with backoff", feed.chain_name, msg);
                    last_err = msg;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(crate::config::tuning().ws_reconnect_max_backoff());
            }
        }
    }
}

/// How long the swap arm keeps collecting after the last log arrived, and the most it
/// waits in all. A block's logs arrive back to back (well under a ms apart), so a short
/// quiet gap marks the end of the block without holding the scan for the next head.
const SWAP_BATCH_SETTLE: Duration = Duration::from_millis(2);
const SWAP_BATCH_MAX_WAIT: Duration = Duration::from_millis(20);

/// `first` plus every swap event already queued or arriving until the feed has been quiet
/// for [`SWAP_BATCH_SETTLE`] (at most [`SWAP_BATCH_MAX_WAIT`]): one block's logs, plus any
/// that queued while the previous scan ran.
async fn collect_swap_batch(first: SwapEvent, rx: &mut mpsc::Receiver<SwapEvent>) -> Vec<SwapEvent> {
    let deadline = tokio::time::Instant::now() + SWAP_BATCH_MAX_WAIT;
    let mut batch = vec![first];
    loop {
        while let Ok(ev) = rx.try_recv() {
            batch.push(ev);
        }
        // A zero timeout still returns a ready event, so the cap needs its own check.
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return batch;
        }
        match tokio::time::timeout(SWAP_BATCH_SETTLE.min(left), rx.recv()).await {
            Ok(Some(ev)) => batch.push(ev),
            _ => return batch,
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

/// Arrival time and slot position of the event in `batch` that has waited longest.
fn oldest_event(batch: &[SwapEvent]) -> Option<(Instant, Option<(u64, u64)>)> {
    batch.iter().min_by_key(|ev| ev.received_at).map(|ev| (ev.received_at, ev.seen_pos))
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

/// Merge a batch of swap events into one targeted scan: the union of their affected pair
/// indices and pool tokens, plus the pair ids of large swaps (their cooldowns are cleared).
/// Events on pools not in the cache contribute nothing.
fn merge_swap_batch(
    batch: &[SwapEvent],
    tokens_of: impl Fn(Address) -> Option<(Address, Address)>,
    pairs_for: impl Fn(Address, Address) -> Vec<usize>,
    pair_id: impl Fn(usize) -> String,
) -> (HashSet<usize>, Vec<Address>, Vec<String>) {
    let mut mask: HashSet<usize> = HashSet::new();
    let mut tokens: Vec<Address> = Vec::new();
    let mut large_ids: Vec<String> = Vec::new();
    for ev in batch {
        let Some((a, b)) = tokens_of(ev.pool) else { continue };
        let idx = pairs_for(a, b);
        if idx.is_empty() {
            continue;
        }
        for t in [a, b] {
            if !tokens.contains(&t) { tokens.push(t); }
        }
        if ev.magnitude == SwapMagnitude::Large {
            for &i in &idx {
                let id = pair_id(i);
                if !large_ids.contains(&id) { large_ids.push(id); }
            }
        }
        mask.extend(idx);
    }
    (mask, tokens, large_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oldest_event_is_the_longest_waiting() {
        assert!(oldest_event(&[]).is_none());
        let now = Instant::now();
        let ev = |ago_ms: u64, pos| SwapEvent {
            chain_id: 8453, pool: Address::ZERO, block_number: 1, magnitude: SwapMagnitude::Normal,
            received_at: now - Duration::from_millis(ago_ms), seen_pos: pos,
        };
        let (at, pos) = oldest_event(&[ev(5, Some((2, 100))), ev(40, Some((2, 65))), ev(0, None)]).unwrap();
        assert_eq!(at, now - Duration::from_millis(40));
        assert_eq!(pos, Some((2, 65)));
    }

    #[test]
    fn merge_swap_batch_unions_pairs_and_tracks_large_ids() {
        let (usdc, weth, vvv) = (Address::repeat_byte(1), Address::repeat_byte(2), Address::repeat_byte(3));
        let (p_core, p_vvv, p_unknown) = (Address::repeat_byte(10), Address::repeat_byte(11), Address::repeat_byte(12));
        let ev = |pool, large: bool| SwapEvent {
            chain_id: 8453, pool, block_number: 1,
            magnitude: if large { SwapMagnitude::Large } else { SwapMagnitude::Normal },
            received_at: Instant::now(), seen_pos: None,
        };
        let tokens = |p: Address| match p {
            p if p == p_core => Some((usdc, weth)),
            p if p == p_vvv => Some((weth, vvv)),
            _ => None,
        };
        // pair 0 = USDC/WETH, pair 1 = USDC/VVV (reaches WETH/VVV via triangles)
        let pairs_for = |a: Address, b: Address| -> Vec<usize> {
            let mut v = Vec::new();
            if a == weth || b == weth { v.push(0); }
            if a == vvv || b == vvv { v.push(1); }
            v
        };
        let id = |i: usize| format!("pair{i}");
        let batch = vec![ev(p_core, false), ev(p_vvv, true), ev(p_unknown, true), ev(p_core, false)];
        let (mask, toks, large) = merge_swap_batch(&batch, tokens, pairs_for, id);
        assert_eq!(mask, [0usize, 1].into_iter().collect());
        assert_eq!(toks, vec![usdc, weth, vvv]);
        assert_eq!(large, vec!["pair0".to_string(), "pair1".to_string()]);
        // A batch of only unknown pools scans nothing.
        assert!(merge_swap_batch(&[ev(p_unknown, false)], tokens, pairs_for, id).0.is_empty());
    }

    #[tokio::test]
    async fn collect_swap_batch_takes_one_burst_and_stops_when_quiet() {
        let (tx, mut rx) = mpsc::channel::<SwapEvent>(64);
        let ev = |block| SwapEvent {
            chain_id: 8453, pool: Address::ZERO, block_number: block, magnitude: SwapMagnitude::Normal,
            received_at: Instant::now(), seen_pos: None,
        };
        // Two already queued, one arriving while the batch is collected (inside the settle gap).
        tx.send(ev(7)).await.unwrap();
        tx.send(ev(7)).await.unwrap();
        let late = tx.clone();
        tokio::spawn(async move { let _ = late.send(ev(7)).await; });
        let first = rx.recv().await.unwrap();
        let t0 = Instant::now();
        let batch = collect_swap_batch(first, &mut rx).await;
        assert_eq!(batch.len(), 3);
        assert!(t0.elapsed() < SWAP_BATCH_MAX_WAIT + Duration::from_millis(15), "{:?}", t0.elapsed());
        // A steady stream is cut at the max wait.
        let flood = tx.clone();
        let feeder = tokio::spawn(async move {
            loop {
                if flood.send(ev(8)).await.is_err() { break; }
                tokio::task::yield_now().await;
            }
        });
        let first = rx.recv().await.unwrap();
        let t0 = Instant::now();
        let batch = collect_swap_batch(first, &mut rx).await;
        assert!(batch.len() > 1);
        assert!(t0.elapsed() < SWAP_BATCH_MAX_WAIT + Duration::from_millis(15), "{:?}", t0.elapsed());
        feeder.abort();
    }

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
