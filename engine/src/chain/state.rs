use super::*;
use crate::util::{rpc_timeout, rpc_read_timeout};

pub(crate) async fn update_native_price<P: Provider>(
    strategy: &Arc<RwLock<Strategy>>,
    executor: &Arc<Mutex<Executor>>,
    provider: &Arc<P>,
    routers: &[RouterConfig],
    pairs: &[PairConfig],
    cfg: &ChainConfig,
) {
    use crate::abi::{IUniswapV2Router02, IQuoterV2};
    use crate::pool_cache::volatile_max_age;
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
        p.token_in == cfg.wrapped_native
            && ["USDC", "USDT", "DAI"].contains(&p.token_out_symbol.as_str())
    });
    let stable_pair = match stable_pair { Some(p) => p, None => return };

    let token_out: Address = match stable_pair.token_out.parse() { Ok(a) => a, Err(_) => return };
    // Use 0.01 native token to minimise price impact on thin pools (result scaled ×100).
    // Quoting 1 full ETH on a low-TVL pool shifts the price significantly (~$200 off).
    let probe_amount = U256::from(10_000_000_000_000_000u128); // 0.01 ETH (1e16 wei)
    let probe_scale = 100_u128; // multiply back to get per-1-ETH price
    let decimal_scale = 10_f64.powi(stable_pair.token_out_decimals as i32);

    // ── Try local V2 pool cache first (zero RPC on the 60s tick) ────────────
    {
        let strat = strategy.read().await;
        for router in routers.iter().filter(|r| r.router_type == RouterType::V2) {
            if let Some(out) = strat.pool_cache.get_amount_out_by_key_str(
                &router.id,
                wrapped_native,
                token_out,
                probe_amount,
                Some(volatile_max_age()),
            ) {
                if !out.is_zero() {
                    let price = out.to::<u128>() as f64 * probe_scale as f64 / decimal_scale;
                    debug!("[{}] {} price (V2 cache) router={} → ${:.2}", cfg.name, cfg.native_currency, router.id, price);
                    drop(strat);
                    { let mut strat = strategy.write().await; strat.update_native_price(price); }
                    { let mut exec = executor.lock().await; exec.update_native_price(price); }
                    return;
                }
            }
        }
    }

    // ── Try V3 QuoterV2 (more accurate on chains with thin V2 pools) ──
    // Iterate ALL V3 routers, each using its own quoter (per-router takes priority
    // over chain-level). Keep the HIGHEST price across all routers and fee tiers —
    // the deepest pool gives the most accurate market price.
    {
        let mut best_v3_price = 0.0;
        for router in routers.iter().filter(|r| r.router_type == RouterType::V3) {
            // Effective quoter: per-router first, chain-level fallback
            let quoter_str = router.quoter_address.as_deref()
                .map(str::to_owned)
                .or_else(|| cfg.quoter_v2_address.clone());
            let quoter = match quoter_str.as_deref().and_then(|s| s.parse::<Address>().ok()) {
                Some(q) => q,
                None => continue,
            };
            const DEFAULT_TIERS: &[u32] = &[500, 3000];
            let tiers: &[u32] = if router.fee_tiers.is_empty() { DEFAULT_TIERS } else { &router.fee_tiers };
            for fee in tiers {
                let quoter_v2 = IQuoterV2::new(quoter, provider.as_ref());
                let quote = quoter_v2.quoteExactInputSingle(IQuoterV2::QuoteExactInputSingleParams {
                    tokenIn: wrapped_native,
                    tokenOut: token_out,
                    amountIn: probe_amount,
                    fee: Uint::from(*fee),
                    sqrtPriceLimitX96: Uint::ZERO,
                });
                if let Ok(r) = rpc_timeout(rpc_read_timeout(), quote.call()).await {
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
    for router in routers.iter().filter(|r| r.router_type == RouterType::V2) {
        let router_addr: Address = match router.address.parse() { Ok(a) => a, Err(_) => continue };

        let v2_router = IUniswapV2Router02::new(router_addr, provider.as_ref());
        let amounts_call = v2_router.getAmountsOut(probe_amount, vec![wrapped_native, token_out]);
        if let Ok(amounts) = rpc_timeout(rpc_read_timeout(), amounts_call.call()).await {
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
pub(crate) async fn refresh_contract_balances<P: Provider + Clone>(
    provider: &P,
    contract_addr: Address,
    balances: &Arc<RwLock<HashMap<Address, U256>>>,
) {
    let token_addrs: Vec<Address> = {
        let b = balances.read().await;
        b.keys().copied().collect()
    };
    if token_addrs.is_empty() {
        return;
    }

    // Fetch all balances concurrently, then write-lock once (same pattern as receipt handler).
    let results = futures::future::join_all(token_addrs.iter().map(|&token| {
        let p = provider.clone();
        async move {
            (
                token,
                rpc_timeout(rpc_read_timeout(), IERC20::new(token, &p).balanceOf(contract_addr).call()).await,
            )
        }
    }))
    .await;

    let mut b = balances.write().await;
    for (token, result) in results {
        match result {
            Ok(bal) => {
                b.insert(token, bal);
            }
            Err(e) => {
                warn!("Failed to fetch balance for {:?}: {}", token, e);
            }
        }
    }
}

/// Startup check: log a warning for any pair whose token_in balance in the
/// ArbitrageExecutor contract is below the configured trade_amount.
/// The engine continues running — detection is still valid; only execution
/// will fail (caught by pre-flight simulation) until the contract is funded.
pub(crate) async fn check_contract_funding(
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

// ─── Startup check of the executor contract ───────────────────────────────────

/// `ArbitrageExecutor.RouterType` ordinal for a configured router type (0 = V2, 1 = V3, 4 = Aerodrome).
pub(crate) fn expected_router_type(t: RouterType) -> u8 {
    match t {
        RouterType::V2 => 0,
        RouterType::V3 => 1,
        RouterType::Aerodrome => 4,
    }
}

pub(crate) fn owner_problem(owner: Address, signer: Address) -> Option<String> {
    (owner != signer).then(|| format!(
        "signer {signer} is not the executor owner ({owner}) — every trade would revert OwnableUnauthorizedAccount. Use the owner's PRIVATE_KEY, or transfer ownership to this key (transferOwnership + acceptOwnership)."
    ))
}

pub(crate) fn router_problem(id: &str, addr: Address, t: RouterType, allowed: bool, onchain_type: u8) -> Option<String> {
    let want = expected_router_type(t);
    if !allowed {
        Some(format!("router {id} ({addr}) is not allowlisted on the executor — setRouter({addr}, {want})"))
    } else if onchain_type != want {
        Some(format!("router {id} ({addr}) is allowlisted as type {onchain_type} but configured as type {want} — setRouter({addr}, {want})"))
    } else {
        None
    }
}

/// Startup check of the executor against this engine's key and routers. Exits with code 2
/// when the signer isn't the owner (any mode) or a router isn't allowlisted / typed right (live
/// mode; dry-run only warns). Returns whether the contract is paused (the chain then starts paused).
/// RPC errors return Err → run_chain's caller retries in 5 s.
pub(crate) async fn check_contract_setup<P: Provider>(
    provider: &P,
    contract: Address,
    signer: Address,
    routers: &[RouterConfig],
    chain: &str,
    dry_run: bool,
    log_tx: &LogBroadcaster,
) -> Result<bool> {
    use crate::abi::ArbitrageExecutor;
    let exec = ArbitrageExecutor::new(contract, provider);

    let owner = rpc_timeout(rpc_read_timeout(), exec.owner().call())
        .await
        .map_err(|e| anyhow::anyhow!("[{chain}] executor {contract}: owner() failed: {e}"))?;
    if let Some(msg) = owner_problem(owner, signer) {
        tracing::error!("[{chain}] {msg}");
        std::process::exit(2);
    }
    let paused = rpc_timeout(rpc_read_timeout(), exec.paused().call())
        .await
        .map_err(|e| anyhow::anyhow!("[{chain}] executor {contract}: paused() failed: {e}"))?;

    let mut problems = Vec::new();
    for r in routers {
        let addr: Address = match r.address.parse() {
            Ok(a) => a,
            Err(_) => {
                problems.push(format!("router {} has an invalid address: {}", r.id, r.address));
                continue;
            }
        };
        let allowed = rpc_timeout(rpc_read_timeout(), exec.allowedRouters(addr).call())
            .await
            .map_err(|e| anyhow::anyhow!("[{chain}] executor {contract}: allowedRouters() failed: {e}"))?;
        let onchain_type = rpc_timeout(rpc_read_timeout(), exec.routerType(addr).call())
            .await
            .map_err(|e| anyhow::anyhow!("[{chain}] executor {contract}: routerType() failed: {e}"))?;
        if let Some(p) = router_problem(&r.id, addr, r.router_type, allowed, onchain_type) {
            problems.push(p);
        }
    }
    for p in &problems {
        tracing::error!("[{chain}] {p}");
        broadcast_log(log_tx, "error", &format!("[{chain}] {p}"), None);
    }
    if !problems.is_empty() {
        if !dry_run {
            tracing::error!("[{chain}] refusing to start live trading: fix the router allowlist (setRouter) or run with DRY_RUN=true");
            std::process::exit(2);
        }
        warn!("[{chain}] dry-run continues — trades through these routers would revert on-chain");
    }
    if paused {
        let msg = format!("[{chain}] Executor contract is paused — chain starts paused (no scans, no sends). unpause() it, then POST /engine/resume.");
        warn!("{msg}");
        broadcast_log(log_tx, "warn", &msg, None);
    }
    info!(
        "[{chain}] Executor {contract}: owner = signer, {} routers allowlisted{}",
        routers.len() - problems.len(),
        if paused { ", PAUSED" } else { "" }
    );
    Ok(paused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn expected_router_type_ordinals() {
        assert_eq!(expected_router_type(RouterType::V2), 0);
        assert_eq!(expected_router_type(RouterType::V3), 1);
        assert_eq!(expected_router_type(RouterType::Aerodrome), 4);
    }

    #[test]
    fn owner_problem_cases() {
        let a = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let b = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
        assert!(owner_problem(a, a).is_none());
        let msg = owner_problem(a, b).expect("mismatch");
        assert!(msg.contains(&a.to_string()) && msg.contains(&b.to_string()), "{msg}");
    }

    #[test]
    fn router_problem_cases() {
        let a = address!("2626664c2603336E57B271c5C0b26F421741e481");
        let msg = router_problem("r", a, RouterType::V3, false, 0).expect("not allowed");
        assert!(msg.contains("not allowlisted") && msg.contains("setRouter"), "{msg}");
        assert!(router_problem("r", a, RouterType::V3, true, 1).is_none());
        let msg = router_problem("r", a, RouterType::Aerodrome, true, 0).expect("wrong type");
        assert!(msg.contains("type 0") && msg.contains("type 4"), "{msg}");
    }
}
