use super::*;
use crate::util::{rpc_timeout, rpc_read_timeout};

static NO_PRICE_SOURCE_WARNED: AtomicBool = AtomicBool::new(false);

/// Plausible ETH/USD band. A quote outside it comes from a junk/test pool (Base Sepolia) or a
/// broken quoter and is ignored; the previous value (initially $2500) stays in use.
const NATIVE_PRICE_MIN_USD: f64 = 100.0;
const NATIVE_PRICE_MAX_USD: f64 = 100_000.0;
/// Warn once after this many consecutive price refreshes without an accepted price.
const PRICE_STALE_WARN_AFTER: u64 = 10;
static PRICE_MISSES: AtomicU64 = AtomicU64::new(0);
static FIRST_PRICE_LOGGED: AtomicBool = AtomicBool::new(false);

pub(crate) fn plausible_native_price(p: f64) -> bool {
    p.is_finite() && (NATIVE_PRICE_MIN_USD..=NATIVE_PRICE_MAX_USD).contains(&p)
}

fn note_price_accepted(cfg: &ChainConfig, price: f64, source: &str) {
    let missed = PRICE_MISSES.swap(0, Ordering::Relaxed);
    if !FIRST_PRICE_LOGGED.swap(true, Ordering::Relaxed) {
        info!("[{}] {} price ${:.2} ({source}) — used for gas-cost and minProfit math", cfg.name, cfg.native_currency, price);
    } else if missed >= PRICE_STALE_WARN_AFTER {
        info!("[{}] {} price updated again after {missed} missed refreshes: ${:.2} ({source})", cfg.name, cfg.native_currency, price);
    }
}

fn note_price_missed(cfg: &ChainConfig, kept: f64) {
    let n = PRICE_MISSES.fetch_add(1, Ordering::Relaxed) + 1;
    if n == PRICE_STALE_WARN_AFTER {
        warn!("[{}] {} price not updated for {n} refreshes (no quote in ${:.0}–${:.0}) — gas-cost and minProfit math keep using ${:.2}",
              cfg.name, cfg.native_currency, NATIVE_PRICE_MIN_USD, NATIVE_PRICE_MAX_USD, kept);
    }
}

/// A configured pair with WETH (wrapped native) on one side and a USD stablecoin on the other:
/// (stablecoin address, decimals). WETH→stable is quoted on that pool in either direction
/// (pool keys are stored both ways), so a USDC→WETH pair works as well as WETH→USDC.
pub(crate) fn native_price_source(pairs: &[PairConfig], wrapped_native: Address) -> Option<(Address, u8)> {
    const STABLES: [&str; 3] = ["USDC", "USDT", "DAI"];
    let is_stable = |s: &str| STABLES.iter().any(|x| x.eq_ignore_ascii_case(s));
    pairs.iter().find_map(|p| {
        let (tin, tout) = (p.token_in.parse::<Address>().ok()?, p.token_out.parse::<Address>().ok()?);
        if tin == wrapped_native && is_stable(&p.token_out_symbol) { Some((tout, p.token_out_decimals)) }
        else if tout == wrapped_native && is_stable(&p.token_in_symbol) { Some((tin, p.token_in_decimals)) }
        else { None }
    })
}

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

    let Some((token_out, stable_decimals)) = native_price_source(pairs, wrapped_native) else {
        if !NO_PRICE_SOURCE_WARNED.swap(true, Ordering::Relaxed) {
            warn!("[{}] No WETH/stablecoin pair configured — {} price stays at ${:.0}; gas-cost and minProfit math use that fixed value",
                  cfg.name, cfg.native_currency, 2500.0);
        }
        return;
    };
    // Use 0.01 native token to minimise price impact on thin pools (result scaled ×100).
    // Quoting 1 full ETH on a low-TVL pool shifts the price significantly (~$200 off).
    let probe_amount = U256::from(10_000_000_000_000_000u128); // 0.01 ETH (1e16 wei)
    let probe_scale = 100_u128; // multiply back to get per-1-ETH price
    let decimal_scale = 10_f64.powi(stable_decimals as i32);

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
                    if !plausible_native_price(price) { continue; }
                    drop(strat);
                    { let mut strat = strategy.write().await; strat.update_native_price(price); }
                    { let mut exec = executor.lock().await; exec.update_native_price(price); }
                    note_price_accepted(cfg, price, "V2 cache");
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
        // Slipstream quoters take a tick spacing — the Uniswap/Pancake V3 routers suffice here.
        // V4 quotes take a PoolKey (V4Quoter) — also left out.
        for router in routers.iter().filter(|r| r.router_type == RouterType::V3 && !r.slipstream && !r.v4) {
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
                        if plausible_native_price(price) && price > best_v3_price { best_v3_price = price; }
                    }
                }
            }
        }
        if best_v3_price > 0.0 {
            debug!("[{}] {} price (V3 best): ${:.2}", cfg.name, cfg.native_currency, best_v3_price);
            { let mut strat = strategy.write().await; strat.update_native_price(best_v3_price); }
            { let mut exec = executor.lock().await; exec.update_native_price(best_v3_price); }
            note_price_accepted(cfg, best_v3_price, "V3");
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
                    if !plausible_native_price(price) { continue; }
                    { let mut strat = strategy.write().await; strat.update_native_price(price); }
                    { let mut exec = executor.lock().await; exec.update_native_price(price); }
                    note_price_accepted(cfg, price, "V2 getAmountsOut");
                    return;
                }
            }
        }
    }

    // All sources failed or quoted outside the plausible band: the previous price stays.
    let kept = executor.lock().await.native_price_usd;
    note_price_missed(cfg, kept);
}

/// True when the signer's ETH (wei) is below `min_eth` (chain.min_native_balance).
pub(crate) fn signer_gas_low(balance_wei: U256, min_eth: f64) -> bool {
    crate::util::u256_to_f64(balance_wei) / 1e18 < min_eth
}

/// Balance-mapping slot (0..=20) of each token, for dry-run's assumed balance: for each slot,
/// `balanceOf(holder)` is called with a marker written at the holder's key; the slot whose
/// read returns the marker is the one. Tokens with no match (proxy layouts beyond slot 20,
/// rebasing tokens, RPC without state overrides) are left out — those keep being skipped
/// when unfunded. Read-only; at most 21 calls per token, at startup.
pub(crate) async fn discover_balance_slots<P: Provider>(
    provider: &P,
    tokens: &[Address],
    holder: Address,
) -> HashMap<Address, U256> {
    let marker = U256::from(0x0bad_cafe_dead_beef_u64) << 64;
    let mut out = HashMap::new();
    for &token in tokens {
        for slot in 0u64..=20 {
            let slot = U256::from(slot);
            let erc20 = IERC20::new(token, provider);
            let call = erc20
                .balanceOf(holder)
                .state(crate::executor::balance_override(token, slot, holder, marker));
            match rpc_timeout(rpc_read_timeout(), call.call()).await {
                Ok(v) if v == marker => {
                    out.insert(token, slot);
                    break;
                }
                Ok(_) => {}
                // No override support (or RPC down): don't probe the rest of the slots.
                Err(_) => break,
            }
        }
    }
    out
}

/// Warn once per low period when the signer's ETH is below chain.min_native_balance.
pub(crate) async fn check_signer_gas<P: Provider>(
    provider: &P,
    signer: Address,
    min_eth: f64,
    warned: &AtomicBool,
    log_tx: &LogBroadcaster,
    chain: &str,
) {
    let Ok(bal) = rpc_timeout(rpc_read_timeout(), provider.get_balance(signer)).await else { return };
    if signer_gas_low(bal, min_eth) {
        if !warned.swap(true, Ordering::Relaxed) {
            let msg = format!(
                "[{}] Signer {} has {:.5} ETH for gas — below chain.min_native_balance {} — top it up or sends will fail",
                chain, signer, crate::util::u256_to_f64(bal) / 1e18, min_eth
            );
            warn!("{}", msg);
            broadcast_log(log_tx, "warn", &msg, None);
        }
    } else {
        warned.store(false, Ordering::Relaxed);
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

/// Pairs whose token_in balance in the executor is below their trade size (trade_amount,
/// capped by max_trade). Such a pair can detect but never execute. No RPC: reads the cache.
pub(crate) fn underfunded_pairs<'a>(
    pairs: &'a [crate::config::PairConfig],
    balances: &HashMap<Address, U256>,
) -> Vec<(&'a crate::config::PairConfig, U256, U256)> {
    pairs.iter().filter_map(|pair| {
        let token_addr = pair.token_in.parse::<Address>().ok()?;
        let balance = balances.get(&token_addr).copied().unwrap_or(U256::ZERO);
        let required = crate::strategy::parse_amount_capped(
            &pair.trade_amount,
            pair.max_trade.as_deref(),
            pair.token_in_decimals,
        )?;
        (balance < required).then_some((pair, balance, required))
    }).collect()
}

/// Startup check: warn (journal + dashboard feed) for any pair whose token_in balance in
/// the ArbitrageExecutor contract is below the configured trade_amount.
/// The engine continues running — detection is still valid; only execution
/// will fail (caught by pre-flight simulation) until the contract is funded.
/// The heartbeat keeps showing `unfunded=k/n` while any pair stays underfunded.
pub(crate) async fn check_contract_funding(
    chain_name: &str,
    pairs: &[crate::config::PairConfig],
    contract_addr: Address,
    balances: &Arc<RwLock<HashMap<Address, U256>>>,
    log_tx: &LogBroadcaster,
) {
    let b = balances.read().await;
    for (pair, balance, required) in underfunded_pairs(pairs, &b) {
        let msg = format!(
            "[{}] ⚠ Contract underfunded for {} | balance={} trade_amount={} {} | fund {} — this pair cannot trade",
            chain_name,
            pair.id,
            balance,
            required,
            pair.token_in_symbol,
            contract_addr,
        );
        warn!("{}", msg);
        broadcast_log(log_tx, "warn", &msg, None);
    }
}

// ─── Startup check of the executor contract ───────────────────────────────────

/// `ArbitrageExecutor.RouterType` ordinal for a configured router type (0 = V2, 1 = V3, 4 = Aerodrome).
pub(crate) fn expected_router_type(t: RouterType, slipstream: bool) -> u8 {
    match t {
        RouterType::V2 => 0,
        RouterType::V3 if slipstream => 5,
        RouterType::V3 => 1,
        RouterType::Aerodrome => 4,
    }
}

pub(crate) fn owner_problem(owner: Address, signer: Address) -> Option<String> {
    (owner != signer).then(|| format!(
        "signer {signer} is not the executor owner ({owner}) — every trade would revert OwnableUnauthorizedAccount. Use the owner's PRIVATE_KEY, or transfer ownership to this key (transferOwnership + acceptOwnership)."
    ))
}

pub(crate) fn router_problem(id: &str, addr: Address, t: RouterType, slipstream: bool, allowed: bool, onchain_type: u8) -> Option<String> {
    let want = expected_router_type(t, slipstream);
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
    borrow: bool,
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
    // Direct venues are checked per pool after discovery (check_direct_pools).
    for r in routers.iter().filter(|r| !r.direct) {
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
        if let Some(p) = router_problem(&r.id, addr, r.router_type, r.slipstream, allowed, onchain_type) {
            problems.push(p);
        }
    }
    if borrow {
        match rpc_timeout(rpc_read_timeout(), exec.poolManager().call()).await {
            Ok(pm) if !pm.is_zero() => info!("[{chain}] Executor borrows from the V4 PoolManager {pm}"),
            Ok(_) => problems.push("chain.borrow is on but the executor was deployed without a PoolManager — redeploy (contract v4) or set chain.borrow: false".to_string()),
            Err(e) => problems.push(format!("chain.borrow is on but the executor has no poolManager() (not contract v4?): {e}")),
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
        routers.iter().filter(|r| !r.direct).count().saturating_sub(problems.len()),
        if paused { ", PAUSED" } else { "" }
    );
    Ok(paused)
}

/// Direct venues trade on their pools, so every pool the engine may use must be allowlisted
/// on the executor with type 6 (DirectCL) / 7 (DirectPair). Reads allowedRouters + routerType
/// for each (one chunked Multicall3) and logs the missing ones with ready `setRouters`
/// commands. Live mode disables them in the pool cache (never quoted); dry-run keeps them, so
/// detection shows what allowlisting would add (their simulations revert RouterNotAllowed).
/// Returns how many are missing.
pub(crate) async fn check_direct_pools<P: Provider>(
    provider: &P,
    contract: Address,
    routers: &[RouterConfig],
    cache: &PoolCache,
    dry_run: bool,
    chain: &str,
    log_tx: &LogBroadcaster,
) -> usize {
    use alloy::sol_types::SolCall;
    use crate::abi::ArbitrageExecutor;
    let want = direct_pools(routers, cache);
    if want.is_empty() {
        return 0;
    }
    let calls: Vec<(Address, Vec<u8>)> = want.iter()
        .flat_map(|(p, _)| [
            (contract, ArbitrageExecutor::allowedRoutersCall { router: *p }.abi_encode()),
            (contract, ArbitrageExecutor::routerTypeCall { router: *p }.abi_encode()),
        ])
        .collect();
    let (raw, lost) = super::discover::discover_mc(calls, provider).await;
    let word = |r: &Option<Vec<u8>>| r.as_ref().and_then(|b| b.get(..32)).map(alloy::primitives::U256::from_be_slice);
    let mut missing: HashMap<u8, Vec<Address>> = HashMap::new();
    for ((pool, ty), r) in want.iter().zip(raw.chunks(2)) {
        let ok = word(&r[0]).is_some_and(|w| w == alloy::primitives::U256::from(1u8))
            && word(&r[1]).is_some_and(|w| w == alloy::primitives::U256::from(*ty));
        if !ok {
            missing.entry(*ty).or_default().push(*pool);
        }
    }
    let n: usize = missing.values().map(Vec::len).sum();
    if n == 0 {
        info!("[{chain}] Direct venues: all {} pools allowlisted on the executor", want.len());
        return 0;
    }
    let msg = format!(
        "[{chain}] Direct venues: {n} of {} pools not allowlisted on the executor{} — {}",
        want.len(),
        if lost > 0 { format!(" ({lost} reads lost)") } else { String::new() },
        if dry_run { "dry-run keeps them (their simulations revert RouterNotAllowed)" } else { "skipped until allowlisted" },
    );
    warn!("{msg}");
    broadcast_log(log_tx, "warn", &msg, None);
    for (ty, pools) in &missing {
        for chunk in pools.chunks(40) {
            if *ty == 8 {
                // V4 pools are registered by PoolKey, not address.
                let keys: Vec<String> = chunk.iter().filter_map(|p| cache.v4_keys.get(p).map(|k| format!(
                    "({},{},{},{},{})", k.currency0, k.currency1, k.fee, k.tick_spacing, Address::ZERO,
                ))).collect();
                info!("[{chain}]   cast send {contract} 'setV4Pools((address,address,uint24,int24,address)[])' '[{}]'", keys.join(","));
                continue;
            }
            let list: Vec<String> = chunk.iter().map(|p| p.to_string()).collect();
            info!("[{chain}]   cast send {contract} 'setRouters(address[],uint8)' '[{}]' {ty}", list.join(","));
        }
        if !dry_run {
            for p in pools {
                cache.disable_pool(*p);
            }
        }
    }
    n
}

/// (pool, executor router type) for every cached pool of a `direct: true` venue:
/// 6 DirectCL for V3-type venues, 7 DirectPair for V2 / Aerodrome-type ones, 8 for V4.
pub(crate) fn direct_pools(routers: &[RouterConfig], cache: &PoolCache) -> Vec<(Address, u8)> {
    let direct: HashMap<&str, u8> = routers.iter().filter(|r| r.direct)
        .map(|r| (r.id.as_str(), if r.v4 { 8 } else if r.router_type == RouterType::V3 { 6 } else { 7 }))
        .collect();
    if direct.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<(Address, u8)> = cache.v3_by_address.iter()
        .filter_map(|e| Some((*e.key(), *direct.get(e.router_id.as_str())?)))
        .collect();
    out.extend(cache.by_address.iter().filter_map(|e| {
        let venue = e.router_id.split("::").next()?;
        Some((*e.key(), *direct.get(venue)?))
    }));
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn plausible_native_price_band() {
        assert!(plausible_native_price(2500.0));
        assert!(plausible_native_price(100.0) && plausible_native_price(100_000.0));
        assert!(!plausible_native_price(99.99));
        assert!(!plausible_native_price(100_000.01));
        assert!(!plausible_native_price(0.0));
        assert!(!plausible_native_price(1e12), "junk testnet pool");
        assert!(!plausible_native_price(f64::NAN));
        assert!(!plausible_native_price(f64::INFINITY));
    }

    #[test]
    fn expected_router_type_ordinals() {
        assert_eq!(expected_router_type(RouterType::V2, false), 0);
        assert_eq!(expected_router_type(RouterType::V3, false), 1);
        assert_eq!(expected_router_type(RouterType::V3, true), 5);
        assert_eq!(expected_router_type(RouterType::Aerodrome, false), 4);
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
        let msg = router_problem("r", a, RouterType::V3, false, false, 0).expect("not allowed");
        assert!(msg.contains("not allowlisted") && msg.contains("setRouter"), "{msg}");
        assert!(router_problem("r", a, RouterType::V3, false, true, 1).is_none());
        let msg = router_problem("r", a, RouterType::Aerodrome, false, true, 0).expect("wrong type");
        assert!(msg.contains("type 0") && msg.contains("type 4"), "{msg}");
    }

    #[test]
    fn shipped_configs_have_native_price_source() {
        let weth = address!("4200000000000000000000000000000000000006");
        let load = |name: &str| {
            let full = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), name);
            let content = std::fs::read_to_string(&full).unwrap().replace("YOUR_", "0000");
            crate::config::parse_config(&content).unwrap_or_else(|e| panic!("{name}: {e}"))
        };
        assert_eq!(
            native_price_source(&load("config.yaml").pairs, weth),
            Some((address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"), 6))
        );
        assert_eq!(
            native_price_source(&load("config-testnet.yaml").pairs, weth),
            Some((address!("036CbD53842c5426634e7929541eC2318f3dCF7e"), 6))
        );
        assert_eq!(native_price_source(&[], weth), None);
    }

    #[test]
    fn underfunded_pairs_compares_balance_to_capped_trade_size() {
        let cfg = crate::config::parse_config(
            &std::fs::read_to_string(format!("{}/config.yaml", env!("CARGO_MANIFEST_DIR"))).unwrap()
                .replace("YOUR_", "0000"),
        ).unwrap();
        let pairs = &cfg.pairs;
        assert!(!pairs.is_empty());
        let empty = HashMap::new();
        assert_eq!(underfunded_pairs(pairs, &empty).len(), pairs.len(), "nothing funded");
        let rich: HashMap<Address, U256> = pairs.iter()
            .map(|p| (p.token_in.parse().unwrap(), U256::MAX))
            .collect();
        assert!(underfunded_pairs(pairs, &rich).is_empty());
    }

    #[test]
    fn signer_gas_low_threshold() {
        assert!(signer_gas_low(U256::from(4_000_000_000_000_000u64), 0.005));
        assert!(!signer_gas_low(U256::from(6_000_000_000_000_000u64), 0.005));
    }

    #[test]
    fn direct_pools_lists_only_direct_venues_with_their_type() {
        use crate::pool_cache::{PoolInfo, V3PoolState};
        let cache = PoolCache::new();
        let v3 = |rid: &str| V3PoolState {
            token0: Address::repeat_byte(1), token1: Address::repeat_byte(2), fee: 50, swap_fee: 300,
            sqrt_price_x96: U256::from(1u64) << 96, liquidity: 1, tick: 0, router_id: rid.into(),
            last_updated: std::time::Instant::now(), vr0: U256::ZERO, vr1: U256::ZERO, fee_num_v3: U256::ZERO,
        };
        cache.insert_v3(Address::repeat_byte(0xa), v3("aero-cl"));
        cache.insert_v3(Address::repeat_byte(0xb), v3("uni-v3"));
        cache.insert(Address::repeat_byte(0xc), PoolInfo {
            token0: Address::repeat_byte(1), token1: Address::repeat_byte(2), reserve0: U256::from(1u64),
            reserve1: U256::from(1u64), fee_bps: 5, router_id: "aero-v2::stable".into(), is_stable: true,
            decimals0: 6, decimals1: 6, last_sync: std::time::Instant::now(), fee_num: U256::ZERO,
        });
        let r = |y: &str| -> RouterConfig { serde_yaml::from_str(y).unwrap() };
        let routers = vec![
            r("{id: aero-cl, name: a, address: '0x0000000000000000000000000000000000000001', type: v3, direct: true}"),
            r("{id: aero-v2, name: b, address: '0x0000000000000000000000000000000000000002', type: aerodrome, direct: true}"),
            r("{id: uni-v3, name: c, address: '0x0000000000000000000000000000000000000003', type: v3}"),
        ];
        assert_eq!(direct_pools(&routers, &cache), vec![(Address::repeat_byte(0xa), 6), (Address::repeat_byte(0xc), 7)]);
    }
}
