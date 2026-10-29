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
