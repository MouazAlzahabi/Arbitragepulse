# Detection review, round 4 ("no opportunities" after the pending block-time fix)

Scope: main at d85cec1, after the pending block-time fix. The server dry-run ran from 00:17 to about 01:30 UTC on 2026-10-31 and found `opps=0`, with `qmc=0` throughout. The question: is the engine missing real opportunities, or is the market flat?

Method:
- Read the code paths behind the heartbeat fields.
- Run three throwaway read-only scanners against live Base (publicnode, `latest`, one Multicall3 call per poll, about one poll per block). Each pulled `slot0` and `liquidity` for every V3-style pool on the configured token pairs. That covered 138 live pools across Uniswap V3, Pancake V3, Slipstream, Slipstream 3 and Aero CL.
- No engine run, no fork, no transactions.

Result: **no major issue. Phase 1 matches the live chain, and the market had no gap above fees.** No code changes.
- `cargo test -p engine`: 98/98 pass.

## Findings

| # | question | verdict |
|---|---|---|
| R4-1 | Did the block-time override (d85cec1) suppress opportunities? | **FALSE** |
| R4-2 | Is the engine's local 2-hop view (`spot2`) wrong? | **FALSE** |
| R4-3 | Is the Phase 1.5 tick-cap gate blocking deep pools at the new sizes (10K / 5 WETH)? | **FALSE** |
| R4-4 | Is the triangular view (`tri`) wrong? | **FALSE** |
| R4-5 | Does the dust-pool gap (WETH/VIRTUAL `uni:100`, +3%) leak into detection? | **FALSE** |

### R4-1: the override is not involved (FALSE)
- The override changes only the pending eth_calls: Phase 1.5 quotes, the sims and the optimizer.
- `qmc=0/0/0/0` on every heartbeat means no Multicall3 quote chunk was sent at all. No candidate reached the stage the override changes.
- On the server, the startup probe logged `Pending calls carry the chain's block time (override honoured)`, and there was no clock warning.

### R4-2: `spot2` matches an independent copy of the engine math (FALSE)
The emulator copies `quote_v3_spot_key` and the evaluate.rs Phase 2 filters:
- virtual reserves, with input capped at 2% of `vr_in` (`v3_spot_max_vr_fraction_bps` = 200);
- xy=k with the `fee()` ppm;
- L ≥ 1e9 (`min_v3_liquidity`);
- the 5% phantom cap;
- configured sizes per pair.

Over 84 polls (3 min), the best route excluding dust pools was **USDC/USDT slip:1 → aerocl:1 at −0.0052%**. The server's `spot2` was −0.005%, so they agree to within 0.0002%. Best per pair, with both legs at full size:

| pair | best | route |
|---|---|---|
| USDC/USDT | −0.0052% | slip:1 → aerocl:1 |
| WETH/cbBTC | −0.0070% | aerocl:10 → aerocl:1 |
| USDC/EURC | −0.0407% | aerocl:1 → slip:1 |
| USDC/WETH | −0.0558% | aerocl:50 → aerocl:100 |
| USDC/cbBTC | −0.0652% | uni:500 → aerocl:50 |
| others | −0.2% … −7.6% | |

The Phase 1.5 gate needs > +0.005%, and no deep route reached it.

### R4-3: the tick cap does not block deep pools (FALSE)
- At the configured sizes, 77 of 162 pools take the full amount uncapped.
- Every deep pair has several uncapped venues. Examples:
  - USDC/WETH: uni 100/500/3000/10000, pcs 100/500, aerocl 50/100, slip3 50 and more;
  - USDC/USDT: uni 100/500, pcs 100, slip 1, aerocl 1;
  - WETH/cbBTC: 12 pools.
- The capped pools are thin. Their full-size output would be far worse anyway.

### R4-4: `tri` is consistent (FALSE)
- Setup: chained spot math at full size on every leg, starting from USDC 10K, USDT 10K and WETH 5, over every triangle the pool set allows.
- Over 56 polls (2 min), the best was **USDC→WETH→cbBTC (aerocl:50 / aerocl:1 / aerocl:50) at −0.049%**. The server's `tri` was −0.033 … −0.054%.
- The engine also uses V2/Aerodrome legs, which the emulator does not, so a slightly better engine figure is expected.
- No poll was above +0.5 bp.

### R4-5: dust pools are excluded (FALSE)
- WETH/VIRTUAL Uniswap V3 1bp (`0x0e3413b5…`) shows +3% at spot, but it can take only ~6e-7 WETH before it is capped.
- Its L passes the 1e9 floor. The engine's `spot2` never showed the +3%, so the pool is excluded before Phase 2. That fits the depth floor (`min_pool_depth_usd` = 20K, `PoolCache::is_skipped`), which a pool this thin cannot meet.

## Notes (minor, no action)
- **Why the market looks flat.** The engine reads each flashblock after it is sealed. Gaps opened by large swaps are usually closed inside the same flashblock by searchers ordered right behind the swap. So top-of-flashblock state rarely shows a gap above two pool fees, especially overnight. That is a latency and ordering limit, not a detection bug. To get more chances, add venues or pairs, for example Uniswap V4 pools (not watched today), rather than loosen thresholds.
- **Aero CL fees in local math.** These use the pool's `fee()`. For hooked pools the real fee can be higher (see the pending block-time fix). The local screen can therefore be slightly optimistic for Aero CL legs. That only costs extra quotes; it never hides a route.

## Addendum: 24-hour backtest (2026-10-31, blocks ~53286000–53330300)
- **Setup.** The same engine-math emulator was run on historical state: 720 blocks, one every 2 minutes, at full size, over the same 138 pools.
- **Raw result.** 59 blocks showed a 2-hop gap above +0.5 bp. Almost all were WETH/cbBTC across the Aero CL 1/10 pools; the maximum was +0.113%.
- **Real-quote check.** Each flagged block was re-quoted at that block with the real quoters: the Aero Metaquoter, plus Slipstream and Slipstream 3 QuoterV2. **Every one is a loss.**
  - WETH/cbBTC Aero CL round trips: −0.025% to −0.395%. The listed `fee()` understates the hook fee.
  - USDC/WETH Slipstream 100 → Slipstream 3 50, the best non-hooked gap at +0.049% on spot: −0.12% to −0.20% when quoted.
- **Conclusion.** For 24 hours, no profitable 2-hop route existed at block level on the configured pairs and venues. Detection is correct. The setup has no edge at the end of each block.
