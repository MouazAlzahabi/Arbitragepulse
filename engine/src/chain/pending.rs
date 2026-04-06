use alloy::consensus::Transaction as TxTrait;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use futures::StreamExt;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// ─── Selectors ────────────────────────────────────────────────────────────────

/// UniswapV3 / PancakeV3 SwapRouter02 exactInputSingle
const SEL_V3_EXACT_INPUT_SINGLE: [u8; 4] = [0x04, 0xe4, 0x5a, 0xaf];
/// Standard V2 / Aerodrome swapExactTokensForTokens
const SEL_V2_SWAP_EXACT_TOKENS: [u8; 4] = [0x38, 0xed, 0x17, 0x39];
/// V2 swapExactETHForTokens
const SEL_V2_SWAP_EXACT_ETH: [u8; 4] = [0x7f, 0xf3, 0x6a, 0xb5];

// ─── Types ────────────────────────────────────────────────────────────────────

/// A large swap detected in the mempool before it confirms.
/// Used to trigger a same-block arb submission.
#[derive(Debug, Clone)]
pub struct PendingSwapEvent {
    /// Token being swapped IN to the DEX pool (the pool's buy side).
    pub token_in: Address,
    /// Token being swapped OUT of the pool.
    pub token_out: Address,
    /// Router address the pending TX targets.
    pub router: Address,
    /// Amount the user is swapping in (raw, unscaled).
    pub amount_in: U256,
    /// maxPriorityFeePerGas from the pending TX — we match this to land in same block.
    pub tip_wei: u128,
}

// ─── Calldata parser ──────────────────────────────────────────────────────────

/// Parse a swap calldata buffer and return `(token_in, token_out, amount_in)`.
/// Returns `None` for unrecognised selectors or truncated input.
///
/// Supported:
/// - `0x04e45aaf` exactInputSingle (V3)
/// - `0x38ed1739` swapExactTokensForTokens (V2/Aerodrome)
/// - `0x7ff36ab5` swapExactETHForTokens (V2 — amount_in from tx.value, passed as `eth_value`)
pub fn parse_swap_calldata(
    input: &Bytes,
    eth_value: U256,
) -> Option<(Address, Address, U256)> {
    let data = input.as_ref();
    if data.len() < 4 {
        return None;
    }
    let sel: [u8; 4] = data[..4].try_into().ok()?;

    match sel {
        // ── V3 exactInputSingle ───────────────────────────────────────────────
        // struct ExactInputSingleParams {
        //   address tokenIn;     // offset 4  (left-padded in 32 bytes)
        //   address tokenOut;    // offset 36
        //   uint24  fee;         // offset 68
        //   address recipient;   // offset 100
        //   uint256 amountIn;    // offset 132
        //   ...
        // }
        SEL_V3_EXACT_INPUT_SINGLE => {
            if data.len() < 164 {
                return None;
            }
            let token_in  = parse_address(&data[4..36])?;
            let token_out = parse_address(&data[36..68])?;
            let amount_in = parse_u256(&data[132..164]);
            Some((token_in, token_out, amount_in))
        }

        // ── V2 swapExactTokensForTokens ───────────────────────────────────────
        // (uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline)
        // offset 4:  amountIn     (32 bytes)
        // offset 36: amountOutMin (32 bytes)
        // offset 68: path offset  (32 bytes — ABI dynamic, usually 0xa0 = 5*32)
        // path[0] = tokenIn, path[last] = tokenOut
        SEL_V2_SWAP_EXACT_TOKENS => {
            if data.len() < 196 {
                return None;
            }
            let amount_in = parse_u256(&data[4..36]);
            let path_offset = parse_u256(&data[68..100]).to::<usize>() + 4; // +4 for selector
            if data.len() < path_offset + 64 {
                return None;
            }
            let path_len = parse_u256(&data[path_offset..path_offset + 32]).to::<usize>();
            if path_len < 2 {
                return None;
            }
            let path_data_start = path_offset + 32;
            if data.len() < path_data_start + path_len * 32 {
                return None;
            }
            let token_in  = parse_address(&data[path_data_start..path_data_start + 32])?;
            let token_out_offset = path_data_start + (path_len - 1) * 32;
            let token_out = parse_address(&data[token_out_offset..token_out_offset + 32])?;
            Some((token_in, token_out, amount_in))
        }

        // ── V2 swapExactETHForTokens ──────────────────────────────────────────
        // (uint256 amountOutMin, address[] path, address to, uint256 deadline)
        // amountIn = tx.value (ETH sent with the TX)
        SEL_V2_SWAP_EXACT_ETH => {
            if data.len() < 100 {
                return None;
            }
            let path_offset = parse_u256(&data[36..68]).to::<usize>() + 4;
            if data.len() < path_offset + 64 {
                return None;
            }
            let path_len = parse_u256(&data[path_offset..path_offset + 32]).to::<usize>();
            if path_len < 2 {
                return None;
            }
            let path_data_start = path_offset + 32;
            if data.len() < path_data_start + path_len * 32 {
                return None;
            }
            let token_in  = parse_address(&data[path_data_start..path_data_start + 32])?;
            let token_out_offset = path_data_start + (path_len - 1) * 32;
            let token_out = parse_address(&data[token_out_offset..token_out_offset + 32])?;
            Some((token_in, token_out, eth_value))
        }

        _ => None,
    }
}

fn parse_address(word: &[u8]) -> Option<Address> {
    if word.len() < 32 {
        return None;
    }
    Some(Address::from_slice(&word[12..32]))
}

fn parse_u256(word: &[u8]) -> U256 {
    if word.len() < 32 {
        return U256::ZERO;
    }
    U256::from_be_slice(&word[..32])
}

// ─── Subscription task ────────────────────────────────────────────────────────

/// Spawn a background task that subscribes to pending transactions and sends
/// `PendingSwapEvent`s whenever a large swap is detected targeting a watched router.
///
/// Uses `provider.subscribe_full_pending_transactions()` — the same alloy API
/// pattern as `subscribe_blocks()` / `subscribe_logs()` in mod.rs.
///
/// The task runs indefinitely with exponential-backoff reconnect on error.
/// If the WS endpoint doesn't expose the mempool, it retries silently (no crash).
pub fn spawn_pending_monitor<P: Provider + Clone + 'static>(
    provider: P,
    chain_name: String,
    router_addrs: HashSet<Address>,
    min_swap_amount: u128,
    tx: mpsc::Sender<PendingSwapEvent>,
) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match provider.subscribe_full_pending_transactions().await {
                Ok(sub) => {
                    backoff = Duration::from_secs(1);
                    info!("[{}] Pending TX monitor active ({} routers watched)", chain_name, router_addrs.len());
                    let mut stream = sub.into_stream();

                    while let Some(pending_tx) = stream.next().await {
                        // Access the inner envelope for transaction fields
                        let inner = &pending_tx.inner;

                        // Only care about TXs targeting our watched routers
                        let to_addr = match inner.to() {
                            Some(addr) => addr,
                            None => continue, // contract creation
                        };
                        if !router_addrs.contains(&to_addr) {
                            continue;
                        }

                        let input = inner.input();
                        let eth_value = inner.value();

                        let parsed = parse_swap_calldata(input, eth_value);
                        let (token_in, token_out, amount_in) = match parsed {
                            Some(p) => p,
                            None => continue,
                        };

                        // Filter dust swaps
                        if amount_in < U256::from(min_swap_amount) {
                            continue;
                        }

                        let tip_wei = inner.max_priority_fee_per_gas()
                            .unwrap_or_else(|| inner.gas_price().unwrap_or(0));

                        debug!(
                            "[{}] Pending swap: {:?}→{:?} amount={} tip={}wei router={:?}",
                            chain_name, token_in, token_out, amount_in, tip_wei, to_addr
                        );

                        let event = PendingSwapEvent {
                            token_in,
                            token_out,
                            router: to_addr,
                            amount_in,
                            tip_wei,
                        };

                        // Non-blocking send — if the main loop is busy, drop and continue
                        if tx.try_send(event).is_err() {
                            debug!("[{}] Pending TX channel full — dropping event", chain_name);
                        }
                    }

                    warn!("[{}] Pending TX subscription stream ended — reconnecting in {:?}", chain_name, backoff);
                }
                Err(e) => {
                    if backoff.as_secs() <= 2 {
                        warn!("[{}] Pending TX subscription failed: {} — retrying in {:?}", chain_name, e, backoff);
                    } else {
                        debug!("[{}] Pending TX subscription error: {} — retry in {:?}", chain_name, e, backoff);
                    }
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });
}
