//! Background pool maintenance, off the scan path: the shallow-pool set
//! (`tuning.min_pool_depth_usd`), periodic rediscovery and swap-fee refresh.

use super::*;
use crate::strategy::{NATIVE_SYMBOLS, USD_STABLE_SYMBOLS};
use std::time::Instant;

/// Decimals of every configured token, and which of them have a price from outside the
/// pool cache: USD stables (1.0) and the wrapped native (`native_price_usd`, `true`).
pub(crate) fn token_tables(pairs: &[PairConfig]) -> (HashMap<Address, u8>, Vec<(Address, u8, bool)>) {
    let mut decimals = HashMap::new();
    let mut known: Vec<(Address, u8, bool)> = Vec::new();
    for p in pairs {
        for (addr, sym, dec) in [
            (&p.token_in, &p.token_in_symbol, p.token_in_decimals),
            (&p.token_out, &p.token_out_symbol, p.token_out_decimals),
        ] {
            let Ok(a) = addr.parse::<Address>() else { continue };
            decimals.insert(a, dec);
            let native = NATIVE_SYMBOLS.iter().any(|s| s.eq_ignore_ascii_case(sym));
            let stable = USD_STABLE_SYMBOLS.iter().any(|s| s.eq_ignore_ascii_case(sym));
            if (native || stable) && !known.iter().any(|k| k.0 == a) {
                known.push((a, dec, native));
            }
        }
    }
    (decimals, known)
}

/// Recompute the shallow-pool set now and every `tuning.pool_depth_refresh_ms`. Pure cache
/// math (no RPC); logs when the count changes.
pub(crate) fn spawn_depth_refresh(
    strategy: Arc<RwLock<Strategy>>,
    cache: Arc<PoolCache>,
    pairs: Vec<PairConfig>,
    chain_name: String,
) {
    let (decimals, known_tokens) = token_tables(&pairs);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::config::tuning().pool_depth_refresh());
        let mut last: Option<usize> = None;
        loop {
            tick.tick().await;
            let native = strategy.read().await.native_price_usd;
            let known: HashMap<Address, (f64, u8)> = known_tokens.iter()
                .map(|&(a, d, is_native)| (a, (if is_native { native } else { 1.0 }, d)))
                .collect();
            let min = crate::config::tuning().min_pool_depth_usd;
            let (n, total) = cache.recompute_shallow(&known, &decimals, min);
            if last != Some(n) {
                info!("[{}] {} of {} pools below ${:.0} depth — skipped until they fill", chain_name, n, total, min);
                last = Some(n);
            }
        }
    });
}

/// Re-run pool discovery every `tuning.pool_rediscover_ms` on the read provider (the free
/// http_rpc when configured) into a scratch cache, and add only pools the live cache
/// doesn't have: live pools keep their log-driven state. When pools were added, restart
/// the log feeds with the new address list; the old feeds are aborted only after the new
/// ones had time to connect, so the switch drops no logs.
pub(crate) fn spawn_rediscovery(
    reads: alloy::providers::DynProvider,
    routers: Vec<RouterConfig>,
    pairs: Vec<PairConfig>,
    cache: Arc<PoolCache>,
    start_log_feeds: Arc<dyn Fn() -> Vec<tokio::task::JoinHandle<()>> + Send + Sync>,
    mut feeds: Vec<tokio::task::JoinHandle<()>>,
    chain_name: String,
) {
    let every = crate::config::tuning().pool_rediscover_ms;
    if every == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(every));
        tick.tick().await; // the first tick is immediate: startup discovery just ran
        loop {
            tick.tick().await;
            let scratch = PoolCache::new();
            let lost = discover::discover_pools(&routers, &pairs, &reads, &scratch).await
                + discover::discover_v3_pools(&routers, &pairs, &reads, &scratch).await;
            let added = adopt_new_pools(&cache, &scratch);
            info!("[{}] Pool rediscovery: {} new pool(s){}", chain_name, added,
                  if lost > 0 { format!(", {} call(s) lost — retried next round", lost) } else { String::new() });
            if added == 0 {
                continue;
            }
            let fresh = start_log_feeds();
            tokio::time::sleep(Duration::from_secs(15)).await;
            for h in feeds.drain(..) {
                h.abort();
            }
            feeds = fresh;
        }
    });
}

/// Copy pools present in `scratch` but missing from `cache`. Returns how many were added.
pub(crate) fn adopt_new_pools(cache: &PoolCache, scratch: &PoolCache) -> usize {
    let mut added = 0;
    for e in scratch.by_address.iter() {
        if !cache.by_address.contains_key(e.key()) {
            cache.insert(*e.key(), e.value().clone());
            added += 1;
        }
    }
    for e in scratch.v3_by_address.iter() {
        if !cache.v3_by_address.contains_key(e.key()) {
            cache.insert_v3(*e.key(), e.value().clone());
            added += 1;
        }
    }
    added
}

/// Keep pool swap fees current. Slipstream-type pools (`slipstream: true` routers) have a
/// dynamic fee (observed 150 → 698 ppm within 40 blocks): every `tuning.cl_fee_refresh_ms`,
/// `fee()` is re-read for those that traded since the last read. Aerodrome V2 fees are
/// admin-set (`factory.getFee(pool, stable)`) and re-read every `tuning.v2_fee_refresh_ms`.
/// Uniswap V3 / PancakeSwap V3 / Uniswap V2 fees are fixed per pool and never re-read.
/// One chunked Multicall3 per round on the read provider (free http_rpc), outside the
/// quote gate, so it never competes with detection.
pub(crate) fn spawn_fee_refresh(
    reads: alloy::providers::DynProvider,
    routers: Vec<RouterConfig>,
    cache: Arc<PoolCache>,
    chain_name: String,
) {
    use alloy::sol_types::SolCall;
    let t = crate::config::tuning();
    let (cl_every, v2_every) = (t.cl_fee_refresh_ms, t.v2_fee_refresh_ms);
    let slipstream: HashSet<String> = routers.iter().filter(|r| r.slipstream).map(|r| r.id.clone()).collect();
    let aero_factories: HashMap<String, Address> = routers.iter()
        .filter(|r| r.router_type == RouterType::Aerodrome)
        .filter_map(|r| Some((r.id.clone(), r.factory_address.as_deref()?.parse().ok()?)))
        .collect();
    if cl_every > 0 && !slipstream.is_empty() {
        let (reads, cache, name) = (reads.clone(), cache.clone(), chain_name.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(cl_every));
            tick.tick().await;
            let mut last_read = Instant::now(); // discovery just read every fee
            loop {
                tick.tick().await;
                let pools = traded_since(&cache, &slipstream, last_read);
                last_read = Instant::now();
                if pools.is_empty() { continue; }
                let calls = pools.iter()
                    .map(|p| (*p, crate::abi::IUniswapV3Pool::feeCall {}.abi_encode()))
                    .collect();
                let (raw, _) = discover::discover_mc(calls, &reads).await;
                let changed = pools.iter().zip(raw).filter(|(p, r)| {
                    r.as_deref()
                        .and_then(|r| crate::abi::IUniswapV3Pool::feeCall::abi_decode_returns(r).ok())
                        .is_some_and(|f| cache.set_v3_swap_fee(**p, f.to::<u32>()))
                }).count();
                if changed > 0 {
                    debug!("[{}] Slipstream fee refresh: {} of {} pools changed", name, changed, pools.len());
                }
            }
        });
    }
    if v2_every > 0 && !aero_factories.is_empty() {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(v2_every));
            tick.tick().await;
            loop {
                tick.tick().await;
                // (pool, factory, stable, current fee)
                let pools: Vec<(Address, Address, bool, u32)> = cache.by_address.iter().filter_map(|e| {
                    let router = e.router_id.split("::").next()?;
                    Some((*e.key(), *aero_factories.get(router)?, e.is_stable, e.fee_bps))
                }).collect();
                if pools.is_empty() { continue; }
                let calls = pools.iter()
                    .map(|&(p, f, stable, _)| (f, crate::abi::IAerodromeFactory::getFeeCall { pool: p, _stable: stable }.abi_encode()))
                    .collect();
                let (raw, _) = discover::discover_mc(calls, &reads).await;
                let changed = pools.iter().zip(raw).filter(|((p, _, _, cur), r)| {
                    // An unreadable fee keeps the current one.
                    let fee = discover::aerodrome_fee_bps(r.as_deref(), *cur);
                    cache.set_v2_fee_bps(*p, fee)
                }).count();
                if changed > 0 {
                    info!("[{}] Aerodrome V2 fee refresh: {} of {} pools changed", chain_name, changed, pools.len());
                }
            }
        });
    }
}

/// V3-type pools of `routers` whose state changed after `since` (they traded).
pub(crate) fn traded_since(cache: &PoolCache, routers: &HashSet<String>, since: Instant) -> Vec<Address> {
    cache.v3_by_address.iter()
        .filter(|e| routers.contains(&e.router_id) && e.last_updated > since)
        .map(|e| *e.key())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool_cache::{PoolInfo, V3PoolState};

    fn v2(r0: u64) -> PoolInfo {
        PoolInfo {
            token0: Address::repeat_byte(1), token1: Address::repeat_byte(2),
            reserve0: U256::from(r0), reserve1: U256::from(r0), fee_bps: 30, router_id: "r".into(),
            is_stable: false, decimals0: 18, decimals1: 18, last_sync: Instant::now(), fee_num: U256::ZERO,
        }
    }

    /// Rediscovery adds only pools the live cache lacks; a known pool keeps its live
    /// (log-driven) reserves instead of the rediscovery snapshot, and its address enters
    /// the watch list for the restarted feeds.
    #[test]
    fn adopt_new_pools_adds_only_missing_and_keeps_live_state() {
        let (known, new) = (Address::repeat_byte(0xa), Address::repeat_byte(0xb));
        let live = PoolCache::new();
        live.insert(known, v2(500));
        let scratch = PoolCache::new();
        scratch.insert(known, v2(100));
        scratch.insert(new, v2(7));
        assert_eq!(adopt_new_pools(&live, &scratch), 1);
        assert_eq!(live.by_address.get(&known).unwrap().reserve0, U256::from(500u64));
        assert!(live.watch_addresses().contains(&new));
        assert_eq!(adopt_new_pools(&live, &scratch), 0);
    }

    fn cl(router: &str, updated: Instant) -> V3PoolState {
        V3PoolState {
            token0: Address::repeat_byte(1), token1: Address::repeat_byte(2), fee: 100, swap_fee: 150,
            sqrt_price_x96: U256::from(1u64) << 96, liquidity: 10u128.pow(24), tick: 0,
            router_id: router.into(), last_updated: updated,
            vr0: U256::ZERO, vr1: U256::ZERO, fee_num_v3: U256::ZERO,
        }
    }

    /// Only Slipstream-type pools that traded since the last read are re-read, and a new
    /// fee changes the local quote at once.
    #[test]
    fn fee_refresh_targets_traded_slipstream_pools_and_reprices() {
        let cache = PoolCache::new();
        let since = Instant::now();
        let (old_slip, traded_slip, traded_uni) = (Address::repeat_byte(0xa), Address::repeat_byte(0xb), Address::repeat_byte(0xc));
        cache.insert_v3(old_slip, cl("slip", since - Duration::from_secs(5)));
        cache.insert_v3(traded_slip, cl("slip", since + Duration::from_millis(1)));
        cache.insert_v3(traded_uni, cl("uni", since + Duration::from_millis(1)));
        let slip: HashSet<String> = ["slip".to_string()].into_iter().collect();
        assert_eq!(traded_since(&cache, &slip, since), vec![traded_slip]);

        let key = crate::pool_cache::V3PoolKey::new("slip", Address::repeat_byte(1), Address::repeat_byte(2), 100);
        let amt = U256::from(10u64).pow(U256::from(18u64));
        let before = cache.quote_v3_spot_key(&key, Address::repeat_byte(1), amt).unwrap().0;
        assert!(cache.set_v3_swap_fee(traded_slip, 698));
        assert!(!cache.set_v3_swap_fee(traded_slip, 698));
        let after = cache.quote_v3_spot_key(&key, Address::repeat_byte(1), amt).unwrap().0;
        assert!(after < before, "a higher fee must lower the local quote");
    }

    #[test]
    fn v2_fee_update_reprices() {
        let cache = PoolCache::new();
        let p = Address::repeat_byte(0xd);
        cache.insert(p, v2(1_000_000_000));
        let before = cache.get_amount_out(p, Address::repeat_byte(1), U256::from(1000u64)).unwrap();
        assert!(cache.set_v2_fee_bps(p, 100));
        assert!(!cache.set_v2_fee_bps(p, 100));
        assert!(cache.get_amount_out(p, Address::repeat_byte(1), U256::from(1000u64)).unwrap() < before);
    }
}
