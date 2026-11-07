use alloy::primitives::{Address, U256}; // U256 used by u256_to_f64
use std::sync::atomic::{AtomicU64, Ordering};

/// OP Stack WETH predeploy (Base, Base Sepolia) — the executor's `WETH`: a V4 native ETH
/// pool is cached under it and the executor wraps / unwraps on the leg.
pub const WETH: Address = alloy::primitives::address!("4200000000000000000000000000000000000006");

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


// ─── Chain head and block position ──────────────────────────────────────────
// Base builds a canonical block every 200 ms (Denim upgrade, 2026-11-05; Flashblocks and
// the `pending` preview were retired with it). Block timestamps are whole seconds, so
// ~5 blocks share one: positions are counted from when the head arrived, not from its
// timestamp.

/// Latest head: its number and the wall-clock ms it arrived at (0 = none yet).
static HEAD_NUM: AtomicU64 = AtomicU64::new(0);
static HEAD_SEEN_MS: AtomicU64 = AtomicU64::new(0);
/// Block time in ms (Base: 200).
static SLOT_MS: AtomicU64 = AtomicU64::new(200);

fn wall_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn set_slot_ms(block_time_ms: u64) {
    SLOT_MS.store(block_time_ms.max(1), Ordering::Relaxed);
}

/// Record a new head. Returns the server clock's lead over the chain in seconds (wall
/// clock minus head timestamp; negative = the server clock is behind the chain).
pub fn note_head(number: u64, timestamp: u64) -> i64 {
    let now = wall_ms();
    HEAD_NUM.store(number, Ordering::Relaxed);
    HEAD_SEEN_MS.store(now, Ordering::Relaxed);
    (now / 1000) as i64 - timestamp as i64
}

/// The block being built at wall-clock `now_ms` and how many ms into its slot that is:
/// the head's successor while the head is younger than a slot, then one block per slot.
/// None before the first head.
fn block_position_at(now_ms: u64, head_num: u64, head_seen_ms: u64, slot_ms: u64) -> Option<(u64, u64)> {
    if head_seen_ms == 0 {
        return None;
    }
    let since_head = now_ms.saturating_sub(head_seen_ms);
    Some((head_num + 1 + since_head / slot_ms, since_head % slot_ms))
}

/// [`block_position_at`] now: `(block being built, ms into its slot)`, for the swap-event,
/// send and receipt logs.
pub fn block_position_now() -> Option<(u64, u64)> {
    block_position_at(
        wall_ms(),
        HEAD_NUM.load(Ordering::Relaxed),
        HEAD_SEEN_MS.load(Ordering::Relaxed),
        SLOT_MS.load(Ordering::Relaxed),
    )
}

// ── Latency windows for the heartbeat's `lat` field ──────────────────────────────
// Logging only: each window collects ms samples between two heartbeats, which then
// takes p50/p95/max and clears it.

/// Samples kept per window and beat; later ones overwrite round-robin (the count stays exact).
const LATENCY_WINDOW_CAP: usize = 4096;

/// ms samples between two heartbeats (see [`LatencyWindow::take_summary`]).
pub struct LatencyWindow {
    inner: std::sync::Mutex<(Vec<u64>, usize)>,
}

impl LatencyWindow {
    pub const fn new() -> Self {
        Self { inner: std::sync::Mutex::new((Vec::new(), 0)) }
    }

    pub fn record(&self, ms: u64) {
        let Ok(mut g) = self.inner.lock() else { return };
        let (samples, n) = &mut *g;
        if samples.len() < LATENCY_WINDOW_CAP {
            samples.push(ms);
        } else {
            samples[*n % LATENCY_WINDOW_CAP] = ms;
        }
        *n += 1;
    }

    /// `p50/p95/max(n)` in ms since the last call, or `-` without samples; clears the window.
    pub fn take_summary(&self) -> String {
        let Ok(mut g) = self.inner.lock() else { return "-".into() };
        let (mut samples, n) = std::mem::take(&mut *g);
        match percentiles(&mut samples) {
            Some((p50, p95, max)) => format!("{}/{}/{}({})", p50, p95, max, n),
            None => "-".into(),
        }
    }
}

/// (p50, p95, max) by nearest rank; None when empty. Sorts `samples`.
fn percentiles(samples: &mut [u64]) -> Option<(u64, u64, u64)> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let rank = |p: usize| samples[(samples.len() * p).div_ceil(100).saturating_sub(1)];
    Some((rank(50), rank(95), samples[samples.len() - 1]))
}

/// Pool logs received on the sealed-block feed (heartbeat `logs=`).
pub static LOGS_RECEIVED: AtomicU64 = AtomicU64::new(0);
/// Swap scans that answered events from more than one block, because the previous scan
/// was still running when the next block's logs came in (heartbeat `merged=`).
pub static SCANS_MERGED: AtomicU64 = AtomicU64::new(0);
/// How long a swap event waited in the scan queue while the chain loop was busy.
pub static LAT_QUEUE: LatencyWindow = LatencyWindow::new();
/// Scan duration (the chain loop is busy for it): full (periodic) and targeted (swap).
pub static LAT_SCAN_FULL: LatencyWindow = LatencyWindow::new();
pub static LAT_SCAN_TARGETED: LatencyWindow = LatencyWindow::new();
/// Swap events dropped because the scan queue was full.
pub static SWAP_DROPS: AtomicU64 = AtomicU64::new(0);
/// Last endpoint round trips (`host=Xms …`) from the background RTT sampler.
pub static ENDPOINT_RTT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The heartbeat's `lat` field; clears the windows.
pub fn take_latency_summary() -> String {
    let rtt = ENDPOINT_RTT.lock().map(|s| s.clone()).unwrap_or_default();
    format!(
        "logs={} queue={} merged={} scan_full={} scan_targeted={} drops={} rtt=[{}]",
        LOGS_RECEIVED.swap(0, Ordering::Relaxed),
        LAT_QUEUE.take_summary(),
        SCANS_MERGED.swap(0, Ordering::Relaxed),
        LAT_SCAN_FULL.take_summary(),
        LAT_SCAN_TARGETED.take_summary(),
        SWAP_DROPS.swap(0, Ordering::Relaxed),
        if rtt.is_empty() { "-" } else { rtt.as_str() },
    )
}

// ─── Repeated warnings ───────────────────────────────────────────────────────
// A broken pool or a refusing endpoint repeats the same warning every scan (2.35M lines
// in 5 h on 2026-11-05): log each key at most once per window, with how many were held.

static THROTTLED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, u64)>>> =
    std::sync::LazyLock::new(Default::default);

/// Window for [`throttle`]d warnings.
pub const WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

/// `Some(held)` when the warning keyed `key` should be logged now (`held` = repeats
/// suppressed since it was last logged), `None` while it is inside `every`.
pub fn throttle(key: &str, every: std::time::Duration) -> Option<u64> {
    throttle_at(&THROTTLED, key, every, std::time::Instant::now())
}

fn throttle_at(
    map: &std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, u64)>>,
    key: &str,
    every: std::time::Duration,
    now: std::time::Instant,
) -> Option<u64> {
    let Ok(mut m) = map.lock() else { return Some(0) };
    match m.get_mut(key) {
        Some((last, held)) if now.duration_since(*last) < every => {
            *held += 1;
            None
        }
        Some((last, held)) => {
            *last = now;
            Some(std::mem::take(held))
        }
        None => {
            m.insert(key.to_string(), (now, 0));
            Some(0)
        }
    }
}

/// " (N more since the last report)" for a throttled warning, empty when nothing was held.
pub fn held_suffix(held: u64) -> String {
    if held == 0 { String::new() } else { format!(" ({} more since the last report)", held) }
}

// ─── RPC timeouts ─────────────────────────────────────────────────────────────
// Every RPC call the chain loop awaits must be bounded: one hung request (stuck HTTP
// connection, wedged node) otherwise stops all scanning for that chain until it returns.

/// Background reads: native price, contract balances, RPC ping, nonce, gas price.
/// On timeout the caller keeps its previous value.
pub fn rpc_read_timeout() -> std::time::Duration { crate::config::tuning().rpc_read_timeout() }

/// Pre-flight simulation (eth_call of the signed tx) on the path to every trade.
/// A simulation slower than this is stale anyway, so that candidate is skipped.
pub fn rpc_simulate_timeout() -> std::time::Duration { crate::config::tuning().rpc_simulate_timeout() }

/// eth_sendRawTransaction. On timeout the tx may still have been broadcast, so the
/// nonce is treated as consumed and re-fetched.
pub fn rpc_send_timeout() -> std::time::Duration { crate::config::tuning().rpc_send_timeout() }

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

// ─── RPC health ───────────────────────────────────────────────────────────────
// One rule set, one writer per transition:
//   • a scan or the executor reports an outage → degraded (execution off, detection on)
//   • a per-chain monitor task probes while degraded and clears it once the node is fast
//     again and the hold has passed
//   • pausing the chain clears it; while paused it is never reported
// No state depends on scans happening, so a quiet market can't leave it stuck.

/// Hold after an outage before the monitor may clear it.
pub fn rpc_hold_base() -> std::time::Duration { crate::config::tuning().health_hold_base() }
/// Longest hold (repeated relapses double it: 10, 20, 40, 80 s).
pub fn rpc_hold_max() -> std::time::Duration { crate::config::tuning().health_hold_max() }
/// Degrading again within this long after recovering counts as a relapse (the probe
/// passed but execution still failed), so the hold doubles instead of resetting.
pub fn rpc_relapse_window() -> std::time::Duration { crate::config::tuning().health_relapse_window() }
/// How often the monitor probes a degraded chain.
pub fn rpc_monitor_every() -> std::time::Duration { crate::config::tuning().health_monitor_every() }

/// The node counts as healthy when a plain eth_call (the method simulations use)
/// succeeds within this — the simulation timeout, since a node slower than that can't
/// complete a simulation (seen on a fork at 1.5 s latency: execution flapped every
/// ~15 s and nothing ran). Any error, including a gateway's JSON-RPC "upstream timeout",
/// is unhealthy; a rate limit is healthy (the node is up — pairs cool down separately).
pub fn rpc_probe_healthy() -> std::time::Duration { rpc_simulate_timeout() }

/// Hold for an outage starting now, given the previous hold and how long ago the chain
/// last recovered (`None` = never, or not since the last pause).
pub fn next_hold(prev: std::time::Duration, since_recovery: Option<std::time::Duration>) -> std::time::Duration {
    match since_recovery {
        Some(t) if t < rpc_relapse_window() => (prev * 2).clamp(rpc_hold_base(), rpc_hold_max()),
        _ => rpc_hold_base(),
    }
}

#[derive(Debug, Default)]
struct HealthState {
    /// Some = degraded (execution off) since then, for `hold`.
    degraded_since: Option<std::time::Instant>,
    hold: std::time::Duration,
    last_recovered: Option<std::time::Instant>,
}

/// Per-chain RPC health, shared by the scan loop, the executor, the monitor task and the
/// API (one per chain run; held by the Executor). A plain mutex: a few uncontended
/// accesses per scan, never held across an await.
#[derive(Debug, Default)]
pub struct RpcHealth {
    state: std::sync::Mutex<HealthState>,
    /// Last rate-limit report, for log throttling.
    rate_limit_logged: std::sync::Mutex<Option<std::time::Instant>>,
}

pub type SharedRpcHealth = std::sync::Arc<RpcHealth>;

/// Minimum interval between rate-limit reports per chain.
pub fn rate_limit_log_every() -> std::time::Duration { crate::config::tuning().rate_limit_log_every() }

impl RpcHealth {
    fn lock(&self) -> std::sync::MutexGuard<'_, HealthState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Report an outage. Returns the hold if this started a new degraded period, `None`
    /// if the chain was already degraded (same outage — e.g. the gas fetch reported it
    /// earlier in this scan), so callers log once per outage.
    pub fn mark_outage(&self) -> Option<std::time::Duration> {
        let mut st = self.lock();
        if st.degraded_since.is_some() {
            return None;
        }
        st.hold = next_hold(st.hold, st.last_recovered.map(|t| t.elapsed()));
        st.degraded_since = Some(std::time::Instant::now());
        Some(st.hold)
    }

    /// Execution is off.
    pub fn is_degraded(&self) -> bool {
        self.lock().degraded_since.is_some()
    }

    /// Degraded and the hold has passed: the monitor should probe.
    pub fn probe_due(&self) -> bool {
        let st = self.lock();
        st.degraded_since.is_some_and(|t| t.elapsed() >= st.hold)
    }

    /// The monitor found the node healthy after the hold: execution back on.
    /// Returns how long the chain was degraded (None if it no longer was).
    pub fn recover(&self) -> Option<std::time::Duration> {
        let mut st = self.lock();
        let since = st.degraded_since.take()?;
        st.last_recovered = Some(std::time::Instant::now());
        Some(since.elapsed())
    }

    /// The chain was paused: drop any degraded state and its escalation (start fresh).
    pub fn clear(&self) {
        *self.lock() = HealthState::default();
    }

    /// True at most once per [`rate_limit_log_every()`].
    pub fn rate_limit_log_due(&self) -> bool {
        let mut last = self.rate_limit_logged.lock().unwrap_or_else(|e| e.into_inner());
        let due = last.is_none_or(|t| t.elapsed() >= rate_limit_log_every());
        if due {
            *last = Some(std::time::Instant::now());
        }
        due
    }
}

/// Text of the error returned when a call is skipped because the chain is degraded.
/// Classified as [`RpcFailure::Unreachable`], so it never counts toward the breaker.
pub const RPC_BACKOFF_MARKER: &str = "rpc degraded";

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
        // Base's public endpoint: {"code":-32011,"message":"N/second request limit reached"}
        || e.contains("request limit")
    {
        RpcFailure::RateLimited
    } else {
        RpcFailure::None
    }
}

/// Whether a failed primary send may still have put the tx (and its nonce) on chain.
/// `broadcast_fired`: the signed tx was already sent to the secondary submission RPCs, so a
/// rate-limit or server error on the primary says nothing about whether it landed.
pub fn send_nonce_consumed(err: &str, broadcast_fired: bool) -> bool {
    let e = err.to_lowercase();
    let rpc = classify_rpc_failure(&e);
    e.contains("nonce too low")
        || rpc == RpcFailure::Unreachable
        || e.contains(RPC_OUTCOME_UNKNOWN_MARKER)
        || e.contains("already known")
        || e.contains("replacement transaction")
        || (broadcast_fired
            && (rpc == RpcFailure::RateLimited
                || ["internal error", "server error", "bad gateway", "service unavailable", "gateway timeout"]
                    .iter()
                    .any(|m| e.contains(m))))
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

/// URL for logs: scheme + host[:port], path and query masked — API keys live in the path
/// (Alchemy `/v2/<KEY>`) or query.
pub fn redact_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(u) => {
            let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
            let masked = !(u.path().is_empty() || u.path() == "/") || u.query().is_some();
            format!("{}://{}{}{}", u.scheme(), u.host_str().unwrap_or("?"), port, if masked { "/…" } else { "" })
        }
        Err(_) => "<unparseable url>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn latency_window_percentiles() {
        assert_eq!(percentiles(&mut []), None);
        assert_eq!(percentiles(&mut [7]), Some((7, 7, 7)));
        let mut v: Vec<u64> = (1..=100).rev().collect();
        assert_eq!(percentiles(&mut v), Some((50, 95, 100)));
        let w = LatencyWindow::new();
        assert_eq!(w.take_summary(), "-");
        for ms in [30, 10, 20] { w.record(ms); }
        assert_eq!(w.take_summary(), "20/30/30(3)");
        assert_eq!(w.take_summary(), "-"); // cleared
        for ms in 0..(LATENCY_WINDOW_CAP as u64 + 10) { w.record(ms); }
        assert!(w.take_summary().ends_with(&format!("({})", LATENCY_WINDOW_CAP + 10)));
    }

    #[test]
    fn redact_url_masks_keys() {
        let r = redact_url("wss://base-mainnet.g.alchemy.com/v2/SECRETKEY");
        assert_eq!(r, "wss://base-mainnet.g.alchemy.com/…");
        assert!(!r.contains("SECRETKEY"));
        assert_eq!(redact_url("wss://mainnet.base.org"), "wss://mainnet.base.org");
        assert_eq!(redact_url("ws://127.0.0.1:8547"), "ws://127.0.0.1:8547");
        assert_eq!(redact_url("https://user:pw@h.example/x?k=1"), "https://h.example/…");
        assert_eq!(redact_url("not a url"), "<unparseable url>");
    }

    #[test]
    fn send_nonce_consumed_cases() {
        assert!(send_nonce_consumed("nonce too low", false));
        assert!(!send_nonce_consumed("HTTP 429 Too Many Requests", false));
        assert!(send_nonce_consumed("HTTP 429 Too Many Requests", true));
        assert!(send_nonce_consumed("server returned an error response: error code -32603: internal error", true));
        assert!(!send_nonce_consumed("insufficient funds for gas * price + value", true));
        assert!(!send_nonce_consumed("execution reverted", true));
        assert!(send_nonce_consumed("RPC request timed out after 5s (outcome unknown)", false));
    }

    #[test]
    fn hold_starts_at_base_and_doubles_only_on_relapse() {
        let s = Duration::from_secs;
        assert_eq!(next_hold(Duration::ZERO, None), rpc_hold_base());
        // Relapse within the window doubles…
        assert_eq!(next_hold(s(10), Some(s(5))), s(20));
        assert_eq!(next_hold(s(40), Some(s(59))), s(80));
        // …capped…
        assert_eq!(next_hold(s(80), Some(s(1))), rpc_hold_max());
        // …and a healthy stretch resets it, however long the previous hold was.
        assert_eq!(next_hold(s(80), Some(rpc_relapse_window())), rpc_hold_base());
        // A first relapse after a zero/short hold never goes below base.
        assert_eq!(next_hold(Duration::ZERO, Some(s(1))), rpc_hold_base());
    }

    #[test]
    fn one_outage_marks_once_and_recover_clears() {
        let h = RpcHealth::default();
        assert!(!h.is_degraded());
        assert_eq!(h.mark_outage(), Some(rpc_hold_base()));
        assert!(h.is_degraded());
        assert_eq!(h.mark_outage(), None, "same outage must not be reported twice");
        assert!(!h.probe_due(), "hold has not passed yet");
        assert!(h.recover().is_some());
        assert!(!h.is_degraded());
        assert!(h.recover().is_none());
        // Relapse right after recovering: hold doubles.
        assert_eq!(h.mark_outage(), Some(rpc_hold_base() * 2));
        h.clear();
        assert!(!h.is_degraded());
        assert_eq!(h.mark_outage(), Some(rpc_hold_base()), "pause resets escalation");
    }

    #[test]
    fn classifies_rpc_failures() {
        assert_eq!(classify_rpc_failure("Send failed: RPC request timed out after 5s (outcome unknown)"), RpcFailure::Timeout);
        assert_eq!(classify_rpc_failure("error sending request for url"), RpcFailure::Unreachable);
        assert_eq!(classify_rpc_failure("dns error: failed to lookup address"), RpcFailure::Unreachable);
        assert_eq!(classify_rpc_failure(&format!("gas price not fetched: {}", RPC_BACKOFF_MARKER)), RpcFailure::Unreachable);
        assert_eq!(classify_rpc_failure("HTTP error 429 Too Many Requests"), RpcFailure::RateLimited);
        assert_eq!(classify_rpc_failure("exceeded compute units per second"), RpcFailure::RateLimited);
        // "429" inside hex revert data is not a rate limit.
        assert_eq!(classify_rpc_failure("execution reverted, data: 0xa4291f00"), RpcFailure::None);
        assert_eq!(classify_rpc_failure("server returned an error response: nonce too low"), RpcFailure::None);
    }

    #[test]
    fn hard_down_excludes_one_off_failures() {
        assert!(is_hard_down("Connection refused (os error 61)"));
        assert!(is_hard_down("backend connection task has stopped"));
        assert!(!is_hard_down("connection reset by peer"));
        assert!(!is_hard_down("dns error"), "a resolver blip may be one-off");
        assert!(!is_hard_down(RPC_BACKOFF_MARKER));
    }

    #[test]
    fn sim_failures_match_only_engine_prefixes() {
        assert!(is_sim_failure("Simulation failed: execution reverted"));
        assert!(is_sim_failure("Triangular simulation failed: x"));
        assert!(is_sim_failure("Pre-flight eth_call rejected: x"));
        assert!(!is_sim_failure("Send failed: node said: simulation failed"));
    }

    #[test]
    fn block_position_is_the_block_being_built() {
        // Head 50 seen at t=100.0 s, 200 ms slots: block 51 is being built.
        assert_eq!(block_position_at(100_000, 50, 100_000, 200), Some((51, 0)));
        assert_eq!(block_position_at(100_150, 50, 100_000, 200), Some((51, 150)));
        // A slot later with no new head: block 52, then 53.
        assert_eq!(block_position_at(100_200, 50, 100_000, 200), Some((52, 0)));
        assert_eq!(block_position_at(100_430, 50, 100_000, 200), Some((53, 30)));
        // No head yet.
        assert_eq!(block_position_at(100_000, 0, 0, 200), None);
    }

    #[test]
    fn throttle_logs_once_per_window_with_the_held_count() {
        let map = std::sync::Mutex::new(std::collections::HashMap::new());
        let (t0, w) = (std::time::Instant::now(), std::time::Duration::from_secs(600));
        assert_eq!(throttle_at(&map, "a", w, t0), Some(0));
        assert_eq!(throttle_at(&map, "a", w, t0 + std::time::Duration::from_secs(1)), None);
        assert_eq!(throttle_at(&map, "a", w, t0 + std::time::Duration::from_secs(2)), None);
        assert_eq!(throttle_at(&map, "b", w, t0 + std::time::Duration::from_secs(2)), Some(0), "keys are independent");
        assert_eq!(throttle_at(&map, "a", w, t0 + w), Some(2));
        assert_eq!(throttle_at(&map, "a", w, t0 + w + std::time::Duration::from_secs(1)), None);
    }
}
