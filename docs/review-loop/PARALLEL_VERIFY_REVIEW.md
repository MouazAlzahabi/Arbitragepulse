# Review: perf/parallel-verify (main..perf/parallel-verify, 6 commits)

`cargo test -p engine`: 80 passed, 0 failed. No files were edited.

## CONFIRMED

### C1. A failed prefetch read drops candidates without a retry (missed opportunities)
- **Where:** engine/src/strategy/mod.rs:446-454 (`prefetch_quiet_reserves` inserts `None` for unreadable pools); engine/src/chain/scan.rs:94 and :117.
- **Claim:** If the prefetch multicall chunk fails (a timeout, a 429 or a transport error, so `run_multicall` returns `None`), the pool is stored as `Some(None)`. `recheck_quiet_pools` builds `missing` only from pools that are not in the map (`!prefetched.contains_key`), so it never re-reads that pool. `read.get(pool).copied().flatten()` gives `None`, the pool goes into `bad`, and every candidate through it is dropped as "on-chain=unreadable".
- **Before the change:** the recheck read the pool itself at T2, right before execution, and that read could succeed. The prefetch now fires at the same moment as the QuoterV2 burst (Phase 1.5, 1.5c and the tri verify, each `buffered(8)`), so a transient failure is more likely than before.
- **Severity:** Medium. Real candidates are dropped under rate limiting.
- **Fix:** in `prefetch_quiet_reserves`, insert only successful reads (`if let Some(r) = reserves { have.insert(pool, Some(r)) }`). Alternatively, have the recheck treat `Some(None)` as missing.

### C2. Prefetch never stamps a pool, so quiet pools on gated but unprofitable routes are re-read every scan (extra RPC)
- **Where:** engine/src/strategy/evaluate.rs:600-618 (pool list), :959 (always awaited); engine/src/strategy/mod.rs:424-456; engine/src/strategy/triangular.rs (the `pools` list joined with `verify_v3_triangular`).
- **Claim:** Before the change, quiet pools were read only when a final candidate used them, and `update_reserves` then stamped them so they stayed non-quiet for max_age. The prefetch now reads every quiet V2/Aerodrome pool of every gated pair's forward tasks, every 1.5c candidate, and up to 2N V3 plus N other triangular routes, whether or not any candidate survives. It does not stamp them, and the map is cleared at each scan start (scan.rs:338). A quiet pool on a route that keeps passing the gate but is never profitable is therefore read on every scan (every 2 s timer scan plus every swap or pending scan). Before the change it was read zero times.
- **Severity:** Low-Medium (CU / rate-limit budget). It also feeds C1 and P3.
- **Fix:** in `prefetch_quiet_reserves`, when the on-chain reserves equal the cached ones, call `pool_cache.update_reserves(pool, r0, r1)`. That confirms the pool is current and stops repeat reads. Do NOT stamp when they differ: leave that case for the recheck, which drops the candidate and corrects the cache. If you stamped there, the stale-built candidate would no longer count as quiet and would be kept.

### C3. Prefetch can add a standalone round trip to the 2-hop path when no QuoterV2 call happens
- **Where:** engine/src/strategy/evaluate.rs:617 and :959.
- **Claim:** `prefetch_fut` is always awaited in the 1.5b/1.5c join, even when `p15b_cache_misses == 0` and `p15c_queued == 0`, and Phase 1.5 had `cache_misses == 0` (or `p15_gate` was empty). If any pool is quiet and the feed is live, the join is then a dedicated eth_call round trip on evaluate's critical path. The comment says the read rides on a QuoterV2 round, but in this case there is none. Before the change, the read happened only if a candidate existed. On quiet markets the quote cache hits a lot, so this case is the steady state.
- **Severity:** Low. Latency is hidden only when `detect_triangular` (joined with evaluate) is slower.
- **Fix:** if Phase 1.5 did not poll it and `p15b_cache_misses == 0 && p15c_queued == 0`, drop `prefetch_fut` instead of awaiting it. The recheck then reads only for real candidates, as before.

### C4. A speculative 1.5b hit is not written to the QuoteCache, so the 1.5b round trip moves to the next scan
- **Where:** engine/src/strategy/evaluate.rs:824-833 (spec bound pushed into `p15b_pre` as a cache hit); 1.5b processing (~:975) inserts into `quote_cache` only for MC results.
- **Claim:** Steady state with unchanged pools:
  - Scan N: the forward leg misses, so spec is added. 1.5b is answered by the bracket, nothing is cached for `(p15b_key, quoter_out)`, and there are 2 extra calls per V3 q_b.
  - Scan N+1: the forward leg hits the cache, so `p15_mc` is empty and spec is cleared (`if p15_mc.is_empty()`). The 1.5b cache misses and a 1.5b round trip runs.
  - Before the change, scan N+1 was a full cache hit with 0 calls.

  The round trip is postponed by one scan rather than saved, and the spec calls cost 2× the 1.5b calls they replace.
- **Severity:** Low (efficiency only; no correctness impact).
- **Fix:** when a spec bound is used, `quote_cache.insert(&pool_cache, p15b_key, quoter_out, state_b, bound)`, with `state_b = v3_price_state(&rev_key)` captured when the spec request was built. The bound is conservative, so caching it is safe. Alternatively, only add spec when the reverse pool's state is unchanged from a cached entry.

## PLAUSIBLE

### P1. Prefetched reserves are older than a recheck-time read, so a pool that moved in between can be kept
- **Where:** engine/src/chain/scan.rs:94-124.
- **Claim:** Before the change, quiet pools were read at T2, after detection. They are now read at T1, during Phase 1.5 or tri verify, which can be hundreds of ms earlier.
  - If the pool trades on-chain between T1 and T2 and its Sync has not reached the feed by T2, the prefetched value still equals the cache. The candidate is kept and the pool is stamped as fresh.
  - If the Sync does arrive before T2, `last_sync` is updated, the pool is no longer quiet, and the prefetched value is never used. So the one case affected is a missed or lagging Sync, which is exactly what the recheck exists to catch.
- **Mitigation:** the downstream preflight eth_call or simulation still catches it. That is a wasted sim, not a lost trade.
- **Severity:** Low.
- **Fix:** none needed if accepted. Otherwise, re-read pools whose prefetch is older than X ms.

### P2. Higher `rpc_concurrency` multiplies with concurrent multicalls
- **Where:** engine/config.yaml:36.
- **Claim:** `rpc_concurrency` is a per-call `buffered(n)`, not a global cap. Phase 1.5, 1.5c and the prefetch are now joined (3 × 8 chunks). They also run concurrently with the triangular verify plus its prefetch (2 × 8). The real burst can therefore be about 40 chunks (about 400 calls), not the 80 the config comment describes. Before the change it was about 3-9 chunks.
- **Effect:** 429s turn into `(None, false)`. QuoterV2 legs then fail, which means missed opportunities (failures are not cached, so there is no phantom), and the prefetch fails too (C1).
- **Severity:** Low-Medium, depending on the provider plan.
- **Fix:** add a shared semaphore for the quote endpoint, or update the comment and watch the heartbeat lost-call counts.

## Checked and found FALSE (no defect)

- **MaybeDone (`p15c_fut`, `prefetch_fut`):** each is polled at most in the Phase 1.5 join and in the 1.5b join. Polling `Done` returns `Ready(())`. `take_output` is called exactly once, after the final join has driven it to `Done`, and it is never polled again, so the `Gone` state is never polled. No early `return` sits between the pin and the final join. `p15c_raw` cannot be lost or taken twice. `p15c_pre`, `p15c_sent_state` and `p15c_mc` are built in the same loop as before, so they stay aligned, and `p15c_queued` equals the old `p15c_mc.len()`.
- **Moving 1.5c collection before Phase 1.5:** the inputs (`rev_tasks`, `pair_resolved`, `pair_quotes`, the 1.5c reverse-key entries in `quote_cache`, pool state) are not written by Phase 1.5. Phase 1.5 inserts forward keys in the token_in→token_out direction. A collision would need another pair's forward key with exactly the same amount, which is negligible. 1.5b inserts happen after collection in both versions.
- **Spec split / alignment:** `run_multicall_answered` returns exactly one entry per call. Every path (timeout, RPC error, sequential fallback) pads `chunk_len`, and Multicall3 `aggregate3` returns one result per Call3. So `p15_raw.len() == forward_misses + spec_n`, and `split_off(len - spec_n)` is exact. When `p15_mc` is empty, spec is cleared, so `spec_n = 0`.
- **Spec bound conservativeness:**
  - Direction: `encode_quote(quoter_b, token_out, token_in, q_b.fee, amt)` matches the 1.5b request exactly. Slipstream tick-spacing is passed as `fee` the same way. The key `(pi, a_id, fee_a, b_id, fee_b)` uses `q_b.fee` on both sides, and `p15_gate` is a HashSet, so there are no duplicate points.
  - Bound: `v3_leg_lower_bound` is a lower bound for any `quoter_out`. It is the chord inside the points, the origin chord below `lo`, and flat output above `hi`. All three rely only on output being monotone and concave with f(0)=0. That holds for V3 exact-input, and also when liquidity is exhausted, where the output goes flat. So `hi` does not need to be a true upper bound of `quoter_out`.
  - Only `tight` bounds are used, the haircut and phantom check still apply, and spec points and the forward quote share one multicall, so they read the same block state.
  - Result: no phantom risk, only slightly understated profit.
- **RwLock re-entrancy:** `strategy.read().await` in the `timing` format (scan.rs:446) and in `spawn_k_state_check` is not called while another strategy guard is alive. The detect block's `strat` guard is dropped at the end of its block, the optimizer guard (scan.rs:596) is block-scoped, and the callers in chain/mod.rs drop their `strat` guards before `evaluate_and_execute`. The std `Mutex` on `recheck_prefetch` is never held across an `.await`. There is no deadlock. The only cost is that a K() failure path waits behind a queued writer, such as a native-price update.
- **ScanTimings:** scans are serialized per chain by the select loop. evaluate and detect_triangular write disjoint fields and reset them at their own start. The summary is read after both finish. `p15bc_ms` is about 0 when 1.5c already finished in the Phase 1.5 join; that matches "remaining" semantics.
- **`spawn_k_state_check`:** `raw.chunks(3)` is aligned with `pools`, since there are exactly 3 calls per pool and one result per call. It is diagnostic only.
- **revert.rs K() reclass:** `pauses_chain` covers only Blocked and RouterNotAllowed, so moving K() from Unknown to InsufficientOutput changes only labels.

## Fixes applied (same branch)
- **#1 Unreadable prefetch:** `prefetch_quiet_reserves` stores only successful reads; the recheck re-reads any pool missing from the map.
- **#2 Re-read every scan:** the prefetch stamps a pool (`update_reserves`) when its on-chain reserves equal the cached ones. Changed pools stay quiet, so the recheck still drops their candidates.
- **#3 Lone prefetch round trip:** in the 1.5b join, the prefetch is polled only when that join is a round trip anyway (1.5b misses, or 1.5c still pending) or it already finished.
- **#4 Bracket not cached:** a speculative 1.5b bound is written to the QuoteCache under `(reverse pool, quoter_out)`, using the reverse pool's state from request time.
- **Concurrency (plausible):** `rpc_concurrency` lowered from 8 to 6. That still fits a triangular verify round (about 60 quotes) in one wave, with less peak stacking across joined batches.
- **Not changed (plausible):** prefetched reserves are older than a read taken at recheck time. A missed Sync in that window costs a failed simulation, not a bad trade.
