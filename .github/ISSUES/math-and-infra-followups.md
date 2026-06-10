# Math & infrastructure follow-ups (P2 / P3)

**Status:** Open  
**Priority:** Low–Medium (after P0/P1 deploy + 24–48h observation)  
**Depends on:** [router-min-out-slack-bps.md](./router-min-out-slack-bps.md) (execution slack, if reverts persist)

## Completed in P0 / P1 (do not re-do)

- [x] `P15_QUOTER_HAIRCUT_BPS` centralized in `strategy/mod.rs` (15 bps)
- [x] Phase 1.5b + `optimize.rs` use same haircut (was hardcoded 8 bps)
- [x] V3 spot single-tick cap (~2% of `vr_in`) in `pool_cache::quote_v3_spot_str`
- [x] Phase 2 spread uses `u256_to_f64` consistently
- [x] Stale `VOLATILE_MAX_AGE=60s` comments corrected to 120s

---

## P2 — Engine tuning & hygiene

### 1. Raise `ROUTER_MIN_OUT_SLACK_BPS` (200 → 250)

See dedicated ticket: **router-min-out-slack-bps.md**

Only if `Too little received` reverts remain high after P0/P1 deploy.

### 2. Wire `pool_cache.prune_stale()` on heartbeat

**Area:** `engine/src/chain/mod.rs` (60s `price_tick`)

`prune_stale()` exists but is never called. Long-running engines accumulate dead
pool entries and stale `by_key` indexes.

**Suggested TTLs:**

- V2/Solidly/SyncSwap: 30–60 min without `Sync`
- V3: 30–60 min without `Swap`

Log evicted counts at `debug!` level.

### 3. Export tuning knobs to `config.yaml`

Move from compile-time constants to hot-reloadable config:

| Constant | File | Purpose |
|----------|------|---------|
| `P15_QUOTER_HAIRCUT_BPS` | `strategy/mod.rs` | Detection buffer |
| `ROUTER_MIN_OUT_SLACK_BPS` | `executor.rs` | On-chain sell minOut |
| `SELL_LEG_MINPROFIT_MUL_BPS` | `executor.rs` | On-chain sell minOut |
| `V3_SPOT_MAX_VR_FRACTION_BPS` | `pool_cache.rs` | V3 spot cap |

---

## P3 — Structural / contract

### 4. Contract: buy-leg slippage protection

**Area:** `contract/src/ArbitrageExecutor.sol`

Buy leg uses `amountOutMin = 0`. If leg 1 under-delivers vs quote, leg 2 must still
return `amountIn + minProfit` → frequent `Too little received` on sell leg.

**Options (design review required):**

- Non-zero `minOut` on buy leg from quoted mid amount
- Dynamic sell `minOut` based on actual `tokenOutReceived` (not fixed `amountIn + minProfit`)

### 5. Unit tests for pool cache AMM math

**Area:** `engine/src/pool_cache.rs` (new `#[cfg(test)]` module)

Fixture tests against known on-chain reserves / V3 slot0:

- `amount_out_v2` vs Uniswap `getAmountOut`
- `amount_out_stable` vs Solidly router quote
- `quote_v3_spot_str` vs QuoterV2 for small trades within one tick

---

## Acceptance criteria (P2 bundle)

- [ ] P0/P1 deployed; revert rate measured 24–48h
- [ ] Slack ticket applied only if needed
- [ ] `prune_stale` wired; memory stable over 24h run
- [ ] Optional: config.yaml knobs for haircut + slack

## Test plan

1. Baseline revert % after P0/P1
2. After P2 slack (if applied): same metric window
3. After `prune_stale`: `watch_addresses().len()` stable on idle chain
4. After unit tests: CI `cargo test` green
