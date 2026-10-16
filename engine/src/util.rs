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

/// Per-chain RPC health, shared lock-free by the scan loop and the executor (one per
/// chain; held by the Executor).
#[derive(Debug, Default)]
pub struct RpcHealth {
    /// Backoff deadline: milliseconds since process start (+1), 0 = no backoff.
    backoff_until: std::sync::atomic::AtomicU64,
    /// When a rate limit was last reported (same clock), for log throttling.
    rate_limit_logged: std::sync::atomic::AtomicU64,
    /// Backoffs started since execution last succeeded; each doubles the next backoff
    /// (see [`backoff_len`]), so a node that passes the probe but still can't execute
    /// doesn't flap every 10 s.
    strikes: std::sync::atomic::AtomicU32,
}

pub type SharedRpcHealth = std::sync::Arc<RpcHealth>;

/// Minimum interval between rate-limit reports per chain.
pub const RATE_LIMIT_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

/// Longest backoff after repeated outages (10 s doubling: 10, 20, 40, 80).
pub const RPC_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(80);

/// Backoff length after `strikes` outages without an execution succeeding in between.
pub fn backoff_len(strikes: u32) -> std::time::Duration {
    let base = RPC_BACKOFF.as_millis() as u64;
    let ms = base.saturating_mul(1u64 << strikes.saturating_sub(1).min(8));
    std::time::Duration::from_millis(ms.min(RPC_BACKOFF_MAX.as_millis() as u64))
}

impl RpcHealth {
    /// Deadline for a backoff starting now — the one place its length is decided.
    fn deadline(&self) -> u64 {
        let strikes = self.strikes.load(std::sync::atomic::Ordering::Relaxed).max(1);
        monotonic_ms() + backoff_len(strikes).as_millis() as u64
    }

    /// Start a backoff after an outage (counts a strike, so repeats back off longer).
    /// If one is already running (e.g. the executor's gas fetch started it earlier in the
    /// same scan), it's the same outage: no extra strike, deadline left as is.
    pub fn backoff_start(&self) -> u64 {
        if self.backoff_state() == Some(true) {
            return self.backoff_until.load(std::sync::atomic::Ordering::Relaxed);
        }
        self.strikes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.backoff_hold()
    }

    /// (Re)start the backoff without counting a strike — while a probe runs, or when a
    /// probe finds the node still down. Returns the new deadline, which
    /// [`RpcHealth::backoff_resolve`] uses to act only on its own backoff.
    pub fn backoff_hold(&self) -> u64 {
        let until = self.deadline();
        self.backoff_until.store(until, std::sync::atomic::Ordering::Relaxed);
        until
    }

    /// An execution reached the node and succeeded (a pre-flight passed, a dry run or a
    /// send went through): the node is usable, so the next outage starts short again.
    pub fn note_success(&self) {
        self.strikes.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Length of the backoff currently in force (for logs).
    pub fn current_backoff_len(&self) -> std::time::Duration {
        backoff_len(self.strikes.load(std::sync::atomic::Ordering::Relaxed).max(1))
    }

    /// Backoff running or expired-but-not-yet-probed: execution is off either way.
    pub fn execution_off(&self) -> bool {
        self.backoff_state().is_some()
    }

    /// Clear the backoff and its strikes (the chain was paused/resumed: start fresh).
    pub fn backoff_clear(&self) {
        self.backoff_until.store(0, std::sync::atomic::Ordering::Relaxed);
        self.strikes.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// `None` = no backoff; `Some(true)` = running; `Some(false)` = expired (probe due).
    pub fn backoff_state(&self) -> Option<bool> {
        match self.backoff_until.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            until => Some(monotonic_ms() < until),
        }
    }

    /// Resolve the backoff a probe started (deadline `started`): clear it if the node is
    /// healthy, restart it if not. A no-op if anything changed it since (a pause cleared
    /// it, or an execution failure restarted it) — the probe's result is stale then.
    /// Returns whether it applied.
    pub fn backoff_resolve(&self, started: u64, healthy: bool) -> bool {
        let next = if healthy { 0 } else { self.deadline() };
        self.backoff_until
            .compare_exchange(started, next, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed)
            .is_ok()
    }

    /// True at most once per [`RATE_LIMIT_LOG_EVERY`].
    pub fn rate_limit_log_due(&self) -> bool {
        let now = monotonic_ms();
        let last = self.rate_limit_logged.load(std::sync::atomic::Ordering::Relaxed);
        (last == 0 || now.saturating_sub(last) >= RATE_LIMIT_LOG_EVERY.as_millis() as u64)
            && self.rate_limit_logged
                .compare_exchange(last, now, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed)
                .is_ok()
    }
}

/// A backoff probe counts the node as healthy only if its eth_call answers within this —
/// the simulation timeout, since a node slower than that can't complete a simulation (seen
/// on a fork at 1.5 s latency: backoff flapping every ~15 s, nothing executed). The probe
/// is an eth_call (same method as simulations) with one retry for jitter. If simulations
/// still time out after a pass, repeated backoffs double ([`backoff_len`]), so flapping
/// dies down instead of repeating every 10 s.
pub const RPC_PROBE_HEALTHY: std::time::Duration = RPC_SIMULATE_TIMEOUT;

/// Text of the error returned when a call is skipped because the backoff is running.
/// Classified as [`RpcFailure::Unreachable`], so it never counts toward the breaker.
pub const RPC_BACKOFF_MARKER: &str = "rpc backoff active";

/// Unreachable markers meaning the node is definitely down (refusing connections, the WS
/// backend gone): nothing will get through until it comes back.
const HARD_DOWN_MARKERS: [&str; 3] =
    ["connection refused", "backend connection task has stopped", "pubsub service dropped"];

/// Unreachable markers that can be one-off (a reset socket, a resolver blip): the node may
/// well answer the next request.
const TRANSIENT_UNREACHABLE_MARKERS: [&str; 6] = [
    "error sending request", "connection reset", "connection closed", "broken pipe",
    "dns error", "failed to lookup address",
];

/// An Unreachable error that means the node is definitely down — see [`HARD_DOWN_MARKERS`].
pub fn is_hard_down(err: &str) -> bool {
    let e = err.to_lowercase();
    HARD_DOWN_MARKERS.iter().any(|m| e.contains(m))
}

/// Whether the node itself answered (with a JSON-RPC error), as opposed to the request
/// failing locally or at a gateway (502/503, signer, filler). alloy writes this prefix for
/// every JSON-RPC error response.
pub fn node_answered(err: &str) -> bool {
    err.to_lowercase().contains("server returned an error response")
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
    } else if HARD_DOWN_MARKERS.iter().chain(TRANSIENT_UNREACHABLE_MARKERS.iter()).any(|m| e.contains(m))
        // Written by the engine itself when a call is skipped during a backoff.
        || e.contains(RPC_BACKOFF_MARKER)
    {
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
