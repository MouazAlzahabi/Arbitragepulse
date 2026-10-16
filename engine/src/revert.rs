//! Mined-tx revert diagnosis and classification (Phase 0 telemetry).

use alloy::consensus::Transaction as TxFields;
use alloy::network::TransactionResponse;
use alloy::primitives::{hex, Bytes, B256};
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
    Unknown,
    /// Replay RPC failed or node returned no revert data.
    Unavailable,
    /// No revert bytes; sell leg is V3 (historical ~99% of Base failures).
    V3TooLittleLikely,
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
            Self::Unknown => "unknown",
            Self::Unavailable => "unavailable",
            Self::V3TooLittleLikely => "v3_too_little_likely",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RevertDiagnosis {
    pub class: RevertClass,
    pub detail: Option<String>,
}

// ArbitrageExecutor v3 errors (+ the OpenZeppelin ones it can raise), for decoding.
alloy::sol! {
    error NotProfitable(uint256 balanceBefore, uint256 balanceAfter, uint256 minProfit);
    error DeadlineExpired(uint256 deadline, uint256 currentTimestamp);
    error ZeroAmount();
    error ZeroAddress();
    error TransferFailed();
    error RouterNotAllowed(address router);
    error UnsupportedRouterType(address router, uint8 rtype);
    error InsufficientBalance(address token, uint256 balance, uint256 needed);
    error InvalidPath();
    error InvalidV3Fee(uint8 leg, address router);
    error LegFailed(uint8 leg, address router, uint8 rtype, bytes reason);
    error NoOutput(uint8 leg, address router);
    error OwnableUnauthorizedAccount(address account);
    error EnforcedPause();
    error SafeERC20FailedOperation(address token);
    // Pre-v3 executor errors (still deployed until the redeploy).
    error InsufficientOutput(uint256 got, uint256 min);
    error PoolNotFound(address factory);
}

/// Custom errors the routers themselves raise (inside `LegFailed.reason`). Aerodrome's
/// router and pools use argument-less errors; Uniswap/Pancake use `Error(string)`.
mod router_errors {
    alloy::sol! {
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

/// `(error name, what it means, class)` for a known router custom error.
fn router_error(reason: &[u8]) -> Option<(&'static str, &'static str, RevertClass)> {
    use alloy::sol_types::SolError;
    use router_errors::*;
    let sel: [u8; 4] = reason.get(..4)?.try_into().ok()?;
    let found = [
        (InsufficientOutputAmount::SELECTOR, InsufficientOutputAmount::SIGNATURE,
            "output below the leg minimum — price moved", RevertClass::InsufficientOutput),
        (Expired::SELECTOR, Expired::SIGNATURE, "router deadline passed", RevertClass::DeadlineExpired),
        (PoolDoesNotExist::SELECTOR, PoolDoesNotExist::SIGNATURE,
            "no pool for this pair/stable flag", RevertClass::PoolNotFound),
        (InsufficientLiquidity::SELECTOR, InsufficientLiquidity::SIGNATURE, "pool has too little liquidity", RevertClass::Unknown),
        (K::SELECTOR, K::SIGNATURE, "pool invariant check failed", RevertClass::Unknown),
        (InsufficientInputAmount::SELECTOR, InsufficientInputAmount::SIGNATURE, "pool received no input", RevertClass::TransferFailed),
        (InvalidPath::SELECTOR, InvalidPath::SIGNATURE, "router rejected the route", RevertClass::Unknown),
        (SameAddresses::SELECTOR, SameAddresses::SIGNATURE, "router rejected the route", RevertClass::Unknown),
        (ZeroAddress::SELECTOR, ZeroAddress::SIGNATURE, "router rejected the route", RevertClass::Unknown),
    ]
    .into_iter()
    .find(|(s, ..)| *s == sel)?;
    Some((found.1, found.2, found.3))
}

fn router_type_name(t: u8) -> &'static str {
    match t { 0 => "V2", 1 => "V3", 4 => "Aerodrome", _ => "retired type" }
}

/// Classify revert bytes from `eth_call` replay or RPC error payload. `detail` is a
/// human-readable sentence (see [`describe_revert`]).
pub fn classify_revert_bytes(data: &[u8]) -> RevertDiagnosis {
    RevertDiagnosis { class: classify_inner(data), detail: describe_revert(data) }
}

fn classify_inner(data: &[u8]) -> RevertClass {
    use alloy::sol_types::SolError;
    if data.len() < 4 {
        return RevertClass::Unknown;
    }
    let sel: [u8; 4] = data[..4].try_into().unwrap_or_default();
    match sel {
        s if s == LegFailed::SELECTOR => match LegFailed::abi_decode(data) {
            // The router's own reason decides the class (e.g. V3 "Too little received").
            Ok(e) if !e.reason.is_empty() => match router_error(&e.reason) {
                Some((_, _, class)) => class,
                None => classify_inner(&e.reason),
            },
            _ => RevertClass::Unknown,
        },
        s if s == NotProfitable::SELECTOR => RevertClass::NotProfitable,
        s if s == InsufficientOutput::SELECTOR => RevertClass::InsufficientOutput,
        s if s == DeadlineExpired::SELECTOR => RevertClass::DeadlineExpired,
        s if s == RouterNotAllowed::SELECTOR => RevertClass::RouterNotAllowed,
        s if s == PoolNotFound::SELECTOR => RevertClass::PoolNotFound,
        s if s == InsufficientBalance::SELECTOR => RevertClass::InsufficientBalance,
        s if s == NoOutput::SELECTOR => RevertClass::NoOutput,
        s if s == UnsupportedRouterType::SELECTOR || s == InvalidPath::SELECTOR
            || s == InvalidV3Fee::SELECTOR || s == ZeroAmount::SELECTOR
            || s == ZeroAddress::SELECTOR => RevertClass::InvalidRoute,
        s if s == EnforcedPause::SELECTOR || s == OwnableUnauthorizedAccount::SELECTOR => RevertClass::Blocked,
        s if s == SafeERC20FailedOperation::SELECTOR || s == TransferFailed::SELECTOR => RevertClass::TransferFailed,
        _ => match decode_abi_revert(data) {
            Some(msg) => {
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
            None => RevertClass::Unknown,
        },
    }
}

/// Human-readable explanation of executor revert data, e.g.
/// `leg 2 (V3 router 0x2626…) reverted: "Too little received"`.
pub fn describe_revert(data: &[u8]) -> Option<String> {
    use alloy::sol_types::SolError;
    if data.is_empty() {
        return None;
    }
    if data.len() < 4 {
        return Some(format!("0x{}", hex::encode(data)));
    }
    let sel: [u8; 4] = data[..4].try_into().unwrap_or_default();
    let d = match sel {
        s if s == LegFailed::SELECTOR => {
            let e = LegFailed::abi_decode(data).ok()?;
            let why = if e.reason.is_empty() {
                "reverted with no reason (no pool for this pair/fee tier, or wrong router type — check setRouter)"
                    .to_string()
            } else if let Some((name, meaning, _)) = router_error(&e.reason) {
                format!("reverted: {name} ({meaning})")
            } else {
                format!("reverted: {}", describe_revert(&e.reason).unwrap_or_default())
            };
            format!("leg {} ({} router {}) {}", e.leg, router_type_name(e.rtype), e.router, why)
        }
        s if s == NotProfitable::SELECTOR => {
            let e = NotProfitable::abi_decode(data).ok()?;
            format!("not profitable: balance {} → {}, needed +{}", e.balanceBefore, e.balanceAfter, e.minProfit)
        }
        s if s == DeadlineExpired::SELECTOR => {
            let e = DeadlineExpired::abi_decode(data).ok()?;
            format!("deadline {} passed (block time {})", e.deadline, e.currentTimestamp)
        }
        s if s == RouterNotAllowed::SELECTOR => {
            format!("router {} is not whitelisted on the executor", RouterNotAllowed::abi_decode(data).ok()?.router)
        }
        s if s == UnsupportedRouterType::SELECTOR => {
            let e = UnsupportedRouterType::abi_decode(data).ok()?;
            format!("router {} has unsupported type {}", e.router, e.rtype)
        }
        s if s == InsufficientBalance::SELECTOR => {
            let e = InsufficientBalance::abi_decode(data).ok()?;
            format!("executor holds {} of token {} but the trade needs {}", e.balance, e.token, e.needed)
        }
        s if s == InvalidV3Fee::SELECTOR => {
            let e = InvalidV3Fee::abi_decode(data).ok()?;
            format!("leg {}: V3 router {} given fee 0", e.leg, e.router)
        }
        s if s == NoOutput::SELECTOR => {
            let e = NoOutput::abi_decode(data).ok()?;
            format!("leg {}: router {} delivered no output", e.leg, e.router)
        }
        s if s == InvalidPath::SELECTOR => "route repeats a token".to_string(),
        s if s == ZeroAmount::SELECTOR => "amount is zero".to_string(),
        s if s == ZeroAddress::SELECTOR => "zero address in route".to_string(),
        s if s == EnforcedPause::SELECTOR => "executor is paused".to_string(),
        s if s == OwnableUnauthorizedAccount::SELECTOR => {
            format!("caller {} is not the executor owner", OwnableUnauthorizedAccount::abi_decode(data).ok()?.account)
        }
        s if s == SafeERC20FailedOperation::SELECTOR => {
            format!("token {} rejected an approve/transfer", SafeERC20FailedOperation::abi_decode(data).ok()?.token)
        }
        s if s == InsufficientOutput::SELECTOR => {
            let e = InsufficientOutput::abi_decode(data).ok()?;
            format!("output {} below minimum {}", e.got, e.min)
        }
        s if s == PoolNotFound::SELECTOR => format!("pool not found ({})", PoolNotFound::abi_decode(data).ok()?.factory),
        _ => match decode_abi_revert(data) {
            Some(msg) => format!("\"{}\"", msg),
            None => format!("0x{}", hex::encode(data)),
        },
    };
    Some(d)
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

fn revert_bytes_from_transport(e: &TransportError) -> Option<Vec<u8>> {
    let payload = e.as_error_resp()?;
    let mut rd_opt = payload.as_revert_data();
    if rd_opt.is_none() {
        if let Some(raw) = payload.data.as_ref() {
            if let Some(b) = spelunk_revert_hex_in_json(raw.get()) {
                rd_opt = Some(Bytes::from(b));
            }
        }
    }
    rd_opt.map(|b| b.to_vec())
}

/// Replay a mined transaction at its inclusion block to recover revert data.
pub async fn diagnose_mined_revert<P: Provider>(
    provider: &P,
    tx_hash: &B256,
    block_number: Option<u64>,
    sell_leg_is_v3: bool,
) -> RevertDiagnosis {
    let _ = block_number;
    if let Ok(Some(tx)) = provider.get_transaction_by_hash(*tx_hash).await {
        let inner = &tx.inner;
        let mut req = TransactionRequest::default()
            .from(tx.from())
            .input(inner.input().clone().into())
            .value(inner.value());
        if let Some(to) = inner.to() {
            req.to = Some(to.into());
        }
        req.gas = Some(inner.gas_limit());

        match provider.call(req).await {
            Ok(_) => {
                debug!("replay eth_call succeeded for reverted tx — unexpected");
            }
            Err(e) => {
                if let Some(bytes) = revert_bytes_from_transport(&e) {
                    return classify_revert_bytes(&bytes);
                }
                return RevertDiagnosis {
                    class: if sell_leg_is_v3 {
                        RevertClass::V3TooLittleLikely
                    } else {
                        RevertClass::Unavailable
                    },
                    detail: Some(e.to_string()),
                };
            }
        }
    }

    if sell_leg_is_v3 {
        RevertDiagnosis {
            class: RevertClass::V3TooLittleLikely,
            detail: None,
        }
    } else {
        RevertDiagnosis {
            class: RevertClass::Unavailable,
            detail: None,
        }
    }
}

/// Format JSON-RPC failures from `eth_call`, including revert hex when the node returns it.
pub fn format_eth_call_transport_error(e: &TransportError) -> String {
    let Some(payload) = e.as_error_resp() else { return e.to_string() };
    let revert = payload.as_revert_data().map(|b| b.to_vec()).or_else(|| {
        let raw = payload.data.as_ref()?;
        spelunk_revert_hex_in_json(raw.get()).or_else(|| {
            let s = raw.get().trim().trim_matches('"');
            let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
            hex::decode(h).ok().filter(|b| !b.is_empty())
        })
    });
    match revert {
        // Decoded reason first: it is the part a human needs, and the transport text
        // only repeats the raw bytes (which follow for grep/replay).
        Some(rd) => match describe_revert(&rd) {
            Some(desc) => format!("{desc} | revert_data=0x{}", hex::encode(&rd)),
            None => format!("{e} | revert_data=0x{}", hex::encode(&rd)),
        },
        None => e.to_string(),
    }
}

pub fn router_type_label(t: &crate::config::RouterType) -> &'static str {
    match t {
        crate::config::RouterType::V2 => "V2",
        crate::config::RouterType::V3 => "V3",
        crate::config::RouterType::Aerodrome => "Aerodrome",
    }
}

/// Final leg router type for triangular (C→A sell-back).
pub fn triangular_sell_leg_is_v3(opp: &crate::strategy::TriangularOpportunity) -> bool {
    matches!(opp.router_ca_type, crate::config::RouterType::V3)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(d.detail.unwrap().contains("check setRouter"));
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
