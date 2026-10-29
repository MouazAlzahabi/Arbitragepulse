//! Lost-race telemetry: after a mined revert, who touched our pools first in the same block,
//! and with what tip. Runs in the receipt task (never on the scan → send path).

use alloy::consensus::Transaction as TxFields;
use alloy::network::TransactionResponse;
use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::sol_types::SolEvent;

use crate::config::RouterType;
use crate::pool_cache::{PoolCache, V2PoolKey, V3PoolKey};
use crate::util::{rpc_read_timeout, rpc_timeout};

/// At most this many competitor txs are looked up per revert.
const MAX_COMPETITORS: usize = 5;

/// One leg of a route: (router id, type, fee, token in, token out).
pub type Leg<'a> = (&'a str, RouterType, u32, Address, Address);

/// Pool addresses a route trades through, from the local pool cache.
pub fn route_pools(cache: &PoolCache, legs: &[Leg<'_>]) -> Vec<Address> {
    let mut out = Vec::new();
    for &leg in legs {
        if let Some(p) = leg_pool(cache, leg).filter(|p| !out.contains(p)) {
            out.push(p);
        }
    }
    out
}

/// The pool one leg trades through, from the local pool cache.
pub fn leg_pool(cache: &PoolCache, (router_id, rtype, fee, tin, tout): Leg<'_>) -> Option<Address> {
    match rtype {
        RouterType::V3 => cache.v3_by_key.get(&V3PoolKey::new(router_id, tin, tout, fee)).map(|p| *p),
        RouterType::V2 => cache.by_key.get(&V2PoolKey::new(router_id, tin, tout)).map(|p| *p),
        RouterType::Aerodrome => {
            // 2-hop routes carry the cache id ("<router>::volatile|stable"); triangular
            // routes carry the router id and the stable flag in `fee`.
            let id = if router_id.contains("::") {
                router_id.to_string()
            } else {
                format!("{router_id}::{}", if fee != 0 { "stable" } else { "volatile" })
            };
            cache.by_key.get(&V2PoolKey::new(&id, tin, tout)).map(|p| *p)
        }
    }
}

/// A tx that touched one of our pools earlier in the block.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Competitor {
    pub tx: B256,
    pub index: u64,
    pub from: Address,
    pub tip_wei: u128,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RaceReport {
    /// "tip": someone ahead in our block bid a higher tip. "ordering": someone ahead with
    /// an equal/lower tip (they reached the sequencer first). "timing": nobody ahead in our
    /// block — the price moved in an earlier block/flashblock (we were late).
    pub cause: &'static str,
    pub our_index: u64,
    pub our_tip_wei: u128,
    pub competitors: Vec<Competitor>,
}

/// Cause from our tip and the tips of txs ahead of us on the same pools.
pub fn classify(our_tip: u128, ahead_tips: &[u128]) -> &'static str {
    match ahead_tips.iter().max() {
        None => "timing",
        Some(&max) if max > our_tip => "tip",
        Some(_) => "ordering",
    }
}

/// Analyse a reverted tx: logs on `pools` in its block from txs before `our_index`.
/// None when the lookup fails (best effort — telemetry only).
pub async fn analyze<P: Provider>(
    provider: &P,
    block_hash: B256,
    our_index: u64,
    our_tip: u128,
    our_sender: Address,
    pools: &[Address],
) -> Option<RaceReport> {
    if pools.is_empty() {
        return None;
    }
    let filter = alloy::rpc::types::Filter::new()
        .at_block_hash(block_hash)
        .address(pools.to_vec())
        .event_signature(vec![
            crate::abi::Sync::SIGNATURE_HASH,
            alloy::primitives::keccak256(b"Sync(uint256,uint256)"),
            crate::abi::Swap::SIGNATURE_HASH,
            crate::abi::IPancakeV3Pool::Swap::SIGNATURE_HASH,
        ]);
    let logs = rpc_timeout(rpc_read_timeout(), provider.get_logs(&filter)).await.ok()?;
    let mut ahead: Vec<(u64, B256)> = logs
        .iter()
        .filter_map(|l| Some((l.transaction_index?, l.transaction_hash?)))
        .filter(|(i, _)| *i < our_index)
        .collect();
    ahead.sort();
    ahead.dedup();
    let mut competitors = Vec::new();
    for (index, hash) in ahead.into_iter().take(MAX_COMPETITORS) {
        let Ok(Some(tx)) = rpc_timeout(rpc_read_timeout(), provider.get_transaction_by_hash(hash)).await else {
            continue;
        };
        if tx.from() == our_sender {
            continue;
        }
        let inner = &tx.inner;
        let tip_wei = inner.max_priority_fee_per_gas().unwrap_or_else(|| inner.gas_price().unwrap_or(0));
        competitors.push(Competitor { tx: hash, index, from: tx.from(), tip_wei });
    }
    let tips: Vec<u128> = competitors.iter().map(|c| c.tip_wei).collect();
    Some(RaceReport { cause: classify(our_tip, &tips), our_index, our_tip_wei: our_tip, competitors })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn race_causes() {
        assert_eq!(classify(10, &[]), "timing");
        assert_eq!(classify(10, &[5, 20]), "tip");
        assert_eq!(classify(10, &[10, 3]), "ordering");
    }
}
