use super::*;
use alloy::network::ReceiptResponse;
use alloy::primitives::B256;
use crate::util::{transport_timeout, RpcFailure, RPC_SEND_TIMEOUT, RPC_SIMULATE_TIMEOUT};

// ─── Helpers ──────────────────────────────────────────────────────────────────

async fn diagnose_and_count_revert<P: Provider>(
    provider: &P,
    metrics: &Metrics,
    chain_name: &str,
    tx_hash: &B256,
    block_number: Option<u64>,
    sell_leg_is_v3: bool,
) -> crate::revert::RevertDiagnosis {
    let diagnosis =
        crate::revert::diagnose_mined_revert(provider, tx_hash, block_number, sell_leg_is_v3).await;
    metrics
        .reverts_on_chain
        .with_label_values(&[chain_name, diagnosis.class.as_str()])
        .inc();
    diagnosis
}

/// Structured receipt line for grep / dashboards: timing, trigger, tip, block position.
fn log_exec_confirm(
    chain: &str,
    route: &str,
    ok: bool,
    receipt: &impl ReceiptResponse,
    trigger: &'static str,
    max_priority_fee_wei: u128,
    exec_start: Instant,
    send_rpc_ms: u64,
    profit_usd: f64,
    opp_id: &str,
    tx_hash: &str,
    revert_class: Option<&str>,
    scan_to_submit_ms: Option<u64>,
    router_a_type: &str,
    router_b_type: &str,
) {
    let block = receipt.block_number();
    let tx_index = receipt.transaction_index();
    let gas_used = receipt.gas_used();
    let eff_gas = receipt.effective_gas_price();
    let total_ms = exec_start.elapsed().as_millis() as u64;
    let msg = format!(
        "[{}] exec_confirm | route={} | ok={} | revert_class={} | scan_to_submit_ms={} | router_a={} | router_b={} | block={:?} | tx_index={:?} | gas_used={} | eff_gas_price_wei={} | total_ms={} | send_rpc_ms={} | trigger={} | tip_wei={} | profit_usd={:.4} | opp={} | tx={}",
        chain,
        route,
        ok,
        revert_class.unwrap_or("-"),
        scan_to_submit_ms.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
        router_a_type,
        router_b_type,
        block,
        tx_index,
        gas_used,
        eff_gas,
        total_ms,
        send_rpc_ms,
        trigger,
        max_priority_fee_wei,
        profit_usd,
        opp_id,
        tx_hash,
    );
    if ok {
        info!("{}", msg);
    } else {
        warn!("{}", msg);
    }
}

/// Dashboard `trade` events mean **included & successful on-chain** (matches block explorer).
fn broadcast_confirmed_trade(
    log_tx: &LogBroadcaster,
    chain_name: &str,
    tx_hash: &str,
    display_id: &str,
    profit_usd: f64,
    scan_to_submit_ms: u64,
    submit_to_confirm_ms: u64,
) {
    broadcast_log(
        log_tx,
        "trade",
        &format!(
            "[{}] Confirmed tx={} | pair={} | profit=${:.4} | scan→submit={}ms | submit→confirm={}ms",
            chain_name, tx_hash, display_id, profit_usd, scan_to_submit_ms, submit_to_confirm_ms
        ),
        Some(serde_json::json!({
            "chain":                 chain_name,
            "pair_id":               display_id,
            "profit_usd":            profit_usd,
            "tx_hash":               tx_hash,
            "confirmed":             true,
            "scan_to_submit_ms":     scan_to_submit_ms,
            "submit_to_confirm_ms":  submit_to_confirm_ms,
            "pipeline_ms":           scan_to_submit_ms,
        })),
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn evaluate_and_execute<P: Provider + Clone + 'static>(
    strategy: &Arc<RwLock<Strategy>>,
    executor: &Arc<Mutex<Executor>>,
    provider: &Arc<P>,
    shared_state: &SharedState,
    log_tx: &LogBroadcaster,
    cfg: &ChainConfig,
    metrics: &Arc<Metrics>,
    pending_pairs: &mut HashSet<String>,
    cooldowns: &mut HashMap<String, Instant>,
    consecutive_failures: &mut u32,
    best_raw_profit: &Arc<AtomicU64>,
    best_spread_bits: &Arc<AtomicU64>,
    best_verified_spread_bits: &Arc<AtomicU64>,
    last_fwd_count: &Arc<AtomicU64>,
    last_multi_count: &Arc<AtomicU64>,
    last_active_count: &Arc<AtomicU64>,
    last_opp_count: &Arc<AtomicU64>,
    contract_balances: &Arc<RwLock<HashMap<Address, U256>>>,
    // None = full scan; Some((pair_mask, token_filter)) = targeted scan from a Swap event.
    targeted: Option<(HashSet<usize>, Vec<Address>)>,
    // Pre-read from shared_state by the caller (alongside the paused check) to avoid
    // an extra shared_state.read().await here on every scan.
    disabled_set: std::sync::Arc<std::collections::HashSet<String>>,
    // When Some, override the executor's priority_fee_wei for this submission.
    // Used by pending TX monitor to match the trigger TX's tip for same-block landing.
    tip_override: Option<u128>,
    // Scan source: block | poll | swap | pending — execution telemetry (see chain/mod.rs call sites).
    trigger: &'static str,
) {
    // Detection phase timer (evaluate + triangular) — shared across all candidates this scan.
    let detect_start = Instant::now();
    // Warn on return (any path) if this scan held the chain loop too long. The heartbeat
    // can't report it: it runs in the same loop, so it never fires mid-scan.
    let _slow_scan_guard = SlowScanGuard { chain: &cfg.name, trigger, start: detect_start };

    let (all_opportunities, optimistic_submission) = {
        let strat = strategy.read().await;
        let optimistic_submission = strat.optimistic_submission;

        // Merge targeted mask with disabled-pairs exclusion mask.
        // Targeted scans filter only the small mask (1–3 pairs) instead of scanning all pairs.
        let final_mask: Option<HashSet<usize>> = if disabled_set.is_empty() {
            targeted.as_ref().map(|(m, _)| m.clone())
        } else {
            match targeted.as_ref().map(|(m, _)| m) {
                None => Some(
                    strat
                        .pairs
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| p.chain_id == cfg.id && !disabled_set.contains(&p.id))
                        .map(|(i, _)| i)
                        .collect(),
                ),
                Some(t) => Some(
                    t.iter()
                        .copied()
                        .filter(|&i| !disabled_set.contains(&strat.pairs[i].id))
                        .collect(),
                ),
            }
        };

        let is_full_scan = targeted.is_none();
        // Targeted scans: 2-hop on pair mask; triangular only on swap/pending tokens (not full O(N³)).
        let token_filter = targeted.as_ref().map(|(_, tokens)| tokens.as_slice());
        let ((opps_2hop, best_2hop, verified_spread_2hop, fwd_ok, multi_dex, spread_2hop, active_pairs), (opps_tri, best_tri)) =
            if is_full_scan {
                tokio::join!(
                    strat.evaluate(provider.as_ref(), final_mask.as_ref(), true),
                    strat.detect_triangular(provider.as_ref(), 5, None, &*disabled_set),
                )
            } else {
                tokio::join!(
                    strat.evaluate(provider.as_ref(), final_mask.as_ref(), false),
                    strat.detect_triangular(provider.as_ref(), 5, token_filter, &*disabled_set),
                )
            };

        // Update quote diagnostic counters only on full scans.
        // Targeted swap-event scans evaluate 1-2 pairs → would make heartbeat show
        // "active=1/13" right before the log fires, hiding real coverage data.
        if is_full_scan {
            last_fwd_count.store(fwd_ok as u64, Ordering::Relaxed);
            last_multi_count.store(multi_dex as u64, Ordering::Relaxed);
            last_active_count.store(active_pairs as u64, Ordering::Relaxed);
        }

        // Update best-seen profit for the heartbeat log
        let best_raw = f64::max(best_2hop, best_tri);
        if best_raw > 0.0 {
            // Keep running maximum (load → compare → store)
            let current = f64::from_bits(best_raw_profit.load(Ordering::Relaxed));
            if best_raw > current {
                best_raw_profit.store(best_raw.to_bits(), Ordering::Relaxed);
            }
        }

        // Update best signed spread % (running maximum; reset each heartbeat)
        if spread_2hop.is_finite() {
            let current_spread = f64::from_bits(best_spread_bits.load(Ordering::Relaxed));
            if spread_2hop > current_spread {
                best_spread_bits.store(spread_2hop.to_bits(), Ordering::Relaxed);
            }
        }
        // Update best Phase 1.5 QuoterV2-verified spread (running maximum; reset each heartbeat)
        if verified_spread_2hop.is_finite() {
            let current_v = f64::from_bits(best_verified_spread_bits.load(Ordering::Relaxed));
            if verified_spread_2hop > current_v {
                best_verified_spread_bits.store(verified_spread_2hop.to_bits(), Ordering::Relaxed);
            }
        }

        let mut merged = Vec::new();
        merged.extend(opps_2hop.into_iter().map(Opportunity::TwoHop));
        merged.extend(opps_tri.into_iter().map(Opportunity::Triangular));

        if merged.len() > 1 {
            merged.sort_by(|a, b| {
                b.profit_usd()
                    .partial_cmp(&a.profit_usd())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        (merged, optimistic_submission)
    };
    let detect_ms = detect_start.elapsed().as_millis() as u64;

    // Track opp count for heartbeat display
    last_opp_count.store(all_opportunities.len() as u64, Ordering::Relaxed);

    // Log all opportunities found this scan so it's visible which pairs are generating them.
    // This is key for diagnosing "single pair always" — it shows whether other pairs have
    // spreads below threshold vs not being scanned at all.
    if all_opportunities.len() > 1 {
        let mut summary = String::with_capacity(all_opportunities.len() * 20);
        for (i, o) in all_opportunities.iter().enumerate() {
            if i > 0 { summary.push_str(", "); }
            let _ = std::fmt::write(&mut summary, format_args!("{}=${:.4}", o.pair_id(), o.profit_usd()));
        }
        info!("[{}] {} opps this scan: {}", cfg.name, all_opportunities.len(), summary);
    }

    if all_opportunities.is_empty() {
        return;
    }

    metrics.opportunities.with_label_values(&[&cfg.name]).inc();

    // Only lock executor when we have candidates — avoids mutex on empty scans.
    let (ghost_profit_bits, rpc_health) = {
        let exec = executor.lock().await;
        (exec.ghost_profit_usd_bits.clone(), exec.rpc_health.clone())
    };

    // Read once before the loop — dry_run doesn't change mid-scan.
    let dry_run = shared_state.read().await.dry_run;

    // Try each opportunity in priority order (profit descending). On gas-rejection or
    // pre-flight failure, fall through to the next candidate. On a successful send or
    // dry-run, break. `continue 'candidates` is used for cheap skips (cooldown, balance
    // guard, detect-only) so higher-ranked blocked pairs don't starve the rest.
    // Chain-level RPC backoff: after an outage-class failure, skip EXECUTION for
    // RPC_BACKOFF instead of having every scan wait out 10–15 s of timeouts on its next
    // candidate. Opportunities are still logged/broadcast. When the backoff expires, a
    // background probe decides whether to resume; execution stays skipped until it does.
    let mut skip_execution = match rpc_health.backoff_state() {
        None => false,
        Some(true) => true,
        Some(false) => {
            spawn_backoff_probe(provider.clone(), rpc_health.clone(), cfg.name.clone());
            true
        }
    };

    // What failures so far this scan call for — see handle_execution_failure.
    let mut failure = FailureAction::default();
    let mut scan_rpc = ScanRpc { sim_timeouts: 0, health: &rpc_health };
    'candidates: for best_opp in &all_opportunities {
        // Stop executing (but keep walking candidates so they're still logged) after an
        // outage (failure, or the executor's gas fetch timing out) or a rate limit.
        if !skip_execution && (failure.stop_scan || rpc_health.backoff_state() == Some(true)) {
            if failure.stop_scan && !failure.outage {
                debug!("[{}] RPC rate limited — not executing further candidates this scan", cfg.name);
            } else {
                warn!("[{}] RPC outage — skipping execution of remaining candidates", cfg.name);
            }
            skip_execution = true;
        }
    let fingerprint = best_opp.fingerprint();
    let display_id = best_opp.pair_id();

    // Cooldown check (deadline style)
    let expire_at_opt = cooldowns.get(&fingerprint).copied();
    if let Some(expire_at) = expire_at_opt {
        if expire_at > Instant::now() {
            let remaining = expire_at.duration_since(Instant::now()).as_secs();
            debug!("[{}] {} in cooldown ({}s remaining), skipping", cfg.name, best_opp.pair_id(), remaining);
            continue 'candidates;
        }
        cooldowns.remove(&fingerprint); // expired entry — clean up
    }

    // Pair-level cooldown: a successful TX was recently sent for this token pair on a
    // different router combo. Block all combos for the same pair until the cooldown clears.
    // Prevents two competing TXs (e.g. UniV3→Aerodrome and PancakeV3→Aerodrome) from
    // firing in the same block when triggered by two rapid Swap events.
    if let Some(&pair_expire) = cooldowns.get(display_id) {
        if pair_expire > Instant::now() {
            let remaining = pair_expire.duration_since(Instant::now()).as_secs();
            debug!("[{}] {} pair in cooldown ({}s remaining), skipping", cfg.name, display_id, remaining);
            continue 'candidates;
        }
        cooldowns.remove(display_id); // expired — clean up
    }

    // Pending tx dedup
    if pending_pairs.contains(&fingerprint) {
        debug!("[{}] {} tx already in-flight, skipping", cfg.name, best_opp.pair_id());
        broadcast_log(log_tx, "info",
            &format!("[{}] Skipped {} — tx already in-flight", cfg.name, best_opp.pair_id()),
            None);
        continue 'candidates;
    }

    // ── SyncSwap detect-only check ────────────────────────────────────────────
    // SyncSwap opportunities are broadcast to the live feed but never sent to
    // the contract (no execution support yet). Continue to next candidate so an
    // executable opportunity below it can still be attempted this scan.

    if !best_opp.is_executable() {
        broadcast_log(
            log_tx,
            "opportunity",
            &format!(
                "[{}] [DETECT-ONLY/SyncSwap] {} | profit=${:.4}",
                cfg.name,
                display_id,
                best_opp.profit_usd(),
            ),
            Some(serde_json::json!({
                "chain":      cfg.name,
                "pair_id":    display_id,
                "profit_usd": best_opp.profit_usd(),
                "detect_only": true,
            })),
        );
        continue 'candidates;
    }

    // ── Pending-only submission gate ──────────────────────────────────────────
    // When pending_only_submission is enabled, block-scan / poll-tick / swap-event
    // triggered calls have tip_override=None. Skip TX for those — log the opportunity
    // so detection is visible, but don't submit (they land at position 500+ on Base).
    // Pending-monitor triggered calls carry Some(tip_override) and always proceed.
    if cfg.pending_only_submission && tip_override.is_none() {
        broadcast_log(
            log_tx,
            "opportunity",
            &format!(
                "[{}] OPP! (pending-only mode, no submit) {} | profit=${:.4}",
                cfg.name, display_id, best_opp.profit_usd(),
            ),
            Some(serde_json::json!({
                "chain":      cfg.name,
                "pair_id":    display_id,
                "profit_usd": best_opp.profit_usd(),
                "detect_only": true,
            })),
        );
        continue 'candidates;
    }

    // ── Dispatch based on opportunity type ────────────────────────────────────
    // Per-candidate timer — excludes time spent on prior failed attempts this scan.
    let candidate_start = Instant::now();

    match best_opp {
        Opportunity::TwoHop(opp) => {
            // Two-round adaptive size optimizer (only for 2-hop)
            let max_bal = {
                let bals = contract_balances.read().await;
                bals.get(&opp.token_in).copied()
            };
            // Skip optimizer in optimistic mode — each round costs ~100ms of QuoterV2 calls.
            // On FCFS chains (Base), 200ms of optimizer latency = ~65 positions lost.
            // Also skipped during an RPC backoff: it makes its own QuoterV2 multicalls, and
            // the candidate won't be executed anyway.
            let optimized = if optimistic_submission || !cfg.optimize_size || skip_execution {
                opp.clone()
            } else {
                strategy.read().await.optimize(opp, provider.as_ref(), max_bal).await
            };

            // Log the opportunity first so detection is always visible.
            broadcast_log(
                log_tx,
                "opportunity",
                &format!(
                    "[{}] 2-hop {} | profit=${:.4} | {}/{}",
                    cfg.name, optimized.pair_id, optimized.profit_usd, optimized.router_a_id, optimized.router_b_id
                ),
                Some(serde_json::json!({
                    "chain":      cfg.name,
                    "pair_id":    optimized.pair_id,
                    "profit_usd": optimized.profit_usd,
                    "router_a":   optimized.router_a_id,
                    "router_b":   optimized.router_b_id,
                })),
            );

            // Guard: cached balance (refreshed every 30s) must cover amount_in.
            // Prevents sending txs that will STF when contract has no token_in.
            if let Some(bal) = max_bal {
                if bal < optimized.amount_in {
                    let msg = format!(
                        "[{}] Skipping {} — insufficient token_in balance (have {}, need {}) → cd={}s",
                        cfg.name, optimized.pair_id, bal, optimized.amount_in, COOLDOWN_SECS
                    );
                    warn!("{}", msg);
                    broadcast_log(log_tx, "warn", &msg, None);
                    cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(COOLDOWN_SECS));
                    continue 'candidates;
                }
            }

            if skip_execution { continue 'candidates; }
            pending_pairs.insert(fingerprint.clone());

            let exec_start = std::time::Instant::now();
            let router_ids = vec![optimized.router_a_id.clone(), optimized.router_b_id.clone()];

            if dry_run {
                // Dry-run: simulate with lock held (test mode, latency not critical)
                let mut exec = executor.lock().await;
                exec.dry_run = true;
                match exec.execute(provider.as_ref(), &optimized).await {
                    Ok(tx_hash) => {
                        let scan_to_submit_ms = detect_ms + candidate_start.elapsed().as_millis() as u64;
                        drop(exec);
                        cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(COOLDOWN_SECS));
                        cooldowns.insert(display_id.to_string(), Instant::now() + Duration::from_secs(COOLDOWN_SECS));
                        rpc_health.note_success();
                        handle_execution_success(
                            &tx_hash, &fingerprint, optimized.profit_usd, &optimized.pair_id,
                            &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                            shared_state, log_tx, metrics, scan_to_submit_ms,
                            COOLDOWN_SECS,
                        ).await;
                        break 'candidates;
                    }
                    Err(e) => {
                        drop(exec);
                        failure |= handle_execution_failure(&mut scan_rpc, 
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns, COOLDOWN_SECS,
                            consecutive_failures, cfg, shared_state, metrics, log_tx,
                            &ghost_profit_bits, optimized.profit_usd,
                        ).await;
                        // fall through: try next candidate
                    }
                }
            } else {
                // Live: prepare (nonce + tx build) with brief lock, release, then send
                // without holding the mutex. Prevents blocking the select! loop for
                // the 500ms–2s it takes send_transaction to complete on Linea.
                let prep_result: Result<TxPrep> = {
                    let mut exec = executor.lock().await;
                    exec.dry_run = false;
                    exec.prepare_2hop(provider.as_ref(), &optimized, tip_override).await
                    // lock drops here — secp256k1 signing happens below, outside the lock
                };
                // Sign for secondary broadcast outside the mutex (~10ms freed from lock hold).
                let prep_result = match prep_result {
                    Ok(p) => Ok(p.sign_if_needed().await),
                    Err(e) => Err(e),
                };
                match prep_result {
                    Err(e) => {
                        failure |= handle_execution_failure(&mut scan_rpc, 
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns, COOLDOWN_SECS,
                            consecutive_failures, cfg, shared_state, metrics, log_tx,
                            &ghost_profit_bits, optimized.profit_usd,
                        ).await;
                    }
                    Ok(prep) => {
                        if cfg.preflight_eth_call {
                            match transport_timeout(RPC_SIMULATE_TIMEOUT, provider.call(prep.tx.clone())).await {
                                // The node answered a simulation: it isn't hanging.
                                Ok(_) => { scan_rpc.sim_timeouts = 0; scan_rpc.health.note_success(); }
                                Err(e) => {
                                    let detail = crate::revert::format_eth_call_transport_error(&e);
                                    warn!(
                                        "[{}] preflight_eth_call revert | pair={} | {}",
                                        cfg.name, optimized.pair_id, detail
                                    );
                                    {
                                        let mut exec = executor.lock().await;
                                        exec.record_failed(false);
                                    }
                                    if let Some(db) = prep.db.clone() {
                                        let cn = prep.chain_name.clone();
                                        let cid = prep.chain_id;
                                        let oid = prep.opp_id.clone();
                                        let ra = prep.router_a.clone();
                                        let rb = prep.router_b.clone();
                                        let p = prep.profit_usd;
                                        tokio::task::spawn_blocking(move || {
                                            let _ = db.insert_trade(cid, &cn, &oid, &ra, &rb, None, p, false, "", false);
                                        });
                                    }
                                    failure |= handle_execution_failure(&mut scan_rpc, 
                                        anyhow::anyhow!("Pre-flight eth_call rejected: {}", detail),
                                        &fingerprint, &router_ids, pending_pairs, cooldowns, SEND_COOLDOWN_SECS,
                                        consecutive_failures, cfg, shared_state, metrics, log_tx,
                                        &ghost_profit_bits, prep.profit_usd,
                                    ).await;
                                    continue 'candidates;
                                }
                            }
                        }
                        // Pre-flight skipped by default: on Base, gas per revert is <$0.02 and adding
                        // an eth_call round-trip (~200ms) closes the opportunity window.
                        // The contract enforces amountIn+minProfit on-chain; reverts are atomic.
                        debug!(
                            "[{}] Submitting | pair={} amountIn={} gross=${:.4} gas=${:.4} net=${:.4} feeA={} feeB={}",
                            cfg.name, optimized.pair_id, optimized.amount_in,
                            prep.profit_usd, prep.gas_cost_usd, prep.net_profit_usd,
                            optimized.fee_a, optimized.fee_b,
                        );
                        // ── Multi-RPC broadcast (fire-and-forget) ─────────────
                        // Send pre-signed raw bytes to secondary HTTP endpoints concurrently
                        // using a shared reqwest::Client connection pool — no TCP/TLS handshake
                        // per transaction after the first submission.
                        if !prep.submission_rpcs.is_empty() {
                            if let Some(raw_bytes) = prep.raw_tx.clone() {
                                let raw_hex = format!("0x{}", alloy::primitives::hex::encode(&raw_bytes));
                                let urls_sec = prep.submission_rpcs.clone();
                                let client = prep.http_client.clone();
                                let cname_sec = cfg.name.clone();
                                tokio::spawn(async move {
                                    let body = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "method": "eth_sendRawTransaction",
                                        "params": [raw_hex],
                                        "id": 1
                                    });
                                    let futs = urls_sec.into_iter().map(|url| {
                                        let b = body.clone();
                                        let c = client.clone();
                                        let cname = cname_sec.clone();
                                        async move {
                                            match c.post(url.as_str()).json(&b).send().await {
                                                Ok(r) => debug!("[{}] secondary RPC: {}", cname, r.status()),
                                                Err(e) => debug!("[{}] secondary RPC error: {}", cname, e),
                                            }
                                        }
                                    });
                                    futures::future::join_all(futs).await;
                                });
                            }
                        }
                        let send_start = std::time::Instant::now();
                        // Use pre-signed raw bytes when available (skips re-signing in WalletFiller).
                        let send_result = if let Some(ref raw) = prep.raw_tx {
                            transport_timeout(RPC_SEND_TIMEOUT, provider.send_raw_transaction(raw)).await
                        } else {
                            transport_timeout(RPC_SEND_TIMEOUT, provider.send_transaction(prep.tx.clone())).await
                        };
                        match send_result {
                            Ok(pending) => {
                                let tx_hash = format!("{:?}", pending.tx_hash());
                                let elapsed_send = send_start.elapsed().as_millis() as u64;
                                let scan_to_submit_ms = detect_ms + candidate_start.elapsed().as_millis() as u64;
                                let submit_at = Instant::now();
                                let exec_ms = exec_start.elapsed().as_millis() as u64;
                                let tip_w = tip_override.unwrap_or(cfg.priority_fee_wei);
                                info!(
                                    "[{}] exec_send | route=2hop | scan_to_submit_ms={} | detect_ms={} | exec_ms={} | send_rpc_ms={} | trigger={} | tip_wei={} | gross_usd={:.4} | net_usd={:.4} | pair={} | tx={}",
                                    cfg.name, scan_to_submit_ms, detect_ms, exec_ms, elapsed_send, trigger, tip_w,
                                    prep.profit_usd, prep.net_profit_usd, optimized.pair_id,
                                    &tx_hash[..10.min(tx_hash.len())],
                                );
                                { let mut exec = executor.lock().await; exec.record_sent(&tx_hash, elapsed_send); }
                                cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(SEND_COOLDOWN_SECS));
                                cooldowns.insert(display_id.to_string(), Instant::now() + Duration::from_secs(SEND_COOLDOWN_SECS));

                                // Fire-and-forget receipt task using cloned prep fields
                                let tx_hash_bg = tx_hash.clone();
                                let tx_hash_b256 = pending.tx_hash().clone();
                                let provider_bg = provider.clone();
                                let ra_type_bg = crate::revert::router_type_label(&optimized.router_a_type);
                                let rb_type_bg = crate::revert::router_type_label(&optimized.router_b_type);
                                let sell_leg_v3_bg = matches!(optimized.router_b_type, RouterType::V3);
                                let confirmed_success_bg = prep.confirmed_success.clone();
                                let confirmed_failed_bg = prep.confirmed_failed.clone();
                                let confirmed_profit_bg = prep.confirmed_profit_bits.clone();
                                let chain_name_bg = prep.chain_name.clone();
                                let db_bg = prep.db.clone();
                                let chain_id_bg = prep.chain_id;
                                let opp_id_bg = prep.opp_id.clone();
                                let router_a_bg = prep.router_a.clone();
                                let router_b_bg = prep.router_b.clone();
                                let profit_bg = prep.profit_usd;
                                let contract_addr_bg = prep.contract_addr;
                                let contract_balances_bg = prep.contract_balances.clone();
                                let detected_profit_bg = prep.profit_usd;
                                let trigger_bg = trigger;
                                let tip_bg = tip_override.unwrap_or(cfg.priority_fee_wei);
                                let exec_start_bg = exec_start;
                                let send_rpc_bg = elapsed_send;
                                let scan_to_submit_bg = scan_to_submit_ms;
                                let submit_at_bg = submit_at;
                                let log_tx_bg = log_tx.clone();
                                let metrics_bg = metrics.clone();
                                tokio::spawn(async move {
                                    let receipt_result = tokio::time::timeout(
                                        std::time::Duration::from_secs(300),
                                        pending.get_receipt(),
                                    ).await;
                                    let receipt_result = match receipt_result {
                                        Ok(r) => r,
                                        Err(_) => {
                                            warn!("[{}] Receipt wait timed out after 300s for tx {}", chain_name_bg, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                                            return;
                                        }
                                    };
                                    match receipt_result {
                                        Ok(receipt) => {
                                            let ok = receipt.status();
                                            let submit_to_confirm_ms = submit_at_bg.elapsed().as_millis() as u64;
                                            let block_num = receipt.block_number();
                                            let revert_class_str: Option<&str>;
                                            let revert_detail: Option<String>;
                                            if ok {
                                                revert_class_str = None;
                                                revert_detail = None;
                                            } else {
                                                let diagnosis = diagnose_and_count_revert(
                                                    provider_bg.as_ref(),
                                                    &metrics_bg,
                                                    &chain_name_bg,
                                                    &tx_hash_b256,
                                                    block_num,
                                                    sell_leg_v3_bg,
                                                )
                                                .await;
                                                revert_class_str = Some(diagnosis.class.as_str());
                                                revert_detail = diagnosis.detail;
                                            }
                                            log_exec_confirm(
                                                &chain_name_bg,
                                                "2hop",
                                                ok,
                                                &receipt,
                                                trigger_bg,
                                                tip_bg,
                                                exec_start_bg,
                                                send_rpc_bg,
                                                detected_profit_bg,
                                                &opp_id_bg,
                                                &tx_hash_bg,
                                                revert_class_str,
                                                Some(scan_to_submit_bg),
                                                ra_type_bg,
                                                rb_type_bg,
                                            );
                                            if ok {
                                                confirmed_success_bg.fetch_add(1, Ordering::Relaxed);
                                                confirmed_profit_bg.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                                                    Some((f64::from_bits(bits) + profit_bg).to_bits())
                                                }).ok();
                                                metrics_bg
                                                    .profit_usd
                                                    .with_label_values(&[&chain_name_bg])
                                                    .add(profit_bg);
                                                broadcast_confirmed_trade(
                                                    &log_tx_bg,
                                                    &chain_name_bg,
                                                    &tx_hash_bg,
                                                    &opp_id_bg,
                                                    profit_bg,
                                                    scan_to_submit_bg,
                                                    submit_to_confirm_ms,
                                                );
                                                if let Some(bals) = contract_balances_bg {
                                                    let token_addrs: Vec<Address> = { let b = bals.read().await; b.keys().copied().collect() };
                                                    // Fetch all balances concurrently, then write-lock once.
                                                    let results = futures::future::join_all(
                                                        token_addrs.iter().map(|&token| {
                                                            let p = provider_bg.clone();
                                                            async move {
                                                                (token, IERC20::new(token, &p).balanceOf(contract_addr_bg).call().await.ok())
                                                            }
                                                        })
                                                    ).await;
                                                    let mut b = bals.write().await;
                                                    for (token, bal_opt) in results {
                                                        if let Some(bal) = bal_opt { b.insert(token, bal); }
                                                    }
                                                }
                                            } else {
                                                confirmed_failed_bg.fetch_add(1, Ordering::Relaxed);
                                                broadcast_log(
                                                    &log_tx_bg,
                                                    "warn",
                                                    &format!(
                                                        "[{}] tx reverted on-chain | revert_class={} | pair={} | profit=${:.4} | scan_to_submit_ms={} | tx={}",
                                                        chain_name_bg,
                                                        revert_class_str.unwrap_or("unknown"),
                                                        opp_id_bg,
                                                        profit_bg,
                                                        scan_to_submit_bg,
                                                        tx_hash_bg,
                                                    ),
                                                    Some(serde_json::json!({
                                                        "chain":               chain_name_bg,
                                                        "pair_id":             opp_id_bg,
                                                        "profit_usd":          profit_bg,
                                                        "tx_hash":             tx_hash_bg,
                                                        "success":             false,
                                                        "revert_class":        revert_class_str,
                                                        "revert_detail":       revert_detail,
                                                        "scan_to_submit_ms":   scan_to_submit_bg,
                                                        "router_a_type":       ra_type_bg,
                                                        "router_b_type":       rb_type_bg,
                                                    })),
                                                );
                                            }
                                            if let Some(db) = db_bg {
                                                let success = receipt.status();
                                                let revert_db = revert_class_str.map(String::from);
                                                let _ = tokio::task::spawn_blocking(move || {
                                                    let _ = db.insert_trade_extended(
                                                        chain_id_bg,
                                                        &chain_name_bg,
                                                        &opp_id_bg,
                                                        &router_a_bg,
                                                        &router_b_bg,
                                                        None,
                                                        profit_bg,
                                                        success,
                                                        &tx_hash_bg,
                                                        false,
                                                        revert_db.as_deref(),
                                                        Some(scan_to_submit_bg),
                                                    );
                                                }).await;
                                            }
                                        }
                                        Err(e) => {
                                            confirmed_failed_bg.fetch_add(1, Ordering::Relaxed);
                                            warn!("[{}] Receipt error for {}: {}", chain_name_bg, &tx_hash_bg[..10.min(tx_hash_bg.len())], e);
                                        }
                                    }
                                    drop(provider_bg);
                                });

                                rpc_health.note_success();
                                handle_execution_success(
                                    &tx_hash, &fingerprint, optimized.profit_usd, &optimized.pair_id,
                                    &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                                    shared_state, log_tx, metrics, scan_to_submit_ms,
                                    SEND_COOLDOWN_SECS,
                                ).await;
                                break 'candidates;
                            }
                            Err(e) => {
                                warn!(
                                    "[{}] Send failed | pair={} | {}{}",
                                    cfg.name, display_id, e,
                                    if e.to_string().contains(crate::util::RPC_OUTCOME_UNKNOWN_MARKER) {
                                        " — tx may still land; nonce will be re-fetched"
                                    } else { "" },
                                );
                                {
                                    let err_str = e.to_string().to_lowercase();
                                    // "outcome unknown" = send timed out: the tx may have been
                                    // broadcast, so never reuse its nonce.
                                    // A send that died on a reset/closed connection may also
                                    // have reached the node — same treatment.
                                    let rpc = crate::util::classify_rpc_failure(&err_str);
                                    let nonce_consumed = err_str.contains("nonce too low")
                                        || rpc == RpcFailure::Unreachable
                                        || err_str.contains(crate::util::RPC_OUTCOME_UNKNOWN_MARKER)
                                        || err_str.contains("already known")
                                        || err_str.contains("replacement transaction");
                                    let mut exec = executor.lock().await;
                                    exec.record_failed(nonce_consumed);
                                    // Only re-fetch nonce from chain when it was consumed by
                                    // another TX. For network/gas failures the pre-incremented
                                    // nonce was reclaimed by record_failed — no RPC needed.
                                    // Not after an RPC-class failure: the node is hanging or down,
                                    // and nonce=None already makes the next send re-fetch it.
                                    if nonce_consumed && rpc == RpcFailure::None {
                                        exec.prefetch_nonce(provider.as_ref()).await;
                                    }
                                }
                                if let Some(db) = prep.db.clone() {
                                    let cn = prep.chain_name.clone();
                                    let cid = prep.chain_id;
                                    let oid = prep.opp_id.clone();
                                    let ra = prep.router_a.clone();
                                    let rb = prep.router_b.clone();
                                    let p = prep.profit_usd;
                                    tokio::task::spawn_blocking(move || {
                                        let _ = db.insert_trade(cid, &cn, &oid, &ra, &rb, None, p, false, "", false);
                                    });
                                }
                                failure |= handle_execution_failure(&mut scan_rpc, 
                                    anyhow::anyhow!("Send failed: {}", e),
                                    &fingerprint, &router_ids, pending_pairs, cooldowns, SEND_COOLDOWN_SECS,
                                    consecutive_failures, cfg, shared_state, metrics, log_tx,
                                    &ghost_profit_bits, prep.profit_usd,
                                ).await;
                                // fall through: try next candidate
                            }
                        }
                    }
                }
            }
        }

        Opportunity::Triangular(opp) => {
            // Log the opportunity first so detection is always visible.
            broadcast_log(
                log_tx,
                "opportunity",
                &format!(
                    "[{}] triangular {} | profit=${:.4} | {}/{}/{}",
                    cfg.name, opp.triplet_id, opp.profit_usd, opp.router_ab_id, opp.router_bc_id, opp.router_ca_id
                ),
                Some(serde_json::json!({
                    "chain":        cfg.name,
                    "pair_id":      opp.triplet_id,
                    "triplet_id":   opp.triplet_id,
                    "profit_usd":   opp.profit_usd,
                    "router_ab":    opp.router_ab_id,
                    "router_bc":    opp.router_bc_id,
                    "router_ca":    opp.router_ca_id,
                })),
            );

            // Guard: check cached token_a balance (the starting capital) before executing.
            // Triangular arb starting with an unfunded token (e.g. WETH) will STF on Leg 1.
            {
                let bal = {
                    let bals = contract_balances.read().await;
                    bals.get(&opp.token_a).copied()
                };
                if let Some(b) = bal {
                    if b < opp.amount_in {
                        let msg = format!(
                            "[{}] Skipping triangular {} — insufficient token_a balance (have {}, need {}) → cd={}s",
                            cfg.name, opp.triplet_id, b, opp.amount_in, COOLDOWN_SECS
                        );
                        warn!("{}", msg);
                        broadcast_log(log_tx, "warn", &msg, None);
                        cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(COOLDOWN_SECS));
                        continue 'candidates;
                    }
                }
            }

            if skip_execution { continue 'candidates; }
            pending_pairs.insert(fingerprint.clone());

            let exec_start = std::time::Instant::now();
            let router_ids = vec![
                opp.router_ab_id.clone(),
                opp.router_bc_id.clone(),
                opp.router_ca_id.clone(),
            ];

            if dry_run {
                // Dry-run: simulate with lock held (test mode, latency not critical)
                let mut exec = executor.lock().await;
                exec.dry_run = true;
                match exec.execute_triangular(provider.as_ref(), opp).await {
                    Ok(tx_hash) => {
                        let scan_to_submit_ms = detect_ms + candidate_start.elapsed().as_millis() as u64;
                        drop(exec);
                        cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(COOLDOWN_SECS));
                        cooldowns.insert(display_id.to_string(), Instant::now() + Duration::from_secs(COOLDOWN_SECS));
                        rpc_health.note_success();
                        handle_execution_success(
                            &tx_hash, &fingerprint, opp.profit_usd, &opp.triplet_id,
                            &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                            shared_state, log_tx, metrics, scan_to_submit_ms,
                            COOLDOWN_SECS,
                        ).await;
                        break 'candidates;
                    }
                    Err(e) => {
                        drop(exec);
                        failure |= handle_execution_failure(&mut scan_rpc, 
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns, COOLDOWN_SECS,
                            consecutive_failures, cfg, shared_state, metrics, log_tx,
                            &ghost_profit_bits, opp.profit_usd,
                        ).await;
                        // fall through: try next candidate
                    }
                }
            } else {
                // Live: prepare with brief lock, release, then send without holding mutex
                let prep_result: Result<TxPrep> = {
                    let mut exec = executor.lock().await;
                    exec.dry_run = false;
                    exec.prepare_triangular(provider.as_ref(), opp, tip_override).await
                    // lock drops here — secp256k1 signing happens below, outside the lock
                };
                let prep_result = match prep_result {
                    Ok(p) => Ok(p.sign_if_needed().await),
                    Err(e) => Err(e),
                };
                match prep_result {
                    Err(e) => {
                        failure |= handle_execution_failure(&mut scan_rpc, 
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns, COOLDOWN_SECS,
                            consecutive_failures, cfg, shared_state, metrics, log_tx,
                            &ghost_profit_bits, opp.profit_usd,
                        ).await;
                    }
                    Ok(prep) => {
                        if cfg.preflight_eth_call {
                            match transport_timeout(RPC_SIMULATE_TIMEOUT, provider.call(prep.tx.clone())).await {
                                // The node answered a simulation: it isn't hanging.
                                Ok(_) => { scan_rpc.sim_timeouts = 0; scan_rpc.health.note_success(); }
                                Err(e) => {
                                    let detail = crate::revert::format_eth_call_transport_error(&e);
                                    warn!(
                                        "[{}] preflight_eth_call revert | triplet={} | {}",
                                        cfg.name, opp.triplet_id, detail
                                    );
                                    {
                                        let mut exec = executor.lock().await;
                                        exec.record_failed(false);
                                    }
                                    if let Some(db) = prep.db.clone() {
                                        let cn = prep.chain_name.clone();
                                        let cid = prep.chain_id;
                                        let oid = prep.opp_id.clone();
                                        let ra = prep.router_a.clone();
                                        let rb = prep.router_b.clone();
                                        let rc = prep.router_c.clone();
                                        let p = prep.profit_usd;
                                        tokio::task::spawn_blocking(move || {
                                            let _ = db.insert_trade(cid, &cn, &oid, &ra, &rb, rc.as_deref(), p, false, "", false);
                                        });
                                    }
                                    failure |= handle_execution_failure(&mut scan_rpc, 
                                        anyhow::anyhow!("Pre-flight eth_call rejected: {}", detail),
                                        &fingerprint, &router_ids, pending_pairs, cooldowns, SEND_COOLDOWN_SECS,
                                        consecutive_failures, cfg, shared_state, metrics, log_tx,
                                        &ghost_profit_bits, prep.profit_usd,
                                    ).await;
                                    continue 'candidates;
                                }
                            }
                        }
                        // Pre-flight skipped by default: same reasoning as 2-hop path above.
                        if !prep.submission_rpcs.is_empty() {
                            if let Some(raw_bytes) = prep.raw_tx.clone() {
                                let raw_hex = format!("0x{}", alloy::primitives::hex::encode(&raw_bytes));
                                let urls_sec = prep.submission_rpcs.clone();
                                let client = prep.http_client.clone();
                                let cname_sec = cfg.name.clone();
                                tokio::spawn(async move {
                                    let body = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "method": "eth_sendRawTransaction",
                                        "params": [raw_hex],
                                        "id": 1
                                    });
                                    let futs = urls_sec.into_iter().map(|url| {
                                        let b = body.clone();
                                        let c = client.clone();
                                        let cname = cname_sec.clone();
                                        async move {
                                            match c.post(url.as_str()).json(&b).send().await {
                                                Ok(r) => debug!("[{}] secondary RPC: {}", cname, r.status()),
                                                Err(e) => debug!("[{}] secondary RPC error: {}", cname, e),
                                            }
                                        }
                                    });
                                    futures::future::join_all(futs).await;
                                });
                            }
                        }
                        let send_start = std::time::Instant::now();
                        let send_result = if let Some(ref raw) = prep.raw_tx {
                            transport_timeout(RPC_SEND_TIMEOUT, provider.send_raw_transaction(raw)).await
                        } else {
                            transport_timeout(RPC_SEND_TIMEOUT, provider.send_transaction(prep.tx.clone())).await
                        };
                        match send_result {
                            Ok(pending) => {
                                let tx_hash = format!("{:?}", pending.tx_hash());
                                let elapsed_send = send_start.elapsed().as_millis() as u64;
                                let scan_to_submit_ms = detect_ms + candidate_start.elapsed().as_millis() as u64;
                                let submit_at = Instant::now();
                                let exec_ms = exec_start.elapsed().as_millis() as u64;
                                let tip_w = tip_override.unwrap_or(cfg.priority_fee_wei);
                                info!(
                                    "[{}] exec_send | route=triangular | scan_to_submit_ms={} | detect_ms={} | exec_ms={} | send_rpc_ms={} | trigger={} | tip_wei={} | gross_usd={:.4} | net_usd={:.4} | triplet={} | tx={}",
                                    cfg.name, scan_to_submit_ms, detect_ms, exec_ms, elapsed_send, trigger, tip_w,
                                    prep.profit_usd, prep.net_profit_usd, opp.triplet_id,
                                    &tx_hash[..10.min(tx_hash.len())],
                                );
                                { let mut exec = executor.lock().await; exec.record_sent(&tx_hash, elapsed_send); }
                                cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(SEND_COOLDOWN_SECS));
                                cooldowns.insert(display_id.to_string(), Instant::now() + Duration::from_secs(SEND_COOLDOWN_SECS));

                                // Fire-and-forget receipt task
                                let tx_hash_bg = tx_hash.clone();
                                let tx_hash_b256 = pending.tx_hash().clone();
                                let provider_bg = provider.clone();
                                let ra_type_bg = crate::revert::router_type_label(&opp.router_ab_type);
                                let rb_type_bg = crate::revert::router_type_label(&opp.router_ca_type);
                                let sell_leg_v3_bg = crate::revert::triangular_sell_leg_is_v3(opp);
                                let confirmed_success_bg = prep.confirmed_success.clone();
                                let confirmed_failed_bg = prep.confirmed_failed.clone();
                                let confirmed_profit_bg = prep.confirmed_profit_bits.clone();
                                let chain_name_bg = prep.chain_name.clone();
                                let db_bg = prep.db.clone();
                                let chain_id_bg = prep.chain_id;
                                let opp_id_bg = prep.opp_id.clone();
                                let router_a_bg = prep.router_a.clone();
                                let router_b_bg = prep.router_b.clone();
                                let router_c_bg = prep.router_c.clone();
                                let profit_bg = prep.profit_usd;
                                let contract_addr_bg = prep.contract_addr;
                                let contract_balances_bg = prep.contract_balances.clone();
                                let detected_profit_bg = prep.profit_usd;
                                let trigger_bg = trigger;
                                let tip_bg = tip_override.unwrap_or(cfg.priority_fee_wei);
                                let exec_start_bg = exec_start;
                                let send_rpc_bg = elapsed_send;
                                let scan_to_submit_bg = scan_to_submit_ms;
                                let submit_at_bg = submit_at;
                                let log_tx_bg = log_tx.clone();
                                let metrics_bg = metrics.clone();
                                tokio::spawn(async move {
                                    let receipt_result = tokio::time::timeout(
                                        std::time::Duration::from_secs(300),
                                        pending.get_receipt(),
                                    ).await;
                                    let receipt_result = match receipt_result {
                                        Ok(r) => r,
                                        Err(_) => {
                                            warn!("[{}] Receipt wait timed out after 300s for tx {}", chain_name_bg, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                                            return;
                                        }
                                    };
                                    match receipt_result {
                                        Ok(receipt) => {
                                            let ok = receipt.status();
                                            let submit_to_confirm_ms = submit_at_bg.elapsed().as_millis() as u64;
                                            let block_num = receipt.block_number();
                                            let revert_class_str: Option<&str>;
                                            let revert_detail: Option<String>;
                                            if ok {
                                                revert_class_str = None;
                                                revert_detail = None;
                                            } else {
                                                let diagnosis = diagnose_and_count_revert(
                                                    provider_bg.as_ref(),
                                                    &metrics_bg,
                                                    &chain_name_bg,
                                                    &tx_hash_b256,
                                                    block_num,
                                                    sell_leg_v3_bg,
                                                )
                                                .await;
                                                revert_class_str = Some(diagnosis.class.as_str());
                                                revert_detail = diagnosis.detail;
                                            }
                                            log_exec_confirm(
                                                &chain_name_bg,
                                                "triangular",
                                                ok,
                                                &receipt,
                                                trigger_bg,
                                                tip_bg,
                                                exec_start_bg,
                                                send_rpc_bg,
                                                detected_profit_bg,
                                                &opp_id_bg,
                                                &tx_hash_bg,
                                                revert_class_str,
                                                Some(scan_to_submit_bg),
                                                ra_type_bg,
                                                rb_type_bg,
                                            );
                                            if ok {
                                                confirmed_success_bg.fetch_add(1, Ordering::Relaxed);
                                                confirmed_profit_bg.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                                                    Some((f64::from_bits(bits) + profit_bg).to_bits())
                                                }).ok();
                                                metrics_bg
                                                    .profit_usd
                                                    .with_label_values(&[&chain_name_bg])
                                                    .add(profit_bg);
                                                broadcast_confirmed_trade(
                                                    &log_tx_bg,
                                                    &chain_name_bg,
                                                    &tx_hash_bg,
                                                    &opp_id_bg,
                                                    profit_bg,
                                                    scan_to_submit_bg,
                                                    submit_to_confirm_ms,
                                                );
                                                if let Some(bals) = contract_balances_bg {
                                                    let token_addrs: Vec<Address> = { let b = bals.read().await; b.keys().copied().collect() };
                                                    let results = futures::future::join_all(
                                                        token_addrs.iter().map(|&token| {
                                                            let p = provider_bg.clone();
                                                            async move {
                                                                (token, IERC20::new(token, &p).balanceOf(contract_addr_bg).call().await.ok())
                                                            }
                                                        })
                                                    ).await;
                                                    let mut b = bals.write().await;
                                                    for (token, bal_opt) in results {
                                                        if let Some(bal) = bal_opt { b.insert(token, bal); }
                                                    }
                                                }
                                            } else {
                                                confirmed_failed_bg.fetch_add(1, Ordering::Relaxed);
                                                broadcast_log(
                                                    &log_tx_bg,
                                                    "warn",
                                                    &format!(
                                                        "[{}] triangular tx reverted on-chain | revert_class={} | pair={} | profit=${:.4} | scan_to_submit_ms={} | tx={}",
                                                        chain_name_bg,
                                                        revert_class_str.unwrap_or("unknown"),
                                                        opp_id_bg,
                                                        profit_bg,
                                                        scan_to_submit_bg,
                                                        tx_hash_bg,
                                                    ),
                                                    Some(serde_json::json!({
                                                        "chain":               chain_name_bg,
                                                        "pair_id":             opp_id_bg,
                                                        "profit_usd":          profit_bg,
                                                        "tx_hash":             tx_hash_bg,
                                                        "success":             false,
                                                        "revert_class":        revert_class_str,
                                                        "revert_detail":       revert_detail,
                                                        "scan_to_submit_ms":   scan_to_submit_bg,
                                                        "router_a_type":       ra_type_bg,
                                                        "router_b_type":       rb_type_bg,
                                                    })),
                                                );
                                            }
                                            if let Some(db) = db_bg {
                                                let success = receipt.status();
                                                let revert_db = revert_class_str.map(String::from);
                                                let _ = tokio::task::spawn_blocking(move || {
                                                    let _ = db.insert_trade_extended(
                                                        chain_id_bg,
                                                        &chain_name_bg,
                                                        &opp_id_bg,
                                                        &router_a_bg,
                                                        &router_b_bg,
                                                        router_c_bg.as_deref(),
                                                        profit_bg,
                                                        success,
                                                        &tx_hash_bg,
                                                        false,
                                                        revert_db.as_deref(),
                                                        Some(scan_to_submit_bg),
                                                    );
                                                }).await;
                                            }
                                        }
                                        Err(e) => {
                                            confirmed_failed_bg.fetch_add(1, Ordering::Relaxed);
                                            warn!("[{}] Triangular receipt error for {}: {}", chain_name_bg, &tx_hash_bg[..10.min(tx_hash_bg.len())], e);
                                        }
                                    }
                                    drop(provider_bg);
                                });

                                rpc_health.note_success();
                                handle_execution_success(
                                    &tx_hash, &fingerprint, opp.profit_usd, &opp.triplet_id,
                                    &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                                    shared_state, log_tx, metrics, scan_to_submit_ms,
                                    SEND_COOLDOWN_SECS,
                                ).await;
                                break 'candidates;
                            }
                            Err(e) => {
                                warn!(
                                    "[{}] Send failed | pair={} | {}{}",
                                    cfg.name, display_id, e,
                                    if e.to_string().contains(crate::util::RPC_OUTCOME_UNKNOWN_MARKER) {
                                        " — tx may still land; nonce will be re-fetched"
                                    } else { "" },
                                );
                                {
                                    let err_str = e.to_string().to_lowercase();
                                    // "outcome unknown" = send timed out: the tx may have been
                                    // broadcast, so never reuse its nonce.
                                    // A send that died on a reset/closed connection may also
                                    // have reached the node — same treatment.
                                    let rpc = crate::util::classify_rpc_failure(&err_str);
                                    let nonce_consumed = err_str.contains("nonce too low")
                                        || rpc == RpcFailure::Unreachable
                                        || err_str.contains(crate::util::RPC_OUTCOME_UNKNOWN_MARKER)
                                        || err_str.contains("already known")
                                        || err_str.contains("replacement transaction");
                                    let mut exec = executor.lock().await;
                                    exec.record_failed(nonce_consumed);
                                    // Not after an RPC-class failure: the node is hanging or down,
                                    // and nonce=None already makes the next send re-fetch it.
                                    if nonce_consumed && rpc == RpcFailure::None {
                                        exec.prefetch_nonce(provider.as_ref()).await;
                                    }
                                }
                                if let Some(db) = prep.db.clone() {
                                    let cn = prep.chain_name.clone();
                                    let cid = prep.chain_id;
                                    let oid = prep.opp_id.clone();
                                    let ra = prep.router_a.clone();
                                    let rb = prep.router_b.clone();
                                    let rc = prep.router_c.clone();
                                    let p = prep.profit_usd;
                                    tokio::task::spawn_blocking(move || {
                                        let _ = db.insert_trade(cid, &cn, &oid, &ra, &rb, rc.as_deref(), p, false, "", false);
                                    });
                                }
                                failure |= handle_execution_failure(&mut scan_rpc, 
                                    anyhow::anyhow!("Triangular send failed: {}", e),
                                    &fingerprint, &router_ids, pending_pairs, cooldowns, SEND_COOLDOWN_SECS,
                                    consecutive_failures, cfg, shared_state, metrics, log_tx,
                                    &ghost_profit_bits, prep.profit_usd,
                                ).await;
                                // fall through: try next candidate
                            }
                        }
                    }
                }
            }
        }
    } // end match best_opp
    } // end 'candidates: for

    if failure.outage {
        rpc_health.backoff_start();
        warn!("[{}] Backing off execution for {:?} (repeated outages back off longer)", cfg.name, rpc_health.current_backoff_len());
    }

}

/// Probe an RPC whose backoff has expired, off the scan loop. The backoff is held first,
/// so later scans keep skipping execution (and don't spawn more probes) while it runs.
///
/// The probe is an eth_call (Multicall3.getBlockNumber) — the same RPC method simulations
/// use, so it sees the same node-side queueing — tried up to twice so one jittery answer
/// doesn't hold the backoff. Healthy = answered within [`crate::util::RPC_PROBE_HEALTHY`]
/// (a revert counts as an answer), or rate-limited (the node is up; per-pair cooldowns
/// handle 429s). The probe only resolves the backoff it started: if a pause cleared it or
/// a failure restarted it meanwhile, the result is dropped.
fn spawn_backoff_probe<P: Provider + Clone + 'static>(
    provider: Arc<P>,
    rpc_health: crate::util::SharedRpcHealth,
    chain: String,
) {
    let started = rpc_health.backoff_hold();
    tokio::spawn(async move {
        const MULTICALL3: Address = alloy::primitives::address!("cA11bde05977b3631167028862bE2a173976CA11");
        const GET_BLOCK_NUMBER: [u8; 4] = [0x42, 0xcb, 0xb1, 0x5c]; // getBlockNumber()
        let mut last = (false, Duration::ZERO, None::<String>);
        for _ in 0..2 {
            let tx = alloy::rpc::types::TransactionRequest::default()
                .to(MULTICALL3)
                .input(alloy::primitives::Bytes::from_static(&GET_BLOCK_NUMBER).into());
            let t = Instant::now();
            let result = crate::util::rpc_timeout(crate::util::RPC_READ_TIMEOUT, provider.call(tx)).await;
            let took = t.elapsed();
            let answered = match &result {
                Ok(_) => true,
                Err(e) => crate::util::node_answered(e) || crate::util::classify_rpc_failure(e) == RpcFailure::RateLimited,
            };
            last = (answered && took <= crate::util::RPC_PROBE_HEALTHY, took, result.err());
            if last.0 { break; }
        }
        let (healthy, took, err) = last;
        if !rpc_health.backoff_resolve(started, healthy) {
            return; // paused/resumed or restarted meanwhile — stale result
        }
        if healthy {
            info!("[{}] RPC answering again ({:.1}s) — resuming execution", chain, took.as_secs_f64());
        } else if err.is_none() || err.as_deref().is_some_and(crate::util::node_answered) {
            warn!("[{}] RPC answering but slow ({:.1}s) — skipping execution for {:?}",
                  chain, took.as_secs_f64(), rpc_health.current_backoff_len());
        } else {
            warn!("[{}] RPC still not answering — skipping execution for {:?}", chain, rpc_health.current_backoff_len());
        }
    });
}

/// Per-scan RPC failure state passed to [`handle_execution_failure`].
pub(crate) struct ScanRpc<'a> {
    /// Consecutive simulation timeouts (two in a row = the node is hanging).
    sim_timeouts: u32,
    /// The chain's RPC health (rate-limit log throttle), borrowed from the scan.
    health: &'a crate::util::RpcHealth,
}

/// What a failed candidate calls for. Merged across a scan's candidates with `|=`.
#[derive(Default, Clone, Copy)]
pub(crate) struct FailureAction {
    /// Don't execute further candidates this scan (outage or rate limit).
    stop_scan: bool,
    /// The node is hanging or down: start the chain-level backoff.
    outage: bool,
}

impl std::ops::BitOrAssign for FailureAction {
    fn bitor_assign(&mut self, o: Self) {
        self.stop_scan |= o.stop_scan;
        self.outage |= o.outage;
    }
}

/// Scan duration above which [`SlowScanGuard`] warns (normal scans take < 100 ms).
const SLOW_SCAN_WARN: Duration = Duration::from_secs(10);

/// Logs a warning when dropped if the scan ran longer than [`SLOW_SCAN_WARN`].
struct SlowScanGuard<'a> {
    chain: &'a str,
    trigger: &'a str,
    start: Instant,
}

impl Drop for SlowScanGuard<'_> {
    fn drop(&mut self) {
        let took = self.start.elapsed();
        if took > SLOW_SCAN_WARN {
            warn!("[{}] Slow scan: {} scan held the chain loop for {:.1}s — RPC slow or down?",
                  self.chain, self.trigger, took.as_secs_f64());
        }
    }
}

/// Handle successful execution (both 2-hop and triangular).
pub(crate) async fn handle_execution_success(
    tx_hash: &str,
    pair_id: &str,
    profit_usd: f64,
    display_id: &str,
    router_ids: &[String],
    pending_pairs: &mut HashSet<String>,
    consecutive_failures: &mut u32,
    dry_run: bool,
    cfg: &ChainConfig,
    shared_state: &SharedState,
    log_tx: &LogBroadcaster,
    metrics: &Arc<Metrics>,
    scan_to_submit_ms: u64,
    cooldown_secs: u64,
) {
    *consecutive_failures = 0;
    pending_pairs.remove(pair_id);

    let dry_label = if dry_run { "true" } else { "false" };
    metrics.executed.with_label_values(&[&cfg.name, dry_label]).inc();
    // Live `arb_profit_usd_total` and dashboard TRDE lines are updated only after a successful receipt.

    // total_attempts = tx sent; total_success is updated in the heartbeat
    // from executor.confirmed_success (set after receipt confirms on-chain).
    let mut state = shared_state.write().await;
    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
        chain.total_attempts += 1;
    }

    if dry_run {
        broadcast_log(
            log_tx,
            "trade",
            &format!(
                "[{}] DRY-RUN ok | simulated tx={} | pair={} | profit=${:.4} | scan→submit={}ms",
                cfg.name, tx_hash, display_id, profit_usd, scan_to_submit_ms,
            ),
            Some(serde_json::json!({
                "chain":              cfg.name,
                "pair_id":            display_id,
                "profit_usd":         profit_usd,
                "tx_hash":            tx_hash,
                "dry_run":            true,
                "scan_to_submit_ms":  scan_to_submit_ms,
                "pipeline_ms":        scan_to_submit_ms,
            })),
        );
    } else {
        broadcast_log(
            log_tx,
            "info",
            &format!(
                "[{}] Submitted (pending) tx={} | pair={} | profit=${:.4} | scan→submit={}ms → cd={}s — awaiting receipt",
                cfg.name, tx_hash, display_id, profit_usd, scan_to_submit_ms, cooldown_secs,
            ),
            Some(serde_json::json!({
                "chain":                  cfg.name,
                "pair_id":                display_id,
                "profit_usd":             profit_usd,
                "tx_hash":                tx_hash,
                "pending_confirmation":   true,
                "scan_to_submit_ms":      scan_to_submit_ms,
                "pipeline_ms":            scan_to_submit_ms,
            })),
        );
    }
}

/// Handle execution failure (both 2-hop and triangular).
pub(crate) async fn handle_execution_failure(
    scan_rpc: &mut ScanRpc<'_>,
    error: anyhow::Error,
    pair_id: &str,
    router_ids: &[String],
    pending_pairs: &mut HashSet<String>,
    cooldowns: &mut HashMap<String, Instant>,
    cooldown_secs: u64,
    consecutive_failures: &mut u32,
    cfg: &ChainConfig,
    shared_state: &SharedState,
    metrics: &Arc<Metrics>,
    log_tx: &LogBroadcaster,
    ghost_profit_bits: &Arc<AtomicU64>,
    opportunity_profit_usd: f64,
) -> FailureAction {
    pending_pairs.remove(pair_id);

    // Gas-profitability rejects: route has a real spread but profit < gas cost.
    // Apply a much longer cooldown (GAS_REJECT_COOLDOWN_SECS=120s) so the same
    // route doesn't spam the log every 15s. Gas prices are stable on L2s, and the
    // spread is unlikely to change in 15s — there's no value in retrying sooner.
    let err_str = error.to_string();
    // Simulation / pre-flight failures, matched on the engine's own message prefixes.
    let is_sim_failure = crate::util::is_sim_failure(&err_str);
    let rpc = crate::util::classify_rpc_failure(&err_str);
    // One simulation timeout = that candidate is stale. Two in a row = the node is
    // hanging (in dry-run / pre-flight mode the simulation is the only RPC call). Any
    // answer from the node resets the streak: a simulation or send it rejected (e.g.
    // "nonce too low"); a passed pre-flight resets it at the call site.
    // Local rejects (gas, net profit, paused) never reach the node and don't count.
    // "Answered" means a JSON-RPC error from the node itself — not a gateway 502/503 or a
    // local signer/filler failure, which say nothing about whether the node is hanging.
    if is_sim_failure && rpc == RpcFailure::Timeout {
        scan_rpc.sim_timeouts += 1;
    } else if rpc == RpcFailure::None && crate::util::node_answered(&err_str) {
        scan_rpc.sim_timeouts = 0;
    }
    // Node hanging or down: the caller stops executing further candidates and backs off,
    // since each would hit the same wall (seen: a 150 s outage held the scan loop for the
    // whole outage, 10–15 s per candidate). A rate limit is often one heavy call's burst,
    // so it only cools down this pair (below) rather than pausing the whole chain.
    let outage = match rpc {
        RpcFailure::Unreachable => true,
        RpcFailure::Timeout => !is_sim_failure || scan_rpc.sim_timeouts >= 2,
        RpcFailure::RateLimited | RpcFailure::None => false,
    };
    // A rate limit (simulation included) also stops this scan's remaining candidates:
    // more calls now just prolong the limit. It doesn't start the chain backoff.
    let action = FailureAction { outage, stop_scan: outage || rpc == RpcFailure::RateLimited };
    let is_gas_reject = err_str.contains("below threshold") || err_str.contains("negative after gas");
    let effective_cooldown = if is_gas_reject { GAS_REJECT_COOLDOWN_SECS } else { cooldown_secs };
    cooldowns.insert(pair_id.to_string(), Instant::now() + Duration::from_secs(effective_cooldown));

    if is_gas_reject {
        // Accumulate ghost profit: gross USD that was left on the table due to gas cost.
        ghost_profit_bits.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
            Some((f64::from_bits(bits) + opportunity_profit_usd).to_bits())
        }).ok();
        debug!("[{}] Skipped (unprofitable after gas): {}", cfg.name, err_str);
        broadcast_log(log_tx, "info",
            &format!("[{}] Skipped {} — unprofitable after gas (gross=${:.4}, cooldown={}s)", cfg.name, pair_id, opportunity_profit_usd, GAS_REJECT_COOLDOWN_SECS),
            None);
        return action;
    }

    // RPC rate-limits and outages (timeout / unreachable) are infra issues, not strategy
    // failures. Don't increment the circuit-breaker counter — just log and cool down.
    // Counting them would pause the chain (manual resume) after a few minutes' outage.
    if rpc == RpcFailure::RateLimited {
        // Can hit every candidate on every scan while it lasts: report at most once per
        // RATE_LIMIT_LOG_EVERY per chain, the rest at debug.
        debug!("[{}] Execute failed (rate limited): {} | pair={}", cfg.name, error, pair_id);
        if scan_rpc.health.rate_limit_log_due() {
            warn!("[{}] RPC rate limiting — affected pairs cool down {}s (further reports muted {:?})",
                  cfg.name, effective_cooldown, crate::util::RATE_LIMIT_LOG_EVERY);
            broadcast_log(log_tx, "warn",
                &format!("[{}] RPC rate limited: {} → cd={}s", cfg.name, error, effective_cooldown), None);
        }
        return action;
    }

    // Outages: logged at most once per backoff, since the scan stops executing after this.
    if rpc != RpcFailure::None && !is_sim_failure {
        warn!("[{}] Execute failed (RPC {:?}): {} | pair={} → cd={}s", cfg.name, rpc, error, pair_id, effective_cooldown);
        broadcast_log(
            log_tx,
            "error",
            &format!("[{}] Execute failed: {} → cd={}s", cfg.name, error, effective_cooldown),
            None,
        );
        return action;
    }

    // Pre-flight simulation failures are expected — the opportunity closed between
    // detection and execution. Don't count toward circuit breaker; this is working
    // as intended. Only tx SEND failures indicate systemic problems (nonce, wallet).
    if is_sim_failure {
        broadcast_log(
            log_tx,
            "warn",
            &format!("[{}] Pre-flight failed (skipping): {} → cd={}s", cfg.name, error, effective_cooldown),
            None,
        );
        return action;
    }

    *consecutive_failures += 1;

    metrics.failures.with_label_values(&[&cfg.name]).inc();

    let mut state = shared_state.write().await;
    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
        chain.total_attempts += 1;
    }

    // Circuit breaker
    if *consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
        warn!(
            "[{}] Circuit breaker: {} consecutive failures — pausing chain",
            cfg.name, consecutive_failures
        );
        if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
            chain.paused = true;
        }
        broadcast_log(
            log_tx,
            "error",
            &format!(
                "[{}] Circuit breaker triggered ({} failures) — chain paused. Resume via API.",
                cfg.name, consecutive_failures
            ),
            None,
        );
    }

    broadcast_log(
        log_tx,
        "error",
        &format!("[{}] Execute failed: {} → cd={}s", cfg.name, error, effective_cooldown),
        None,
    );
    action
}
