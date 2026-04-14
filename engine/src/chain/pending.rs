use alloy::consensus::Transaction as TxTrait;
use alloy::network::Ethereum;
use alloy::primitives::{Address, Bytes, TxHash, U256};
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
/// Uniswap UniversalRouter execute(bytes commands, bytes[] inputs, uint256 deadline)
/// Most large Base swaps route through this (aggregates V2+V3, handles permit2).
const SEL_UNIVERSAL_ROUTER: [u8; 4] = [0x35, 0x93, 0x56, 0x4c];

// UniversalRouter command bytes (first command in the commands array)
const CMD_V3_SWAP_EXACT_IN: u8 = 0x00;
const CMD_V2_SWAP_EXACT_IN: u8 = 0x08;

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
/// - `0x3593564c` UniversalRouter execute — V3_SWAP_EXACT_IN (cmd 0x00) and V2_SWAP_EXACT_IN (cmd 0x08)
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

        // ── Uniswap UniversalRouter execute ───────────────────────────────────
        // execute(bytes commands, bytes[] inputs, uint256 deadline)
        //
        // ABI layout (all offsets from byte 0 of calldata including selector):
        //   [4..36]   commands_offset  (uint256) — offset to commands blob from byte 4
        //   [36..68]  inputs_offset    (uint256) — offset to inputs array from byte 4
        //   [68..100] deadline         (uint256)
        //
        // commands blob (at data[4 + commands_offset]):
        //   [0..32]   length           (uint256)
        //   [32..]    command bytes    (one byte per command)
        //
        // inputs array (at data[4 + inputs_offset]):
        //   [0..32]   array_length     (uint256)
        //   [32..]    element offsets  (one uint256 per element, relative to inputs base)
        //   then each element: [0..32] bytes_length, [32..] bytes_data
        //
        // We only handle the first command and only for single-hop swaps.
        // Multi-command TXs return None (safe fallback).
        SEL_UNIVERSAL_ROUTER => {
            if data.len() < 100 {
                return None;
            }
            // Offsets are relative to byte 4 (after selector)
            let commands_offset = parse_u256(&data[4..36]).to::<usize>();
            let inputs_offset   = parse_u256(&data[36..68]).to::<usize>();

            // commands blob
            let cmd_blob_start = 4 + commands_offset;
            if data.len() < cmd_blob_start + 33 {
                return None;
            }
            let cmd_len = parse_u256(&data[cmd_blob_start..cmd_blob_start + 32]).to::<usize>();
            if cmd_len == 0 || cmd_len > 16 {
                return None; // sanity: skip huge or empty command arrays
            }
            let first_cmd = data[cmd_blob_start + 32] & 0x3f; // mask off flag bits (top 2)

            // inputs array base
            let inputs_base = 4 + inputs_offset;
            if data.len() < inputs_base + 32 {
                return None;
            }
            let inputs_len = parse_u256(&data[inputs_base..inputs_base + 32]).to::<usize>();
            if inputs_len == 0 {
                return None;
            }
            // ABI spec: array element offsets are measured from byte 0 of the array
            // encoding (the length word at inputs_base). With N=1, the header is
            // 64 bytes (32 length + 32 offset), so typical elem0_offset = 0x40 (64).
            // elem0_start = inputs_base + elem0_offset is correct per the ABI spec.
            if data.len() < inputs_base + 64 {
                return None;
            }
            let elem0_offset = parse_u256(&data[inputs_base + 32..inputs_base + 64]).to::<usize>();
            let elem0_start  = inputs_base + elem0_offset;
            if data.len() < elem0_start + 32 {
                return None;
            }
            let elem0_len = parse_u256(&data[elem0_start..elem0_start + 32]).to::<usize>();
            let elem0_data_start = elem0_start + 32;
            if data.len() < elem0_data_start + elem0_len {
                return None;
            }
            let elem0 = &data[elem0_data_start..elem0_data_start + elem0_len];

            match first_cmd {
                // V3_SWAP_EXACT_IN: abi.encode(address recipient, uint256 amountIn,
                //   uint256 amountOutMin, bytes path, bool payerIsUser)
                // path = abi.encodePacked(tokenIn[20], fee[3], tokenOut[20]) for single hop
                CMD_V3_SWAP_EXACT_IN => {
                    // elem0 ABI layout:
                    //   [0..32]   recipient
                    //   [32..64]  amountIn
                    //   [64..96]  amountOutMin
                    //   [96..128] path_offset (relative to elem0 start)
                    //   [128..160] payerIsUser
                    if elem0.len() < 160 {
                        return None;
                    }
                    let amount_in  = parse_u256(&elem0[32..64]);
                    let path_off   = parse_u256(&elem0[96..128]).to::<usize>();
                    if elem0.len() < path_off + 32 {
                        return None;
                    }
                    let path_len = parse_u256(&elem0[path_off..path_off + 32]).to::<usize>();
                    let path_data_start = path_off + 32;
                    // Single-hop path = 20 + 3 + 20 = 43 bytes
                    if path_len < 43 || elem0.len() < path_data_start + path_len {
                        return None;
                    }
                    let path = &elem0[path_data_start..path_data_start + path_len];
                    let token_in  = Address::from_slice(&path[0..20]);
                    let token_out = Address::from_slice(&path[path_len - 20..path_len]);
                    if token_in == token_out {
                        return None;
                    }
                    Some((token_in, token_out, amount_in))
                }

                // V2_SWAP_EXACT_IN: abi.encode(address recipient, uint256 amountIn,
                //   uint256 amountOutMin, address[] path, bool payerIsUser)
                CMD_V2_SWAP_EXACT_IN => {
                    // elem0 ABI layout:
                    //   [0..32]   recipient
                    //   [32..64]  amountIn
                    //   [64..96]  amountOutMin
                    //   [96..128] path_offset (relative to elem0 start)
                    //   [128..160] payerIsUser
                    if elem0.len() < 160 {
                        return None;
                    }
                    let amount_in = parse_u256(&elem0[32..64]);
                    let path_off  = parse_u256(&elem0[96..128]).to::<usize>();
                    if elem0.len() < path_off + 32 {
                        return None;
                    }
                    let path_len = parse_u256(&elem0[path_off..path_off + 32]).to::<usize>();
                    if path_len < 2 || path_len > 100 {
                        return None;
                    }
                    let path_data_start = path_off + 32;
                    if elem0.len() < path_data_start + path_len * 32 {
                        return None;
                    }
                    let token_in  = parse_address(&elem0[path_data_start..path_data_start + 32])?;
                    let token_out_off = path_data_start + (path_len - 1) * 32;
                    let token_out = parse_address(&elem0[token_out_off..token_out_off + 32])?;
                    if token_in == token_out {
                        return None;
                    }
                    Some((token_in, token_out, amount_in))
                }

                _ => None, // multi-hop, WRAP_ETH, SWEEP, etc — ignore safely
            }
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

// ─── Shared tx processor ──────────────────────────────────────────────────────

/// Extract a `PendingSwapEvent` from a resolved transaction's raw fields.
/// Returns `None` if the tx is not a recognised swap or is below `min_swap_amount`.
fn try_extract_event(
    to_addr: Option<Address>,
    input: &Bytes,
    eth_value: U256,
    tip_wei: u128,
    min_swap_amount: u128,
) -> Option<PendingSwapEvent> {
    let router = to_addr?; // skip contract deployments
    let (token_in, token_out, amount_in) = parse_swap_calldata(input, eth_value)?;
    if amount_in < U256::from(min_swap_amount) {
        return None;
    }
    Some(PendingSwapEvent { token_in, token_out, router, amount_in, tip_wei })
}

// ─── Subscription task ────────────────────────────────────────────────────────

/// Spawn a background task that connects to WS endpoints and subscribes to
/// pending transactions.
///
/// **Subscription strategy (tried in order per URL):**
/// 1. `eth_subscribe("newPendingTransactions", true)` — full transaction bodies.
///    Supported by private nodes and some paid RPC tiers (e.g. Alchemy Growth+).
/// 2. Hash-only fallback: `eth_subscribe("newPendingTransactions")` + `eth_getTransactionByHash`
///    per hash. Works on Alchemy free tier and any node that exposes the mempool.
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

            let provider = match connect_result {
                Err(e) => {
                    warn!("[{}] Pending TX monitor: WS connect failed ({}): {} — next URL", chain_name, short_url, e);
                    url_idx += 1;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
                Ok(p) => p,
            };

            // ── Strategy 1: full pending transactions ─────────────────────────
            // Sends eth_subscribe("newPendingTransactions", true).
            // Returns full tx bodies — zero extra RPC calls needed.
            // Supported by private/paid nodes (Alchemy Growth+, QuickNode, etc.).
            match provider.subscribe_full_pending_transactions().await {
                Ok(sub) => {
                    backoff = Duration::from_secs(1);
                    info!("[{}] Pending TX monitor ACTIVE (full-tx mode) on {}", chain_name, short_url);

                    let mut stream = sub.into_stream();
                    let mut stream_events: u64 = 0;
                    let mut matched_events: u64 = 0;

                    while let Some(pending_tx) = stream.next().await {
                        stream_events += 1;

                        if stream_events % 10_000 == 0 {
                            info!(
                                "[{}] Pending TX monitor heartbeat (full): seen={} matched={} total_fired={}",
                                chain_name, stream_events, matched_events, events_received
                            );
                        }

                        let inner = &pending_tx.inner;
                        let tip_wei = inner.max_priority_fee_per_gas()
                            .unwrap_or_else(|| inner.gas_price().unwrap_or(0));

                        if let Some(event) = try_extract_event(
                            inner.to(),
                            inner.input(),
                            inner.value(),
                            tip_wei,
                            min_swap_amount,
                        ) {
                            matched_events += 1;
                            events_received += 1;
                            info!(
                                "[{}] Pending swap (full): {:?}→{:?} amount={} tip={}wei (seen={} matched={})",
                                chain_name, event.token_in, event.token_out,
                                event.amount_in, tip_wei, stream_events, matched_events
                            );
                            if tx.try_send(event).is_err() {
                                debug!("[{}] Pending TX channel full — dropping event", chain_name);
                            }
                        }
                    }

                    if stream_events == 0 {
                        warn!(
                            "[{}] Pending TX monitor (full): {} returned 0 events — \
                            node may not support full pending TX subscription. \
                            Trying hash-only fallback next.",
                            chain_name, short_url
                        );
                        // Don't advance url_idx — try hash-only on the same URL next iteration
                    } else {
                        info!(
                            "[{}] Pending TX monitor stream ended (full, seen={} matched={}), reconnecting...",
                            chain_name, stream_events, matched_events
                        );
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    // Fall through to retry loop — next attempt tries hash-only on same URL
                    // because we don't advance url_idx when full-tx returns 0 events.
                    // Once hash-only is tried and also fails, url_idx advances.
                }

                Err(full_err) => {
                    // ── Strategy 2: hash-only + resolve ──────────────────────
                    // eth_subscribe("newPendingTransactions") returns tx hashes.
                    // Works on Alchemy free tier and most nodes with mempool access.
                    // Cost: one eth_getTransactionByHash per pending tx seen.
                    warn!(
                        "[{}] Pending TX monitor: full-tx subscription failed on {} ({}), \
                        trying hash-only fallback",
                        chain_name, short_url, full_err
                    );

                    match provider.subscribe_pending_transactions().await {
                        Err(hash_err) => {
                            warn!(
                                "[{}] Pending TX monitor: hash-only subscription also failed on {}: {} — \
                                node does not expose the mempool. Trying next URL.",
                                chain_name, short_url, hash_err
                            );
                            url_idx += 1;
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(60));
                        }

                        Ok(hash_sub) => {
                            backoff = Duration::from_secs(1);
                            info!(
                                "[{}] Pending TX monitor ACTIVE (hash-only mode) on {} — \
                                resolving each hash via eth_getTransactionByHash",
                                chain_name, short_url
                            );

                            let mut stream = hash_sub.into_stream();
                            let mut stream_events: u64 = 0;
                            let mut matched_events: u64 = 0;

                            while let Some(hash) = stream.next().await {
                                stream_events += 1;

                                if stream_events % 10_000 == 0 {
                                    info!(
                                        "[{}] Pending TX monitor heartbeat (hash): seen={} matched={} total_fired={}",
                                        chain_name, stream_events, matched_events, events_received
                                    );
                                }

                                let hash: TxHash = hash;
                                let resolved = match provider.get_transaction_by_hash(hash).await {
                                    Ok(Some(tx_body)) => tx_body,
                                    Ok(None) => continue, // tx already confirmed / dropped
                                    Err(_) => continue,   // RPC error — skip this tx
                                };

                                let inner = &resolved.inner;
                                let tip_wei = inner.max_priority_fee_per_gas()
                                    .unwrap_or_else(|| inner.gas_price().unwrap_or(0));

                                if let Some(event) = try_extract_event(
                                    inner.to(),
                                    inner.input(),
                                    inner.value(),
                                    tip_wei,
                                    min_swap_amount,
                                ) {
                                    matched_events += 1;
                                    events_received += 1;
                                    info!(
                                        "[{}] Pending swap (hash): {:?}→{:?} amount={} tip={}wei (seen={} matched={})",
                                        chain_name, event.token_in, event.token_out,
                                        event.amount_in, tip_wei, stream_events, matched_events
                                    );
                                    if tx.try_send(event).is_err() {
                                        debug!("[{}] Pending TX channel full — dropping event", chain_name);
                                    }
                                }
                            }

                            if stream_events == 0 {
                                warn!(
                                    "[{}] Pending TX monitor (hash): {} returned 0 events — \
                                    node does not expose the mempool. Trying next URL.",
                                    chain_name, short_url
                                );
                                url_idx += 1;
                            } else {
                                info!(
                                    "[{}] Pending TX monitor stream ended (hash, seen={} matched={}), reconnecting...",
                                    chain_name, stream_events, matched_events
                                );
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
