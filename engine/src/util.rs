use alloy::primitives::{Address, U256}; // U256 used by u256_to_f64

/// Convert a U256 to f64 (lossy for very large values).
/// Fast path for values ≤ u128::MAX (all configured trade amounts) — no heap alloc.
pub fn u256_to_f64(value: U256) -> f64 {
    if value.is_zero() {
        return 0.0;
    }
    let limbs = value.into_limbs();
    if limbs[2] == 0 && limbs[3] == 0 {
        let v = limbs[0] as u128 | ((limbs[1] as u128) << 64);
        return v as f64;
    }
    for i in (0..4).rev() {
        if limbs[i] != 0 {
            return (limbs[i] as f64) * 2f64.powi(64 * i as i32);
        }
    }
    0.0
}

/// Lowercase hex string for an address — used as pool cache key components.
pub fn addr_key(addr: Address) -> String {
    format!("{:?}", addr).to_lowercase()
}


// ─── RPC timeouts ─────────────────────────────────────────────────────────────
// Every RPC call the chain loop awaits must be bounded: one hung request (stuck HTTP
// connection, wedged node) otherwise stops all scanning for that chain until it returns.

/// Background reads: native price, contract balances, RPC ping, nonce, gas price.
/// On timeout the caller keeps its previous value.
pub const RPC_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Pre-flight simulation (eth_call of the signed tx) on the path to every trade.
/// A simulation slower than this is stale anyway, so that candidate is skipped.
pub const RPC_SIMULATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// eth_sendRawTransaction. On timeout the tx may still have been broadcast, so the
/// nonce is treated as consumed and re-fetched.
pub const RPC_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Await an RPC future with a deadline. `Err` carries either the RPC error or a
/// "timed out" message, so callers that only log or count failures can use it as-is.
pub async fn rpc_timeout<T, E: std::fmt::Display>(
    limit: std::time::Duration,
    fut: impl std::future::IntoFuture<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err(format!("timed out after {:?}", limit)),
    }
}

/// Like [`rpc_timeout`] but keeps alloy's `TransportError` type: a timeout becomes a
/// custom transport error whose message says "outcome unknown", so callers' existing
/// handling (revert decoding, nonce bookkeeping) applies unchanged. Send paths match on
/// "outcome unknown" to treat the nonce as possibly consumed.
pub async fn transport_timeout<T>(
    limit: std::time::Duration,
    fut: impl std::future::IntoFuture<Output = alloy::transports::TransportResult<T>>,
) -> alloy::transports::TransportResult<T> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => Err(alloy::transports::TransportErrorKind::custom_str(&format!(
            "RPC request timed out after {:?} (outcome unknown)", limit
        ))),
    }
}
