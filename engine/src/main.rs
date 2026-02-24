mod abi;
mod api;
mod chain;
mod config;
mod db;
mod executor;
mod listener;
mod metrics;
mod router_health;
mod strategy;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // ── Logging ──
    let env = config::load_env().unwrap_or_else(|e| {
        eprintln!("Config error: {e}");
        std::process::exit(1);
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
        tracing::error!("Config load failed: {e}");
        std::process::exit(1);
    });

    let enabled_chains: Vec<_> = cfg.chains.iter().filter(|c| c.enabled).collect();
    if enabled_chains.is_empty() {
        tracing::error!("No enabled chains in config");
        std::process::exit(1);
    }

    info!(
        "Loaded {} chains, {} routers, {} pairs",
        enabled_chains.len(),
        cfg.routers.len(),
        cfg.pairs.len()
    );

    // ── Metrics ──
    let metrics = metrics::Metrics::new().unwrap_or_else(|e| {
        tracing::error!("Metrics init failed: {e}");
        std::process::exit(1);
    });

    // ── Database ──
    let db = match db::Database::open("trades.db") {
        Ok(d) => {
            info!("SQLite database opened (trades.db)");
            Some(Arc::new(d))
        }
        Err(e) => {
            tracing::warn!("Could not open trades.db: {} — running without persistence", e);
            None
        }
    };

    // ── API server ──
    let api_server = api::ApiServer::new(env.port, env.api_key.clone(), db.clone(), metrics.clone(), env.dry_run);
    let log_tx = api_server.log_sender();
    let shared_state = api_server.shared_state();

    // Seed initial chain stats from DB (live trades only)
    if let Some(ref db_ref) = db {
        let db_for_seed = Arc::clone(db_ref);
        match tokio::task::spawn_blocking(move || db_for_seed.get_chain_stats()).await {
            Ok(Ok(rows)) => {
                let mut state = shared_state.write().await;
                for (chain_id, chain_name, attempts, successes, profit) in rows {
                    state.chains.push(api::ChainStats {
                        chain_id,
                        chain_name,
                        total_scans: 0,
                        total_attempts: attempts,
                        total_success: successes,
                        total_profit_usd: profit,
                        dry_run: true,
                        paused: false,
                        rpc_ok: false,
                        last_block: 0,
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
                let key = (pair.chain_id, addr.to_lowercase());
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
                        chain_id: pair.chain_id,
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

    // ── Chain engines ──
    let mut handles = Vec::new();

    for chain_cfg in enabled_chains {
        let chain_cfg = chain_cfg.clone();
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

        let handle = tokio::spawn(async move {
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
        });

        handles.push(handle);
    }

    info!("All chain engines started. API at http://0.0.0.0:{}", env.port);

    // ── Graceful shutdown: wait for SIGTERM or Ctrl-C ──
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT (Ctrl-C) — shutting down gracefully...");
        }
        _ = wait_sigterm() => {
            info!("Received SIGTERM — shutting down gracefully...");
        }
    }

    // Signal all chain engines to stop
    let _ = shutdown_tx.send(());

    // Give engines up to 30 s to drain in-flight work, then force exit
    let drain = async {
        for handle in handles {
            let _ = handle.await;
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(30), drain).await.is_err() {
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
