# Token scout

`scripts/token-scout.py` finds Base tokens that are worth adding to `engine/config.yaml`. A good
candidate has deep pools on at least two venues the engine trades, is Halal-acceptable, and is
not already arbitraged by many bots.

It uses Python 3's standard library only and needs no API keys. It never reads `.env` and never
sends transactions. Data comes from the public GeckoTerminal and DexScreener APIs and from
read-only `eth_call`s on `https://mainnet.base.org`.

## Quick start

```bash
# One token: venues, pools, depth, proxy flag (~10 s)
python3 scripts/token-scout.py --check 0xacfE6019Ed1A7Dc6f7B508C02d1b04ec88cC21bf

# Full scan of the top 200 Base pools, ranked (~15–25 min, rate limits)
python3 scripts/token-scout.py --json scout.json --yaml

# Faster scan without the competition stage (~3 min)
python3 scripts/token-scout.py --no-trades

# Re-score a shortlist with an on-chain spread test (5 min per token)
python3 scripts/token-scout.py --tokens 0xAAA…,0xBBB… --persist-minutes 5
```

Progress goes to stderr. The table goes to stdout.

## What it does

1. **Universe.** Reads the top Base pools from GeckoTerminal (`--pages`, up to 10 pages of 20)
   and collects their tokens. USDC, USDT, WETH and cbBTC are the quote side and are never
   candidates.
2. **Pools.** Asks DexScreener for each token's pools with their USD liquidity and 24 h volume.
3. **Venue.** Calls `factory()` on each pool and matches the result against the
   `factory_address` values under `routers:` in `engine/config.yaml`. It never trusts the DEX
   labels, because GeckoTerminal labels Aero and Aerodrome pools inconsistently. Pools on
   known factories the engine does not cover (Aero CL/V2, Slipstream 3, Uniswap V2, Sushi V3)
   are listed as `nc:`. This is how you see what a new venue would unlock.
4. **Eligible.** A token needs `--min-pools` (2) covered pools of at least `--min-pool-usd`
   ($200k) each. Arbitrage needs two prices.
5. **Competition.** Reads the last trades of the token's top covered pools from GeckoTerminal
   (`--trade-pools`, 4):
   - `arbA` is the share of transactions that touch two or more of the token's pools, which is
     what an arbitrage looks like.
   - `arbB` is how many different senders made those transactions.

   Low A and B mean few bots compete.
6. **Persistence** (optional, `--persist-minutes N`). Every 30 s it reads each covered pool's
   on-chain price (`slot0` or `getReserves`). `P` is the share of samples where two pools
   against the same quote token disagree by more than both pools' fees combined: a gap the
   engine could have traded. It uses on-chain prices only, because API "prices" are last-trade
   prices and show fake spreads.
7. **Score.**
   `log10(L2) × min(V/TVL, 5) × (0.1 + P) / (1 + A × ln(1 + B))`
   - **L2:** depth of the second-deepest covered pool. This caps how much you can trade.
   - **V/TVL:** turnover. Volume moves prices and creates gaps.
   - **P:** gaps that persist.
   - **Denominator:** penalizes competition.

   Use the score to rank the shortlist, not as an absolute measure.
8. **Halal screen.** `scripts/token-scout-denylist.txt` matches tokens by address or symbol.
   - `deny` rows are dropped, for example lending, yield-bearing, derivatives and memes with
     hooks.
   - `flag` rows stay in the table with a note, for example staking tokens. They need your
     decision.
   - Edit the file to change policy.

## Reading the table

```
 # symbol   score  covTVL   vol24 pools  arbA arbB     P  halal  notes
 1 VVV       0.55  $12.8M  $11.0M     4  0.00    1  0.00  ok     in config; nc: Aero CL $1.9M
```

| Column | Meaning |
|---|---|
| `covTVL` | Liquidity on venues the engine trades today |
| `pools` | Number of covered pools at or above `--min-pool-usd` |
| `arbA` | Share of transactions that hit two or more of the token's pools |
| `arbB` | Distinct senders of those multi-pool transactions |
| `P` | Share of on-chain samples with a gap larger than fees (`-` = not measured) |
| `notes` | `in config` if already traded; `nc:` uncovered pools; `proxy` = upgradeable token (admin risk) |

- `--all` also prints ineligible and denied rows.
- `--check` prints every pool with its venue and address.

## Adding a token (checklist)

1. **Shortlist.** Pick `halal ok`, with 2 or more covered pools and a low `arbA` and `arbB`.
   Run `--persist-minutes 5` on your top picks.
2. **Check for a transfer fee.** The scout does not test this. Test on an Anvil fork that uses
   `ALCHEMY_TEST_KEY`, never the production key:
   1. Impersonate a pool that holds the token.
   2. Transfer an amount to a fresh address.
   3. Confirm the amount received equals the amount sent.

   A token that takes a fee on transfer breaks every route.
3. **Add the pair.** `--yaml` prints a ready `pairs:` block for eligible, allowed tokens not yet
   in config. Paste it under `pairs:` in `engine/config.yaml`.
4. **Keep it off Alchemy.** Add the token address to `chain.preconf_only_tokens`. Its pools are
   then watched on the free Flashblocks feed only, so the Alchemy event budget doesn't grow.
5. **Watch dust pools.** On a dry-run, look for `phantom skipped` warnings on the new pair. These
   come from near-empty pools. If a WETH-start pair triggers them, drop that pair: triangles
   through USDC still reach the WETH pools.

## Options

| Option | Default | Use |
|---|---|---|
| `--check ADDR` | – | One token, full pool detail |
| `--tokens A,B` | – | Analyse these instead of the top-pools universe |
| `--pages N` | 10 | GeckoTerminal pages (20 pools each, API max 10) |
| `--min-pool-usd X` | 200000 | Minimum liquidity per covered pool |
| `--min-pools N` | 2 | Covered pools needed |
| `--trade-pools N` | 4 | Pools sampled for competition |
| `--no-trades` | off | Skip the competition stage (much faster) |
| `--persist-minutes M` | 0 | On-chain spread sampling per token |
| `--top N` / `--all` | 25 / off | Rows shown |
| `--json FILE` | – | All rows as JSON, for comparing runs over time |
| `--yaml` | off | Print `pairs:` YAML for new eligible tokens |
| `--rpc URL` | mainnet.base.org | Any keyless read-only Base RPC |
| `--gt-gap S` / `--ds-gap S` | 6 / 0.3 | Seconds between API calls; raise them if you hit HTTP 429 |

## Limits

- GeckoTerminal allows about 10 calls a minute and returns at most 200 top pools. A full run with
  competition takes about 15–25 minutes. Use `--tokens` to re-check a known list.
- A trades window is the last 300 trades, which covers 1–5 h on busy pools. A result of
  `arbA = 0` is a hint, not proof.
- When a venue is added to the engine (new `factory_address` in config), re-run the scout. Pools
  that were `nc:` become covered, and new tokens become eligible.
