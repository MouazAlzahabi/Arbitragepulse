//! Mined-tx revert diagnosis and classification (Phase 0 telemetry).

use alloy::consensus::Transaction as TxFields;
use alloy::network::TransactionResponse;
use alloy::primitives::{hex, keccak256, Bytes, B256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::transports::TransportError;
use tracing::debug;

/// Stable label for logs, DB, and Prometheus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertClass {
    V3TooLittle,
    NotProfitable,
    InsufficientOutput,
    DeadlineExpired,
    RouterNotAllowed,
    PoolNotFound,
    TransferFailed,
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

fn selector_is(sel: &[u8], signature: &str) -> bool {
    if sel.len() < 4 {
        return false;
    }
    let h = keccak256(signature.as_bytes());
    sel == &h[..4]
}

/// Classify revert bytes from `eth_call` replay or RPC error payload.
pub fn classify_revert_bytes(data: &[u8]) -> RevertDiagnosis {
    if data.len() >= 4 {
        let sel = &data[..4];
        if selector_is(sel, "NotProfitable(uint256,uint256,uint256)") {
            return RevertDiagnosis {
                class: RevertClass::NotProfitable,
                detail: Some(format!("selector=0x{}", hex::encode(sel))),
            };
        }
        if selector_is(sel, "InsufficientOutput(uint256,uint256)") {
            return RevertDiagnosis {
                class: RevertClass::InsufficientOutput,
                detail: Some(format!("selector=0x{}", hex::encode(sel))),
            };
        }
        if selector_is(sel, "DeadlineExpired(uint256,uint256)") {
            return RevertDiagnosis {
                class: RevertClass::DeadlineExpired,
                detail: Some(format!("selector=0x{}", hex::encode(sel))),
            };
        }
        if selector_is(sel, "RouterNotAllowed(address)") {
            return RevertDiagnosis {
                class: RevertClass::RouterNotAllowed,
                detail: Some(format!("selector=0x{}", hex::encode(sel))),
            };
        }
        if selector_is(sel, "PoolNotFound(address)") {
            return RevertDiagnosis {
                class: RevertClass::PoolNotFound,
                detail: Some(format!("selector=0x{}", hex::encode(sel))),
            };
        }
    }

    if let Some(msg) = decode_abi_revert(data) {
        let lower = msg.to_ascii_lowercase();
        let class = if lower.contains("too little received") {
            RevertClass::V3TooLittle
        } else if lower.contains("stf") || lower.contains("transfer") {
            RevertClass::TransferFailed
        } else if lower.contains("insufficient") {
            RevertClass::InsufficientOutput
        } else {
            RevertClass::Unknown
        };
        return RevertDiagnosis {
            class,
            detail: Some(msg),
        };
    }

    RevertDiagnosis {
        class: RevertClass::Unknown,
        detail: if data.is_empty() {
            None
        } else {
            Some(format!("0x{}", hex::encode(data)))
        },
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
    let mut parts = vec![e.to_string()];
    if let Some(payload) = e.as_error_resp() {
        let mut rd_opt = payload.as_revert_data();
        if rd_opt.is_none() {
            if let Some(raw) = payload.data.as_ref() {
                if let Some(b) = spelunk_revert_hex_in_json(raw.get()) {
                    rd_opt = Some(Bytes::from(b));
                }
            }
        }

        if let Some(rd) = rd_opt.as_ref() {
            parts.push(format!("revert_data=0x{}", hex::encode(rd.as_ref())));
            if let Some(msg) = decode_abi_revert(rd.as_ref()) {
                parts.push(format!("decoded={}", msg));
            } else if rd.len() >= 4 {
                parts.push(format!("selector=0x{}", hex::encode(&rd[..4])));
            }
        } else if let Some(raw) = payload.data.as_ref() {
            let s = raw.get().trim().trim_matches('"');
            if let Some(hex_part) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                if let Ok(bytes) = hex::decode(hex_part) {
                    if !bytes.is_empty() {
                        parts.push(format!("data=0x{}", hex::encode(&bytes)));
                        if let Some(msg) = decode_abi_revert(&bytes) {
                            parts.push(format!("decoded={}", msg));
                        }
                    }
                }
            }
        }
    }
    parts.join(" | ")
}

pub fn router_type_label(t: &crate::config::RouterType) -> &'static str {
    match t {
        crate::config::RouterType::V2 => "V2",
        crate::config::RouterType::V3 => "V3",
        crate::config::RouterType::Solidly => "Solidly",
        crate::config::RouterType::SyncSwap => "SyncSwap",
        crate::config::RouterType::Aerodrome => "Aerodrome",
    }
}

/// Final leg router type for triangular (C→A sell-back).
pub fn triangular_sell_leg_is_v3(opp: &crate::strategy::TriangularOpportunity) -> bool {
    matches!(opp.router_ca_type, crate::config::RouterType::V3)
}
