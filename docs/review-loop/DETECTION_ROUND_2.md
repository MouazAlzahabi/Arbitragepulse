# Detection review — round 2

Reviewer pass over `a1aa77d..HEAD` (round-1 fixes D1, D3, D4, D6, Mint/Burn guard,
recheck_drop, v3 fresh recompute), plus a search for anything else still blocking detection.
Read-only review. Unit tests: `cargo test --bins` gives 75 passed, 0 failed.
On-chain numbers come from read-only `cast call` against mainnet.base.org (block ~53,146,650).

**Bottom line:** the round-1 diff causes no regression that suppresses detection. D1 does
**not** block V3-forward verification on deep pools (numbers in A1). What still blocks
detection is the fixed 10k trade size (R2-1). The code does not; the market at 10k does.
There is also one partial-failure cache-poisoning bug (R2-2) and some stats distortion.

---

## A. Round-1 diff: regression check

### A1. D1 `p15_gate_eligible` and deep pools: OK, no regression
- `evaluate.rs:24-26`: `full_in == Some(capped_in)`. Both sides come from the same
  `resolved.amount_in`. The forward quote is called with it (`evaluate.rs:180`), and
  `cap_v3_spot_amount_in` (`pool_cache.rs:158`) returns `amount_in.min(max_in)`. So an
  uncapped leg returns the *identical* U256 and the comparison is exact. No rounding is involved:
  the cap only rounds `max_in`, and it is used only when it is smaller.
- The cap is 2% of the input-side **virtual** reserve at the current tick
  (`vr_usdc = L·sqrtP/2^96`). At 10k USDC a leg is uncapped iff `vr_usdc ≥ 500k USDC`.
  Measured on-chain:

| Pool (USDC→X) | vr_usdc | 2% cap | 10k |
|---|---|---|---|
| Uni V3 USDC/WETH 0.05% | 42.6M | 852k | uncapped |
| Uni V3 USDC/WETH 0.01% | 2.09M | 41.7k | uncapped |
| Uni V3 USDC/WETH 0.30% | ~1.8B | — | uncapped |
| Slipstream USDC/WETH ts100 | 39.5M | 790k | uncapped |
| Slipstream USDC/WETH ts1 | 206k | 4.1k | capped |
| Pancake USDC/WETH 0.01% | 24.2M | 483k | uncapped |
| Pancake USDC/WETH 0.05% | 9.0M | 180k | uncapped |
| Uni cbBTC 0.05% / 0.3%, Slip cbBTC ts100 | 67M / 15.8M / 25M | — | uncapped |
| Uni USDT 0.01%, Slip USDT ts1, Uni USDT 0.3% | 279M / 53M / 812k | — | uncapped |
| Uni EURC 0.05%, Slip EURC ts1 | 8.2M / 18M | — | uncapped |
| every AERO V3 pool (Uni 500/3000, Slip 100/200) | ≤ 278k | ≤ 5.6k | capped |

  The WETH-start pairs (4 WETH): Uni 0.05% vr_weth ≈ 18.5k WETH, so they are uncapped.
  The main V3 pools all clear the gate. Only thin pools are excluded, and a capped leg at 10k
  means ≥ ~2–4% price impact, which no realistic edge survives. D1 is sound.

### A2. D4 `multicall_error_is_revert`: OK
`aggregate3` with `allowFailure=true` never reverts because of one inner quote. An inner
QuoterV2 revert comes back as `Result{success:false}`, and `run_multicall` maps it to `None`
per call. The aggregate itself errors only on out-of-gas or the gas cap (`"out of gas"` /
`"gas required exceeds allowance"`, which match `gas`), and those still fall back per call.
HTTP 429 is a `Transport` error, not `ErrorResp`, so it is correctly treated as non-revert.
alloy 1.7 `is_retry_err` (429, -32005, …) never matches "execution reverted". See R2-2 for
the follow-on cache problem this change makes more likely.

### A3. D6 1.5c QuoteCache key: OK
The key is `V3PoolKey::new(router_b_id, token_out, token_in, fee_b)`, and the amount is
`token_out_amount`. That is the same key shape as 1.5b's `rev_v3_key` and as `v3_by_key`. The
quote depends only on pool B's state and the input amount. Candidates and `pre` stay index-aligned,
and MC results are consumed in order only on misses. Correct. (Unbounded inner map: see R2-6.)

### A4. Mint/Burn `positioned` guard: OK
All three ingestion paths carry positions: the sealed `subscribe_logs` (`chain/mod.rs:1064`),
the `get_logs` poll (`listener.rs:325`), and Flashblocks `pendingLogs` when it supplies them.
No code builds synthetic `Log`s (grep: no `Log {` outside tests, no backfill path). A
position-less pending Mint is skipped, and its sealed copy is applied. Swap logs carry absolute
`L`, so any miss self-corrects at the next swap. Residual edge (pre-existing, low): if a
positioned pending Swap at (N,k+1) lands before an unpositioned/missed Mint at (N,k), the
sealed Mint is `Less` and dropped. The next Swap fixes `L`.

### A5. D3 haircut, recheck_drop, v3 fresh: OK
2 bps is applied uniformly (Phase 2, 1.5, 1.5b, 1.5c, triangular). The heartbeat formatting
and recompute are correct.

---

## B. Remaining issues

### R2-1 — HIGH (detection blocker, economic) — fixed 10k size; no size search before the profit gate
- **Where:** `config.yaml:156…` (`trade_amount: "10000"` on every USDC pair), `config.yaml:67`
  (`optimize_size: false`), `evaluate.rs:117` (Phase 1 quotes only `resolved.amount_in`),
  `scan.rs:~505` (the optimizer runs only on opps that are *already* profitable at full size).
- **Claim:** every route is evaluated only at 10k. A divergence that is profitable at 1–3k but
  not at 10k is never surfaced. Price impact at 10k uses up most of a realistic edge.
- **Proof (QuoterV2 round trips, same block, USDC→WETH→USDC):**

| route | 10k | 1k |
|---|---|---|
| Uni 0.05% → Pancake 0.01% | −0.084% | −0.027% |
| Pancake 0.01% → Uni 0.05% | −0.161% | −0.106% |
| Uni 0.01% → Pancake 0.01% | −0.467% | −0.076% |
| Pancake 0.01% → Uni 0.01% | −0.639% | −0.063% |

  At 10k the required mid-price divergence is about 0.06–0.55% above fees. At 1k it is 0.03–0.08%.
  The detection floors themselves are not binding (see R2-7).
- **Effect:** ~0 opps comes mostly from size, not from a code gate.
- **Fix direction:** evaluate 2–3 sizes per pair in Phase 2 (e.g. 10k / 3k / 1k, local math
  is free). Or compute the closed-form optimal size for xy=k / V3-spot legs and gate 1.5/1.5c
  at that size. Alternatively, lower `trade_amount` on the thin pairs (AERO, VIRTUAL, EURC).

### R2-2 — MEDIUM (suppresses detection ≤ 3 blocks) — ZERO cached for transport-failed calls when another chunk succeeded
- **Where:** `evaluate.rs:592` (`p15_batch_ok`), `:841` (`p15b_batch_ok`), `:928`
  (`p15c_batch_ok`), plus `strategy/mod.rs` `run_multicall`.
- **Claim:** `batch_ok = raw.iter().any(Option::is_some)` is computed per *batch*, but failures
  happen per *chunk* (`multicall_chunk_size: 10`, `rpc_concurrency: 3`). If one chunk gets
  rate-limited or times out while another chunk succeeds, `batch_ok` is true and every call
  in the failed chunk is cached as `U256::ZERO`, a "real revert". After D4, transport errors
  no longer fall back per call, so this case now happens on every 429 from mainnet.base.org.
- **Effect:** those pools/amounts are skipped (`if cached_val.is_zero() { continue }`) until
  the pool moves or `quote_cache_max_age` (6 s) passes. A live spread can be dropped.
- **Prove:** a mocked Asserter with 11 calls (2 chunks): chunk 1 OK with a valid quote, chunk 2
  `-32011 request limit`. Assert that `quote_cache.get` for a chunk-2 key returns `Some(0)`.
- **Fix:** have `run_multicall` return a per-call tri-state (`Ok(bytes)` / `Reverted` /
  `NotSent`), or a parallel `Vec<bool>` of "chunk answered", and cache ZERO only for answered
  chunks.

### R2-3 — MEDIUM (missed route class) — same-DEX cross-fee-tier V3 arbs are excluded
- **Where:** `evaluate.rs:308` (`q_a.router_addr == q_b.router_addr → continue`) and `:311`.
  The 1.5b filter `t.router_id != router_a_id` (`evaluate.rs:~635`) does the same.
- **Claim:** this rule predates QuoterV2 verification ("spot math phantoms"). Now both legs are
  QuoterV2-exact at full size, and the contract takes `routerA == routerB` with distinct
  `feeA/feeB` (`ArbitrageExecutor.sol:239-254`, no same-router check). Uni 0.01%↔0.05%↔0.3%,
  Pancake 0.01%↔0.05%, and Slipstream ts1↔ts100 are never evaluated. (Keep the Aerodrome
  vol×stable exclusion. That one has a different cause.)
- **Effect:** a whole class of Base arbs is invisible. Today's snapshot shows a 0.255% mid-price
  gap between Uni USDC/WETH 0.3% and 0.05%. That is not profitable after the 0.35% fees, but it
  shows these tiers diverge.
- **Prove:** count rev_tasks skipped at `:308` for V3 router pairs over a run, then
  QuoterV2-quote a few of them.

### R2-4 — LOW-MEDIUM (stats corruption) — `best=` and `spread=` include unverified V3 spot and capped-input spreads
- **Where:** `evaluate.rs:420` and `:452`. `best_spread_pct` and `best_raw_usd` are updated
  for every Phase-2 task *before* the `v3_allowed` check. That includes V3-forward spot quotes,
  which are never opps without 1.5, and capped forwards, which after D1 are never verified at all.
  Triangular already avoids this (`verified_later`, `triangular.rs`).
- **Effect:** the heartbeat `best=`/`spread=` and the "raw profit ≥ min but 0 opportunities"
  warn (`evaluate.rs:~1068`) report phantom edges. The heartbeat looks like real detection is
  failing when it isn't. This misleads investigations like this one.
- **Fix:** update `best_raw_usd`/`best_spread_pct` in Phase 2 only when `v3_allowed`. Keep the
  rest in `bestV`.

### R2-5 — LOW (stats) — `opps=` undercounts detected candidates
- **Where:** `scan.rs:430-527` and `:998-1004`. `opps_found` increments only after the per-route
  cooldown, the pair-level cooldown, and the in-flight dedup. It is skipped entirely for
  `gas_log_throttled` routes (60 s).
- **Effect:** in dry-run, a DRY-RUN-ok puts a 15 s cooldown on the route *and the pair*, so later
  detections on that pair are silently not counted. A route rejected once for gas is not counted
  for 60 s. `opps=` measures "attempted", not "detected".
- **Fix:** count at `evaluate`/merge time (after `recheck_quiet_pools`), and keep the current
  counter as `attempts`.

### R2-6 — LOW (memory) — QuoteCache inner map never evicts
- **Where:** `strategy/mod.rs:160-168`. The outer map is cleared only when there are more than
  1024 pool keys. The real count is ~100 and never reaches that. The per-pool
  `HashMap<amount, …>` keeps every stale amount.
- 1.5b (`quoter_out`) and now 1.5c (`token_out_amount`) key on amounts that change with every
  reserve move, so the map grows without bound over a long run (~150 B/entry).
- **Fix:** on insert, drop the pool's entries whose `(sqrtP, L)` differs from `state_at_send`,
  or cap entries per pool.

### R2-7 — INFO (checked, not blocking): profit floors and gas
- The floors are `min_profit_usd 0.0001` and `min_triangular_profit_usd 0.15` (= 0.0015% at
  10k). No pairs override them. With a 2 bps haircut, a 0.01% exact edge at 10k is $0.80 net
  of haircut.
- Gas gate (`executor.rs:418-430`, `:634-645`): 700k × (base ~0.005 gwei + tip 0.01 gwei) ≈
  1.05e13 wei ≈ $0.03–0.04. The gate is `net > 0`, with no multiplier.
  `onchain_min_profit_bps` only sets the on-chain `minProfit` and does not filter detection.
  The native price defaults to 2500 in both Strategy and Executor until the first update, so
  profit is never zeroed at startup.
- Triangular (`triangular.rs:280-300`): a capped V3 leg scores output for the capped input
  against the full input. This kills any triangle with a thin V3 leg, which is the same
  economics as R2-1 and conservative by design.

---

## Fixes (round 2)

Branch `debug/no-opps`, one commit per fix. Each item was checked in the code before it was fixed.
`cargo test -p engine`: 79/79 pass (75 before + 4 new). `cargo build -p engine` is green with the same
11 pre-existing warnings. Clippy shows no new warnings on changed lines. The contract is untouched.

| fix | commit | change | test |
|-----|--------|--------|------|
| R2-2 | ffbfbc5 | `run_multicall_answered` returns `(result, answered)` per call. `answered` is false when the call's chunk, or its sequential fallback call, timed out or hit a transport/RPC error. `run_multicall` is now a thin wrapper. At 1.5/1.5b/1.5c, ZERO is cached only for a call whose own chunk was answered, which replaces the per-batch `*_batch_ok`. triangular.rs and optimize.rs don't cache, so they are unchanged. | `failed_chunk_calls_are_not_answered` (Asserter, 11 calls: chunk 1 OK with one inner revert, chunk 2 `-32011`) |
| R2-3 | ba9333f | New pure fn `route_legs_eligible`, used by Phase 2 and by the 1.5b `q_b` filter. Different router contracts are always eligible. On the same contract, only the same V3 router id with different fees or tick spacings is eligible, and only when verification is on (`!optimistic_submission`). Still excluded: the identical pool, and Aerodrome vol×stable. Checked: `executeArbitrage` (`ArbitrageExecutor.sol:239-257`) has no routerA≠routerB check. The engine has no other same-router guard. Phase 2 never emits V3 routes when verification is on: V3 forwards go to 1.5, then 1.5b for the V3 reverse. `fingerprint()` already includes `fee_a`/`fee_b`. | `same_router_routes_only_across_v3_fee_tiers` |
| R2-5 | 222da7b | `opps_found` is counted at the top of the candidate loop, before the route cooldown, pair cooldown, in-flight, and pending-only skips. Each fingerprint is counted at most once per `cooldown_secs` window, keyed `fingerprint\|counted` in the existing `cooldowns` map, so it is cleaned up by the normal `retain`. Routes that are `gas_log_throttled` (rejected after gas in the last 60 s) are still not counted. The three old increment sites are removed. Execution is unchanged. | `detections_count_once_per_window_without_cooling_the_route` |
| R2-6 | e7bf927 | `QuoteCache::insert_settled` first drops the pool's entries that can't be served again. That means entries stored under a state other than the pool's *current* (sqrtP, L), or older than `quote_cache_max_age()`. Comparing against the current state rather than `state_at_send` means a late result from an older state can't evict valid entries. Then it caps the pool at 64 amounts, dropping the oldest first. | `insert_evicts_other_states_and_caps_amounts_per_pool` |
| R2-4 | 6a9009d | Phase 2 updates `best_spread_pct`/`best_raw_usd` only when `v3_allowed`, the same condition that lets a Phase-2 route become an opp. V3 routes feed these values only through their verified 1.5/1.5b/1.5c results. The "raw profit ≥ min but 0 opportunities" warning reads `best_raw_usd`, so spot phantoms no longer trigger it. | none (inline in `evaluate`; build-checked) |

Notes:
- R2-5 counts detections from the merged candidate list, which means net of the haircut and `min_profit_usd`.
  The gas check happens later, in the executor. Only routes already known to fail after gas (`gas_log_throttled`) are left out.
- R2-4 side effect: when no V2/Aerodrome-only route exists and no V3 route verifies positive, `spread=` and `best=`
  now stay at their empty values. Before, they showed a spot-math number. `bestV=` still shows verified losses.
- R2-1 (multi-size probing / trade size) is parked for the user. R2-7 needed no change.
