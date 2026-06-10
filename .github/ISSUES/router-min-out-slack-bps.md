# Raise `ROUTER_MIN_OUT_SLACK_BPS` if reverts persist after P15 haircut

**Status:** Open  
**Priority:** Medium (follow-up — only if needed)  
**Area:** `engine/src/executor.rs`  
**Related:** `P15_QUOTER_HAIRCUT_BPS` (15 bps, centralized in `strategy/mod.rs`) — see also [math-and-infra-followups.md](./math-and-infra-followups.md)

## Context

~99% of on-chain failures are **Too little received** on the sell leg (V3 router
`amountOutMinimum` not met). The contract sets that floor from `amountIn + minProfit`.

Step 1 (done separately): raise **`P15_QUOTER_HAIRCUT_BPS`** from 5 → 15 bps so
thin-margin opportunities are filtered at detection (Phase 1.5 QuoterV2), with no
added latency.

## Proposed change (Step 2)

If revert rate is still high **24–48h after** the haircut deploy, raise execution-side slack:

| Constant | Current | Proposed |
|----------|---------|----------|
| `ROUTER_MIN_OUT_SLACK_BPS` | `200` (2.00%) | `250` (2.50%) |

Location: `engine/src/executor.rs` — used in `gas_cost_to_token_floor()` when encoding
`minProfit` for `executeArbitrage` / sell-leg router minOut.

```rust
const ROUTER_MIN_OUT_SLACK_BPS: u64 = 200; // → 250
```

Combined with `SELL_LEG_MINPROFIT_MUL_BPS = 6200`, this further lowers on-chain
`minProfit` so the sell leg is easier to satisfy at inclusion time.

## Expected effect

- **Fewer** mined reverts (`Too little received`)
- **No** added latency (encoding-only change)
- Slightly looser on-chain profit floor — off-chain `min_profit_usd` and net-after-gas
  checks remain the economic guard
- Watch for `NotProfitable` contract reverts (should stay rare)

## Do NOT

- Lower `ROUTER_MIN_OUT_SLACK_BPS` — tightens minOut and increases reverts
- Enable `preflight_eth_call` as the primary fix — adds ~100–250ms; poor fit for
  FCFS / pending-monitor path

## Acceptance criteria

- [x] Deploy haircut (15 bps) + P0/P1 math fixes — observe 24–48h
- [ ] Compare revert rate vs confirmed wins from logs / dashboard
- [ ] Only apply slack 200 → 250 if reverts remain materially high
- [ ] Optional follow-up: expose both constants in `config.yaml` for hot tuning

## Test plan

1. Note baseline: revert % and `exec_confirm ok=false` lines with `Too little received`
2. After slack change: same window, same pairs — revert rate should drop
3. Confirm `opps=` / submission count not collapsed to zero
4. Spot-check one successful receipt: gross profit still above gas + `min_profit_usd`
