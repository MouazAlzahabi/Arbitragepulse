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

    /// Full opportunity fingerprint for deduplication. Starts with the pair/triplet id
    /// (large-swap cooldown clearing matches on it) and includes each leg's fee, so V3
    /// fee tiers and Aerodrome volatile/stable legs are separate routes.
    pub fn fingerprint(&self) -> String {
        match self {
            Opportunity::TwoHop(o) => format!(
                "{}|{}:{}|{}:{}|{}",
                o.pair_id, o.router_a_id, o.fee_a, o.router_b_id, o.fee_b, o.amount_in
            ),
            Opportunity::Triangular(o) => format!(
                "{}|{}:{}|{}:{}|{}:{}|{}",
                o.triplet_id, o.router_ab_id, o.fee_ab, o.router_bc_id, o.fee_bc,
                o.router_ca_id, o.fee_ca, o.amount_in
            ),
        }
    }

    /// The pools this opportunity trades through (V2/Aerodrome/V3), from the local cache.
    pub fn pools(&self, cache: &crate::pool_cache::PoolCache) -> Vec<alloy::primitives::Address> {
        match self {
            Opportunity::TwoHop(o) => crate::race::route_pools(cache, &[
                (&o.router_a_id, o.router_a_type, o.fee_a, o.token_in, o.token_out),
                (&o.router_b_id, o.router_b_type, o.fee_b, o.token_out, o.token_in),
            ]),
            Opportunity::Triangular(o) => crate::race::route_pools(cache, &[
                (&o.router_ab_id, o.router_ab_type, o.fee_ab, o.token_a, o.token_b),
                (&o.router_bc_id, o.router_bc_type, o.fee_bc, o.token_b, o.token_c),
                (&o.router_ca_id, o.router_ca_type, o.fee_ca, o.token_c, o.token_a),
            ]),
        }
    }

    /// Cooldown-map key that throttles the opportunity log and ghost-profit accounting for a
    /// route whose last attempt was rejected as unprofitable after gas. It never gates
    /// execution: the route is re-evaluated on every scan.
    pub fn gas_log_key(fingerprint: &str) -> String {
        format!("{fingerprint}|gaslog")
    }

    /// Cooldown-map key that dedupes the `opps_found` count: a standing edge re-detected
    /// on every scan counts once per [`crate::chain::cooldown_secs()`] window. Never gates
    /// execution.
    pub fn opp_count_key(fingerprint: &str) -> String {
        format!("{fingerprint}|counted")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_separates_fee_tiers_and_starts_with_pair_id() {
        let opp = |fee_a| Opportunity::TwoHop(ArbOpportunity {
            chain_id: 8453,
            pair_id: "base-usdc-weth".into(),
            token_in: Address::ZERO,
            token_out: Address::ZERO,
            amount_in: U256::from(1_000_000_000u64),
            router_a: Address::ZERO,
            router_b: Address::ZERO,
            router_a_type: RouterType::V3,
            router_b_type: RouterType::Aerodrome,
            fee_a,
            fee_b: 0,
            expected_profit: U256::ZERO,
            profit_usd: 1.0,
            router_a_id: "uniswap-v3-base".into(),
            router_b_id: "aerodrome-base::volatile".into(),
        });
        let (a, b) = (opp(500).fingerprint(), opp(3000).fingerprint());
        assert_ne!(a, b);
        assert!(a.starts_with("base-usdc-weth|"));
        assert!(Opportunity::gas_log_key(&a).starts_with(&a));
    }
}
