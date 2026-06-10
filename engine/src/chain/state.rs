use super::*;

pub(crate) async fn update_native_price<P: Provider>(
    strategy: &Arc<RwLock<Strategy>>,
    executor: &Arc<Mutex<Executor>>,
    provider: &Arc<P>,
    routers: &[RouterConfig],
    pairs: &[PairConfig],
    cfg: &ChainConfig,
) {
    use crate::abi::{IUniswapV2Router02, IQuoterV2};
    use crate::pool_cache::VOLATILE_MAX_AGE;
    use crate::util::addr_key;
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

    // ── Try local V2 pool cache first (zero RPC on the 60s tick) ────────────
    {
        let strat = strategy.read().await;
        let w_key = addr_key(wrapped_native);
        let s_key = addr_key(token_out);
        for router in routers.iter().filter(|r| r.chain_id == cfg.id && r.router_type == RouterType::V2) {
            let key = format!("{}:{}:{}", router.id, w_key, s_key);
            if let Some(out) = strat.pool_cache.get_amount_out_by_key_str(
                &key,
                wrapped_native,
                probe_amount,
                Some(VOLATILE_MAX_AGE),
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
        for router in routers.iter().filter(|r| r.chain_id == cfg.id && r.router_type == RouterType::V3) {
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
                IERC20::new(token, &p).balanceOf(contract_addr).call().await,
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
