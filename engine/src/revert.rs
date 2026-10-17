//! Mined-tx revert diagnosis and classification (Phase 0 telemetry).

use alloy::consensus::Transaction as TxFields;
use alloy::network::TransactionResponse;
use alloy::primitives::{hex, B256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::transports::TransportError;
use tracing::debug;

/// Stable label for logs, DB, and Prometheus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertClass {
    /// Pre-v3 executor: V3 sell leg's router min-out ("Too little received").
    V3TooLittle,
    /// Route completed below `amountIn + minProfit`. On executor v3 this is where every
    /// price-moved failure lands (routers get min-out 0).
    NotProfitable,
    /// Router-level output check: pre-v3 min-out, or a pool rejecting a dust output.
    InsufficientOutput,
    DeadlineExpired,
    RouterNotAllowed,
    PoolNotFound,
    TransferFailed,
    /// Executor holds less of the start token than the trade needs.
    InsufficientBalance,
    /// A leg completed but delivered nothing.
    NoOutput,
    /// Config/route problem caught before swapping (V3 fee 0, retired router type,
    /// repeated token).
    InvalidRoute,
    /// Contract paused, or caller is not the owner.
    Blocked,
    /// A router reverted with no data and the tx was not out of gas: a missing pool /
    /// fee tier, the wrong router type, or a reason-less revert inside the pool.
    RouterNoReason,
    /// A leg used all its gas (`LegExhaustedGas`, also in simulation), or the tx used ≈ its
    /// whole limit (receipt). Out of gas — or an invalid opcode in the router/pool.
    OutOfGas,
    Unknown,
    /// Replay RPC failed or node returned no revert data.
    Unavailable,
}

impl RevertClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V3TooLittle => "v3_too_little",
            Self::NotProfitable => "not_profitable",
            Self::InsufficientOutput => "insufficient_output",
            Self::DeadlineExpired => "deadline_expired",
            Self::RouterNotAllowed => "router_not_allowed",
            Self::PoolNotFound => "pool_not_found",
            Self::TransferFailed => "transfer_failed",
            Self::InsufficientBalance => "insufficient_balance",
            Self::NoOutput => "no_output",
            Self::InvalidRoute => "invalid_route",
            Self::Blocked => "blocked",
            Self::RouterNoReason => "router_no_reason",
            Self::OutOfGas => "out_of_gas",
            Self::Unknown => "unknown",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RevertDiagnosis {
    pub class: RevertClass,
    pub detail: Option<String>,
}

// Every error the executor can raise (v3 + OpenZeppelin + pre-v3), decoded once into
// `Executor::ExecutorErrors`. `explain_executor` matches it exhaustively, so a new contract
// error that is added here but not handled is a compile error — class and text can't drift.
alloy::sol! {
    interface Executor {
        error NotProfitable(uint256 balanceBefore, uint256 balanceAfter, uint256 minProfit);
        error DeadlineExpired(uint256 deadline, uint256 currentTimestamp);
        error ZeroAmount();
        error ZeroAddress();
        error TransferFailed();
        error RouterNotAllowed(address router);
        error UnsupportedRouterType(address router, uint8 rtype);
        error InsufficientBalance(address token, uint256 balance, uint256 needed);
        error InvalidPath();
        error InvalidV3Fee(uint8 leg, address router, uint24 fee);
        error LegFailed(uint8 leg, address router, uint8 rtype, bytes reason);
        error NoOutput(uint8 leg, address router);
        error NotAContract(address router);
        error LegExhaustedGas(uint8 leg, address router, bytes reason);
        error RenounceDisabled();
        error OwnableUnauthorizedAccount(address account);
        error EnforcedPause();
        error SafeERC20FailedOperation(address token);
        // Pre-v3 executor errors (still deployed until the redeploy).
        error InsufficientOutput(uint256 got, uint256 min);
        error PoolNotFound(address factory);
    }

    /// Custom errors the routers themselves raise (inside `LegFailed.reason`). Aerodrome's
    /// router and pools use argument-less errors; Uniswap/Pancake use `Error(string)`.
    interface Router {
        error InsufficientOutputAmount();
        error Expired();
        error InsufficientLiquidity();
        error PoolDoesNotExist();
        error K();
        error InsufficientInputAmount();
        error InvalidPath();
        error SameAddresses();
        error ZeroAddress();
    }
}

fn router_type_name(t: u8) -> &'static str {
    match t { 0 => "V2", 1 => "V3", 4 => "Aerodrome", _ => "retired type" }
}

/// Classify revert bytes from `eth_call` replay or RPC error payload. `detail` is a
/// human-readable sentence (see [`describe_revert`]).
pub fn classify_revert_bytes(data: &[u8]) -> RevertDiagnosis {
    let (class, detail) = explain(data);
    RevertDiagnosis { class, detail }
}

/// Human-readable explanation of revert data, e.g.
/// `leg 2 (V3 router 0x2626…) reverted: "Too little received"`.
pub fn describe_revert(data: &[u8]) -> Option<String> {
    explain(data).1
}

/// `(class, sentence)` for any revert payload. Never loses the bytes: anything that
/// doesn't decode (including a known selector with malformed arguments) falls back to hex.
fn explain(data: &[u8]) -> (RevertClass, Option<String>) {
    use alloy::sol_types::SolInterface;
    if data.is_empty() {
        return (RevertClass::Unknown, None);
    }
    if let Ok(e) = Executor::ExecutorErrors::abi_decode(data) {
        let (class, text) = explain_executor(e);
        return (class, Some(text));
    }
    let hex_text = format!("0x{}", hex::encode(data));
    if data.len() >= 4 && Executor::ExecutorErrors::valid_selector(data[..4].try_into().unwrap_or_default()) {
        return (RevertClass::Unknown, Some(format!("malformed executor error data {hex_text}")));
    }
    if let Some((name, meaning, class)) = explain_router(data) {
        return (class, Some(format!("{name} ({meaning})")));
    }
    match decode_abi_revert(data) {
        Some(msg) => (class_from_message(&msg), Some(format!("\"{msg}\""))),
        None => (RevertClass::Unknown, Some(hex_text)),
    }
}

fn explain_executor(e: Executor::ExecutorErrors) -> (RevertClass, String) {
    use Executor::ExecutorErrors as X;
    use RevertClass as C;
    match e {
        X::LegFailed(e) => {
            let (class, why) = if e.reason.is_empty() {
                (C::RouterNoReason,
                 "reverted with no reason (no pool for this pair/fee tier, wrong router type — check setRouter — or out of gas)".to_string())
            } else {
                // A router's own reason: its errors first. Aerodrome's InvalidPath() and
                // ZeroAddress() share selectors with the executor's and must not read as ours.
                let (inner, text) = match explain_router(&e.reason) {
                    Some((name, meaning, class)) => (class, format!("{name} ({meaning})")),
                    None => {
                        let (class, text) = explain(&e.reason);
                        (class, text.unwrap_or_default())
                    }
                };
                (inner, format!("reverted: {text}"))
            };
            (class, format!("leg {} ({} router {}) {}", e.leg, router_type_name(e.rtype), e.router, why))
        }
        X::NotProfitable(e) => (C::NotProfitable,
            format!("not profitable: balance {} → {}, needed +{}", e.balanceBefore, e.balanceAfter, e.minProfit)),
        X::DeadlineExpired(e) => (C::DeadlineExpired,
            format!("deadline {} passed (block time {})", e.deadline, e.currentTimestamp)),
        X::RouterNotAllowed(e) => (C::RouterNotAllowed,
            format!("router {} is not whitelisted on the executor", e.router)),
        X::UnsupportedRouterType(e) => (C::InvalidRoute,
            format!("router {} has unsupported type {}", e.router, e.rtype)),
        X::InsufficientBalance(e) => (C::InsufficientBalance,
            format!("executor holds {} of token {} but the trade needs {}", e.balance, e.token, e.needed)),
        X::InvalidV3Fee(e) => (C::InvalidRoute,
            format!("leg {}: fee {} is not a V3 fee tier (router {})", e.leg, e.fee, e.router)),
        X::NoOutput(e) => (C::NoOutput, format!("leg {}: router {} delivered no output", e.leg, e.router)),
        X::NotAContract(e) => (C::InvalidRoute, format!("{} has no code — not a router", e.router)),
        X::LegExhaustedGas(e) => (C::OutOfGas, format!(
            "leg {}: router {} used all its gas{} — raise the tx gas limit (if it persists at a higher \
             limit, the router or pool hit an invalid opcode / assert)",
            e.leg, e.router,
            if e.reason.is_empty() { String::new() }
            else { format!(" (router said {})", explain(&e.reason).1.unwrap_or_default()) })),
        X::InvalidPath(_) => (C::InvalidRoute, "route repeats a token".to_string()),
        X::ZeroAmount(_) => (C::InvalidRoute, "amount is zero".to_string()),
        X::ZeroAddress(_) => (C::InvalidRoute, "zero address in route".to_string()),
        X::RenounceDisabled(_) => (C::Blocked, "renounceOwnership is disabled".to_string()),
        X::EnforcedPause(_) => (C::Blocked, "executor is paused".to_string()),
        X::OwnableUnauthorizedAccount(e) => (C::Blocked,
            format!("caller {} is not the executor owner", e.account)),
        X::SafeERC20FailedOperation(e) => (C::TransferFailed,
            format!("token {} rejected an approve/transfer", e.token)),
        X::TransferFailed(_) => (C::TransferFailed, "ETH transfer failed".to_string()),
        X::InsufficientOutput(e) => (C::InsufficientOutput, format!("output {} below minimum {}", e.got, e.min)),
        X::PoolNotFound(e) => (C::PoolNotFound, format!("pool not found ({})", e.factory)),
    }
}

/// `(error name, what it means, class)` for a router's own custom error.
fn explain_router(data: &[u8]) -> Option<(&'static str, &'static str, RevertClass)> {
    use alloy::sol_types::SolInterface;
    use Router::RouterErrors as R;
    use RevertClass as C;
    Some(match R::abi_decode(data).ok()? {
        R::InsufficientOutputAmount(_) =>
            ("InsufficientOutputAmount()", "output below the leg minimum — price moved", C::InsufficientOutput),
        R::Expired(_) => ("Expired()", "router deadline passed", C::DeadlineExpired),
        R::PoolDoesNotExist(_) => ("PoolDoesNotExist()", "no pool for this pair/stable flag", C::PoolNotFound),
        R::InsufficientLiquidity(_) =>
            ("InsufficientLiquidity()", "pool too thin for this trade size", C::InsufficientOutput),
        R::K(_) => ("K()", "pool invariant check failed (fee-on-transfer token?)", C::Unknown),
        R::InsufficientInputAmount(_) => ("InsufficientInputAmount()", "pool received no input", C::TransferFailed),
        R::InvalidPath(_) => ("InvalidPath()", "router rejected the route", C::InvalidRoute),
        R::SameAddresses(_) => ("SameAddresses()", "router rejected the route", C::InvalidRoute),
        R::ZeroAddress(_) => ("ZeroAddress()", "router rejected the route", C::InvalidRoute),
    })
}

/// Class for a plain `Error(string)` revert (pre-v3 executors bubbled router strings).
fn class_from_message(msg: &str) -> RevertClass {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("too little received") {
        RevertClass::V3TooLittle
    } else if lower.contains("stf") || lower.contains("transfer") {
        RevertClass::TransferFailed
    } else if lower.contains("insufficient") {
        RevertClass::InsufficientOutput
    } else {
        RevertClass::Unknown
    }
}

/// Best-effort decode of standard `Error(string)` and Solidity panic selectors.
pub fn decode_abi_revert(data: &[u8]) -> Option<String> {
    use alloy::primitives::U256;
    if data.len() < 4 {
        return None;
    }
    if &data[..4] == [0x08, 0xc3, 0x79, 0xa0] {
        if data.len() < 4 + 64 {
            return None;
        }
        let offset = usize::try_from(U256::from_be_slice(&data[4..36])).ok()?;
        let payload_base = 4usize.checked_add(offset)?;
        if payload_base + 32 > data.len() {
            return None;
        }
        let msg_len =
            usize::try_from(U256::from_be_slice(&data[payload_base..payload_base + 32])).ok()?;
        let msg_start = payload_base + 32;
        if msg_start + msg_len > data.len() {
            return None;
        }
        return Some(String::from_utf8_lossy(&data[msg_start..msg_start + msg_len]).into_owned());
    }
    if &data[..4] == [0x4e, 0x48, 0x7b, 0x71] && data.len() >= 36 {
        let code = u32::from_be_bytes(data[4..8].try_into().ok()?);
        return Some(format!("Solidity panic code {code}"));
    }
    None
}

fn spelunk_revert_hex_in_json(raw: &str) -> Option<Vec<u8>> {
    use serde_json::Value;

    fn walk(v: &Value) -> Option<Vec<u8>> {
        match v {
            Value::String(s) => {
                let t = s.trim();
                let h = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))?;
                hex::decode(h).ok().filter(|b| !b.is_empty())
            }
            Value::Object(map) => map.values().find_map(walk),
            Value::Array(a) => a.iter().find_map(walk),
            _ => None,
        }
    }

    let v = serde_json::from_str(raw).ok()?;
    walk(&v)
}

/// Revert data carried by a JSON-RPC error — only when the error *is* an execution failure:
/// code 3 (geth/reth/anvil), or a message saying so ("execution reverted", Nethermind's
/// -32015 "VM execution error", …). Other errors (rate limits, timeouts) may carry
/// unrelated hex such as request ids, which must not be decoded as a revert.
fn revert_bytes_from_transport(e: &TransportError) -> Option<Vec<u8>> {
    let payload = e.as_error_resp()?;
    let msg = payload.message.to_ascii_lowercase();
    let is_execution_failure = payload.code == 3
        || msg.contains("revert")
        || msg.contains("execution error")
        || msg.contains("vm execution");
    if !is_execution_failure {
        return None;
    }
    if let Some(b) = payload.as_revert_data() {
        return Some(b.to_vec());
    }
    let raw = payload.data.as_ref()?.get();
    spelunk_revert_hex_in_json(raw).or_else(|| {
        let s = raw.trim().trim_matches('"');
        let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
        hex::decode(h).ok().filter(|b| !b.is_empty())
    })
}

/// A revert that used this share of its gas limit ran out of gas: the executor can't report
/// that itself (an out-of-gas router call looks like any reason-less revert).
const OUT_OF_GAS_USED_PCT: u64 = 97;

/// Replay a mined transaction to recover revert data. Without revert bytes the class is
/// `Unavailable` — no guessing from the route's shape — unless the receipt shows the gas
/// limit was exhausted. `gas_limit` is the limit the engine sent, so the out-of-gas check
/// works even when the tx lookup fails.
pub async fn diagnose_mined_revert<P: Provider>(
    provider: &P,
    tx_hash: &B256,
    block_number: Option<u64>,
    gas_used: u64,
    gas_limit: u64,
) -> RevertDiagnosis {
    let diagnosis = match provider.get_transaction_by_hash(*tx_hash).await {
        Ok(Some(tx)) => replay_revert(provider, &tx, block_number).await,
        _ => RevertDiagnosis { class: RevertClass::Unavailable, detail: None },
    };
    out_of_gas_override(diagnosis, gas_used, gas_limit)
}

/// Reclassify as `OutOfGas` when the receipt used ≥97% of the limit and the replay found
/// nothing more specific (no data, or a reason-less leg failure).
fn out_of_gas_override(d: RevertDiagnosis, gas_used: u64, gas_limit: u64) -> RevertDiagnosis {
    let exhausted = gas_limit > 0 && gas_used.saturating_mul(100) >= gas_limit.saturating_mul(OUT_OF_GAS_USED_PCT);
    let vague = matches!(d.class, RevertClass::Unavailable | RevertClass::Unknown | RevertClass::RouterNoReason);
    if !exhausted || !vague {
        return d;
    }
    RevertDiagnosis {
        class: RevertClass::OutOfGas,
        detail: Some(format!(
            "ran out of gas: used {gas_used} of {gas_limit} — raise tuning.gas_limit_2hop / gas_limit_triangular{}",
            d.detail.map(|x| format!(" (replay: {x})")).unwrap_or_default()
        )),
    }
}

/// Re-run the mined tx as an `eth_call` (same sender, calldata, value and gas limit) on the
/// state before its block — the closest available to what it ran on. `latest` would replay
/// against pools that moved since, giving a different (or no) error.
async fn replay_revert<P: Provider>(
    provider: &P,
    tx: &<alloy::network::Ethereum as alloy::network::Network>::TransactionResponse,
    block_number: Option<u64>,
) -> RevertDiagnosis {
    let inner = &tx.inner;
    let mut req = TransactionRequest::default()
        .from(tx.from())
        .input(inner.input().clone().into())
        .value(inner.value());
    if let Some(to) = inner.to() {
        req.to = Some(to.into());
    }
    req.gas = Some(inner.gas_limit());
    let call = provider.call(req);
    let call = match block_number {
        Some(n) if n > 0 => call.block(alloy::eips::BlockId::number(n - 1)),
        _ => call,
    };
    match call.await {
        Ok(_) => {
            debug!("replay eth_call succeeded for reverted tx — unexpected");
            RevertDiagnosis { class: RevertClass::Unavailable, detail: None }
        }
        Err(e) => match revert_bytes_from_transport(&e) {
            Some(bytes) => classify_revert_bytes(&bytes),
            None => RevertDiagnosis { class: RevertClass::Unavailable, detail: Some(e.to_string()) },
        },
    }
}

/// Format `eth_call` failures for logs. A revert reads `execution reverted: <decoded
/// reason> | revert_data=0x…` (reason first — the part a human needs). Anything else keeps
/// the node's own text (rate limit, timeout, …).
pub fn format_eth_call_transport_error(e: &TransportError) -> String {
    match revert_bytes_from_transport(e) {
        Some(rd) => match describe_revert(&rd) {
            Some(desc) => format!("execution reverted: {desc} | revert_data=0x{}", hex::encode(&rd)),
            None => e.to_string(),
        },
        None => e.to_string(),
    }
}

/// A simulation the contract itself rejected (the node returned revert data). Typed, so
/// callers can tell "the contract said no" from an RPC failure without matching text —
/// decoded reasons contain numbers (balances, minProfit) that look like "429" or worse.
#[derive(Debug)]
pub struct SimReverted(pub String);

impl std::fmt::Display for SimReverted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}

impl std::error::Error for SimReverted {}

/// `prefix: <error>` for a failed `eth_call` simulation: a [`SimReverted`] when the node
/// returned revert data, a plain error (classified as RPC trouble or not) otherwise.
pub fn eth_call_error(prefix: &str, e: &TransportError) -> anyhow::Error {
    if revert_bytes_from_transport(e).is_some() {
        anyhow::Error::new(SimReverted(format!("{prefix}: {}", format_eth_call_transport_error(e))))
    } else {
        anyhow::anyhow!("{prefix}: {e}")
    }
}

pub fn router_type_label(t: &crate::config::RouterType) -> &'static str {
    match t {
        crate::config::RouterType::V2 => "V2",
        crate::config::RouterType::V3 => "V3",
        crate::config::RouterType::Aerodrome => "Aerodrome",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::Executor::*;
    use alloy::primitives::{address, Bytes, U256};
    use alloy::sol_types::SolError;

    fn error_string(msg: &str) -> Vec<u8> {
        alloy::sol_types::Revert { reason: msg.to_string() }.abi_encode()
    }

    #[test]
    fn leg_failed_unwraps_router_reason() {
        let router = address!("2626664c2603336E57B271c5C0b26F421741e481");
        let data = LegFailed { leg: 2, router, rtype: 1, reason: Bytes::from(error_string("Too little received")) }
            .abi_encode();
        let d = classify_revert_bytes(&data);
        assert_eq!(d.class, RevertClass::V3TooLittle);
        let detail = d.detail.unwrap();
        assert!(detail.starts_with("leg 2 (V3 router 0x2626"), "{detail}");
        assert!(detail.contains("\"Too little received\""), "{detail}");
    }

    #[test]
    fn leg_failed_without_reason_hints_at_router_type() {
        let data = LegFailed { leg: 1, router: Default::default(), rtype: 0, reason: Bytes::new() }.abi_encode();
        let d = classify_revert_bytes(&data);
        assert_eq!(d.class, RevertClass::RouterNoReason);
        assert!(d.detail.unwrap().contains("check setRouter"));
    }

    #[test]
    fn exhausted_gas_leg_is_named_even_without_a_receipt() {
        let router = address!("2626664c2603336E57B271c5C0b26F421741e481");
        let d = classify_revert_bytes(&LegExhaustedGas { leg: 2, router, reason: Bytes::new() }.abi_encode());
        assert_eq!(d.class, RevertClass::OutOfGas);
        assert!(d.detail.unwrap().contains("leg 2: router 0x2626664c2603336E57B271c5C0b26F421741e481 used all its gas"));
        // Uniswap's "STF" re-raise of an inner out-of-gas: still out of gas, reason shown.
        let reason = Bytes::from(error_string("STF"));
        let d = classify_revert_bytes(&LegExhaustedGas { leg: 1, router, reason }.abi_encode());
        assert_eq!(d.class, RevertClass::OutOfGas);
        assert!(d.detail.unwrap().contains("(router said \"STF\")"));
    }

    #[test]
    fn bad_fee_is_named() {
        let router = address!("2626664c2603336E57B271c5C0b26F421741e481");
        let fee = alloy::primitives::Uint::<24, 1>::from(30u32);
        let d = classify_revert_bytes(&InvalidV3Fee { leg: 1, router, fee }.abi_encode());
        assert_eq!(d.class, RevertClass::InvalidRoute);
        assert!(d.detail.unwrap().contains("fee 30 is not a V3 fee tier"));
    }

    #[test]
    fn router_errors_sharing_executor_selectors_read_as_the_router() {
        // Aerodrome InvalidPath() inside LegFailed: same selector as the executor's InvalidPath().
        let router = address!("cF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43");
        let reason = Bytes::from(InvalidPath {}.abi_encode());
        let d = classify_revert_bytes(&LegFailed { leg: 2, router, rtype: 4, reason }.abi_encode());
        assert!(d.detail.unwrap().contains("InvalidPath() (router rejected the route)"));
    }

    #[test]
    fn exhausted_gas_limit_reads_as_out_of_gas() {
        let vague = RevertDiagnosis { class: RevertClass::Unavailable, detail: None };
        let d = out_of_gas_override(vague.clone(), 699_000, 700_000);
        assert_eq!(d.class, RevertClass::OutOfGas);
        assert!(d.detail.unwrap().contains("used 699000 of 700000"));
        // Plenty of gas left → keep the replay's answer.
        assert_eq!(out_of_gas_override(vague, 300_000, 700_000).class, RevertClass::Unavailable);
        // A reason-less leg failure near the limit is out of gas too.
        let no_reason = RevertDiagnosis { class: RevertClass::RouterNoReason, detail: None };
        assert_eq!(out_of_gas_override(no_reason, 699_000, 700_000).class, RevertClass::OutOfGas);
        // A specific contract error wins even near the limit.
        let specific = RevertDiagnosis { class: RevertClass::NotProfitable, detail: None };
        assert_eq!(out_of_gas_override(specific, 699_000, 700_000).class, RevertClass::NotProfitable);
    }

    fn rpc_error(code: i64, message: &str, data_hex: &str) -> TransportError {
        // Payload type inferred from the variant (alloy re-exports it under a feature we don't use).
        TransportError::ErrorResp(
            serde_json::from_value(serde_json::json!({ "code": code, "message": message, "data": data_hex })).unwrap(),
        )
    }

    #[test]
    fn simulation_reverts_are_typed_and_rate_limits_are_not() {
        let np = NotProfitable { balanceBefore: U256::from(1000), balanceAfter: U256::from(998), minProfit: U256::from(429) };
        let hexdata = format!("0x{}", hex::encode(np.abi_encode()));
        // geth-style code 3 and Nethermind-style -32015 both count as reverts.
        for (code, msg) in [(3, "execution reverted"), (-32015, "VM execution error.")] {
            let e = eth_call_error("Simulation failed", &rpc_error(code, msg, &hexdata));
            assert!(e.is::<SimReverted>(), "{code}");
            assert!(e.to_string().contains("needed +429"));
            assert_eq!(crate::util::classify_rpc_failure(&e.to_string()), crate::util::RpcFailure::RateLimited,
                       "the text alone would look like a rate limit — the type is what saves it");
        }
        // A rate-limit error with hex in its data is not a revert.
        let e = eth_call_error("Simulation failed", &rpc_error(-32005, "rate limit exceeded", "0xdeadbeef"));
        assert!(!e.is::<SimReverted>());
        assert!(e.to_string().contains("rate limit"), "node text kept");
    }

    #[test]
    fn malformed_known_error_keeps_the_bytes() {
        // LegFailed selector with truncated arguments: never drop the detail.
        let mut data = LegFailed { leg: 1, router: Default::default(), rtype: 0, reason: Bytes::new() }.abi_encode();
        data.truncate(40);
        let d = classify_revert_bytes(&data);
        assert_eq!(d.class, RevertClass::Unknown);
        assert!(d.detail.unwrap().starts_with("malformed executor error data 0x"));
    }

    #[test]
    fn leg_failed_decodes_aerodrome_custom_errors() {
        // Payload captured from the real Aerodrome router on a Base fork (no-spread round trip).
        let router = address!("cF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43");
        let reason = Bytes::from(alloy::hex::decode("42301c23").unwrap());
        let data = LegFailed { leg: 2, router, rtype: 4, reason }.abi_encode();
        let d = classify_revert_bytes(&data);
        assert_eq!(d.class, RevertClass::InsufficientOutput);
        assert_eq!(
            d.detail.unwrap(),
            "leg 2 (Aerodrome router 0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43) reverted: \
             InsufficientOutputAmount() (output below the leg minimum — price moved)"
        );
    }

    #[test]
    fn contract_errors_are_readable() {
        let data = InsufficientBalance { token: Default::default(), balance: U256::from(5), needed: U256::from(9) }
            .abi_encode();
        let d = classify_revert_bytes(&data);
        assert_eq!(d.class, RevertClass::InsufficientBalance);
        assert!(d.detail.unwrap().contains("holds 5"));

        let d = classify_revert_bytes(&NotProfitable {
            balanceBefore: U256::from(100), balanceAfter: U256::from(99), minProfit: U256::from(1),
        }.abi_encode());
        assert_eq!(d.class, RevertClass::NotProfitable);
        assert_eq!(d.detail.unwrap(), "not profitable: balance 100 → 99, needed +1");

        assert_eq!(classify_revert_bytes(&EnforcedPause {}.abi_encode()).class, RevertClass::Blocked);
        assert_eq!(classify_revert_bytes(&InvalidPath {}.abi_encode()).class, RevertClass::InvalidRoute);
    }
}
