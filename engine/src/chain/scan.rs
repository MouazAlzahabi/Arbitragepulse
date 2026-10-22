use super::*;
use alloy::network::ReceiptResponse;
use alloy::primitives::B256;
use crate::util::{transport_timeout, RpcFailure, rpc_send_timeout};

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// A mined revert of a class that repeats on every send (`RevertClass::pauses_chain`): pause
/// the chain until the operator fixes it and calls POST /engine/resume.
async fn pause_chain_for_revert(
    shared_state: &SharedState,
    chain_id: u64,
    chain_name: &str,
    log_tx: &LogBroadcaster,
    class: &str,
    detail: Option<&str>,
) {
    {
        let mut st = shared_state.write().await;
        match st.chains.iter_mut().find(|c| c.chain_id == chain_id) {
            Some(c) if !c.paused => c.paused = true,
            _ => return, // already paused (another receipt got here first) or unknown chain
        }
    }
    let msg = format!(
        "[{}] Chain paused: tx reverted on-chain with `{}`{} — it will fail the same way on every send (contract paused, signer not the owner, or router not allowlisted). Fix it, then POST /engine/resume.",
        chain_name, class, detail.map(|d| format!(" ({d})")).unwrap_or_default(),
    );
    warn!("{}", msg);
    broadcast_log(log_tx, "error", &msg, None);
}

/// `get_receipt` failed (e.g. the node saw the tx mined but its receipt lookup returned
/// null on a lagging backend, or the WS reconnected): wait briefly and look it up once more.
async fn requery_receipt<P: Provider>(
    provider: &P,
    tx_hash: B256,
    first_err: String,
) -> Result<alloy::rpc::types::TransactionReceipt, String> {
    tokio::time::sleep(Duration::from_millis(1500)).await;
    match crate::util::rpc_timeout(crate::util::rpc_read_timeout(), provider.get_transaction_receipt(tx_hash)).await {
        Ok(Some(r)) => Ok(r),
        Ok(None) => Err(format!("{first_err}; still no receipt on re-query")),
        Err(e) => Err(format!("{first_err}; re-query: {e}")),
    }
}

/// Receipt still unknown after the re-query: the tx may have succeeded, so it's reported
/// as unknown (dashboard warning), not counted as failed.
fn warn_receipt_unknown(log_tx: &LogBroadcaster, chain_name: &str, tx_hash: &str, err: &str) {
    let msg = format!("[{}] receipt lookup failed — outcome unknown | tx={} | {}", chain_name, tx_hash, err);
    warn!("{}", msg);
    broadcast_log(log_tx, "warn", &msg, None);
}

async fn diagnose_and_count_revert<P: Provider>(
    provider: &P,
    metrics: &Metrics,
    chain_name: &str,
    tx_hash: &B256,
    block_number: Option<u64>,
    gas_used: u64,
    gas_limit: u64,
) -> crate::revert::RevertDiagnosis {
    let diagnosis =
        crate::revert::diagnose_mined_revert(provider, tx_hash, block_number, gas_used, gas_limit).await;
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
                        .filter(|(_, p)| !disabled_set.contains(&p.id))
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
    // While the chain's RPC is degraded (see util::RpcHealth), skip EXECUTION instead of
    // having every scan wait out 10–15 s of timeouts on its next candidate. Opportunities
    // are still logged/broadcast. The chain's monitor task decides when to resume.
    let mut skip_execution = rpc_health.is_degraded();

    // What failures so far this scan call for — see handle_execution_failure.
    let mut failure = FailureAction::default();
    let mut scan_rpc = ScanRpc { sim_timeouts: 0, health: &rpc_health };
    'candidates: for best_opp in &all_opportunities {
        // Stop executing (but keep walking candidates so they're still logged) after an
        // outage (failure, or the executor's gas fetch timing out) or a rate limit.
        if !skip_execution && (failure.stop_scan || rpc_health.is_degraded()) {
            if failure.stop_scan && !failure.outage {
                debug!("[{}] RPC rate limited — not executing further candidates this scan", cfg.name);
            } else {
                warn!("[{}] RPC outage — skipping execution of remaining candidates", cfg.name);
            }
            skip_execution = true;
        }
    let fingerprint = best_opp.fingerprint();
    let display_id = best_opp.pair_id();
    // Route was rejected as unprofitable after gas within gas_reject_cooldown_secs: still
    // evaluated and attempted (the edge may have grown), but its opportunity log goes to debug.
    let gas_log_throttled = cooldowns
        .get(&Opportunity::gas_log_key(&fingerprint))
        .is_some_and(|t| *t > Instant::now());

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
                let strat = strategy.read().await;
                match strat.preconf_quote_provider() {
                    Some(pc) => strat.optimize(opp, &pc, max_bal).await,
                    None => strat.optimize(opp, provider.as_ref(), max_bal).await,
                }
            };

            // Log the opportunity first so detection is always visible.
            if gas_log_throttled {
                debug!(
                    "[{}] 2-hop {} | profit=${:.4} | {}/{} (recently below gas)",
                    cfg.name, optimized.pair_id, optimized.profit_usd, optimized.router_a_id, optimized.router_b_id
                );
            } else {
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
            }

            // Guard: cached balance (refreshed every 30s) must cover amount_in.
            // Prevents sending txs that will STF when contract has no token_in.
            if let Some(bal) = max_bal {
                if bal < optimized.amount_in {
                    let msg = format!(
                        "[{}] Skipping {} — insufficient token_in balance (have {}, need {}) → cd={}s",
                        cfg.name, optimized.pair_id, bal, optimized.amount_in, cooldown_secs()
                    );
                    warn!("{}", msg);
                    broadcast_log(log_tx, "warn", &msg, None);
                    cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(cooldown_secs()));
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
                        cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(cooldown_secs()));
                        cooldowns.insert(display_id.to_string(), Instant::now() + Duration::from_secs(cooldown_secs()));
                        handle_execution_success(
                            &tx_hash, &fingerprint, optimized.profit_usd, &optimized.pair_id,
                            &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                            shared_state, log_tx, metrics, scan_to_submit_ms,
                            cooldown_secs(),
                        ).await;
                        break 'candidates;
                    }
                    Err(e) => {
                        drop(exec);
                        failure |= handle_execution_failure(&mut scan_rpc, 
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns, cooldown_secs(),
                            consecutive_failures, cfg, shared_state, metrics, log_tx,
                            &ghost_profit_bits, optimized.profit_usd,
                        ).await;
                        // fall through: try next candidate
                    }
                }
            } else {
                // Live: prepare (nonce + tx build) with brief lock, release, then send
                // without holding the mutex. Prevents blocking the select! loop for
                // the 500ms–2s a slow send_transaction can take.
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
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns, cooldown_secs(),
                            consecutive_failures, cfg, shared_state, metrics, log_tx,
                            &ghost_profit_bits, optimized.profit_usd,
                        ).await;
                    }
                    Ok(prep) => {
                        if cfg.preflight_eth_call {
                            match crate::executor::simulate_call(provider.as_ref(), prep.sim_provider.as_ref(), prep.tx.clone()).await {
                                Ok(_) => {}
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
                                        crate::revert::eth_call_error("Pre-flight eth_call rejected", &e),
                                        &fingerprint, &router_ids, pending_pairs, cooldowns, send_cooldown_secs(),
                                        consecutive_failures, cfg, shared_state, metrics, log_tx,
                                        &ghost_profit_bits, prep.profit_usd,
                                    ).await;
                                    continue 'candidates;
                                }
                            }
                        }
                        // Pre-flight skipped by default: on Base, gas per revert is <$0.02 and adding
                        // an eth_call round-trip (~200ms) closes the opportunity window.
                        // The contract enforces final balance ≥ starting balance + minProfit on-chain; reverts are atomic.
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
                                                Err(e) => debug!("[{}] secondary RPC error: {}", cname, e.without_url()),
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
                            transport_timeout(rpc_send_timeout(), provider.send_raw_transaction(raw)).await
                        } else {
                            transport_timeout(rpc_send_timeout(), provider.send_transaction(prep.tx.clone())).await
                        };
                        match send_result {
                            Ok(pending) => {
                                let tx_hash = format!("{:?}", pending.tx_hash());
                                let elapsed_send = send_start.elapsed().as_millis() as u64;
                                let scan_to_submit_ms = detect_ms + candidate_start.elapsed().as_millis() as u64;
                                let submit_at = Instant::now();
                                let exec_ms = exec_start.elapsed().as_millis() as u64;
                                let tip_w = prep.tx.max_priority_fee_per_gas.unwrap_or_default();
                                info!(
                                    "[{}] exec_send | route=2hop | scan_to_submit_ms={} | detect_ms={} | exec_ms={} | send_rpc_ms={} | trigger={} | tip_wei={} | gross_usd={:.4} | net_usd={:.4} | pair={} | tx={}",
                                    cfg.name, scan_to_submit_ms, detect_ms, exec_ms, elapsed_send, trigger, tip_w,
                                    prep.profit_usd, prep.net_profit_usd, optimized.pair_id,
                                    &tx_hash[..10.min(tx_hash.len())],
                                );
                                { let mut exec = executor.lock().await; exec.record_sent(&tx_hash, elapsed_send); }
                                cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(send_cooldown_secs()));
                                cooldowns.insert(display_id.to_string(), Instant::now() + Duration::from_secs(send_cooldown_secs()));

                                // Fire-and-forget receipt task using cloned prep fields
                                let tx_hash_bg = tx_hash.clone();
                                let gas_limit_bg = prep.tx.gas.unwrap_or_default();
                                let tx_hash_b256 = pending.tx_hash().clone();
                                let provider_bg = provider.clone();
                                // Lost-race telemetry inputs (read after the send returned).
                                let race_pools_bg = {
                                    let strat = strategy.read().await;
                                    crate::race::route_pools(&strat.pool_cache, &[
                                        (&optimized.router_a_id, optimized.router_a_type, optimized.fee_a, optimized.token_in, optimized.token_out),
                                        (&optimized.router_b_id, optimized.router_b_type, optimized.fee_b, optimized.token_out, optimized.token_in),
                                    ])
                                };
                                let our_tip_bg = prep.tx.max_priority_fee_per_gas.unwrap_or_default();
                                let our_sender_bg = prep.tx.from.unwrap_or_default();
                                let ra_type_bg = crate::revert::router_type_label(&optimized.router_a_type);
                                let rb_type_bg = crate::revert::router_type_label(&optimized.router_b_type);
                                let confirmed_success_bg = prep.confirmed_success.clone();
                                let confirmed_failed_bg = prep.confirmed_failed.clone();
                                let confirmed_profit_bg = prep.confirmed_profit_bits.clone();
                                let chain_name_bg = prep.chain_name.clone();
                                let db_bg = prep.db.clone();
                                let chain_id_bg = prep.chain_id;
                                let opp_id_bg = prep.opp_id.clone();
                                let router_a_bg = prep.router_a.clone();
                                let router_b_bg = prep.router_b.clone();
                                let profit_bg = prep.net_profit_usd;
                                let native_price_bg = prep.native_price_usd;
                                let contract_addr_bg = prep.contract_addr;
                                let contract_balances_bg = prep.contract_balances.clone();
                                let detected_profit_bg = prep.profit_usd;
                                let trigger_bg = trigger;
                                let tip_bg = prep.tx.max_priority_fee_per_gas.unwrap_or_default();
                                let exec_start_bg = exec_start;
                                let send_rpc_bg = elapsed_send;
                                let scan_to_submit_bg = scan_to_submit_ms;
                                let submit_at_bg = submit_at;
                                let log_tx_bg = log_tx.clone();
                                let metrics_bg = metrics.clone();
                                let shared_state_bg = shared_state.clone();
                                let executor_bg = executor.clone();
                                tokio::spawn(async move {
                                    let receipt_result = tokio::time::timeout(
                                        std::time::Duration::from_secs(300),
                                        pending.get_receipt(),
                                    ).await;
                                    let receipt_result = match receipt_result {
                                        Ok(r) => r,
                                        Err(_) => {
                                            executor_bg.lock().await.forget_nonce();
                                            let msg = format!("[{}] No receipt after 300s for tx {} — outcome unknown (dropped, stuck, or lost by the RPC); nonce will be re-fetched", chain_name_bg, tx_hash_bg);
                                            warn!("{}", msg);
                                            broadcast_log(&log_tx_bg, "warn", &msg, None);
                                            return;
                                        }
                                    };
                                    let receipt_result = match receipt_result {
                                        Ok(r) => Ok(r),
                                        Err(e) => requery_receipt(provider_bg.as_ref(), tx_hash_b256, e.to_string()).await,
                                    };
                                    match receipt_result {
                                        Ok(receipt) => {
                                            let ok = receipt.status();
                                            let gas_usd = receipt.gas_used() as f64 * receipt.effective_gas_price() as f64 / 1e18 * native_price_bg;
                                            metrics_bg.gas_spent_usd.with_label_values(&[&chain_name_bg, if ok { "success" } else { "reverted" }]).inc_by(gas_usd.max(0.0));
                                            let submit_to_confirm_ms = submit_at_bg.elapsed().as_millis() as u64;
                                            let block_num = receipt.block_number();
                                            let revert_class_str: Option<&str>;
                                            let revert_detail: Option<String>;
                                            let mut pause = false;
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
                                                    receipt.gas_used(),
                                                    gas_limit_bg,
                                                )
                                                .await;
                                                pause = diagnosis.class.pauses_chain();
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
                                                // Pause first: race analysis costs several RPC round-trips, and other pairs
                                                // would keep sending certain-to-revert txs meanwhile.
                                                if pause {
                                                    pause_chain_for_revert(&shared_state_bg, chain_id_bg, &chain_name_bg, &log_tx_bg, revert_class_str.unwrap_or("unknown"), revert_detail.as_deref()).await;
                                                }
                                                let race = match (receipt.block_hash(), receipt.transaction_index()) {
                                                    (Some(bh), Some(idx)) => crate::race::analyze(
                                                        provider_bg.as_ref(), bh, idx, our_tip_bg, our_sender_bg, &race_pools_bg,
                                                    ).await,
                                                    _ => None,
                                                };
                                                let race_cause = race.as_ref().map(|r| r.cause).unwrap_or("unknown");
                                                metrics_bg.lost_races.with_label_values(&[&chain_name_bg, race_cause]).inc();
                                                broadcast_log(
                                                    &log_tx_bg,
                                                    "warn",
                                                    &format!(
                                                        "[{}] tx reverted on-chain | revert_class={} | pair={} | profit=${:.4} | scan_to_submit_ms={} | lost_race={} | tx={}",
                                                        chain_name_bg,
                                                        revert_class_str.unwrap_or("unknown"),
                                                        opp_id_bg,
                                                        profit_bg,
                                                        scan_to_submit_bg,
                                                        race_cause,
                                                        tx_hash_bg,
                                                    ),
                                                    Some(serde_json::json!({
                                                        "chain":               chain_name_bg,
                                                        "pair_id":             opp_id_bg,
                                                        "profit_usd":          profit_bg,
                                                        "gas_usd":          gas_usd,
                                                        "tx_hash":             tx_hash_bg,
                                                        "success":             false,
                                                        "revert_class":        revert_class_str,
                                                        "revert_detail":       revert_detail.clone(),
                                                        "race":                race,
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
                                        Err(e) => warn_receipt_unknown(&log_tx_bg, &chain_name_bg, &tx_hash_bg, &e),
                                    }
                                    drop(provider_bg);
                                });

                                handle_execution_success(
                                    &tx_hash, &fingerprint, optimized.profit_usd, &optimized.pair_id,
                                    &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                                    shared_state, log_tx, metrics, scan_to_submit_ms,
                                    send_cooldown_secs(),
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
                                    // Once the signed tx went to the secondary RPCs, a rate-limit or server
                                    // error on the primary says nothing about whether it landed.
                                    let broadcast_fired = prep.raw_tx.is_some() && !prep.submission_rpcs.is_empty();
                                    let nonce_consumed = crate::util::send_nonce_consumed(&err_str, broadcast_fired);
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
                                    &fingerprint, &router_ids, pending_pairs, cooldowns, send_cooldown_secs(),
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
            if gas_log_throttled {
                debug!(
                    "[{}] triangular {} | profit=${:.4} | {}/{}/{} (recently below gas)",
                    cfg.name, opp.triplet_id, opp.profit_usd, opp.router_ab_id, opp.router_bc_id, opp.router_ca_id
                );
            } else {
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
            }

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
                            cfg.name, opp.triplet_id, b, opp.amount_in, cooldown_secs()
                        );
                        warn!("{}", msg);
                        broadcast_log(log_tx, "warn", &msg, None);
                        cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(cooldown_secs()));
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
                        cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(cooldown_secs()));
                        cooldowns.insert(display_id.to_string(), Instant::now() + Duration::from_secs(cooldown_secs()));
                        handle_execution_success(
                            &tx_hash, &fingerprint, opp.profit_usd, &opp.triplet_id,
                            &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                            shared_state, log_tx, metrics, scan_to_submit_ms,
                            cooldown_secs(),
                        ).await;
                        break 'candidates;
                    }
                    Err(e) => {
                        drop(exec);
                        failure |= handle_execution_failure(&mut scan_rpc, 
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns, cooldown_secs(),
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
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns, cooldown_secs(),
                            consecutive_failures, cfg, shared_state, metrics, log_tx,
                            &ghost_profit_bits, opp.profit_usd,
                        ).await;
                    }
                    Ok(prep) => {
                        if cfg.preflight_eth_call {
                            match crate::executor::simulate_call(provider.as_ref(), prep.sim_provider.as_ref(), prep.tx.clone()).await {
                                Ok(_) => {}
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
                                        crate::revert::eth_call_error("Pre-flight eth_call rejected", &e),
                                        &fingerprint, &router_ids, pending_pairs, cooldowns, send_cooldown_secs(),
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
                                                Err(e) => debug!("[{}] secondary RPC error: {}", cname, e.without_url()),
                                            }
                                        }
                                    });
                                    futures::future::join_all(futs).await;
                                });
                            }
                        }
                        let send_start = std::time::Instant::now();
                        let send_result = if let Some(ref raw) = prep.raw_tx {
                            transport_timeout(rpc_send_timeout(), provider.send_raw_transaction(raw)).await
                        } else {
                            transport_timeout(rpc_send_timeout(), provider.send_transaction(prep.tx.clone())).await
                        };
                        match send_result {
                            Ok(pending) => {
                                let tx_hash = format!("{:?}", pending.tx_hash());
                                let elapsed_send = send_start.elapsed().as_millis() as u64;
                                let scan_to_submit_ms = detect_ms + candidate_start.elapsed().as_millis() as u64;
                                let submit_at = Instant::now();
                                let exec_ms = exec_start.elapsed().as_millis() as u64;
                                let tip_w = prep.tx.max_priority_fee_per_gas.unwrap_or_default();
                                info!(
                                    "[{}] exec_send | route=triangular | scan_to_submit_ms={} | detect_ms={} | exec_ms={} | send_rpc_ms={} | trigger={} | tip_wei={} | gross_usd={:.4} | net_usd={:.4} | triplet={} | tx={}",
                                    cfg.name, scan_to_submit_ms, detect_ms, exec_ms, elapsed_send, trigger, tip_w,
                                    prep.profit_usd, prep.net_profit_usd, opp.triplet_id,
                                    &tx_hash[..10.min(tx_hash.len())],
                                );
                                { let mut exec = executor.lock().await; exec.record_sent(&tx_hash, elapsed_send); }
                                cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(send_cooldown_secs()));
                                cooldowns.insert(display_id.to_string(), Instant::now() + Duration::from_secs(send_cooldown_secs()));

                                // Fire-and-forget receipt task
                                let tx_hash_bg = tx_hash.clone();
                                let gas_limit_bg = prep.tx.gas.unwrap_or_default();
                                let tx_hash_b256 = pending.tx_hash().clone();
                                let provider_bg = provider.clone();
                                // Lost-race telemetry inputs (read after the send returned).
                                let race_pools_bg = {
                                    let strat = strategy.read().await;
                                    crate::race::route_pools(&strat.pool_cache, &[
                                        (&opp.router_ab_id, opp.router_ab_type, opp.fee_ab, opp.token_a, opp.token_b),
                                        (&opp.router_bc_id, opp.router_bc_type, opp.fee_bc, opp.token_b, opp.token_c),
                                        (&opp.router_ca_id, opp.router_ca_type, opp.fee_ca, opp.token_c, opp.token_a),
                                    ])
                                };
                                let our_tip_bg = prep.tx.max_priority_fee_per_gas.unwrap_or_default();
                                let our_sender_bg = prep.tx.from.unwrap_or_default();
                                let ra_type_bg = crate::revert::router_type_label(&opp.router_ab_type);
                                let rb_type_bg = crate::revert::router_type_label(&opp.router_ca_type);
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
                                let profit_bg = prep.net_profit_usd;
                                let native_price_bg = prep.native_price_usd;
                                let contract_addr_bg = prep.contract_addr;
                                let contract_balances_bg = prep.contract_balances.clone();
                                let detected_profit_bg = prep.profit_usd;
                                let trigger_bg = trigger;
                                let tip_bg = prep.tx.max_priority_fee_per_gas.unwrap_or_default();
                                let exec_start_bg = exec_start;
                                let send_rpc_bg = elapsed_send;
                                let scan_to_submit_bg = scan_to_submit_ms;
                                let submit_at_bg = submit_at;
                                let log_tx_bg = log_tx.clone();
                                let metrics_bg = metrics.clone();
                                let shared_state_bg = shared_state.clone();
                                let executor_bg = executor.clone();
                                tokio::spawn(async move {
                                    let receipt_result = tokio::time::timeout(
                                        std::time::Duration::from_secs(300),
                                        pending.get_receipt(),
                                    ).await;
                                    let receipt_result = match receipt_result {
                                        Ok(r) => r,
                                        Err(_) => {
                                            executor_bg.lock().await.forget_nonce();
                                            let msg = format!("[{}] No receipt after 300s for tx {} — outcome unknown (dropped, stuck, or lost by the RPC); nonce will be re-fetched", chain_name_bg, tx_hash_bg);
                                            warn!("{}", msg);
                                            broadcast_log(&log_tx_bg, "warn", &msg, None);
                                            return;
                                        }
                                    };
                                    let receipt_result = match receipt_result {
                                        Ok(r) => Ok(r),
                                        Err(e) => requery_receipt(provider_bg.as_ref(), tx_hash_b256, e.to_string()).await,
                                    };
                                    match receipt_result {
                                        Ok(receipt) => {
                                            let ok = receipt.status();
                                            let gas_usd = receipt.gas_used() as f64 * receipt.effective_gas_price() as f64 / 1e18 * native_price_bg;
                                            metrics_bg.gas_spent_usd.with_label_values(&[&chain_name_bg, if ok { "success" } else { "reverted" }]).inc_by(gas_usd.max(0.0));
                                            let submit_to_confirm_ms = submit_at_bg.elapsed().as_millis() as u64;
                                            let block_num = receipt.block_number();
                                            let revert_class_str: Option<&str>;
                                            let revert_detail: Option<String>;
                                            let mut pause = false;
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
                                                    receipt.gas_used(),
                                                    gas_limit_bg,
                                                )
                                                .await;
                                                pause = diagnosis.class.pauses_chain();
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
                                                // Pause first: race analysis costs several RPC round-trips, and other pairs
                                                // would keep sending certain-to-revert txs meanwhile.
                                                if pause {
                                                    pause_chain_for_revert(&shared_state_bg, chain_id_bg, &chain_name_bg, &log_tx_bg, revert_class_str.unwrap_or("unknown"), revert_detail.as_deref()).await;
                                                }
                                                let race = match (receipt.block_hash(), receipt.transaction_index()) {
                                                    (Some(bh), Some(idx)) => crate::race::analyze(
                                                        provider_bg.as_ref(), bh, idx, our_tip_bg, our_sender_bg, &race_pools_bg,
                                                    ).await,
                                                    _ => None,
                                                };
                                                let race_cause = race.as_ref().map(|r| r.cause).unwrap_or("unknown");
                                                metrics_bg.lost_races.with_label_values(&[&chain_name_bg, race_cause]).inc();
                                                broadcast_log(
                                                    &log_tx_bg,
                                                    "warn",
                                                    &format!(
                                                        "[{}] triangular tx reverted on-chain | revert_class={} | pair={} | profit=${:.4} | scan_to_submit_ms={} | lost_race={} | tx={}",
                                                        chain_name_bg,
                                                        revert_class_str.unwrap_or("unknown"),
                                                        opp_id_bg,
                                                        profit_bg,
                                                        scan_to_submit_bg,
                                                        race_cause,
                                                        tx_hash_bg,
                                                    ),
                                                    Some(serde_json::json!({
                                                        "chain":               chain_name_bg,
                                                        "pair_id":             opp_id_bg,
                                                        "profit_usd":          profit_bg,
                                                        "gas_usd":          gas_usd,
                                                        "tx_hash":             tx_hash_bg,
                                                        "success":             false,
                                                        "revert_class":        revert_class_str,
                                                        "revert_detail":       revert_detail.clone(),
                                                        "race":                race,
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
                                        Err(e) => warn_receipt_unknown(&log_tx_bg, &chain_name_bg, &tx_hash_bg, &e),
                                    }
                                    drop(provider_bg);
                                });

                                handle_execution_success(
                                    &tx_hash, &fingerprint, opp.profit_usd, &opp.triplet_id,
                                    &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                                    shared_state, log_tx, metrics, scan_to_submit_ms,
                                    send_cooldown_secs(),
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
                                    // Once the signed tx went to the secondary RPCs, a rate-limit or server
                                    // error on the primary says nothing about whether it landed.
                                    let broadcast_fired = prep.raw_tx.is_some() && !prep.submission_rpcs.is_empty();
                                    let nonce_consumed = crate::util::send_nonce_consumed(&err_str, broadcast_fired);
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
                                    &fingerprint, &router_ids, pending_pairs, cooldowns, send_cooldown_secs(),
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
        if let Some(hold) = rpc_health.mark_outage() {
            warn!("[{}] RPC outage — execution off for at least {:?} (repeated relapses hold longer)", cfg.name, hold);
        }
    }

}

/// Per-scan RPC failure state passed to [`handle_execution_failure`].
pub(crate) struct ScanRpc<'a> {
    /// Simulation timeouts this scan (two = the node is too slow to simulate on).
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
fn slow_scan_warn() -> Duration { crate::config::tuning().slow_scan_warn() }

/// Logs a warning when dropped if the scan ran longer than [`slow_scan_warn()`].
struct SlowScanGuard<'a> {
    chain: &'a str,
    trigger: &'a str,
    start: Instant,
}

impl Drop for SlowScanGuard<'_> {
    fn drop(&mut self) {
        let took = self.start.elapsed();
        if took > slow_scan_warn() {
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
        if dry_run {
            chain.total_dry_run_ok += 1;
        }
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

/// Records a gas reject for `fingerprint` and says whether to log it (and sample ghost
/// profit): true at most once per `mute_secs` per route. Never cools the route itself down.
fn gas_reject_log_due(
    cooldowns: &mut HashMap<String, Instant>,
    fingerprint: &str,
    now: Instant,
    mute_secs: u64,
) -> bool {
    let log_key = Opportunity::gas_log_key(fingerprint);
    if cooldowns.get(&log_key).is_some_and(|t| *t > now) {
        return false;
    }
    cooldowns.insert(log_key, now + Duration::from_secs(mute_secs));
    true
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

    // Gas-profitability rejects (profit < gas cost) put no cooldown on the route: the next
    // scan may see a much larger edge on it. They only throttle the "Skipped" log and the
    // ghost-profit sample for gas_reject_cooldown_secs() via the route's gas-log key.
    let err_str = error.to_string();
    // Simulation / pre-flight failures, matched on the engine's own message prefixes.
    let is_sim_failure = crate::util::is_sim_failure(&err_str);
    // The contract rejecting a simulation is never RPC trouble, whatever its text says.
    let rpc = if error.is::<crate::revert::SimReverted>() {
        RpcFailure::None
    } else {
        crate::util::classify_rpc_failure(&err_str)
    };
    // One simulation timeout = that candidate is stale. Two in one scan = the node is
    // too slow to simulate on (in dry-run / pre-flight mode the simulation is the only
    // RPC call), which is what degraded means — no reset heuristics.
    if is_sim_failure && rpc == RpcFailure::Timeout {
        scan_rpc.sim_timeouts += 1;
    }
    let outage = match rpc {
        RpcFailure::Unreachable => true,
        RpcFailure::Timeout => !is_sim_failure || scan_rpc.sim_timeouts >= 2,
        RpcFailure::RateLimited | RpcFailure::None => false,
    };
    // A rate limit (simulation included) also stops this scan's remaining candidates:
    // more calls now just prolong the limit. It doesn't start the chain backoff.
    let action = FailureAction { outage, stop_scan: outage || rpc == RpcFailure::RateLimited };
    let is_gas_reject = err_str.contains("below threshold") || err_str.contains("negative after gas");
    if is_gas_reject {
        debug!("[{}] Skipped (unprofitable after gas): {}", cfg.name, err_str);
        if !gas_reject_log_due(cooldowns, pair_id, Instant::now(), gas_reject_cooldown_secs()) {
            return action;
        }
        // Ghost profit: gross USD left on the table due to gas cost, sampled once per
        // throttle window per route.
        ghost_profit_bits.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
            Some((f64::from_bits(bits) + opportunity_profit_usd).to_bits())
        }).ok();
        broadcast_log(log_tx, "info",
            &format!("[{}] Skipped {} — unprofitable after gas (gross=${:.4}, log muted {}s)", cfg.name, pair_id, opportunity_profit_usd, gas_reject_cooldown_secs()),
            None);
        return action;
    }
    let effective_cooldown = cooldown_secs;
    cooldowns.insert(pair_id.to_string(), Instant::now() + Duration::from_secs(effective_cooldown));

    // RPC rate-limits and outages (timeout / unreachable) are infra issues, not strategy
    // failures. Don't increment the circuit-breaker counter — just log and cool down.
    // Counting them would pause the chain (manual resume) after a few minutes' outage.
    if rpc == RpcFailure::RateLimited {
        // Can hit every candidate on every scan while it lasts: report at most once per
        // rate_limit_log_every() per chain, the rest at debug.
        debug!("[{}] Execute failed (rate limited): {} | pair={}", cfg.name, error, pair_id);
        if scan_rpc.health.rate_limit_log_due() {
            warn!("[{}] RPC rate limiting — affected pairs cool down {}s (further reports muted {:?})",
                  cfg.name, effective_cooldown, crate::util::rate_limit_log_every());
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
        chain.total_exec_failed += 1;
    }

    // Circuit breaker
    if *consecutive_failures >= max_consecutive_failures() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gas_reject_mutes_log_but_never_cools_the_route() {
        let mut cooldowns: HashMap<String, Instant> = HashMap::new();
        let fp = "base-usdc-weth|uniswap-v3-base:500|aerodrome-base::volatile:0|1000000000";
        let t0 = Instant::now();
        assert!(gas_reject_log_due(&mut cooldowns, fp, t0, 60));
        // Within the window: muted, and the route's own fingerprint is still not in cooldown,
        // so the candidate loop keeps evaluating (and executing) it.
        assert!(!gas_reject_log_due(&mut cooldowns, fp, t0 + Duration::from_secs(30), 60));
        assert!(!cooldowns.contains_key(fp));
        // A different fee tier is a different route with its own log window.
        let fp3000 = "base-usdc-weth|uniswap-v3-base:3000|aerodrome-base::volatile:0|1000000000";
        assert!(gas_reject_log_due(&mut cooldowns, fp3000, t0 + Duration::from_secs(30), 60));
        // After the window: logged again.
        assert!(gas_reject_log_due(&mut cooldowns, fp, t0 + Duration::from_secs(61), 60));
    }
}
