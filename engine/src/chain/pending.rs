use alloy::consensus::Transaction as TxTrait;
use alloy::network::Ethereum;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::transports::ws::WsConnect;
use futures::StreamExt;
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
    /// Contract address the pending TX targets (router or aggregator).
    #[allow(dead_code)]
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
            if token_in == token_out {
                return None;
            }
            let amount_in = parse_u256(&data[132..164]);
            Some((token_in, token_out, amount_in))
        }

        // ── V2 swapExactTokensForTokens ───────────────────────────────────────
        // (uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline)
        // offset 4:  amountIn     (32 bytes)
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
            if path_len < 2 || path_len > 100 {
                return None;
            }
            let path_data_start = path_offset + 32;
            if data.len() < path_data_start + path_len * 32 {
                return None;
            }
            let token_in  = parse_address(&data[path_data_start..path_data_start + 32])?;
            let token_out_offset = path_data_start + (path_len - 1) * 32;
            let token_out = parse_address(&data[token_out_offset..token_out_offset + 32])?;
            if token_in == token_out {
                return None;
            }
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
            if path_len < 2 || path_len > 100 {
                return None;
            }
            let path_data_start = path_offset + 32;
            if data.len() < path_data_start + path_len * 32 {
                return None;
            }
            let token_in  = parse_address(&data[path_data_start..path_data_start + 32])?;
            let token_out_offset = path_data_start + (path_len - 1) * 32;
            let token_out = parse_address(&data[token_out_offset..token_out_offset + 32])?;
            if token_in == token_out {
                return None;
            }
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

/// Spawn a background task that connects to WS endpoints and subscribes to
/// pending transactions.
///
/// **No router filter**: The selector check in `parse_swap_calldata` is the sole
/// filter. This is intentional — many large swaps route through aggregators
/// (Uniswap Universal Router, 1inch, etc.) that are not in the DEX router list.
/// Filtering by `to` address would silently drop all aggregator-routed swaps.
/// The `pairs_for_tokens` check in the main loop gates which swaps actually
/// trigger a scan.
///
/// **Important**: `wss://mainnet.base.org` does NOT expose the mempool on OP Stack.
/// This task creates its OWN WS connections by cycling through `ws_urls`.
/// Alchemy (`wss://base-mainnet.g.alchemy.com/v2/KEY`) is the recommended source.
///
/// The task runs indefinitely with exponential-backoff reconnect on error.
pub fn spawn_pending_monitor(
    chain_name: String,
    ws_urls: Vec<String>,
    min_swap_amount: u128,
    tx: mpsc::Sender<PendingSwapEvent>,
) {
    if ws_urls.is_empty() {
        warn!("[{}] Pending TX monitor: no WS URLs configured — disabled", chain_name);
        return;
    }

    tokio::spawn(async move {
        let mut url_idx = 0usize;
        let mut backoff = Duration::from_secs(1);
        let mut events_received: u64 = 0;

        loop {
            let url = &ws_urls[url_idx % ws_urls.len()];
            let short_url = url.split('?').next().unwrap_or(url);
            info!("[{}] Pending TX monitor: connecting to {}", chain_name, short_url);

            let connect_result = ProviderBuilder::<_, _, Ethereum>::new()
                .connect_ws(WsConnect::new(url.clone()))
                .await;

            match connect_result {
                Err(e) => {
                    warn!("[{}] Pending TX monitor: WS connect failed ({}): {} — next URL", chain_name, short_url, e);
                    url_idx += 1;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
                Ok(provider) => {
                    match provider.subscribe_full_pending_transactions().await {
                        Err(e) => {
                            warn!("[{}] Pending TX monitor: subscription failed on {}: {} — trying next URL", chain_name, short_url, e);
                            url_idx += 1;
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(60));
                            continue;
                        }
                        Ok(sub) => {
                            backoff = Duration::from_secs(1);
                            info!("[{}] Pending TX monitor active on {} (no router filter — selector match only)", chain_name, short_url);

                            let mut stream = sub.into_stream();
                            let mut stream_events: u64 = 0;
                            let mut matched_events: u64 = 0;

                            while let Some(pending_tx) = stream.next().await {
                                stream_events += 1;

                                // Periodic heartbeat every 10,000 TXs so we can verify
                                // the monitor is alive and diagnose match rate.
                                if stream_events % 10_000 == 0 {
                                    info!(
                                        "[{}] Pending TX monitor heartbeat: seen={} matched={} total_fired={}",
                                        chain_name, stream_events, matched_events, events_received
                                    );
                                }

                                // Access the inner envelope for transaction fields
                                let inner = &pending_tx.inner;

                                // Skip contract deployments (no `to` address)
                                let to_addr = match inner.to() {
                                    Some(addr) => addr,
                                    None => continue,
                                };

                                let input = inner.input();
                                let eth_value = inner.value();

                                let (token_in, token_out, amount_in) = match parse_swap_calldata(input, eth_value) {
                                    Some(p) => p,
                                    None => continue,
                                };

                                // Filter dust swaps
                                if amount_in < U256::from(min_swap_amount) {
                                    continue;
                                }

                                let tip_wei = inner.max_priority_fee_per_gas()
                                    .unwrap_or_else(|| inner.gas_price().unwrap_or(0));

                                matched_events += 1;
                                events_received += 1;

                                info!(
                                    "[{}] Pending swap detected: {:?}→{:?} amount={} tip={}wei target={:?} (seen={} matched={})",
                                    chain_name, token_in, token_out, amount_in, tip_wei, to_addr,
                                    stream_events, matched_events
                                );

                                let event = PendingSwapEvent {
                                    token_in,
                                    token_out,
                                    router: to_addr,
                                    amount_in,
                                    tip_wei,
                                };

                                if tx.try_send(event).is_err() {
                                    debug!("[{}] Pending TX channel full — dropping event", chain_name);
                                }
                            }

                            // Stream ended — log how many events we received on this connection
                            if stream_events == 0 {
                                warn!(
                                    "[{}] Pending TX monitor: {} returned 0 events — node may not expose mempool. \
                                    Configure Alchemy WS in ws_rpc or ws_rpc_fallbacks for pending TX access.",
                                    chain_name, short_url
                                );
                                url_idx += 1;
                            } else {
                                info!("[{}] Pending TX monitor stream ended (seen={} matched={}), reconnecting...", chain_name, stream_events, matched_events);
                            }

                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                        }
                    }
                }
            }
        }
    });
}
