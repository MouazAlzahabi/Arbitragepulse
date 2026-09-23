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
| SyncSwap / router min | `InsufficientOutput` | Local quote &gt; actual out |
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
| **1.2** | Raise Base **`min_profit_usd`** / per-pair floors for thin routes ($0.30–$0.80). | Open |
| **1.3** | **`ROUTER_MIN_OUT_SLACK_BPS` 200 → 250** if Phase 0 shows persistent V3 sell reverts after 1.1–1.2. | Open — see [router-min-out-slack-bps.md](./router-min-out-slack-bps.md) |
| **1.4** | Triangular: block or quoter-verify any route with a **V3 leg** before submit. | Open |
| **1.5** | Skip or re-quote if **`scan_to_submit_ms`** &gt; threshold (e.g. 300–500 ms on Base). | Open |
| **1.6** | Revert-aware cooldown on **fingerprint + pool key**. | Open |

### Phase 1.1 implementation (done)

- Helper: `Strategy::apply_exec_quote_haircut()` in `engine/src/strategy/mod.rs`.
- Applied in `evaluate.rs` (Phase 2 direct path; Phase 1.5/1.5b/1.5c refactored to same helper).
- Applied in `triangular.rs` before profit / threshold checks.

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
