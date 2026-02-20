use alloy::network::Ethereum;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::network::EthereumWallet;
use alloy::transports::ws::WsConnect;
use anyhow::Result;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
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
use crate::strategy::Strategy;

const COOLDOWN_SECS: u64 = 15;
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

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

    // ── Register initial chain stats ──
    {
        let mut state = shared_state.write().await;
        if !state.chains.iter().any(|c| c.chain_id == cfg.id) {
            state.chains.push(ChainStats {
                chain_id: cfg.id,
                chain_name: cfg.name.clone(),
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
    let listener = Listener::new(cfg.id, cfg.name.clone(), chain_pairs.clone());
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

    // ── Safety / efficiency state ──
    let mut pending_pairs: HashSet<String> = HashSet::new();
    let mut cooldowns: HashMap<String, Instant> = HashMap::new();
    let mut consecutive_failures: u32 = 0;

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

            // ── Periodic price update ──────────────────────────────────────────
            _ = price_tick.tick() => {
                update_native_price(&strategy, &executor, &provider, &chain_routers, &chain_pairs, &cfg).await;
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
                // Update RPC health + block number in shared state
                {
                    let mut state = shared_state.write().await;
                    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                        chain.rpc_ok = true;
                        chain.last_block = block_num;
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
                ).await;
            }

            // ── Swap event → immediate scan ───────────────────────────────────
            Some(event) = swap_rx.recv() => {
                debug!("[{}] Swap event on pool {:?}", cfg.name, event.pool);
                let state = shared_state.read().await;
                let chain_paused = state.chains.iter().any(|c| c.chain_id == cfg.id && c.paused);
                if state.paused || chain_paused { continue; }
                drop(state);

                evaluate_and_execute(
                    &strategy, &executor, &provider, &shared_state, &log_tx,
                    &cfg, &metrics, &mut pending_pairs, &mut cooldowns, &mut consecutive_failures,
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
) {
    let opportunities = {
        let strat = strategy.read().await;
        strat.evaluate(provider.as_ref()).await
    };

    if opportunities.is_empty() {
        return;
    }

    metrics.opportunities.with_label_values(&[&cfg.name]).inc();

    let pair_id = opportunities[0].pair_id.clone();

    // Per-pair cooldown
    if let Some(&cooled_at) = cooldowns.get(&pair_id) {
        if cooled_at.elapsed().as_secs() < COOLDOWN_SECS {
            debug!("[{}] {} in cooldown, skipping", cfg.name, pair_id);
            return;
        }
        cooldowns.remove(&pair_id);
    }

    // Pending tx dedup
    if pending_pairs.contains(&pair_id) {
        debug!("[{}] {} tx already in-flight, skipping", cfg.name, pair_id);
        return;
    }

    // Two-round adaptive size optimizer
    let best = {
        let strat = strategy.read().await;
        strat.optimize(&opportunities[0], provider.as_ref()).await
    };

    broadcast_log(
        log_tx,
        "opportunity",
        &format!(
            "[{}] {} | profit=${:.4} | {}/{}",
            cfg.name, best.pair_id, best.profit_usd, best.router_a_id, best.router_b_id
        ),
        Some(serde_json::json!({
            "chain":      cfg.name,
            "pair_id":    best.pair_id,
            "profit_usd": best.profit_usd,
            "router_a":   best.router_a_id,
            "router_b":   best.router_b_id,
        })),
    );

    pending_pairs.insert(pair_id.clone());

    let dry_run = shared_state.read().await.dry_run;
    let mut exec = executor.lock().await;
    exec.dry_run = dry_run;

    match exec.execute(provider.as_ref(), &best).await {
        Ok(tx_hash) => {
            *consecutive_failures = 0;
            pending_pairs.remove(&pair_id);

            let dry_label = if dry_run { "true" } else { "false" };
            metrics.executed.with_label_values(&[&cfg.name, dry_label]).inc();
            if !dry_run {
                metrics.profit_usd.with_label_values(&[&cfg.name]).add(best.profit_usd);
            }

            let mut state = shared_state.write().await;
            if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
                chain.total_attempts += 1;
                chain.total_success += 1;
                chain.total_profit_usd += best.profit_usd;
            }
            broadcast_log(
                log_tx,
                "trade",
                &format!(
                    "[{}] tx={} | pair={} | profit=${:.4}",
                    cfg.name,
                    &tx_hash[..10.min(tx_hash.len())],
                    best.pair_id,
                    best.profit_usd
                ),
                Some(serde_json::json!({
                    "chain":      cfg.name,
                    "pair_id":    best.pair_id,
                    "profit_usd": best.profit_usd,
                })),
            );
        }
        Err(e) => {
            pending_pairs.remove(&pair_id);
            cooldowns.insert(pair_id.clone(), Instant::now());
            *consecutive_failures += 1;

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

            broadcast_log(log_tx, "error", &format!("[{}] Execute failed: {}", cfg.name, e), None);
        }
    }
}

async fn update_native_price<P: Provider>(
    strategy: &Arc<RwLock<Strategy>>,
    executor: &Arc<Mutex<Executor>>,
    provider: &Arc<P>,
    routers: &[RouterConfig],
    pairs: &[PairConfig],
    cfg: &ChainConfig,
) {
    use crate::abi::IUniswapV2Router02;
    use alloy::primitives::U256;
    use crate::config::RouterType;

    let stable_pair = pairs.iter().find(|p| {
        p.chain_id == cfg.id
            && ["WETH", "ETH"].contains(&p.token_in_symbol.as_str())
            && ["USDC", "USDT", "DAI"].contains(&p.token_out_symbol.as_str())
    });
    let stable_pair = match stable_pair { Some(p) => p, None => return };

    let token_in: Address = match stable_pair.token_in.parse() { Ok(a) => a, Err(_) => return };
    let token_out: Address = match stable_pair.token_out.parse() { Ok(a) => a, Err(_) => return };
    let amount_in = U256::from(1_000_000_000_000_000_000u128);

    for router in routers.iter().filter(|r| r.chain_id == cfg.id && r.router_type == RouterType::V2) {
        let router_addr: Address = match router.address.parse() { Ok(a) => a, Err(_) => continue };

        if let Ok(amounts) = IUniswapV2Router02::new(router_addr, provider.as_ref())
            .getAmountsOut(amount_in, vec![token_in, token_out])
            .call()
            .await
        {
            if let Some(&out) = amounts.last() {
                if !out.is_zero() {
                    let price = out.to::<u128>() as f64 / 1_000_000.0;
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
