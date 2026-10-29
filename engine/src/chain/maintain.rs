//! Background pool maintenance, off the scan path: the shallow-pool set
//! (`tuning.min_pool_depth_usd`), periodic rediscovery and swap-fee refresh.

use super::*;
use crate::strategy::{NATIVE_SYMBOLS, USD_STABLE_SYMBOLS};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool_cache::PoolInfo;
    use std::time::Instant;

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
}
