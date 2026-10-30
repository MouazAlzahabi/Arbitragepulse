//! Lost-race telemetry: after a mined revert, who touched our pools first in the same block,
//! and with what tip. Runs in the receipt task (never on the scan → send path).

use alloy::consensus::Transaction as TxFields;
use alloy::network::TransactionResponse;
use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
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
    /// "phantom": our tx also reverts replayed at the start of its block — it was never
    /// profitable, so nobody beat us (a quoting error, not a race). "tip": someone ahead in
    /// our block bid a higher tip. "ordering": someone ahead with an equal/lower tip (they
    /// reached the sequencer first). "timing": nobody ahead in our block — the price moved
    /// in an earlier block/flashblock (we were late).
    pub cause: &'static str,
    pub our_index: u64,
    pub our_tip_wei: u128,
    pub competitors: Vec<Competitor>,
}

/// Cause from the replay outcome (Some(true) = our tx reverts at the start of its block),
/// our tip and the tips of txs ahead of us on the same pools.
pub fn classify(replay_reverted: Option<bool>, our_tip: u128, ahead_tips: &[u128]) -> &'static str {
    if replay_reverted == Some(true) {
        return "phantom";
    }
    match ahead_tips.iter().max() {
        None => "timing",
        Some(&max) if max > our_tip => "tip",
        Some(_) => "ordering",
    }
}

/// The sent tx as a call for [`replay_reverts`]: sender, target, calldata and gas only
/// (no nonce or fees — the replay runs before our tx and must not fail on them).
pub fn replay_request(tx: &TransactionRequest) -> TransactionRequest {
    let mut r = TransactionRequest::default().input(tx.input.clone());
    r.from = tx.from;
    r.to = tx.to;
    r.gas = tx.gas;
    r
}

/// Replay our tx at the end of the parent block with our block's number and time (the
/// hooked pools' fees read the time): Some(true) if it reverts there too, Some(false) if it
/// succeeds, None when the replay couldn't be run.
async fn replay_reverts<P: Provider>(provider: &P, block_hash: B256, tx: TransactionRequest) -> Option<bool> {
    let block = rpc_timeout(rpc_read_timeout(), provider.get_block_by_hash(block_hash)).await.ok()??;
    let number = block.header.number;
    let at = alloy::rpc::types::BlockOverrides {
        number: Some(alloy::primitives::U256::from(number)),
        time: Some(block.header.timestamp),
        ..Default::default()
    };
    let parent = alloy::eips::BlockId::number(number.checked_sub(1)?);
    let call = std::future::IntoFuture::into_future(provider.call(tx).block(parent).with_block_overrides(at));
    match tokio::time::timeout(rpc_read_timeout(), call).await.ok()? {
        Ok(_) => Some(false),
        Err(e) if crate::strategy::multicall_error_is_revert(&e) => Some(true),
        Err(_) => None,
    }
}

/// Analyse a reverted tx: replay it at the start of its block (phantom check), then logs on
/// `pools` in its block from txs before `our_index`. None when the lookup fails (best
/// effort — telemetry only).
pub async fn analyze<P: Provider>(
    provider: &P,
    block_hash: B256,
    our_index: u64,
    our_tip: u128,
    our_sender: Address,
    pools: &[Address],
    replay: TransactionRequest,
) -> Option<RaceReport> {
    let replay_reverted = replay_reverts(provider, block_hash, replay).await;
    if replay_reverted == Some(true) {
        return Some(RaceReport { cause: classify(replay_reverted, our_tip, &[]), our_index, our_tip_wei: our_tip, competitors: vec![] });
    }
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
    Some(RaceReport { cause: classify(replay_reverted, our_tip, &tips), our_index, our_tip_wei: our_tip, competitors })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn race_causes() {
        assert_eq!(classify(None, 10, &[]), "timing");
        assert_eq!(classify(Some(false), 10, &[5, 20]), "tip");
        assert_eq!(classify(None, 10, &[10, 3]), "ordering");
        // A tx that reverts at the start of its block was never profitable: phantom wins.
        assert_eq!(classify(Some(true), 10, &[5, 20]), "phantom");
        assert_eq!(classify(Some(true), 10, &[]), "phantom");
    }
}
