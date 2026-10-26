mod abi;
mod api;
mod chain;
mod config;
mod db;
mod executor;
mod listener;
mod metrics;
mod pool_cache;
mod race;
mod rpc_count;
mod revert;
mod strategy;
mod types;
mod util;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // ── Logging ──
    let env = config::load_env().unwrap_or_else(|e| {
        // Not retryable: exit 2 so systemd doesn't restart it (RestartPreventExitStatus=2).
        eprintln!("Config error: {e}");
        std::process::exit(2);
    });

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(&env.log_level)),
        )
        .init();

    info!("ArbitragePulse engine starting up");

    // ── Config ──
    let cfg = config::load_config(&env.config_path).unwrap_or_else(|e| {
        tracing::error!("Config load failed: {e} — exiting (code 2, not restarted)");
        std::process::exit(2);
    });

    // One instance = one chain (see config::AppConfig).
    config::install_tuning(cfg.tuning.clone(), cfg.chain.block_time_ms);
    info!(
        "Loaded chain {} (id {}), {} routers, {} pairs — block time {} ms",
        cfg.chain.name, cfg.chain.id, cfg.routers.len(), cfg.pairs.len(), cfg.chain.block_time_ms
    );
    let t = config::tuning();
    info!(
        "Tuning: simulate timeout {:?}, quote-cache age {:?}, RPC monitor every {:?}, hold {:?}–{:?}, breaker after {} failures",
        t.rpc_simulate_timeout(), t.quote_cache_max_age(), t.health_monitor_every(),
        t.health_hold_base(), t.health_hold_max(), t.max_consecutive_failures,
    );

    // ── Metrics ──
    let metrics = metrics::Metrics::new().unwrap_or_else(|e| {
        tracing::error!("Metrics init failed: {e}");
        std::process::exit(1);
    });

    // ── Database ──
    let db = match db::Database::open(&env.db_path) {
        Ok(d) => {
            info!("SQLite database opened ({})", env.db_path);
            Some(Arc::new(d))
        }
        Err(e) => {
            tracing::warn!("Could not open {}: {} — running without persistence", env.db_path, e);
            None
        }
    };

    // ── API server ──
    if let Err(e) = config::check_api_key(&env.api_key, env.api_insecure) {
        tracing::error!("{e}");
        std::process::exit(2);
    }
    if env.api_key.trim().is_empty() {
        tracing::warn!("API_INSECURE=1 — HTTP API has NO authentication. Local development only.");
    }
    let api_server = api::ApiServer::new(env.port, env.api_key.clone(), db.clone(), metrics.clone(), env.dry_run);
    let log_tx = api_server.log_sender();
    let shared_state = api_server.shared_state();

    // Seed initial chain stats from DB — only for this instance's chain. Other chains in
    // an old shared DB would otherwise appear with rpc_ok=false ("degraded").
    let this_chain_id = cfg.chain.id;
    if let Some(ref db_ref) = db {
        let db_for_seed = Arc::clone(db_ref);
        match tokio::task::spawn_blocking(move || db_for_seed.get_chain_stats()).await {
            Ok(Ok(rows)) => {
                let mut state = shared_state.write().await;
                for seed in rows {
                    if seed.chain_id != this_chain_id {
                        continue; // another chain's rows
                    }
                    let chain_id = seed.chain_id;
                    state.chains.push(api::ChainStats {
                        chain_id,
                        chain_name: seed.chain_name,
                        total_scans: 0,
                        total_opportunities: 0,
                        total_attempts: seed.attempts,
                        total_success: seed.successes,
                        total_failed: seed.reverted,
                        total_dry_run_ok: 0,
                        total_exec_failed: seed.exec_failed,
                        total_profit_usd: seed.profit_usd,
                        ghost_profit_usd: 0.0,
                        base_fee_gwei: 0.0,
                        dry_run: env.dry_run,
                        paused: false,
                        rpc_ok: false,
                        last_block: 0,
                        last_block_at: 0,
                        rpc_latency_ms: 0.0,
                        rpc_latency_max_ms: 0.0,
                    });
                    info!("Seeded chain stats from DB: chain_id={}", chain_id);
                }
            }
            _ => {}
        }
    }

    // ── Seed token registry from config pairs ──
    {
        use std::collections::HashMap;
        let mut state = shared_state.write().await;

        let mut seen: HashMap<(u64, String), ()> = HashMap::new();
        for pair in &cfg.pairs {
            for (symbol, addr, decimals) in [
                (&pair.token_in_symbol, &pair.token_in, pair.token_in_decimals),
                (&pair.token_out_symbol, &pair.token_out, pair.token_out_decimals),
            ] {
                let key = (cfg.chain.id, addr.to_lowercase());
                if seen.insert(key, ()).is_none() {
                    let category = if ["USDC", "USDT", "DAI", "FRAX", "LUSD", "USDC.E", "USDBC"].contains(&symbol.as_str()) {
                        "stable"
                    } else if ["WETH", "ETH", "WBTC", "BTC", "CBETH"].contains(&symbol.as_str()) {
                        "blue_chip"
                    } else {
                        "defi"
                    };
                    state.tokens.push(api::Token {
                        symbol: symbol.clone(),
                        address: addr.clone(),
                        chain_id: cfg.chain.id,
                        decimals,
                        trusted: true,
                        category: category.to_string(),
                    });
                }
            }
        }
        for pair in &cfg.pairs {
            let path = format!("{}→{}", pair.token_in_symbol, pair.token_out_symbol);
            state.pairs.push((pair.id.clone(), path));
        }
    }

    // Spawn API server
    tokio::spawn(api_server.run());

    // ── Shutdown channel ──
    // Broadcast a `()` to signal all chain engines to stop gracefully.
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // ── Chain engine (one per instance) ──
    // Raced against the shutdown signals below: if it ever ends early (panic or return), the
    // process exits 1 so the service manager restarts it.
    let mut chain_handle = {
        let chain_cfg = cfg.chain.clone();
        let routers = cfg.routers.clone();
        let pairs = cfg.pairs.clone();
        let private_key = env.private_key.clone();
        let shared = shared_state.clone();
        let log = log_tx.clone();
        let db_chain = db.clone();
        let met = metrics.clone();
        let config_path = env.config_path.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();
        let shutdown_tx_chain = shutdown_tx.clone();

        tokio::spawn(async move {
            loop {
                // Check if shutdown was already requested before restarting
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
                match chain::run_chain(
                    chain_cfg.clone(),
                    routers.clone(),
                    pairs.clone(),
                    private_key.clone(),
                    shared.clone(),
                    log.clone(),
                    db_chain.clone(),
                    config_path.clone(),
                    met.clone(),
                    shutdown_tx_chain.subscribe(),
                )
                .await
                {
                    Ok(()) => {
                        // Clean exit = shutdown was requested; don't restart
                        break;
                    }
                    Err(e) => {
                        if shutdown_rx.try_recv().is_ok() {
                            break;
                        }
                        tracing::error!("[{}] Chain engine error: {} — restarting in 5s", chain_cfg.name, e);
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        })
    };

    info!("Chain engine started. API at http://0.0.0.0:{}", env.port);

    // ── Graceful shutdown: wait for SIGTERM or Ctrl-C ──
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT (Ctrl-C) — shutting down gracefully...");
        }
        _ = wait_sigterm() => {
            info!("Received SIGTERM — shutting down gracefully...");
        }
        res = &mut chain_handle => {
            let why = match &res {
                Ok(()) => "returned".to_string(),
                Err(e) if e.is_panic() => "panicked".to_string(),
                Err(e) => e.to_string(),
            };
            tracing::error!("Chain engine task {} before shutdown — exiting (code 1) so the service manager restarts the engine", why);
            std::process::exit(1);
        }
    }

    // Signal all chain engines to stop
    let _ = shutdown_tx.send(());

    // Give the engine up to 30 s to drain in-flight work, then force exit
    if tokio::time::timeout(std::time::Duration::from_secs(30), chain_handle).await.is_err() {
        tracing::warn!("Shutdown timeout — forcing exit");
    }

    info!("Shutdown complete.");
    Ok(())
}

/// Listen for SIGTERM (Unix only; returns pending future on non-Unix).
async fn wait_sigterm() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<()>().await;
    }
}
