use super::*;

// ─── Pool discovery (startup) ─────────────────────────────────────────────────

/// Discover V2 and Solidly-volatile pools for all configured pairs and routers.
/// Populates `pool_cache` with initial reserves so strategy can quote locally.
///
/// Three multicall rounds:
///   1. router.factory()  per V2/Solidly router
///   2. factory.getPair() per (router × pair)
///   3. pair.token0() + pair.getReserves() per discovered pool
///      (+ factory.getFee(pool, stable) per Aerodrome pool — its fee can differ per pool)
///
/// Returns how many discovery calls were lost to RPC failure after retries (0 = complete).
pub(crate) async fn discover_pools<P: Provider>(
    routers: &[RouterConfig],
    pairs: &[PairConfig],
    provider: &P,
    pool_cache: &PoolCache,
) -> usize {
    use alloy::primitives::U256;
    use alloy::sol_types::SolCall;

    // ── Collect V2 and Aerodrome routers ──────────────────────────────────────
    // configured_factory: pre-parsed from config.factory_address — avoids the on-chain
    // factory() call for routers that don't expose it (e.g. Aerodrome uses defaultFactory()).
    struct RouterMeta { id: String, addr: Address, rtype: RouterType, fee_bps: u32, stable_fee_bps: u32, configured_factory: Option<Address> }
    let target_routers: Vec<RouterMeta> = routers
        .iter()
        .filter(|r| r.router_type == RouterType::V2 || r.router_type == RouterType::Aerodrome)
        .filter_map(|r| {
            let addr: Address = r.address.parse().ok()?;
            let stable_fee_bps = r.stable_fee_bps.unwrap_or(r.fee_bps);
            let configured_factory = r.factory_address.as_deref().and_then(|s| s.parse::<Address>().ok());
            Some(RouterMeta { id: r.id.clone(), addr, rtype: r.router_type.clone(), fee_bps: r.fee_bps, stable_fee_bps, configured_factory })
        })
        .collect();

    if target_routers.is_empty() { return 0; }

    // ── Round 1: factory address per router ───────────────────────────────────
    // Routers with factory_address in config skip the on-chain call entirely.
    let factory_calls: Vec<(Address, Vec<u8>)> = target_routers
        .iter()
        .filter(|r| r.configured_factory.is_none())
        .map(|r| (r.addr, IRouterWithFactory::factoryCall {}.abi_encode()))
        .collect();
    let (factory_raw, mut lost) = discover_mc(factory_calls, provider).await;
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
    // Aerodrome routers produce TWO calls per pair: volatile (stable:false) and
    // stable (stable:true). Both pools are discovered and cached independently.
    struct PairMeta { router_id: String, rtype: RouterType, fee_bps: u32, factory: Address, ta: Address, tb: Address, dec_ta: u8, dec_tb: u8, is_stable: bool }
    let mut pair_calls: Vec<(Address, Vec<u8>)> = Vec::new();
    let mut pair_metas: Vec<PairMeta> = Vec::new();

    for (ri, rm) in target_routers.iter().enumerate() {
        let factory = match factories[ri] { Some(f) => f, None => continue };
        for pair in pairs {
            let ta: Address = match pair.token_in.parse() { Ok(a) => a, Err(_) => continue };
            let tb: Address = match pair.token_out.parse() { Ok(a) => a, Err(_) => continue };
            // Decimals from the pair config (what trade sizing uses too) — no decimals() round.
            let (dec_ta, dec_tb) = (pair.token_in_decimals, pair.token_out_decimals);
            match rm.rtype {
                RouterType::V2 => {
                    let cd = IUniswapV2Factory::getPairCall { tokenA: ta, tokenB: tb }.abi_encode();
                    pair_calls.push((factory, cd));
                    pair_metas.push(PairMeta { router_id: rm.id.clone(), rtype: rm.rtype.clone(), fee_bps: rm.fee_bps, factory, ta, tb, dec_ta, dec_tb, is_stable: false });
                }
                RouterType::Aerodrome => {
                    // Aerodrome uses getPool() not getPair() — different 4-byte selector
                    let cd_vol = IAerodromeFactory::getPoolCall { tokenA: ta, tokenB: tb, stable: false }.abi_encode();
                    pair_calls.push((factory, cd_vol));
                    pair_metas.push(PairMeta { router_id: rm.id.clone(), rtype: rm.rtype.clone(), fee_bps: rm.fee_bps, factory, ta, tb, dec_ta, dec_tb, is_stable: false });
                    let cd_sta = IAerodromeFactory::getPoolCall { tokenA: ta, tokenB: tb, stable: true }.abi_encode();
                    pair_calls.push((factory, cd_sta));
                    pair_metas.push(PairMeta { router_id: rm.id.clone(), rtype: rm.rtype.clone(), fee_bps: rm.stable_fee_bps, factory, ta, tb, dec_ta, dec_tb, is_stable: true });
                }
                _ => continue,
            }
        }
    }

    if pair_calls.is_empty() { return lost; }
    let (pair_raw, l) = discover_mc(pair_calls, provider).await;
    lost += l;

    // ── Round 3: token0 + getReserves per pool (decimals come from the pair config) ─
    // The pool address from the stable=true call is a different
    // contract from stable=false, so `seen` naturally allows both.
    // rv_idx: index of this pool's token0 call in rv_calls; getReserves follows, then getFee
    // for Aerodrome pools (so the per-pool fee costs no extra round trip).
    struct PoolMeta { pool: Address, router_id: String, rtype: RouterType, fee_bps: u32, ta: Address, tb: Address, dec_ta: u8, dec_tb: u8, is_stable: bool, rv_idx: usize }
    let mut rv_calls: Vec<(Address, Vec<u8>)> = Vec::new();
    let mut pool_metas: Vec<PoolMeta> = Vec::new();
    let mut seen: std::collections::HashSet<Address> = std::collections::HashSet::new();

    for (pm, raw_opt) in pair_metas.iter().zip(pair_raw.iter()) {
        let pool = match raw_opt.as_ref().and_then(|r| decode_addr(r)) { Some(a) => a, None => continue };
        if !seen.insert(pool) { continue; }
        let rv_idx = rv_calls.len();
        rv_calls.push((pool, IUniswapV2Pair::token0Call {}.abi_encode()));
        rv_calls.push((pool, IUniswapV2Pair::getReservesCall {}.abi_encode()));
        if pm.rtype == RouterType::Aerodrome {
            rv_calls.push((pm.factory, IAerodromeFactory::getFeeCall { pool, _stable: pm.is_stable }.abi_encode()));
        }
        pool_metas.push(PoolMeta {
            pool,
            router_id: pm.router_id.clone(),
            rtype: pm.rtype.clone(),
            fee_bps: pm.fee_bps,
            ta: pm.ta,
            tb: pm.tb,
            dec_ta: pm.dec_ta,
            dec_tb: pm.dec_tb,
            is_stable: pm.is_stable,
            rv_idx,
        });
    }

    if pool_metas.is_empty() { return lost; }
    let (rv_raw, l) = discover_mc(rv_calls, provider).await;
    lost += l;

    // ── Insert into pool_cache ────────────────────────────────────────────────
    for pm in pool_metas.iter() {
        let t0_raw  = match rv_raw.get(pm.rv_idx)     { Some(Some(r)) => r, _ => continue };
        let res_raw = match rv_raw.get(pm.rv_idx + 1) { Some(Some(r)) => r, _ => continue };
        let fee_bps = if pm.rtype == RouterType::Aerodrome {
            aerodrome_fee_bps(rv_raw.get(pm.rv_idx + 2).and_then(|r| r.as_deref()), pm.fee_bps)
        } else {
            pm.fee_bps
        };

        let token0   = match IUniswapV2Pair::token0Call::abi_decode_returns(t0_raw).ok()    { Some(t) => t, None => continue };
        let reserves = match IUniswapV2Pair::getReservesCall::abi_decode_returns(res_raw).ok() { Some(r) => r, None => continue };

        // token1 = whichever of (ta, tb) is not token0
        let token1 = if pm.ta == token0 { pm.tb } else { pm.ta };

        // Map ta/tb decimals to token0/token1 order
        let (decimals0, decimals1) = if pm.ta == token0 { (pm.dec_ta, pm.dec_tb) } else { (pm.dec_tb, pm.dec_ta) };

        // Key suffix: Aerodrome volatile → "::volatile", stable → "::stable", V2 → unchanged
        let effective_id = match pm.rtype {
            RouterType::Aerodrome => if pm.is_stable {
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
            fee_bps,
            router_id: effective_id,
            is_stable: pm.is_stable,
            decimals0,
            decimals1,
            last_sync: Instant::now(),
            fee_num: U256::ZERO, // overwritten by PoolCache::insert()
        });
    }
    lost
}

/// Per-pool Aerodrome fee from a factory.getFee(pool, stable) return, else the configured
/// fallback (call failed / undecodable / not a sane bps value).
fn aerodrome_fee_bps(raw: Option<&[u8]>, fallback_bps: u32) -> u32 {
    use alloy::sol_types::SolCall;
    raw.and_then(|r| IAerodromeFactory::getFeeCall::abi_decode_returns(r).ok())
        .and_then(|f| u32::try_from(f).ok())
        .filter(|&f| f < 10_000)
        .unwrap_or(fallback_bps)
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
///
/// Returns how many discovery calls were lost to RPC failure after retries (0 = complete).
pub(crate) async fn discover_v3_pools<P: Provider>(
    routers: &[RouterConfig],
    pairs: &[PairConfig],
    provider: &P,
    pool_cache: &PoolCache,
) -> usize {
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
        return 0;
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
        return 0;
    }
    let (pool_raw, mut lost) = discover_mc(pool_calls, provider).await;

    // A factory whose every getPool(A, B, uint24 fee) call fails gets no pools, so its
    // router is silently inactive — say so once at startup. A failure is None whether the
    // call reverted or the RPC failed, so tell them apart: the RPC is healthy if another
    // factory answered, or (e.g. a chain with a single V3 router) if it answers a probe.
    let router_failed = |id: &str| pool_queries.iter().zip(pool_raw.iter())
        .filter(|(pq, _)| pq.router_id == id)
        .all(|(_, raw)| raw.is_none());
    let failed: Vec<&V3Router> = v3_routers.iter().filter(|vr| router_failed(&vr.id)).collect();
    let rpc_ok = failed.len() < v3_routers.len()
        || crate::util::rpc_timeout(crate::util::rpc_read_timeout(), provider.get_block_number()).await.is_ok();
    if !failed.is_empty() && !rpc_ok {
        tracing::warn!(
            "V3 pool discovery: no factory answered any getPool call and the RPC is not \
             answering — check RPC health. V3 routers have no pools until the next restart."
        );
    } else {
        for vr in failed {
            tracing::warn!(
                "V3 router {}: every factory getPool(tokenA, tokenB, uint24 fee) call failed while other \
                 factories answered — factory {} is likely not Uniswap-V3 compatible (e.g. Aerodrome \
                 Slipstream uses int24 tickSpacing), so this router finds no pools and is inactive.",
                vr.id, vr.factory,
            );
        }
    }

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
        return lost;
    }
    let (state_raw, l) = discover_mc(state_calls, provider).await;
    lost += l;

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
    lost
}

// ─── Chunked multicall for pool discovery ─────────────────────────────────────

/// Multicall3 helper used at startup by discover_pools / discover_v3_pools.
///
/// Splits `calls` into chunks of `discover_chunk()` items, issuing one Multicall3
/// call per chunk sequentially. This smooths the startup CU burst:
///   • Old: 90 sub-calls in one Multicall3 (or worse, 90 sequential eth_calls on fallback)
///   • New: 5 × 20 = 5 Multicall3 calls, ~5 × 26 CU = 130 CU spread over ~150ms
fn discover_chunk() -> usize { crate::config::tuning().discover_chunk_size }

/// Timeout for one discovery Multicall3 chunk. Longer than the scan-path timeout: startup
/// batches are bigger and a cold node may need to load state first.
fn discover_call_timeout() -> std::time::Duration { crate::config::tuning().discover_call_timeout() }

/// Attempts per multicall chunk when the RPC fails (error or timeout). A sub-call that
/// reverts inside a successful multicall (pool doesn't exist) is an answer, not retried.
const DISCOVER_ATTEMPTS: u32 = 3;

/// Returns one result per call (None = reverted or lost) and how many calls were lost to RPC
/// failure after retries — the caller warns, since those pools are then silently missing.
pub(crate) async fn discover_mc<P: Provider>(
    calls: Vec<(Address, Vec<u8>)>,
    provider: &P,
) -> (Vec<Option<Vec<u8>>>, usize) {
    use alloy::rpc::types::TransactionRequest;
    use alloy::sol_types::SolCall;

    if calls.is_empty() {
        return (vec![], 0);
    }

    let mc3: Address = "0xcA11bde05977b3631167028862bE2a173976CA11"
        .parse()
        .expect("hardcoded Multicall3");

    let mut results: Vec<Option<Vec<u8>>> = Vec::with_capacity(calls.len());
    let mut rpc_failed = 0usize;
    // Once a chunk has exhausted its retries the RPC is likely down: later chunks get one
    // attempt each, so a dead RPC can't stretch startup to chunks × attempts × timeout.
    let mut attempts = DISCOVER_ATTEMPTS;

    for chunk in calls.chunks(discover_chunk()) {
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

        // Some(raw): answered. None after the loop: every attempt hit a transport error or
        // timeout. An error *response* (node rejected the multicall) skips the retries and
        // goes to the sequential fallback below.
        let mut answered: Option<Result<alloy::primitives::Bytes, ()>> = None;
        for attempt in 1..=attempts {
            // Bounded: a hung RPC otherwise blocks startup forever (seen on a cold fork).
            match tokio::time::timeout(discover_call_timeout(), provider.call(tx.clone())).await {
                Ok(Ok(raw)) => { answered = Some(Ok(raw)); break; }
                Ok(Err(alloy::transports::RpcError::ErrorResp(_))) => { answered = Some(Err(())); break; }
                Ok(Err(e)) => tracing::warn!("Pool discovery multicall failed (attempt {attempt}/{attempts}): {e}"),
                Err(_) => tracing::warn!("Pool discovery multicall timed out after {:?} (attempt {attempt}/{attempts})",
                                         discover_call_timeout()),
            }
            if attempt < attempts {
                tokio::time::sleep(std::time::Duration::from_millis(250 * 3u64.pow(attempt - 1))).await;
            }
        }
        let Some(answered) = answered else {
            // RPC stuck/failing: skip the sequential fallback (more calls would only stall
            // longer) and count the chunk as failed.
            tracing::warn!("Pool discovery: {} call(s) lost to RPC failure", chunk.len());
            results.extend(std::iter::repeat_n(None, chunk.len()));
            rpc_failed += chunk.len();
            attempts = 1;
            continue;
        };
        if let Ok(raw) = answered {
            if let Ok(ret) = crate::abi::IMulticall3::aggregate3Call::abi_decode_returns(&raw) {
                results.extend(
                    ret.into_iter()
                        .map(|r| if r.success { Some(r.returnData.to_vec()) } else { None }),
                );
                continue;
            }
        }
        // Fallback: sequential calls for this chunk. An error response is a revert (no pool);
        // a transport error or timeout is a lost call.
        for (target, data) in chunk {
            let tx2 = TransactionRequest::default()
                .to(*target)
                .input(data.clone().into());
            match tokio::time::timeout(crate::util::rpc_read_timeout(), provider.call(tx2)).await {
                Ok(Ok(b)) => results.push(Some(b.to_vec())),
                Ok(Err(alloy::transports::RpcError::ErrorResp(_))) => results.push(None),
                _ => { results.push(None); rpc_failed += 1; }
            }
        }
    }

    (results, rpc_failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;
    use alloy::sol_types::SolValue;

    #[test]
    fn aerodrome_fee_uses_per_pool_getfee_with_config_fallback() {
        let enc = |f: u64| U256::from(f).abi_encode();
        // USDC/AERO stable carries a custom 200 bps fee; the factory default stable fee is 5.
        assert_eq!(aerodrome_fee_bps(Some(&enc(200)), 5), 200);
        assert_eq!(aerodrome_fee_bps(Some(&enc(5)), 1), 5);
        assert_eq!(aerodrome_fee_bps(Some(&enc(0)), 5), 0); // zero-fee pool
        // Failed call, garbage return, or an insane value → configured fallback.
        assert_eq!(aerodrome_fee_bps(None, 5), 5);
        assert_eq!(aerodrome_fee_bps(Some(&[1, 2, 3]), 5), 5);
        assert_eq!(aerodrome_fee_bps(Some(&enc(10_000)), 30), 30);
    }

    #[test]
    fn stable_quote_uses_per_pool_fee() {
        // Same reserves, two stable pools that differ only in fee: the local quote must follow
        // each pool's fee_bps (PoolCache::insert derives fee_num from it).
        let cache = PoolCache::new();
        let usdc: Address = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".parse().unwrap();
        let usdt: Address = "0xfde4C96c8593536E31F229EA8f37b2ADa2699bb2".parse().unwrap();
        let (p5, p200) = (Address::with_last_byte(5), Address::with_last_byte(200));
        for (pool, fee_bps) in [(p5, 5u32), (p200, 200)] {
            cache.insert(pool, PoolInfo {
                token0: usdc, token1: usdt,
                reserve0: U256::from(5_000_000_000_000u64), reserve1: U256::from(5_000_000_000_000u64),
                fee_bps, router_id: format!("aero{fee_bps}::stable"), is_stable: true,
                decimals0: 6, decimals1: 6, last_sync: Instant::now(), fee_num: U256::ZERO,
            });
        }
        let amt = U256::from(100_000_000u64); // 100 USDC
        let r = U256::from(5_000_000_000_000u64);
        let q5 = cache.get_amount_out(p5, usdc, amt).unwrap();
        let q200 = cache.get_amount_out(p200, usdc, amt).unwrap();
        assert_eq!(Some(q5), crate::pool_cache::amount_out_stable(amt, r, r, 6, 6, U256::from(9_995u64)));
        assert_eq!(Some(q200), crate::pool_cache::amount_out_stable(amt, r, r, 6, 6, U256::from(9_800u64)));
        assert!(q200 < q5);
    }
}
