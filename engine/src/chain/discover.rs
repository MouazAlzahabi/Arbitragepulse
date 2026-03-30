use super::*;

// ─── Pool discovery (startup) ─────────────────────────────────────────────────

/// Discover V2 and Solidly-volatile pools for all configured pairs and routers.
/// Populates `pool_cache` with initial reserves so strategy can quote locally.
///
/// Three multicall rounds:
///   1. router.factory()  per V2/Solidly router
///   2. factory.getPair() per (router × pair)
///   3. pair.token0() + pair.getReserves() per discovered pool
pub(crate) async fn discover_pools<P: Provider>(
    routers: &[RouterConfig],
    pairs: &[PairConfig],
    provider: &P,
    pool_cache: &PoolCache,
) {
    use alloy::primitives::U256;
    use alloy::sol_types::SolCall;

    // ── Collect V2, Solidly, and Aerodrome routers ────────────────────────────
    // configured_factory: pre-parsed from config.factory_address — avoids the on-chain
    // factory() call for routers that don't expose it (e.g. Aerodrome uses defaultFactory()).
    struct RouterMeta { id: String, addr: Address, rtype: RouterType, fee_bps: u32, stable_fee_bps: u32, configured_factory: Option<Address> }
    let target_routers: Vec<RouterMeta> = routers
        .iter()
        .filter(|r| r.router_type == RouterType::V2 || r.router_type == RouterType::Solidly || r.router_type == RouterType::Aerodrome)
        .filter_map(|r| {
            let addr: Address = r.address.parse().ok()?;
            let stable_fee_bps = r.stable_fee_bps.unwrap_or(r.fee_bps);
            let configured_factory = r.factory_address.as_deref().and_then(|s| s.parse::<Address>().ok());
            Some(RouterMeta { id: r.id.clone(), addr, rtype: r.router_type.clone(), fee_bps: r.fee_bps, stable_fee_bps, configured_factory })
        })
        .collect();

    if target_routers.is_empty() { return; }

    // ── Round 1: factory address per router ───────────────────────────────────
    // Routers with factory_address in config skip the on-chain call entirely.
    let factory_calls: Vec<(Address, Vec<u8>)> = target_routers
        .iter()
        .filter(|r| r.configured_factory.is_none())
        .map(|r| (r.addr, IRouterWithFactory::factoryCall {}.abi_encode()))
        .collect();
    let factory_raw = discover_mc(factory_calls, provider).await;
    let mut factory_raw_iter = factory_raw.into_iter();
    let factories: Vec<Option<Address>> = target_routers
        .iter()
        .map(|r| {
            if let Some(f) = r.configured_factory {
                Some(f)
            } else {
                factory_raw_iter.next()
                    .flatten()
                    .and_then(|raw| IRouterWithFactory::factoryCall::abi_decode_returns(&raw).ok())
                    .filter(|a: &Address| !a.is_zero())
            }
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
                    // Stable pool (x³y+xy³=k) — uses stable_fee_bps if configured
                    let cd_sta = ISolidlyFactory::getPairCall { tokenA: ta, tokenB: tb, stable: true }.abi_encode();
                    pair_calls.push((factory, cd_sta));
                    pair_metas.push(PairMeta { router_id: rm.id.clone(), rtype: rm.rtype.clone(), fee_bps: rm.stable_fee_bps, ta, tb, is_stable: true });
                }
                RouterType::Aerodrome => {
                    // Aerodrome uses getPool() not getPair() — different 4-byte selector
                    let cd_vol = IAerodromeFactory::getPoolCall { tokenA: ta, tokenB: tb, stable: false }.abi_encode();
                    pair_calls.push((factory, cd_vol));
                    pair_metas.push(PairMeta { router_id: rm.id.clone(), rtype: rm.rtype.clone(), fee_bps: rm.fee_bps, ta, tb, is_stable: false });
                    let cd_sta = IAerodromeFactory::getPoolCall { tokenA: ta, tokenB: tb, stable: true }.abi_encode();
                    pair_calls.push((factory, cd_sta));
                    pair_metas.push(PairMeta { router_id: rm.id.clone(), rtype: rm.rtype.clone(), fee_bps: rm.stable_fee_bps, ta, tb, is_stable: true });
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
            RouterType::Solidly | RouterType::Aerodrome => if pm.is_stable {
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
            fee_num: U256::ZERO, // overwritten by PoolCache::insert()
        });
    }
}

fn decode_addr(raw: &[u8]) -> Option<Address> {
    if raw.len() >= 32 {
        let bytes: [u8; 20] = raw[12..32].try_into().ok()?;
        let a = Address::from(bytes);
        if !a.is_zero() { Some(a) } else { None }
    } else { None }
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
pub(crate) async fn discover_v3_pools<P: Provider>(
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
            // Derived fields: insert_v3 calls refresh_vr() to populate these.
            vr0: U256::ZERO,
            vr1: U256::ZERO,
            fee_num_v3: U256::ZERO,
        });
    }
}

// ─── Chunked multicall for pool discovery ─────────────────────────────────────

/// Multicall3 helper used at startup by discover_pools / discover_v3_pools.
///
/// Splits `calls` into chunks of `DISCOVER_CHUNK` items, issuing one Multicall3
/// call per chunk sequentially. This smooths the startup CU burst:
///   • Old: 90 sub-calls in one Multicall3 (or worse, 90 sequential eth_calls on fallback)
///   • New: 5 × 20 = 5 Multicall3 calls, ~5 × 26 CU = 130 CU spread over ~150ms
const DISCOVER_CHUNK: usize = 20;

pub(crate) async fn discover_mc<P: Provider>(
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
