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
        Err(_) => Err(format!("{} after {:?}", RPC_TIMEOUT_MARKER, limit)),
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
            "RPC request {} after {:?} ({})", RPC_TIMEOUT_MARKER, limit, RPC_OUTCOME_UNKNOWN_MARKER
        ))),
    }
}

/// After an RPC outage-class failure, scans skip execution this long before probing.
pub const RPC_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

/// Timeout for the cheap eth_blockNumber probe that ends an RPC backoff.
pub const RPC_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Chain-level RPC backoff deadline, shared lock-free between the scan loop and the
/// executor: milliseconds since process start (+1), 0 = no backoff.
pub type RpcBackoff = std::sync::Arc<std::sync::atomic::AtomicU64>;

fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

/// Start (or extend) the backoff for [`RPC_BACKOFF`].
pub fn backoff_start(b: &RpcBackoff) {
    b.store(monotonic_ms() + RPC_BACKOFF.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
}

/// Clear the backoff (RPC answered again, or the chain was paused/resumed).
pub fn backoff_clear(b: &RpcBackoff) {
    b.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// `None` = no backoff; `Some(true)` = backoff running; `Some(false)` = expired (probe due).
pub fn backoff_state(b: &RpcBackoff) -> Option<bool> {
    match b.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        until => Some(monotonic_ms() < until),
    }
}

/// Text every timeout error from [`rpc_timeout`] / [`transport_timeout`] contains.
/// [`classify_rpc_failure`] matches on it, so producer and classifier share one string.
pub const RPC_TIMEOUT_MARKER: &str = "timed out";

/// Marks a send whose outcome is unknown (timed out — may still have been broadcast).
pub const RPC_OUTCOME_UNKNOWN_MARKER: &str = "outcome unknown";

/// What an execution error says about the RPC node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcFailure {
    /// Not an RPC-health problem (revert, gas reject, nonce, …).
    None,
    /// A request hit our timeout: the node is hanging.
    Timeout,
    /// The node can't be reached at all (connection refused/reset, DNS, socket closed).
    Unreachable,
    /// The provider is rate-limiting us (429 / compute units).
    RateLimited,
}

/// Classify an execution error by its text. Errors arrive flattened into `anyhow`
/// strings (`"Send failed: {e}"`), so the type is gone by the time they're handled;
/// timeouts are matched on [`RPC_TIMEOUT_MARKER`], which this module itself writes.
pub fn classify_rpc_failure(err: &str) -> RpcFailure {
    let e = err.to_lowercase();
    if e.contains(RPC_TIMEOUT_MARKER) {
        RpcFailure::Timeout
    } else if [
        "error sending request",
        "connection refused",
        "connection reset",
        "connection closed",
        "broken pipe",
        "dns error",
        "failed to lookup address",
        "backend connection task has stopped",
        "pubsub service dropped",
    ].iter().any(|m| e.contains(m)) {
        RpcFailure::Unreachable
    } else if contains_status_429(&e) || e.contains("too many requests")
        || e.contains("compute units") || e.contains("rate limit")
    {
        RpcFailure::RateLimited
    } else {
        RpcFailure::None
    }
}

/// "429" as a standalone number (HTTP status), not as digits inside hex revert data.
fn contains_status_429(e: &str) -> bool {
    let b = e.as_bytes();
    e.match_indices("429").any(|(i, _)| {
        let before = i.checked_sub(1).map(|j| b[j]);
        let after = b.get(i + 3).copied();
        !before.is_some_and(|c| c.is_ascii_alphanumeric()) && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// Prefixes the engine itself writes for simulation / pre-flight failures. Matched only at
/// the start of the error, so node-supplied text deeper in the message can't trigger it.
pub const SIM_FAILURE_PREFIXES: [&str; 3] =
    ["simulation failed", "triangular simulation failed", "pre-flight eth_call rejected"];

pub fn is_sim_failure(err: &str) -> bool {
    let e = err.trim_start().to_lowercase();
    SIM_FAILURE_PREFIXES.iter().any(|p| e.starts_with(p))
}
