# Detection review, round 3 (review + verify of the round-2 fixes)

Scope: `c67e04a..4f86ced` on `debug/no-opps`. That is five engine commits (ffbfbc5, ba9333f, 222da7b,
e7bf927, 6a9009d) plus the round-2 doc. Checks were code traces, plus one throwaway local test (reverted).
No RPC calls were made and the engine was not run.

Result: **no CONFIRMED regressions, and nothing blocking.** No code changes this round.

- `cargo test -p engine`: 79/79 pass.
- `cargo clippy -p engine`: none of its warnings fall on a line that round 2 changed. I matched the clippy spans against the `git diff -U0` hunks.

## Findings

| # | area | question | verdict |
|---|------|----------|---------|
| R3-1 | `run_multicall_answered` | Do the per-call `(result, answered)` flags stay aligned with the input across chunks, the timeout path, the transport-error path and the sequential fallback? | **FALSE** (no bug) |
| R3-2 | same-router V3 routes | Can a same-router route reach execution without both legs QuoterV2-verified? | **FALSE** |
| R3-3 | same-router V3 routes | Can 1.5b quote the reverse leg on the identical pool as the forward? | **FALSE** |
| R3-4 | same-router V3 routes | Does the executor/contract handle the same router twice (approvals, Slipstream tickSpacing in the fee slot)? | **FALSE** |
| R3-5 | same-router V3 routes | Do race.rs `route_pools`, `pools()`, `fingerprint()` and the pair cooldown handle it? | **FALSE** |
| R3-6 | `opps_found` | Does counting before gas inflate it? Can the `\|counted` key be confused with the route cooldown? Does the map grow without bound? | **FALSE** (acceptable, see notes) |
| R3-7 | `QuoteCache` eviction | Can it evict the entry it just inserted, or entries the next scan can use? Is the per-pool cap correct? | **FALSE** |
| R3-8 | `best=` / `spread=` | Is any warning logic now inverted or always firing? | **FALSE** |

### R3-1: multicall alignment (FALSE)

- **Chunk order.** `buffered(concurrency)` is order-preserving, so chunk results come back in input order.
- **Chunk length.** `chunk_len = chunk_size.min(len - offset)` matches the `chunks()` split. Each branch advances `call_offset` by exactly `chunk_len`:
  - timeout: `repeat_n((None,false))`
  - `Err(false)` (transport error): `repeat_n((None,false))`
  - decoded: one entry per `McResult`, `answered = true`
  - fallback: one push per call in `calls[offset..offset+len]`
- **Fallback flags.** A sequential call is marked answered only on `Ok(Ok)`, or on `Ok(Err(e))` where `multicall_error_is_revert(e)`. A timeout gives `(None,false)`.
- **Throwaway test.** I ran a test (passed, then reverted) with two chunks:
  - chunk 1's aggregate reverts and falls back per call: ok, revert, `-32011`, then ok for the rest.
  - chunk 2 decodes as revert + ok.
  - Every index came back with the expected flag: `(Some,true)`, `(None,true)`, `(None,false)`, …, `(None,true)`, `(Some,true)`. The mock queue was fully consumed.
- **Callers.** 1.5/1.5b/1.5c pull one `(raw, answered)` per cache miss, in the order the `*_mc` and `*_sent_state` vecs were built. Cache hits don't consume any. So ZERO is cached only for calls whose own chunk was answered.

### R3-2: no unverified same-router route reaches execution (FALSE)

- **Phase 2 with verification on** (`!optimistic_submission`). Same-router V3×V3 rev_tasks are now built, but `v3_allowed` is false for any V3 leg, so Phase 2 never emits them. They only feed `p15_gate`.
- **Optimistic mode.** `route_legs_eligible(.., verified=false)` rejects any shared `router_addr`. Same-router routes are not even built.
- **1.5b.** The 1.5b path emits the route only from a QuoterV2 reverse (`p15b_*`), whose forward is the 1.5 QuoterV2 result. Both legs are verified at full size.
- **1.5c.** 1.5c requires a V2/Aerodrome forward and a V3 reverse. Those are different router contracts, so it can't produce a same-router route.

### R3-3: identical pool excluded (FALSE)

1.5b filters with `route_legs_eligible((router_a_id, router_a_addr, V3, fee_a), (t.router_id, t.router_addr, t.router_type, t.fee), true)`. That requires `a_fee != b_fee` when the address and id match, so the forward pool is never its own reverse. The unit test asserts it.

The configured V3 routers (Uniswap, Slipstream, Pancake) have distinct factories, so two different router ids can't point at the same pool.

### R3-4: executor and contract with the same router twice (FALSE)

`build_calldata` passes `routerA`/`routerB` and `feeA`/`feeB` independently. In the contract, `executeArbitrage` calls `_checkRouter` per leg, and `_swap` calls `forceApprove(router, amountIn)` on that leg's own `tokenIn`:
- leg 1 approves tokenIn
- leg 2 approves tokenOut

For Slipstream, `_swap` puts `int24(uint24(fee))` into `tickSpacing` for each leg separately. Nothing in the contract or the executor requires routerA ≠ routerB.

The optimizer (`probe_range`) quotes leg A with `fa` and leg B with `fb` through the router's single quoter, so it is correct for the same router. `router_ids` in the success/failure handlers is unused.

### R3-5: race, pools, fingerprint, cooldowns (FALSE)

- `route_pools` keys V3 on `(router_id, tin, tout, fee)`, so the two tiers resolve to two distinct pools. It dedupes anyway.
- `fingerprint()` contains `router:fee` for both legs, so each tier pair is its own route.
- The pair cooldown is keyed on `pair_id`, which is unchanged.

### R3-6: `opps_found` at the top of the loop (FALSE, behaviour is acceptable)

**Counting before the gas check.** Before round 2, the increment sat at the 2-hop/triangular log sites. Those also ran before `exec.execute` (the gas check) and were only gated by `!gas_log_throttled`. So a route's first gas-rejected detection was already counted, and on every scan rather than once per window. The new code is the same, except it dedupes per `cooldown_secs`. That is less inflation than before, not more. After a gas reject, `handle_execution_failure` gets `&fingerprint` as its `pair_id` argument, so `gas_log_key(fingerprint)` matches the key the loop checks, and the route stops being counted while it is gas-muted.

**Key confusion.** The keys don't collide:
- route cooldown: `cooldowns.get(&fingerprint)`
- dedupe: `fingerprint|counted`
- gas mute: `fingerprint|gaslog`
- pair cooldown: `pair_id`, which never contains `|`

The unit test asserts that counting a detection does not create the route key.

**Memory.** Entries carry an expiry, and `cooldowns.retain(expire_at > now)` prunes them at every heartbeat tick. Growth is bounded by the number of distinct fingerprints per window.

**Minor, not fixed.** The large-swap fast path clears every key containing the pair id, so `|counted` goes too. A route can be counted again right after a large swap. A large swap is a new market event, so this reads as intended.

### R3-7: QuoteCache eviction (FALSE)

- **Which entries `retain` drops.** It runs before the insert and drops entries whose stored state ≠ the pool's current `v3_price_state`, or that are past `quote_cache_max_age`. `get()` only serves entries that match the current state and are within max age. So it drops nothing the next scan could use, unless the pool returns to exactly the old `(sqrtP, L)`. That would be a negligible loss: one extra QuoterV2 call.
- **The just-inserted entry.** It can't be dropped by `retain`, because `retain` runs before the insert. The cap removes the minimum `Instant`, and the new entry has the newest timestamp.
- **Late result from an older state.** Its own entry is stored dead, and it leaves entries for the current state untouched.
- **Locking.** The PoolCache read happens before the DashMap shard write lock is taken, so there is no nested DashMap access.
- **Cap size.** Per reverse pool, the distinct amounts per state equal the number of forward legs feeding it, about a dozen. The cap of 64 doesn't bind in normal use.

### R3-8: `best=` / `spread=` (FALSE)

- The warning `opportunities.is_empty() && best_raw_usd > 0 && best_raw_usd >= min_floor` is unchanged.
- Phase 2 now feeds `best_raw_usd` only when `v3_allowed`, which is the same condition that admits the Phase-2 opportunity. So the warning can fire only for:
  - a V2/Aerodrome-only route that misses the higher per-pair `min_profit_usd`, or
  - a verified 1.5x result that misses it.
- It is not inverted and does not always fire.
- `spread=` shows `n/a` more often on V3-only edges, as already documented in round 2. `bestV=` still carries the verified losses.

## Observations (not regressions; for the fork test / later)

- **RPC.** Same-router routes add QuoterV2 calls:
  - 1.5 now fires for forwards gated only by a cross-tier spread.
  - 1.5b quotes up to (tiers − 1) extra reverse pools per gated forward. Slipstream has 6 configured spacings.
  - The QuoteCache and the `v3_spot_uncapped` filter limit this.
  - Watch the heartbeat `rpc=` summary in the fork test.
- **Diagnostics.** `pairs_with_multi` (heartbeat `cross=`) and the dashboard's `cross_count` count distinct router ids only. A pair whose only routes are same-router fee tiers shows as having no cross coverage. Display only.
- **Pre-existing, not from round 2.** In `run_multicall_answered`:
  - The decoded `McResult` count isn't checked against `chunk_len`. A malformed node reply would shift later results.
  - The sequential fallback keeps replaying after a transport error inside the fallback.
  - Neither has been seen in practice. Both predate round 2.

## Fixes (round 3)

None. Nothing CONFIRMED.
