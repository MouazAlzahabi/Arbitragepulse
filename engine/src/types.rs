use alloy::primitives::{Address, U256};
use serde::Serialize;

use crate::config::RouterType;

// ─── 2-hop opportunity ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    pub chain_id: u64,
    pub pair_id: String,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub router_a: Address,
    pub router_b: Address,
    pub router_a_type: RouterType,
    pub router_b_type: RouterType,
    pub fee_a: u32,
    pub fee_b: u32,
    pub expected_profit: U256,
    pub profit_usd: f64,
    pub router_a_id: String,
    pub router_b_id: String,
}

// ─── Triangular opportunity ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TriangularOpportunity {
    pub chain_id: u64,
    pub triplet_id: String, // "USDC-WETH-OP"
    pub token_a: Address,
    pub token_b: Address,
    pub token_c: Address,
    pub amount_in: U256,
    pub router_ab: Address,
    pub router_bc: Address,
    pub router_ca: Address,
    pub router_ab_type: RouterType,
    pub router_bc_type: RouterType,
    pub router_ca_type: RouterType,
    pub fee_ab: u32,
    pub fee_bc: u32,
    pub fee_ca: u32,
    pub expected_profit: U256,
    pub profit_usd: f64,
    pub router_ab_id: String,
    pub router_bc_id: String,
    pub router_ca_id: String,
}

// ─── Unified opportunity ──────────────────────────────────────────────────────

/// Unified opportunity enum for 2-hop and triangular arbitrage.
/// Allows chain.rs to handle both types in a single evaluation loop.
#[derive(Debug, Clone)]
pub enum Opportunity {
    TwoHop(ArbOpportunity),
    Triangular(TriangularOpportunity),
}

impl Opportunity {
    pub fn profit_usd(&self) -> f64 {
        match self {
            Opportunity::TwoHop(o) => o.profit_usd,
            Opportunity::Triangular(o) => o.profit_usd,
        }
    }

    pub fn pair_id(&self) -> &str {
        match self {
            Opportunity::TwoHop(o) => &o.pair_id,
            Opportunity::Triangular(o) => &o.triplet_id,
        }
    }

    /// Returns true if this opportunity can be sent to the contract for execution.
    pub fn is_executable(&self) -> bool {
        true
    }

    /// Full opportunity fingerprint for deduplication.
    pub fn fingerprint(&self) -> String {
        match self {
            Opportunity::TwoHop(o) => {
                format!("{}|{}|{}|{}", o.pair_id, o.router_a_id, o.router_b_id, o.amount_in)
            }
            Opportunity::Triangular(o) => {
                format!("{}|{}|{}|{}|{}", o.triplet_id, o.router_ab_id, o.router_bc_id, o.router_ca_id, o.amount_in)
            }
        }
    }
}

// ─── Per-pair scan snapshot ───────────────────────────────────────────────────

/// Statistics for a single configured pair from the last evaluate() call.
/// Exposed via the /pair-scan API endpoint and the dashboard Pairs tab.
#[derive(Debug, Clone, Serialize)]
pub struct PairScanInfo {
    pub pair_id: String,
    pub chain_id: u64,
    /// Display name without chain prefix, e.g. "USDC→WETH"
    pub display_name: String,
    /// Number of distinct DEXes (router IDs) that produced a non-zero forward quote.
    pub dex_count: usize,
    /// Router IDs that produced a non-zero forward quote.
    pub dex_ids: Vec<String>,
    /// Number of cross-DEX arb combinations examined in reverse phase (≥2 DEXes).
    pub cross_count: usize,
    /// Whether any DEX quoted this pair in the last scan.
    pub was_quoted: bool,
    /// Number of profitable opportunities found for this pair this session.
    pub opp_count: u64,
    /// Whether this pair is disabled via the dashboard toggle.
    pub disabled: bool,
}
