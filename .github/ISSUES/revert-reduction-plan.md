# Revert reduction plan (Base / FCFS)

**Status:** In progress  
**Area:** `engine/` (detection, execution, telemetry), optional `contract/` (Phase 3)  
**Related:** [router-min-out-slack-bps.md](./router-min-out-slack-bps.md), [math-and-infra-followups.md](./math-and-infra-followups.md)

## Problem

Most mined failures are **sell-leg** reverts: Uniswap V3 **Too little received** when
`amountOutMinimum = amountIn + minProfit` is not met. Detection can overstate the
round-trip (V3 spot, stale V2 reserves, buy-leg `amountOutMin = 0` in the contract).

## Root-cause map (summary)

| Class | Typical on-chain signal | Engine cause |
|-------|-------------------------|--------------|
| V3 sell floor | `Too little received` (router string) | Spot / stale cache vs inclusion-time pool |
| Contract profit gate | `NotProfitable` custom error | Legs executed but net &lt; `minProfit` |
| Router min-out | `InsufficientOutput` | Local quote &gt; actual out (pre-v3 executor only) |

Executor v3 passes min-out 0 to every router and enforces `minProfit` on the final balance,
so on v3 all price-moved failures land in `NotProfitable` (with the shortfall); the V3
sell-floor and router min-out rows apply to the pre-v3 executor.
| Time | `DeadlineExpired` | Rare on Base (executor uses 120s deadline) |
| Allowlist | `RouterNotAllowed` | Config / deploy mismatch |

---

## Phase 0 — Measure (revert taxonomy + persistence)

**Goal:** Split reverts by class and route shape before tuning constants.

### Tasks

- [x] **0.1 Receipt post-mortem** — On mined `status = false`, replay tx via `eth_call` at inclusion block when possible; decode revert data (`Error(string)`, custom errors, router strings).
- [x] **0.2 Structured logs** — `exec_confirm` / dashboard payload includes `revert_class`, `revert_detail`, `scan_to_submit_ms`, `router_a_type`, `router_b_type`, route (`2hop` / `triangular`).
- [x] **0.3 SQLite** — Optional columns `revert_class`, `scan_to_submit_ms` on `trades` (migration-safe `ALTER TABLE`).
- [x] **0.4 Metrics** — Prometheus `arb_reverts_on_chain_total{chain,revert_class}`.

### Implementation notes (done)

- Code: `engine/src/revert.rs`, wired from `engine/src/chain/scan.rs` receipt tasks.
- Heuristic: if decode unavailable and sell leg is V3 → classify as `v3_too_little_likely`.

### Exit criteria

- [ ] 24–48h baseline: answer “what % is V3 sell vs NotProfitable vs triangular vs unknown”.
- [ ] Dashboard / grep on `revert_class=` in live feed JSON.

---

## Phase 1 — Engine-only, low latency

| # | Change | Status |
|---|--------|--------|
| **1.1** | Apply **`P15_QUOTER_HAIRCUT_BPS` (15 bps)** to **all** opportunities at detection push, including **Phase 2 V2→V2** and **triangular** (Phase 1.5 paths use shared helper — no double haircut). | **Done** |
| **1.2** | Raise Base **`min_profit_usd`** / per-pair floors for thin routes ($0.30–$0.80). | **Deferred** — skip while trading ~$1k/cycle; absolute $ floors hide most real edges. Prefer **1.1 haircut** + revert telemetry; revisit when size or win rate justifies fewer sends. |
| **1.3** | **`ROUTER_MIN_OUT_SLACK_BPS` 200 → 250** if Phase 0 shows persistent V3 sell reverts after 1.1 and 1.4 (1.2 is deferred, so it no longer gates this). | Superseded on executor v3 — see [router-min-out-slack-bps.md](./router-min-out-slack-bps.md) |
| **1.4** | Triangular: block or quoter-verify any route with a **V3 leg** before submit. | **Done** |
| **1.5** | Skip or re-quote if **`scan_to_submit_ms`** &gt; threshold (e.g. 300–500 ms on Base). | Open |
| **1.6** | Revert-aware cooldown on **fingerprint + pool key**. | Open |

### Phase 1.1 implementation (done)

- Helper: `Strategy::apply_exec_quote_haircut()` in `engine/src/strategy/mod.rs`.
- Applied in `evaluate.rs` (Phase 2 direct path; Phase 1.5/1.5b/1.5c refactored to same helper).
- Applied in `triangular.rs` before profit / threshold checks.

### Phase 1.4 implementation (done)

- `Strategy::verify_v3_triangular()` in `engine/src/strategy/triangular.rs`. Candidates are deduplicated to one route per path (best fee-tier variant) and ranked by USD profit **before** verification, which runs on up to 2 × N V3-leg routes so dropped routes are backfilled.
- Quotes are **always live** via the shared `Strategy::quote_v3_legs()` (batched, deduplicated within the batch, no cross-scan cache).
- **One round trip normally:** each V3 leg's upper-bound input `s` comes from chaining earlier V3 legs at marginal price. A leg whose input depends on an earlier V3 leg is quoted at `s` and `s × (1 − 1%)`; the real input usually falls between, where the chord between the two quotes is a tight lower bound (output is concave). Outside the bracket, the nearest point gives a conservative bound; it is re-quoted next round only if more than 10 bps off (≤ 3 rounds). Verified on a Base fork: 0 extra rounds across all scans.
- The 15 bps haircut and the 5% sanity cap apply to the verified output; routes below `min_triangular_profit_usd` are dropped, with the reason logged at debug.
- With `optimistic_submission: true` (no QuoterV2), V3-leg routes are submitted on spot quotes after deducting 0.30% **per V3 leg**, before the 5% cap and the spread diagnostic.
- Quoter health: warned once when every call has failed for 30 s (never worked; likely address/ABI) or 300 s (worked before), with at least 5 failed batches.
- 2-hop Phase 1.5/1.5b share one `QuoteCache`: an entry stores the pool state from when the quote was **requested**, is served only while the pool still matches and is ≤ 6 s old, and failures are cached only when the rest of the batch succeeded (a transport error never suppresses a pool).
- Multicall chunks time out after 3 s, so a hung RPC request no longer freezes the chain's scan loop.
- Aerodrome Slipstream is disabled in `config.yaml`: its factory reverts Uniswap's `getPool(…, uint24 fee)`. Real support needs a Slipstream router type in the contract.
- This let Base's `min_triangular_profit_usd` drop from $0.80 to $0.15: the floor now applies to verified profit rather than absorbing the V3 spot overestimate.

---

## Phase 2 — Quote freshness & accuracy (moderate latency)

- [ ] WS gap mitigation (`poll_logs_fallback` or targeted slot0/reserves refresh).
- [ ] Pre-submit **sell-leg QuoterV2** when `router_b` is V3 (single call, not full tx `eth_call`).
- [ ] Tighten Phase 1.5 gate / require QuoterV2 for any V3 leg in submit list.
- [ ] Export haircut + slack knobs to `config.yaml` (hot reload).

---

## Phase 3 — Contract alignment (deploy)

- [ ] Non-zero buy-leg `amountOutMin` from quoted mid.
- [ ] Sell `amountOutMin` scaled to actual `tokenOutReceived` after leg 1.
- [ ] Optional: engine-supplied verified minOut in calldata.

---

## Phase 4 — Tests & hygiene

- [ ] Pool cache AMM fixture tests vs QuoterV2 / on-chain quotes.
- [ ] Wire `deadline_seconds` from chain config into executor.
- [ ] Runbook: which paths submit without QuoterV2.

---

## Success metrics (2-week window)

| Metric | Target |
|--------|--------|
| Mined revert / sent tx | Down |
| Confirmed win / sent tx | Up |
| Gas on reverts | Down |
| `revert_class=v3_too_little*` share | Measurable then down after 1.2–1.3 |

---

## Test plan (after Phase 0 + 1.1 deploy)

1. Grep logs: `exec_confirm | ok=false | revert_class=`
2. SQL: `SELECT revert_class, COUNT(*) FROM trades WHERE success=0 GROUP BY revert_class`
3. Compare opps/day and confirm rate vs pre-change week
4. Ensure Phase 2 V2→V2 opps show reduced gross `profit_usd` (~15 bps) in opportunity logs
