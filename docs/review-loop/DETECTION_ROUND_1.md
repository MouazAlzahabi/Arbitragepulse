# Detection review — round 1 (branch debug/no-opps @ a1aa77d)

Reviewer: read-only pass. Evidence came from code reading, `cargo test -p engine` (67/67 pass) and read-only
`cast call` against `https://mainnet.base.org` (Base mainnet, block ~53 145 8xx, 2026-10-27). Pool list and
slot0 snapshot: `getPool` on the Uni V3, Pancake V3 and Slipstream factories for every configured token pair
(98 pools found, 84 with L > 0).

Heartbeat under review:
`scans=94 execs=0 ok=0 | best=$0 bestV=-99.994% spread=+0.055% | quotes=118 active=12/12 cross=12 | opps=0 | v2=30 v3=12/146 | preconf_logs=85 | unfunded=12/12`

## Summary (ranked)

| id | sev | where | one-line |
|----|-----|-------|----------|
| D1 | blocker | evaluate.rs:375-384, 480-504; pool_cache.rs:158-168, 570 | The P15 gate fires on tick-capped thin pools (spread measured on the capped input) and then QuoterV2-verifies them at the full 10k / 4 WETH. That is what produces `bestV=-99.99%`. It wastes RPC every few blocks, and the engine never checks a smaller size. |
| D2 | major | config.yaml `trade_amount`; scan.rs:489-497 | Detection runs at one fixed size. The optimizer only runs on an opportunity already found, so it can never recover an arb that is profitable only below 10k. At 10k, 4 of 7 USD pairs are past the depth of their V3 pools. |
| D3 | major | strategy/mod.rs:39 + every phase (evaluate.rs:387,643,809,875; triangular.rs:483) | A 15 bps haircut applies to every output, including exact `pending` QuoterV2 quotes. At 10k it deducts $15 against about $0.02 of gas. Any route with a net spread under 0.15% after fees is dropped. |
| D4 | major (RPC) | strategy/mod.rs:741-790 | `run_multicall` treats a transport error (HTTP 429, connection reset) like a Multicall3 decode failure and replays the chunk as up to 10 sequential `eth_call`s to the same rate-limited endpoint. That amplifies RPC load 10x while throttled. The public endpoint returned 429 to this reviewer after about 300 calls. |
| D5 | minor | config.rs:546; evaluate.rs:134; triangular.rs:961 | `min_v3_liquidity = 1e9` compares raw L, which depends on token decimals. It filters $1k of liquidity on 6/6-decimal pairs but nothing on 18/18 pairs, so effectively empty WETH/AERO and WETH/VIRT pools pass. |
| D6 | minor (RPC) | evaluate.rs:699-735 | Phase 1.5c has no QuoteCache. Every gate hit re-quotes on every scan, including each swap-triggered targeted scan (about 60 a minute). |
| D7 | minor (stats) | evaluate.rs:366-373; chain/mod.rs:600-606; pool_cache.rs:439-456; scan.rs:500-523 | The diagnostics mislead. `spread=` includes capped thin-pool spreads, `bestV=` shows only thin full-size verifications, `v3=x/y` is cached between updates, and `opps` skips candidates in cooldown or with gas-throttled logging. |
| D8 | minor | pool_cache.rs:164-166 | `cap_v3_spot_amount_in` drops the cap entirely when `vr_in*2% == 0`, so an almost-empty pool is quoted at full size on its virtual reserves. This is harmless now because the output is bounded by `vr_out`, but the comment says the opposite. |
| R* | refuted | — | Checked and not bugs: V3 freshness (`v3=12/146`) is not a detection gate; the Slipstream and Uniswap quote encoding is correct; CountLayer keeps pubsub; the on-chain router allowlist and types are correct; a fresh `trades-base.db` is harmless; the `recheck_quiet_pools` decode is fine. |

**Bottom line.** Deep pools never cross the 0.005% gate, so `bestV` only ever comes from thin pools. In the
slot0 snapshot, the deep low-fee pools for USDC/WETH sat within about 0.03% of each other while the fees on
the cheapest pair of venues total about 0.06%. At a $10k size the market really is efficient across deep pools.
The code-side suppressors are D1, D2 and D3: one oversized trade size, no smaller-size search, and a haircut
proportional to size. D1, D4 and D6 waste RPC on a rate-limited public endpoint.

---

## D1 — blocker: P15 gate verifies capped thin pools at full size (source of bestV = -99.99%)

**Code.**
- `pool_cache.rs:570` `quote_v3_spot_key` caps the input to `v3_spot_max_vr_fraction_bps` (2%) of the
  input-side virtual reserve and returns `(out_for_capped, L, effective_in)`.
- `evaluate.rs:145` stores `capped_amount_in: effective_in`. `evaluate.rs:313` builds the reverse task with
  `amount_in: q_a.capped_amount_in`, so the Phase 2 spread is measured on the capped input.
- `evaluate.rs:380-384`: the gate fires for any V3 forward with spread > 0.005% and never checks
  `capped_amount_in < resolved.amount_in`. The comment at 376-379 says this is intentional.
- `evaluate.rs:480,504`: the gated pool is then QuoterV2-quoted at `full_amount = resolved.amount_in`.
  A pool whose local state already shows it cannot absorb the size returns close to 0.
- `evaluate.rs:597-605`: for every gated forward, Phase 1.5b also queues reverse quotes on every other
  V3 router's pools for the pair.
- QuoteCache (`quote_cache_max_age` is 3 blocks) only throttles the waste: a thin pool's state rarely
  changes, so it is re-quoted about every 6 s forever.

**Evidence (cast).** Spot price and 2% cap per pool, computed from `slot0` and `liquidity` (script output kept
in the scratchpad). Examples of pools that pass `min_v3_liquidity` while being off-market and too small to
absorb the trade:

| pool | L | spot vs deepest | 2% cap of vr_in | full-size QuoterV2 |
|------|---|-----------------|-----------------|--------------------|
| WETH/AERO Uni V3 0.01% `0x18eC0aFa…D8BA` | 2.8e15 | AERO 0.88% cheaper | < 0.01 WETH | 4 WETH → **1.428 AERO** (about $0.95 against $9.2k in: −99.99%) |
| USDT/WETH Pancake 0.05% `0xEED856fC…e51e` | 5.6e10 | +1.3% | 0.05 USDT | 4 WETH → **0.28 USDT** |
| USDC/USDT Uni V3 1% `0x5d165468…Ca4d` | 2.4e5 | +4.2% | 0 | 10k USDC → 0.04 USDT (filtered by min L) |

```
cast call 0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a "quoteExactInputSingle((address,address,uint256,uint24,uint160))(uint256,uint160,uint32,uint256)" \
  "(0x4200000000000000000000000000000000000006,0x940181a94A35A4569E4529A3CDfB74e38FD98631,4000000000000000000,100,0)" --rpc-url https://mainnet.base.org
# 1428447808087816239  (1.428 AERO for 4 WETH; 0.005 WETH gives 1.4267 AERO, so the quote saturates)
```
In the WETH→AERO case, the capped local leg (about 0.005 WETH) priced 0.88% under market with a 0.01% fee,
reversed through a deep pool at 0.3%, shows about +0.5% and fires the gate. The full-size quote then gives
−99.99%. This matches the heartbeat's `bestV=-99.994%`. `bestV` is the maximum over all verified results in
the window, so −99.99% means no deep pool was verified at all in that window.

**Effect.**
- RPC: about 10 QuoterV2 calls a minute per persistently gated thin pool, plus Phase 1.5b fan-out to every
  other V3 pool on the pair. All of it goes to the rate-limited public endpoint (see D4).
- Detection: when a capped pool does show a real edge at its capped size, the engine never checks that size.
  It only tests 10k, which fails.
- Diagnostics: `bestV` and `spread` describe junk pools (D7).

**Fix direction.** In the gate, skip forwards with `capped_amount_in < amount_in`. Or better, verify them at
`capped_amount_in` and emit the opportunity at that size. The executor already takes `opp.amount_in`.
Apply the same rule to the q_b_list used by Phase 1.5b.

**Proof.** Unit test: a PoolCache with a deep V3 pool A at price P and a thin V3 pool B (vr_in such that
2% < amount_in) at 0.9P on another router, one pair at 10k. Call the gate-building part of `evaluate`
(or factor it out) and assert that B's key is not in `p15_gate`, or that it is queued at `capped_amount_in`.
Today it is queued at the full amount.

## D2 — major: one fixed trade size, and the optimizer cannot find size-limited arbs

**Code.** `pair_resolved[pi].amount_in` (10 000 USDC/USDT, 4 WETH) is the only size quoted in Phase 1/2,
P15 and triangular. `scan.rs:489-497` runs `strat.optimize(opp, …)` only for an `opp` that `evaluate` already
returned, and `optimize_size: false` turns even that off. When the full size is unprofitable, nothing tries a
smaller one.

**Evidence: virtual depth at the current tick (vr_in of the token_in side) per pair, deepest V3 pool,
from slot0 and L.**

| pair | deepest V3 vr_in | 10k as % of vr | note |
|------|-----------------|----------------|------|
| USDC/WETH | Uni 0.3% about 1.8B; Uni 0.05% 45M; Slip ts100 40M | 0.02% | fine |
| USDC/cbBTC | Uni 0.05% 69M | 0.01% | fine |
| USDC/USDT | Uni 0.01% 279M | ~0 | fine |
| USDC/EURC | Slip ts50 38M | 0.03% | fine |
| **USDT/WETH** | Uni 0.3% 1.44M; Slip ts100 0.70M; Uni 0.05% 0.15M | 0.7–6.5% | too large |
| **USDC/AERO** | Aerodrome V2 reserve 3.48M; best V3 0.38M | 0.3–2.7% | too large |
| **USDC/VIRTUAL** | Uni 0.3% 2.2M; Pancake 0.05% 0.37M | 0.45–2.7% | too large |
| **WETH/VIRTUAL, WETH/AERO (4 WETH)** | Uni 0.3% / 0.05% about 1.2k–1.7k WETH; Slip about 0.6k | 0.25–0.7% | marginal |

On these pairs, price impact on both legs (amount/vr each) exceeds any normal cross-venue dislocation
(0.05–0.3%), so they can never show an opportunity at this size. The previous run's ~4 opps happened at a
smaller funded size. `cast` check of the AERO route at three sizes (Uni 0.05% then Aerodrome V2):
1k → 992.1, 5k → 4886, 10k → 9596 USDC back. The loss grows with size, as expected.

**Proof.** Add a per-pair `[size/4, size/2, size]` probe (local math only, 0 RPC) in Phase 1/2, and
gate/verify the best size. Or run `optimize` on gate hits rather than only on found opps. Fork test: on an
Anvil fork, move USDT/WETH Slip ts100 by 0.4% with a 50k swap and run one `evaluate`. At 10k it finds nothing;
at 1–2k it should.

## D3 — major: haircut proportional to size on exact quotes

**Code.** `strategy/mod.rs:39-41`: `amount_back * (10000 − 15) / 10000`. It is applied after QuoterV2 in
Phase 1.5 (evaluate.rs:643), 1.5b (809) and 1.5c (875), on local quotes (387), and on triangles
(triangular.rs:483). The quotes are taken at `pending` on the Flashblocks endpoint, so they are already exact
for the state the cache tracks. The haircut was tuned for a $270 trade (comment at triangular.rs:584); at 10k
it is $15 per trade, against gas of 700k × ~0.01 gwei ≈ $0.02 and a 2-hop `min_profit_usd` of $0.0001.

**Effect.** Every opportunity needs a net spread (after pool fees) of more than 0.15%. Real Base
dislocations among deep pools after a large swap are usually 0.02–0.15%, so they are all discarded. It also
explains `best=$0` alongside a positive pre-haircut `spread=+0.055%`. Both legs then still go through the
on-chain `minProfit` and simulation, so the haircut is redundant protection.

**Proof.** Unit test on Phase 1.5c math: a forward giving amount_back = amount_in × 1.001 (+0.10% exact)
yields `profit_usd = 0` (dropped) at 15 bps. Fix direction: make the buffer a fixed USD or gas multiple, or a
few bps only on unverified local legs, not on QuoterV2 `pending` results.

## D4 — major (RPC): transport errors trigger a 10× sequential replay

**Code.** `strategy/mod.rs:741-750`: `p.call(tx).block(block)` → `.map(|r| r.ok())`, so a transport error
(429, 5xx, reset) becomes `Ok(None)`. Lines 764-788: `raw_opt.and_then(decode)` is `None`, which logs
"Multicall3 chunk failed (gas limit exceeded?)" and replays each of the chunk's up to 10 calls one by one, each
with its own 3 s timeout, to the same endpoint. The comment on the timeout branch already says not to do this
("No sequential fallback — the RPC is evidently stuck"); the error branch does it anyway.

**Evidence.** `https://mainnet.base.org` is both `preconf_http_rpc` and `http_rpc`, so it carries every
QuoterV2, recheck, price, balance and simulate call. During this review it returned
`HTTP 429 {"code":-32011,"message":"request limit reached"}` after about 300 `eth_call`s in about 5 minutes
from one IP, and kept rejecting `eth_getLogs`. A rate-limited engine turns each failed multicall into 10 more
requests, which keeps the limit tripped. All quotes come back `None`, so no opportunities are found, and the
quoter-health warnings blame the quoter or ABI.

**Proof.**
1. `grep -c "Multicall3 chunk failed" engine.log` and `grep -c "QuoterV2 for router"` on the AWS log.
2. Unit test: a mock `Provider` whose `call` returns a transport error. Assert that `run_multicall` with
   10 calls makes exactly 1 request. Today it makes 11.

Fix: fall back only when the multicall returned bytes that fail to decode (`Some(raw)` plus decode error),
never on a transport `Err`.

## D5 — minor: `min_v3_liquidity` is a raw-L threshold that depends on decimals

`config.rs:546` sets 1e9; it is used at evaluate.rs:134 and triangular.rs:961. Raw L scales as
√(amount0·amount1) in token units, so 1e9 is about $1k on USDC/USDT (6/6) but tiny on 18/18 pairs. Measured:
WETH/AERO Uni 0.01% L=2.8e15 (vr < 0.5 WETH), WETH/VIRTUAL Pancake 0.01% L=7.4e18 (vr ≈ 0), and USDC/cbBTC
Uni 1% L=1.0007e9 (just above the threshold, $28k vr) all pass. These are the pools that feed D1. Fix: filter
on vr_in in token_in units (for example `capped_amount_in == amount_in`) instead of raw L.

## D6 — minor (RPC): Phase 1.5c bypasses QuoteCache

evaluate.rs:699-735 pushes a fresh QuoterV2 call for every Aerodrome-forward / V3-reverse task above the gate,
on every scan, while 1.5 and 1.5b use `quote_cache`. With about 30 block scans and about 60 swap-targeted
scans a minute, one persistent gate hit costs about 90 calls a minute. Proof: a unit test (or a debug counter)
that runs `evaluate` twice with unchanged pool state and counts the 1.5c multicall entries: 2 today, 1 expected.

## D7 — minor (stats): diagnostics that mislead the investigation

- `spread=`: max of the Phase 2 spread on capped inputs (evaluate.rs:369-373), so it shows the thin-pool
  phantom, not a tradable edge.
- `bestV=`: only thin pools are ever verified (D1), so −99.99% says nothing about the market. Consider
  logging the best verified spread among non-capped forwards, plus a count of gate hits split capped/uncapped.
- `v3=12/146` (pool_cache.rs:439-456): `fresh` is recomputed only when a V3 update set the dirty flag. Pools
  aging out of the 30 s window are not reflected until the next update. It is display-only; see R1.
- `opps` (scan.rs:500-523 and the cooldown `continue` at ~421-440): a candidate skipped by a fingerprint or
  pair cooldown, or logged at debug because it was "recently below gas", is not counted, and candidates
  dropped by `recheck_quiet_pools` are not counted either. "opps=0" therefore means "0 candidates reached
  logging", not "0 detected". Fine as a deliberate choice, but the dashboard label should say so.

## D8 — minor: cap bypass on near-empty pools

`pool_cache.rs:164-166` returns the full `amount_in` when `vr_in × 200 / 10000 == 0`. The doc comment says the
cap exists so near-empty ticks don't overestimate. Output is bounded by `vr_out` (about 0), so this is
harmless today. A test asserting `effective_in ≤ vr_in` documents the intent.

---

## Refuted / verified OK

- **R1: `v3=12/146` is not a detection gate.** `quote_v3_spot_key` (pool_cache.rs:545-582) has no time check.
  The doc comment explicitly removes the TTL, and discovery seeds slot0+L at startup. All 146 V3 pools are quoted
  every scan whatever their age. The V2 feed_live fix is not needed for V3.
- **R2: QuoterV2 encoding is correct.** Live checks, USDC→WETH 10k:
  Uni 0.05% `0x3d4e…` → 4.3202 WETH; Slipstream ts100 `0x254c…` (int24 tickSpacing ABI) → 4.3211 WETH;
  Pancake 0.05% `0xB048…` → 4.3191 WETH; `--block pending` matches. `decode_v3_quote` decodes the same 4-word
  return for both ABIs (unit test `slipstream_quote_uses_int24_…` passes).
- **R3: CountLayer keeps pubsub.** alloy-rpc-client 1.7 `RpcClient::new_layered` (client.rs:108-117)
  downcasts the unlayered `PubSubFrontend` before layering, so `subscribe_logs` and `subscribe_blocks` keep
  working (consistent with `preconf_logs=85` and live quotes).
- **R4: on-chain allowlist is correct.** `0x6dE168a6…3Ff2`: `allowedRouters` is true for all 4 routers.
  `routerType`: SwapRouter02 = 1 (V3), Pancake SmartRouter = 1 (V3), Aerodrome = 4, Slipstream = 5
  (enum `V2,V3,RetiredSolidly,RetiredSyncSwap,Aerodrome,Slipstream`).
- **R5: a fresh `trades-base.db` is harmless.** On startup the db is only used by `get_chain_stats`
  (live rows, `dry_run=0`) to seed attempts, successes and profit. Cooldowns, disabled pairs, scans and opps
  are in-memory only. (Side note: disabled pairs set from the dashboard are lost on restart.)
- **R6: `recheck_quiet_pools` decode is OK.** Aerodrome `getReserves()` returns
  `(uint256,uint256,uint256)`, and decoding it as `IUniswapV2Pair (uint112,uint112,uint32)` works because the
  values fit (AERO reserve 5.2e24 < 2^112). Reading at `pending` matches a cache fed by pendingLogs. A real
  mismatch drops the candidate, which is conservative and does not suppress anything when no candidate exists.
- **R7: native price.** Read from `reads` (HTTP) every 60 s through Uni/Pancake QuoterV2 at 0.01 ETH. It works
  as long as the endpoint is not 429-ing (D4); on failure the last value is kept.

## Not verified (needs AWS log or WS access)

- Whether Base `pendingLogs` sets `blockNumber`/`logIndex`. If either is null, `repeat=false` and a V3
  Mint/Burn would be applied twice (preconf, then sealed), corrupting L (listener.rs:67-75, 140-160).
  Check with `websocat wss://mainnet-preconf.base.org` and
  `{"id":1,"method":"eth_subscribe","params":["pendingLogs",{"address":"0xd0b53D9277642d899DF5C87A3966A349A798F224"}]}`.
- Whether production hits 429 (D4): grep the AWS log for `Multicall3 chunk failed`, `timed out`, `QuoterV2 for router`.

## Verification
Verifier pass: code reading plus `cargo test -p engine` (67/67 pass) and 6 read-only `cast` calls against
`https://mainnet.base.org` on 2026-10-27. After that the endpoint returned `-32011 request limit reached` to a
single `eth_getLogs`, which is itself evidence for D4. D3 was confirmed earlier and is not re-checked here.

| id | verdict | one-line |
|----|---------|----------|
| D1 | **CONFIRMED** (the "crowd-out" part is FALSE as written) | Gate decides on the capped input, then Phase 1.5 quotes `resolved.amount_in`. The gate set has no limit, so deep pools are not excluded; they only compete for the same rate-limited RPC budget. |
| D2 | **CONFIRMED** | `optimize_size: false` (config.yaml:67). Even when it is on, `optimize` runs only on opportunities that were already found (scan.rs:490-497). |
| D4 | **CONFIRMED** | A transport error, 429 included, becomes `Ok(None)`, and each chunk is then replayed as up to 10 sequential `eth_call`s to the same endpoint. |
| D5 | **CONFIRMED** | Raw-L threshold 1e9 means about $1k only on 6/6 pairs. On 18/18 and 6/18 pairs it is effectively off (cents). |
| D6 | **CONFIRMED** | 1.5c never reads or writes `quote_cache`. It costs one QuoterV2 call per gate hit per scan. |
| D7 | **CONFIRMED** | `spread=`, `bestV=`, `v3=` and `opps=` are each misleading in specific ways (listed below). |
| pendingLogs | **CONFIRMED in code, UNCLEAR in practice** | A position-less Mint/Burn is applied, and its sealed copy is applied again. Whether Base actually omits `blockNumber`/`logIndex` could not be probed (429). |

### D1 — CONFIRMED
**Trace.**
- evaluate.rs:132-149: a V3 forward task stores `amount_in: spot_out` and `capped_amount_in: effective_in`. `effective_in` is `min(amount_in, 2%·vr_in)` (pool_cache.rs:158-168, 570).
- evaluate.rs:239: the reverse leg is quoted on `token_out_amount = q_a.amount_in`, which is the output of the capped input.
- evaluate.rs:312: `ReverseTask.amount_in = q_a.capped_amount_in`.
- evaluate.rs:380-384: the gate compares `amount_back` with the capped `task.amount_in` and inserts `(pair, router_a, fee_a)` into `p15_gate`.
- evaluate.rs:474-504: for each gate entry, `full_amount = resolved.amount_in` (line 479) is QuoterV2-quoted (line 503).
- evaluate.rs:574-604: every other V3 router of the pair is then queued in 1.5b with the (tiny) `quoter_out`.

**Crowd-out.** `p15_gate` is a `HashSet` with no limit (evaluate.rs:340). Every V3 forward whose spread exceeds the gate goes into the batch, so a thin hit does not displace a deep one in code. The real coupling is indirect: thin hits fill multicall chunks (10 calls per chunk, `rpc_concurrency: 3`). Under a 429, a chunk that fails takes its deep-pool entries with it (all None), and D4's replay worsens the rate limit. So "crowd-out" is FALSE as a mechanism but TRUE as an RPC-budget effect.

**bestV = -99.99% explained (cast, 2026-10-27).**
- Pool: Uni V3 WETH/AERO 0.01% = `0x18eC0aFa38E54850f51f03A673352DDd0FFFD8BA`.
- `liquidity()` = 2.83e15, which passes `min_v3_liquidity` 1e9.
- `slot0.sqrtPriceX96` = 4.676e30, token0 = WETH.
- From these: vr_WETH = L·Q96/sqrtP ≈ 4.8e13 wei (0.000048 WETH), vr_AERO ≈ 0.167 AERO. The 2% cap is about 1e12 wei of WETH.
- QuoterV2 `0x3d4e…B76a` for 4 WETH, fee 100, returns `1428447808087816239` (1.428 AERO), crossing 12 ticks.
- Phase 1.5 then reverses 1.428 AERO through Aerodrome or another V3 pool and gets about 0.0004 WETH back. `verified_spread` ≈ -0.9999.
- `best_verified_spread` is a max over all 1.5/1.5b/1.5c results in the heartbeat window (scan.rs:322-327, chain/mod.rs:594). A value of -99.99% therefore means only thin-pool verifications ran in that window. No deep forward hit the gate and no 1.5c ran.

**Fix (minimal).** Gate only forwards whose input was not capped. If the cap binds, the trade is at least 2% of current-tick virtual reserves, so price impact on that leg is at least about 2%, which is larger than any cross-venue dislocation. The full-size route is not executable at spot.
- evaluate.rs:380-384. Replace with:
  ```rust
  let full_in = self.pair_resolved.get(task.pair_idx).and_then(|o| o.as_ref()).map(|r| r.amount_in);
  if p15_gate_hit && matches!(task.router_a_type, RouterType::V3) && full_in == Some(task.amount_in) {
      p15_gate.insert((task.pair_idx, task.router_a_id.clone(), task.fee_a));
  }
  ```
  `task.amount_in` here is `q_a.capped_amount_in` (line 312).
- evaluate.rs:576-579 (1.5b `q_b_list`). Also drop V3 reverse pools that are capped on `quoter_out`:
  ```rust
  .filter(|t| !matches!(t.router_type, RouterType::V3) || t.rev_v3_key.as_ref()
      .and_then(|k| self.pool_cache.quote_v3_spot_key(k, token_out, quoter_out))
      .is_some_and(|(_, _, eff)| eff == quoter_out))
  ```
- Caveat: the cap uses current-tick L only. A pool with a thin current tick but deep neighbouring ticks would be skipped. This is acceptable, because those pools also give bad spot math today.
- Alternative (larger change, serves D2): verify at `capped_amount_in` and emit the opportunity at that size when local profit at that size is at least `min_profit_usd`. Not recommended as the first step, because 2% of a thin pool's vr is dust.

**Unit test.** Factor the gate test into a pure fn in evaluate.rs, then add a `#[cfg(test)] mod tests` that asserts:
```rust
pub(super) fn p15_gate_eligible(amount_back: U256, capped_in: U256, full_in: U256, fwd: &RouterType, mul: u64, den: u64) -> bool
```
- (a) `capped_in < full_in` with a +0.5% spread → `false`. This is the WETH/AERO case.
- (b) `capped_in == full_in` with +0.01% → `true`.
- (c) `capped_in == full_in` with +0.004% → `false` (gate 0.005%).
- (d) a V2 forward → `false`.
- For the 1.5b filter, a PoolCache test: insert a V3 pool with vr_in = 100 units, quote 10 units, and assert `effective_in < amount_in`. This documents the predicate the filter relies on.

### D2 — CONFIRMED
- config.yaml:67 `optimize_size: false`.
- scan.rs:490 `if optimistic_submission || !cfg.optimize_size || skip_execution { opp.clone() }`.
- `optimize` (optimize.rs:30-60) probes 10%–200% of `opp.amount_in`, but only for an `ArbOpportunity` that already passed the profit check at full size.
- Phase 1/2 (evaluate.rs:71 `amount_in = resolved.amount_in`), P15 (`full_amount`, line 479), 1.5c (line 709) and triangular (triangular.rs:463, `trip.amount_in`) all use the single configured `trade_amount` (10000 or 4 WETH, config.yaml:156-275).
- An arb that is profitable only below 10k is invisible.

**Fix.** In evaluate.rs Phase 1 (lines 71-160), evaluate each pair at `[amount_in/4, amount_in/2, amount_in]` using local math only (0 RPC). Carry the size on `ForwardTask`/`ReverseTask`, and gate and verify the best size (`full_amount := task size`). The cheaper interim step: set `trade_amount` per pair to about 1–2k for USDT/WETH, USDC/AERO and USDC/VIRTUAL in config.yaml.

**Test.** A PoolCache with two V2 pools at prices P and 1.004·P and reserves of about 300k: assert that the route is profitable at 2.5k and not at 10k, and that the multi-size evaluation selects 2.5k.

### D4 — CONFIRMED
- strategy/mod.rs:746-749: `timeout(..., p.call(tx).block(block)).await.map(|r| r.ok())` turns any RPC error (HTTP 429 `-32011`, 5xx, connection reset) into `Ok(None)`.
- mod.rs:764: `raw_opt.and_then(decode)` is then `None` → lines 775-783 log "Multicall3 chunk failed (gas limit exceeded?)" and issue `chunk_len` (up to `multicall_chunk_size` = 10, config.yaml:322) sequential `eth_call`s to the same provider, each with its own 3 s timeout (config.yaml:323).
- Only the timeout branch (mod.rs:756-762) skips the fallback.
- No 429 classification exists in this path. `classify_rpc_failure` is used only on execution failures (scan.rs:1620-1640).

**Quantified.** Per failed chunk: 1 + 10 = 11 requests instead of 1. A scan with P15 + 1.5b + 1.5c each at one chunk becomes 33 requests instead of 3. At about 30 block scans per minute plus targeted scans, a rate-limited engine sends up to about 10× its normal QuoterV2 traffic into an endpoint that is already refusing.
- Wall-clock: the replays are sequential within a chunk. A 429 returns fast, so the stall is small, but a slow 5xx can cost up to 10 × 3 s = 30 s per chunk.

**Fix.** mod.rs:746-749: keep the error so the caller can tell a transport error from a revert:
```rust
.map(|r| r.map_err(|e| e.to_string()))   // Ok(Ok(raw)) | Ok(Err(msg)) | Err(timeout)
```
In the flatten loop, fall back sequentially only when the call returned `Ok(raw)` that failed to decode, or when the error message is an execution revert or out-of-gas. For a transport error (or `classify_rpc_failure(msg) != RpcFailure::None`), `out.extend(repeat_n(None, chunk_len))`, the same as the timeout branch.

**Test.** A `#[tokio::test]` with an alloy `Asserter` mock transport (`ProviderBuilder::new().connect_mocked_client(asserter)`): push one `push_failure_msg("request limit reached")` and assert that `run_multicall` returns 10 `None`s and that the asserter has no further requests consumed. Today it would consume (and fail on) 10 more.

### D5 — CONFIRMED
- config.rs:546 / config.yaml:301 set `min_v3_liquidity: 1e9`. It is compared with raw `L` at evaluate.rs:134 and triangular.rs:959.
- Raw L is √(x·y) in base units, so vr_in ≈ L·√(price_raw):
  - USDC/USDT (6/6, price ≈ 1): L = 1e9 → about $1k per side.
  - USDC/WETH (6/18): vr_USDC = L·√(4e-9) ≈ L·6.3e-5 → L = 1e9 → $0.06.
  - WETH/AERO (18/18): 1e9 wei.
- Measured: the WETH/AERO 0.01% pool has L = 2.83e15 (above 1e9) and vr_WETH = 0.000048 WETH. It passes and is the D1 trigger.
- **Real effect:** the filter is a no-op on every non-6/6 pair, and that lets dust pools reach Phase 2 and the P15 gate.

**Fix.** The D1 gate fix (`capped_in == full_in`) is the dimension-correct version of this check and makes the raw threshold irrelevant to RPC load. To also keep dust pools out of the Phase 2 metrics, change evaluate.rs:134 to `if liquidity < min_v3_liquidity() || effective_in < amount_in / 50 { v3_cached += 1; continue; }`, which drops pools where the input is more than 50× the 2% cap. Make the same change at triangular.rs:959.

**Test.** A PoolCache with a V3 pool L = 3e15 at WETH/AERO-like sqrtP: assert that a 4 WETH forward quote returns `effective_in < 4e18 / 50`, and that this pool is excluded.

### D6 — CONFIRMED
- evaluate.rs:695-734 pushes `encode_quote(..., task.token_out_amount)` into `p15c_mc` for every V2/Aerodrome-forward, V3-reverse task above the gate.
- evaluate.rs:856-862 decodes it, with no `quote_cache.get` before and no `quote_cache.insert` after. Phases 1.5 (lines 488 and 551) and 1.5b (lines 594 and 794) both use the cache.

**Quantified.** One QuoterV2 call per gated 1.5c route per scan (block scans plus swap-targeted scans of that pair). With the cache, a route on unchanged pool state would cost one call per `quote_cache_max_age` (3 blocks).

**Mitigating note.** A thin V3 reverse pool cannot trigger 1.5c. Its local reverse quote is capped (evaluate.rs:288-292), so `local_back` is underestimated and the gate fails. 1.5c hits are therefore deep-pool hits. They are not wasteful in themselves, only repeated.

**Fix.** Mirror 1.5b. Before line 727, compute `key = V3PoolKey::new(&task.router_b_id, task.token_out, task.token_in, task.fee_b)` and look up `self.quote_cache.get(&self.pool_cache, &key, task.token_out_amount)`:
- On a hit, push `Some(v)` into a `p15c_pre` vec and skip the MC push.
- On a miss, push `None` and record `v3_price_state(&key)`.
- In the result loop (line 856), merge as P15 does and `insert` on success, or insert a zero when the batch otherwise worked.

**Test.** QuoteCache-level: after `insert(key, amt, state, v)` with unchanged pool state, `get(key, amt)` returns `Some(v)`. Better: factor the 1.5c candidate builder into a fn that takes `&QuoteCache` and returns `(mc, pre)`, call it twice on the same state, and assert that the second `mc.len() == 0`.

### D7 — CONFIRMED (display only)
- **`spread=`** (evaluate.rs:366-373) is the max Phase 2 spread over all tasks, measured on the capped input, before the haircut. Phase 1.5x only raise it with post-haircut values (lines 655, 822, 884). The heartbeat's `+0.055%` can therefore be a dust-size thin-pool spread, and it mixes pre- and post-haircut values.
- **`bestV=`** is the max verified result across 1.5/1.5b/1.5c. Because of D1 it is dominated by full-size thin-pool quotes, and it is `n/a` when nothing ran. -99.99% says nothing about deep pools.
- **`v3=x/y`** (pool_cache.rs:439-456): `fresh` is recomputed only when `v3_metrics_dirty` is set by an update. Pools that age past the window without new events keep counting as fresh, so the count goes stale high.
- **`opps=`** (scan.rs:500-507) counts only candidates that reached logging and were not `gas_log_throttled`. Candidates skipped by cooldown or fingerprint earlier in the loop, and candidates dropped by `recheck_quiet_pools`, are not counted.

**Fix.**
- Compute `spread=` only over uncapped tasks (`task.amount_in == full_in`) and apply the same pre-haircut basis everywhere, or log `spread_capped` separately.
- Report `bestV` only over uncapped verifications, and add `gate=N/M` (uncapped/capped hits).
- For `v3=`, always recompute when called from the heartbeat (cost is O(146)).
- Rename `opps` to `opps_logged` or add `opps_skipped`.

**Test.** For `count_fresh_v3_pools`, insert a pool with `last_updated` older than the window and no dirty flag, and assert `fresh == 0`. This test fails today.

### pendingLogs Mint/Burn double-apply — CONFIRMED in code, UNCLEAR in production
listener.rs:67-74:
```rust
let repeat = match log.block_number.zip(log.log_index) {
    Some(pos) => match cache.advance_log_position(pool, pos) { Less => return None, Equal => true, Greater => false },
    None => false,
};
```
Trace when Base `pendingLogs` (chain/mod.rs:1042 → 1070, the same `apply_log_to_cache`) delivers a Mint with `block_number` or `log_index = None`:
1. Pending copy: `repeat = false`, and `advance_log_position` is not called. Lines 140-148 run `apply_v3_liquidity_delta(+amount)`.
2. Sealed copy from `eth_subscribe logs`: `(block, idx)` is `Greater` than the stored position, so `repeat = false` and `apply_v3_liquidity_delta(+amount)` runs again.

**Result:** in-range L is overstated by `amount` (Burn: understated, saturating) until the next Swap log for that pool rewrites `liquidity` absolutely (lines 117-128). For Swap and Sync, the double apply is harmless (absolute state) apart from a second scan trigger. If the pending copy does carry a position, the sealed copy is `Equal`, so `repeat = true` and it is skipped correctly.

The existing test `logs_without_position_still_apply` (listener.rs:557) covers only Sync, and `v3_mint_burn_update_in_range_liquidity_once` always sets positions. Whether Base's Flashblocks `pendingLogs` omits `blockNumber`/`logIndex` could not be checked: HTTP `eth_getLogs` on `pending` was rate-limited. To check it, use `websocat` as described in Open questions.

**Fix (safe whichever way Base behaves).** In listener.rs:139 and 150, apply Mint/Burn only when the log has a position: `if !repeat && log.block_number.is_some() && log.log_index.is_some()`. A position-less Mint is then ignored, and the sealed copy, which always carries a position, applies it exactly once. The cost is up to one block (about 2 s) of delay on the liquidity delta.

**Test.** In listener.rs tests, build `mint(100,200,500,10,1)` with `block_number = None` and `log_index = None`, apply it, then apply the positioned copy, and assert `liq == 1_000_500`. Today the result is `1_001_000`.

## Fixes (round 1)

Branch `debug/no-opps`, one commit per fix. `cargo test -p engine`: 75/75 pass (67 before + 8 new).
`cargo build -p engine` is green with the same 11 pre-existing warnings. `cargo clippy -p engine` adds no new
warnings on changed lines; the `type_complexity` hit on the 1.5c candidate tuple predates this round.

| fix | commit | change | test |
|-----|--------|--------|------|
| D1 | 545f742 | `p15_gate_eligible`: the P15 gate needs a V3 forward, spread > gate, and `capped_in == full_in`. 1.5b skips V3 reverse pools whose spot quote for `quoter_out` is capped (`v3_spot_uncapped`). | `p15_gate_skips_capped_forward_legs` (cases a–d), `v3_spot_uncapped_detects_tick_capped_reverse_pools` |
| D3 | 78dc381 | `quoter_haircut_bps` 15 → 2 (default and config.yaml, with the rationale in a comment). Stale "15 bps" comments updated. | `default_haircut_keeps_a_ten_bps_exact_edge` |
| D4 | 9fa16e2 | `run_multicall`: a transport/RPC error (rate limit, 5xx, reset, or any non-revert error) fails the chunk, as the timeout branch does. Per-call fallback only on a reverted or out-of-gas aggregate, or undecodable bytes. | `rpc_error_does_not_replay_chunk`, `reverted_aggregate_falls_back_per_call` (Asserter) |
| D6 | 30531c9 | Phase 1.5c pre-checks `QuoteCache` and inserts results with the sent-state; ZERO is cached only when the batch otherwise worked. | `p15c_second_scan_on_unchanged_state_hits_quote_cache` |
| pendingLogs | 884fe20 | V3 Mint/Burn deltas are applied only from logs that carry both `block_number` and `log_index`. | `positionless_mint_is_applied_once_via_its_sealed_copy` |
| visibility | f9eacc6 | `recheck_quiet_pools` drops are logged at info, naming each pool with cached vs on-chain reserves (or "unreadable"). The heartbeat shows a cumulative `recheck_drop=N` when N>0, threaded like `opps_found`. | none (needs a full `Strategy`; build-checked) |
| D7 (v3=) | 626eb40 | `count_fresh_v3_pools` recomputes on every call. Removed the dirty flag and cached atomics. | `fresh_v3_count_reflects_pools_aging_out_without_updates` |

Not changed:
- **triangular.rs (~959).** It does not have the same defect. A triangle whose first V3 leg is capped is scored
  *and* QuoterV2-verified at `effective_amount_in`, the capped size (`TriangularOpportunity.amount_in`). It is
  not verified at full size, so no wasted full-size call is made. A capped middle leg only underestimates output.
- **D2** (multi-size probing) is parked for the user. D5, D8, and the other D7 items (`spread=`/`bestV=`/`opps`
  semantics) are not addressed.
