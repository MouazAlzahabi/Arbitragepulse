use super::*;

// ─── Helpers ──────────────────────────────────────────────────────────────────

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
    disabled_set: HashSet<String>,
) {
    // Clone ghost_profit_bits from executor once — passed to handle_execution_failure
    // at each callsite so gas-rejected opportunities accumulate into the metric.
    let ghost_profit_bits = executor.lock().await.ghost_profit_usd_bits.clone();

    let all_opportunities = {
        let strat = strategy.read().await;

        // Merge targeted mask with disabled-pairs exclusion mask.
        let final_mask: Option<HashSet<usize>> = {
            let disabled_mask: Option<HashSet<usize>> = if disabled_set.is_empty() {
                None
            } else {
                let enabled: HashSet<usize> = strat.pairs.iter().enumerate()
                    .filter(|(_, p)| p.chain_id == cfg.id && !disabled_set.contains(&p.id))
                    .map(|(i, _)| i)
                    .collect();
                Some(enabled)
            };
            match (targeted.as_ref().map(|(m, _)| m), disabled_mask) {
                (None, None) => None,
                (Some(t), None) => Some(t.clone()),
                (None, Some(d)) => Some(d),
                (Some(t), Some(d)) => Some(t.intersection(&d).copied().collect()),
            }
        };

        let is_full_scan = targeted.is_none();
        let ((opps_2hop, best_2hop, verified_spread_2hop, fwd_ok, multi_dex, spread_2hop, active_pairs), (opps_tri, best_tri)) = tokio::join!(
            strat.evaluate(provider.as_ref(), final_mask.as_ref(), is_full_scan),
            strat.detect_triangular(provider.as_ref(), 5, targeted.as_ref().map(|(_, t)| t.as_slice()), &disabled_set),
        );

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

        // Sort by profit_usd descending
        merged.sort_by(|a, b| {
            b.profit_usd()
                .partial_cmp(&a.profit_usd())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        merged
    };

    // Track opp count for heartbeat display
    last_opp_count.store(all_opportunities.len() as u64, Ordering::Relaxed);

    // Log all opportunities found this scan so it's visible which pairs are generating them.
    // This is key for diagnosing "single pair always" — it shows whether other pairs have
    // spreads below threshold vs not being scanned at all.
    if all_opportunities.len() > 1 {
        let summary: Vec<String> = all_opportunities.iter()
            .map(|o| format!("{}=${:.4}", o.pair_id(), o.profit_usd()))
            .collect();
        info!("[{}] {} opps this scan: {}", cfg.name, all_opportunities.len(), summary.join(", "));
    }

    if all_opportunities.is_empty() {
        return;
    }

    metrics.opportunities.with_label_values(&[&cfg.name]).inc();

    // Try each opportunity in priority order (profit descending). On gas-rejection or
    // pre-flight failure, fall through to the next candidate. On a successful send or
    // dry-run, break. `continue 'candidates` is used for cheap skips (cooldown, balance
    // guard, detect-only) so higher-ranked blocked pairs don't starve the rest.
    'candidates: for best_opp in &all_opportunities {
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

    // ── Dispatch based on opportunity type ────────────────────────────────────

    match best_opp {
        Opportunity::TwoHop(opp) => {
            // Two-round adaptive size optimizer (only for 2-hop)
            let max_bal = {
                let bals = contract_balances.read().await;
                bals.get(&opp.token_in).copied()
            };
            // Skip optimizer in optimistic mode — each round costs ~100ms of QuoterV2 calls.
            // On FCFS chains (Base), 200ms of optimizer latency = ~65 positions lost.
            let optimized = {
                let strat = strategy.read().await;
                if strat.optimistic_submission || !cfg.optimize_size {
                    opp.clone()
                } else {
                    strat.optimize(opp, provider.as_ref(), max_bal).await
                }
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

            pending_pairs.insert(fingerprint.clone());

            let is_dry_run = shared_state.read().await.dry_run;
            let dry_run = is_dry_run;
            let exec_start = std::time::Instant::now();
            let router_ids = vec![optimized.router_a_id.clone(), optimized.router_b_id.clone()];

            if dry_run {
                // Dry-run: simulate with lock held (test mode, latency not critical)
                let mut exec = executor.lock().await;
                exec.dry_run = true;
                match exec.execute(provider.as_ref(), &optimized).await {
                    Ok(tx_hash) => {
                        let exec_time_ms = exec_start.elapsed().as_millis() as u64;
                        drop(exec);
                        cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(COOLDOWN_SECS));
                        handle_execution_success(
                            &tx_hash, &fingerprint, optimized.profit_usd, &optimized.pair_id,
                            &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                            shared_state, log_tx, metrics, exec_time_ms,
                            COOLDOWN_SECS,
                        ).await;
                        break 'candidates;
                    }
                    Err(e) => {
                        drop(exec);
                        handle_execution_failure(
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
                    exec.prepare_2hop(provider.as_ref(), &optimized).await
                    // lock drops here
                };
                match prep_result {
                    Err(e) => {
                        handle_execution_failure(
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns, COOLDOWN_SECS,
                            consecutive_failures, cfg, shared_state, metrics, log_tx,
                            &ghost_profit_bits, optimized.profit_usd,
                        ).await;
                    }
                    Ok(prep) => {
                        // Pre-flight skipped: on Base, gas per revert is <$0.02 and adding
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
                            provider.send_raw_transaction(raw).await
                        } else {
                            provider.send_transaction(prep.tx.clone()).await
                        };
                        match send_result {
                            Ok(pending) => {
                                let tx_hash = format!("{:?}", pending.tx_hash());
                                let elapsed_send = send_start.elapsed().as_millis() as u64;
                                info!(
                                    "[{}] Arb sent ({}ms) | gross=${:.4} net=${:.4} | tx={} → cd={}s",
                                    cfg.name, elapsed_send, prep.profit_usd, prep.net_profit_usd,
                                    &tx_hash[..10.min(tx_hash.len())], SEND_COOLDOWN_SECS,
                                );
                                { let mut exec = executor.lock().await; exec.record_sent(&tx_hash, elapsed_send); }
                                cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(SEND_COOLDOWN_SECS));

                                // Fire-and-forget receipt task using cloned prep fields
                                let log_tx_bg = log_tx.clone();
                                let tx_hash_bg = tx_hash.clone();
                                let provider_bg = provider.clone();
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
                                tokio::spawn(async move {
                                    match pending.get_receipt().await {
                                        Ok(receipt) => {
                                            if receipt.status() {
                                                confirmed_success_bg.fetch_add(1, Ordering::Relaxed);
                                                confirmed_profit_bg.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                                                    Some((f64::from_bits(bits) + profit_bg).to_bits())
                                                }).ok();
                                                info!("[{}] ✓ confirmed | gas={} | tx={}", chain_name_bg, receipt.gas_used, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                                                if let Some(bals) = contract_balances_bg {
                                                    let token_addrs: Vec<Address> = { let b = bals.read().await; b.keys().copied().collect() };
                                                    for token in token_addrs {
                                                        if let Ok(bal) = IERC20::new(token, &provider_bg).balanceOf(contract_addr_bg).call().await {
                                                            let mut b = bals.write().await;
                                                            b.insert(token, bal);
                                                        }
                                                    }
                                                }
                                            } else {
                                                confirmed_failed_bg.fetch_add(1, Ordering::Relaxed);
                                                let msg = format!(
                                                    "[{}] ✗ tx reverted | pair={} | detected_profit=${:.4} | gas_used={} | tx={}",
                                                    chain_name_bg, opp_id_bg, detected_profit_bg,
                                                    receipt.gas_used,
                                                    &tx_hash_bg[..10.min(tx_hash_bg.len())]
                                                );
                                                warn!("{}", msg);
                                                broadcast_log(&log_tx_bg, "error", &msg, None);
                                            }
                                            if let Some(db) = db_bg {
                                                let success = receipt.status();
                                                let _ = tokio::task::spawn_blocking(move || {
                                                    let _ = db.insert_trade(chain_id_bg, &chain_name_bg, &opp_id_bg, &router_a_bg, &router_b_bg, None, profit_bg, success, &tx_hash_bg, false);
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

                                let exec_time_ms = exec_start.elapsed().as_millis() as u64;
                                handle_execution_success(
                                    &tx_hash, &fingerprint, optimized.profit_usd, &optimized.pair_id,
                                    &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                                    shared_state, log_tx, metrics, exec_time_ms,
                                    SEND_COOLDOWN_SECS,
                                ).await;
                                break 'candidates;
                            }
                            Err(e) => {
                                {
                                    let mut exec = executor.lock().await;
                                    exec.record_failed();
                                    exec.prefetch_nonce(provider.as_ref()).await;
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
                                handle_execution_failure(
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

            pending_pairs.insert(fingerprint.clone());

            let is_dry_run = shared_state.read().await.dry_run;
            let dry_run = is_dry_run;
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
                        let exec_time_ms = exec_start.elapsed().as_millis() as u64;
                        drop(exec);
                        cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(COOLDOWN_SECS));
                        handle_execution_success(
                            &tx_hash, &fingerprint, opp.profit_usd, &opp.triplet_id,
                            &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                            shared_state, log_tx, metrics, exec_time_ms,
                            COOLDOWN_SECS,
                        ).await;
                        break 'candidates;
                    }
                    Err(e) => {
                        drop(exec);
                        handle_execution_failure(
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
                    exec.prepare_triangular(provider.as_ref(), opp).await
                    // lock drops here
                };
                match prep_result {
                    Err(e) => {
                        handle_execution_failure(
                            e, &fingerprint, &router_ids, pending_pairs, cooldowns, COOLDOWN_SECS,
                            consecutive_failures, cfg, shared_state, metrics, log_tx,
                            &ghost_profit_bits, opp.profit_usd,
                        ).await;
                    }
                    Ok(prep) => {
                        // Pre-flight skipped: same reasoning as 2-hop path above.
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
                            provider.send_raw_transaction(raw).await
                        } else {
                            provider.send_transaction(prep.tx.clone()).await
                        };
                        match send_result {
                            Ok(pending) => {
                                let tx_hash = format!("{:?}", pending.tx_hash());
                                let elapsed_send = send_start.elapsed().as_millis() as u64;
                                info!(
                                    "[{}] Triangular arb sent ({}ms) | gross=${:.4} net=${:.4} | tx={} → cd={}s",
                                    cfg.name, elapsed_send, prep.profit_usd, prep.net_profit_usd,
                                    &tx_hash[..10.min(tx_hash.len())], SEND_COOLDOWN_SECS,
                                );
                                { let mut exec = executor.lock().await; exec.record_sent(&tx_hash, elapsed_send); }
                                cooldowns.insert(fingerprint.clone(), Instant::now() + Duration::from_secs(SEND_COOLDOWN_SECS));

                                // Fire-and-forget receipt task
                                let log_tx_bg = log_tx.clone();
                                let tx_hash_bg = tx_hash.clone();
                                let provider_bg = provider.clone();
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
                                tokio::spawn(async move {
                                    match pending.get_receipt().await {
                                        Ok(receipt) => {
                                            if receipt.status() {
                                                confirmed_success_bg.fetch_add(1, Ordering::Relaxed);
                                                confirmed_profit_bg.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                                                    Some((f64::from_bits(bits) + profit_bg).to_bits())
                                                }).ok();
                                                info!("[{}] ✓ triangular confirmed | gas={} | tx={}", chain_name_bg, receipt.gas_used, &tx_hash_bg[..10.min(tx_hash_bg.len())]);
                                                if let Some(bals) = contract_balances_bg {
                                                    let token_addrs: Vec<Address> = { let b = bals.read().await; b.keys().copied().collect() };
                                                    for token in token_addrs {
                                                        if let Ok(bal) = IERC20::new(token, &provider_bg).balanceOf(contract_addr_bg).call().await {
                                                            let mut b = bals.write().await;
                                                            b.insert(token, bal);
                                                        }
                                                    }
                                                }
                                            } else {
                                                confirmed_failed_bg.fetch_add(1, Ordering::Relaxed);
                                                let msg = format!(
                                                    "[{}] ✗ triangular reverted | pair={} | detected_profit=${:.4} | gas_used={} | tx={}",
                                                    chain_name_bg, opp_id_bg, detected_profit_bg,
                                                    receipt.gas_used,
                                                    &tx_hash_bg[..10.min(tx_hash_bg.len())]
                                                );
                                                warn!("{}", msg);
                                                broadcast_log(&log_tx_bg, "error", &msg, None);
                                            }
                                            if let Some(db) = db_bg {
                                                let success = receipt.status();
                                                let _ = tokio::task::spawn_blocking(move || {
                                                    let _ = db.insert_trade(chain_id_bg, &chain_name_bg, &opp_id_bg, &router_a_bg, &router_b_bg, router_c_bg.as_deref(), profit_bg, success, &tx_hash_bg, false);
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

                                let exec_time_ms = exec_start.elapsed().as_millis() as u64;
                                handle_execution_success(
                                    &tx_hash, &fingerprint, opp.profit_usd, &opp.triplet_id,
                                    &router_ids, pending_pairs, consecutive_failures, dry_run, cfg,
                                    shared_state, log_tx, metrics, exec_time_ms,
                                    SEND_COOLDOWN_SECS,
                                ).await;
                                break 'candidates;
                            }
                            Err(e) => {
                                {
                                    let mut exec = executor.lock().await;
                                    exec.record_failed();
                                    exec.prefetch_nonce(provider.as_ref()).await;
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
                                handle_execution_failure(
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
    execution_time_ms: u64,
    cooldown_secs: u64,
) {
    *consecutive_failures = 0;
    pending_pairs.remove(pair_id);

    let dry_label = if dry_run { "true" } else { "false" };
    metrics.executed.with_label_values(&[&cfg.name, dry_label]).inc();
    if !dry_run {
        metrics.profit_usd.with_label_values(&[&cfg.name]).add(profit_usd);
    }

    // total_attempts = tx sent; total_success is updated in the heartbeat
    // from executor.confirmed_success (set after receipt confirms on-chain).
    let mut state = shared_state.write().await;
    if let Some(chain) = state.chains.iter_mut().find(|c| c.chain_id == cfg.id) {
        chain.total_attempts += 1;
    }

    broadcast_log(
        log_tx,
        "trade",
        &format!(
            "[{}] tx={} | pair={} | profit=${:.4} → cd={}s",
            cfg.name,
            tx_hash,
            display_id,
            profit_usd,
            cooldown_secs,
        ),
        Some(serde_json::json!({
            "chain":      cfg.name,
            "pair_id":    display_id,
            "profit_usd": profit_usd,
            "tx_hash":    tx_hash,
        })),
    );
}

/// Handle execution failure (both 2-hop and triangular).
pub(crate) async fn handle_execution_failure(
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
) {
    pending_pairs.remove(pair_id);

    // Gas-profitability rejects: route has a real spread but profit < gas cost.
    // Apply a much longer cooldown (GAS_REJECT_COOLDOWN_SECS=120s) so the same
    // route doesn't spam the log every 15s. Gas prices are stable on L2s, and the
    // spread is unlikely to change in 15s — there's no value in retrying sooner.
    let err_str = error.to_string();
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
        return;
    }

    // RPC rate-limit (429) is an infra issue, not a strategy failure.
    // Don't increment the circuit-breaker counter — just log and cool down.
    if err_str.contains("429")
        || err_str.contains("compute units")
        || err_str.contains("rate limit")
    {
        broadcast_log(
            log_tx,
            "error",
            &format!("[{}] Execute failed: {} → cd={}s", cfg.name, error, effective_cooldown),
            None,
        );
        return;
    }

    // Pre-flight simulation failures are expected — the opportunity closed between
    // detection and execution. Don't count toward circuit breaker; this is working
    // as intended. Only tx SEND failures indicate systemic problems (nonce, wallet).
    if err_str.contains("simulation failed") || err_str.contains("Pre-flight") {
        broadcast_log(
            log_tx,
            "warn",
            &format!("[{}] Pre-flight failed (skipping): {} → cd={}s", cfg.name, error, effective_cooldown),
            None,
        );
        return;
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
}
