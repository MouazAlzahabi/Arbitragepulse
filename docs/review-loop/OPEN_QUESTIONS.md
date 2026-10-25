# Open questions — review loop

Things the deploy-readiness review loop found that need a human decision. Each entry: where, what,
options, recommendation. Newest at the bottom.

### [contract] Single hot key is both trader and sole withdrawer
- Where: contract/src/ArbitrageExecutor.sol:224, :260 (`onlyOwner` trade entry points), :171 (`setRouter`), :438 (`unpause`), :441-464 (withdrawals to `owner()`); contract/script/Deploy.s.sol
- What / why unsure: The engine must hold the owner key to trade, and that key can also `withdrawToken`/`withdrawETH`, `setRouter` (allowlist an attacker contract), `unpause` and `transferOwnership`. A leaked engine host or `.env` = all capital in the contract lost. It's a security-model / ops decision, not a code bug, and the fix needs a redeploy.
- Options + trade-offs:
  1. Keep one key (today). Simplest ops. Mitigation: keep only working capital in the contract and withdraw profits often. Deploy.s.sol and the README now say the engine key must be the owner.
  2. Add an `executor` (trader) role: `address public executor` + `setExecutor` (onlyOwner); gate both trade functions with `msg.sender == executor`; keep withdrawals / setRouter / unpause / ownership on a cold owner; `pause` allowed for owner or executor (engine kill-switch). Same calldata, same hot-path gas (one SLOAD replaces Ownable's). Cost: ABI change + redeploy, two keys to fund with gas ETH, and an engine startup check (engine round) must compare the signer against `executor()` instead of `owner()`. A leaked trader key can then only run trades that must end profitable through allowlisted routers, plus burn its own gas ETH.
- Recommendation: Option 2 before putting meaningful capital on mainnet. Option 1 + small balances is acceptable for the Base Sepolia stage / initial small-capital mainnet runs.

### [contract] Out-of-gas inside a V2 pair's token transfer is labelled LegFailed("<Fork>: TRANSFER_FAILED")
- Where: contract/src/ArbitrageExecutor.sol:369, :385-391 (`_mayHideOutOfGas` matches only empty / "TF" / "STF" / "SA"); engine/src/revert.rs:352-355 (`out_of_gas_override` only rewrites "vague" classes)
- What / why unsure: Verified on a Base fork: BaseSwap V2 at gas 142,500-144,250 reverts `LegFailed(1, BaseSwap, V2, "Pancake: TRANSFER_FAILED")` for what is really out-of-gas (the pair's `_safeTransfer` re-raises the inner OOG). The operator would chase a token/pair problem instead of raising `tuning.gas_limit_2hop`. Aerodrome, Uniswap SwapRouter02 and Pancake SmartRouter are correctly reported as LegExhaustedGas. Exposure today is ~nil: V2 routers are commented out in engine/config.yaml (:108-120), and the 700k/900k limits are far above real costs.
- Options + trade-offs:
  1. Contract: add fork strings to `_mayHideOutOfGas`. Needs a redeploy; brittle (each V2 fork brands its own string — "UniswapV2:", "Pancake:", "BaseSwap:"…; the first guess missed the real one).
  2. Engine: in `out_of_gas_override`, when the receipt shows ≥97% of the limit used, reclassify to OutOfGas whatever the replay class, keeping the replay text as detail. No redeploy; covers every fork. Risk: a genuine specific revert that happens to use ≥97% gets relabelled (unlikely — real reverts refund unused gas).
  3. Do nothing while V2 routers are disabled.
- Recommendation: 3 now; 2 in the engine round (or whenever a V2 router is re-enabled).
### [engine] UniversalRouter element offset in the pending-tx parser
- Where: engine/src/chain/pending.rs, `SEL_UNIVERSAL_ROUTER` arm (`elem0_start = inputs_base + offset`)
- What / why unsure: standard ABI encodes the offsets of a `bytes[]` array's elements relative
  to the word *after* the length word (alloy gives 0x20 for one element; the real UniversalRouter
  decodes it the same way). The parser adds the offset to the length word's position (its comment
  expects 0x40), so a real `execute(bytes,bytes[],uint256)` lands on the wrong word and returns
  None. Net effect today: UniversalRouter swaps in the mempool never trigger a targeted
  "pending" scan. Found while writing the round-1A parser test; not part of that plan, so not
  fixed (it is safe: wrong offsets only produce None). The test patches its calldata to the
  parser's convention and points here.
- Options + trade-offs:
  1. Fix: `elem0_start = inputs_base + 32 + offset` (one line; update the test helper to drop
     the patch). Enables UR-triggered pending scans that have never run live: more targeted
     scans and RPC reads on busy blocks; each still goes through the normal simulate-before-send.
  2. Leave as is (UR pending detection effectively off).
- Recommendation: 1, verified on the Anvil fork with a real UR calldata sample, in a later round.

### [engine] Pause after N consecutive mined reverts of any class
- Where: engine/src/chain/scan.rs (both receipt tasks); engine/src/revert.rs `RevertClass::pauses_chain`
- What / why unsure: Round 1 pauses the chain at once only on deterministic reverts (`blocked`, `router_not_allowed`). Other repeating causes — `insufficient_balance`, `invalid_route`, persistent `not_profitable` phantom edges, `out_of_gas` with a too-tight limit — still cost gas on every send with only warn lines. `not_profitable` is part of the normal steady state (engine/config.yaml:43-45), so a low N would pause a working engine.
- Options + trade-offs:
  1. Pause after N consecutive mined reverts of any class, reset on a confirmed success. Needs N picked from live revert rates.
  2. Add `insufficient_balance` and `invalid_route` to the immediate-pause list. Deterministic per pair, but pauses the whole chain for one underfunded pair.
  3. Escalate the per-pair cooldown instead of pausing the chain.
- Recommendation: measure the revert mix during the first live runs, then option 1 with N ≈ 3× the worst observed streak.

### [engine] Hot reload of pairs and routers
- Where: engine/src/chain/mod.rs config_tick; pool discovery only at startup (chain/mod.rs:161, :166); `contract_balances` built once (chain/mod.rs:202-210)
- What / why unsure: A reload swaps `strategy.pairs`/`routers` but discovers no pools, adds no `token_in` to the balance guard and doesn't check new routers against the executor allowlist, so an added pair or router never quotes. Round 1 changed the docs to "adding pairs or routers needs a restart".
- Options + trade-offs:
  1. Keep restart-required (docs now say so).
  2. On a reload that changes pairs/routers: discover pools for the new entries, rebuild the log-subscription filter, extend `contract_balances`, re-run the startup router check. Costs a discovery RPC burst mid-run and a subscription restart.
- Recommendation: 1.
- Update (review loop 2, R3-1): "never quotes" is not true when the new pair's pools are already cached (e.g. the reverse of an existing pair) — it starts trading after the 30 s reload. Its token_in was missing from `contract_balances`, which skipped the balance guard and let unfunded sends revert InsufficientBalance every 15 s. Fixed: a token missing from the cache now counts as unfunded (skipped with a warning) until a restart seeds it.

### [engine] Per-pair `routers:` lists
- Where: engine/src/config.rs `PairConfig` (now `deny_unknown_fields`); engine/config.yaml pairs
- What / why unsure: The YAML listed routers per pair, but the engine never read the key — every pair is quoted on every router. Round 1 removed the stale lists (they named routers that are no longer configured) and made unknown pair keys an error, so behaviour is unchanged. Whether per-pair venue restriction should exist is a product decision.
- Options + trade-offs:
  1. No per-pair lists (today's behaviour).
  2. Implement `routers:` as a filter in discovery and `rebuild_router_maps`. Changes which venues each pair trades (e.g. USDC/EURC would lose aerodrome-base).
- Recommendation: 1 until a specific pair needs it.

### [engine] Realised P&L: L1 data fee, receipt-based profit, gas in the DB
- Where: engine/src/chain/scan.rs receipt tasks; engine/src/db.rs trades table; engine/src/api.rs `ChainStats.total_profit_usd`
- What / why unsure: Round 1 made profit accounting net of *estimated* L2 gas and added `arb_gas_spent_usd_total{outcome}` (Prometheus) from receipts. Still missing: the OP-stack L1 data fee (not in alloy's Ethereum receipt type), realised profit from the contract's balance delta / event, and reverted-tx gas in the DB and dashboard.
- Options + trade-offs:
  1. Keep estimates + the Prometheus counter.
  2. Raw `eth_getTransactionReceipt` for `l1Fee`, new `gas_usd` / `net_usd` DB columns (migration) and dashboard fields.
  3. Switch the receipt types to op-alloy.
- Recommendation: 2 before scaling capital.

### [engine] Relabel ≥97%-gas reverts as out-of-gas whatever the replay class
- Where: engine/src/revert.rs `out_of_gas_override`
- What / why unsure: Engine half of "[contract] Out-of-gas inside a V2 pair's token transfer…" above (review E1-17). Still undecided; exposure ~nil while V2 routers are disabled.
- Options + trade-offs:
  1. Leave as is.
  2. When the receipt used ≥97% of the limit, relabel every class except NotProfitable, DeadlineExpired, Blocked, RouterNotAllowed, InsufficientBalance and InvalidRoute to OutOfGas (replay text kept in detail). Tests: LegFailed("Pancake: TRANSFER_FAILED") at 99% → OutOfGas; NotProfitable at 99% stays. Risk: a genuine InsufficientOutput / PoolNotFound at ≥97% gets relabelled.
- Recommendation: 2 when a V2 router is re-enabled.

### [engine] Dead live branches of `Executor::execute` / `execute_triangular`
- Where: engine/src/executor.rs (the parts of `execute` and `execute_triangular` after the dry-run return)
- What / why unsure: scan.rs calls these only in dry-run, so ~340 lines of live send / receipt code are unreachable and have drifted from the real live path (`prepare_*` in scan.rs: different tip, no persistence, no revert diagnosis). Round 1 only fixed the triangular dry-run tip and DB row.
- Options + trade-offs:
  1. Delete the live halves (the functions become dry-run simulators), or route dry-run through `prepare_*` + `eth_call` with a nonce rollback. Pure cleanup; merge-conflict risk with other executor work.
  2. Leave.
- Recommendation: 1 (delete) in a separate cleanup commit.

### [engine] Watch the tx by hash when the primary send fails after the secondary broadcast
- Where: engine/src/chain/scan.rs send-error arms (2-hop and triangular)
- What / why unsure: Round 1 treats a rate-limit / server error on the primary, after the signed tx already went to the secondary submission RPCs, as "nonce consumed" (no nonce reuse). The tx itself is still not watched: if it lands, its success/revert is missing from stats, DB and revert diagnosis, and it is logged as a send failure.
- Options + trade-offs:
  1. Leave (rare undercount).
  2. Compute the hash from `raw_tx` (keccak256 of the bytes) and spawn the normal receipt task for it instead of counting a send failure.
- Recommendation: 2 if rate-limited primary sends show up in production logs.

### [repo] Verify skill update is untracked (.claude/ is gitignored)
- Where: .claude/skills/verify/SKILL.md; .gitignore:23 (`.claude/`)
- What / why unsure: Engine round 1 updated the local verify skill (owner storage cheat before start, mandatory API_KEY=verifykey, exit-code contract 2/1/75, router allowlist checks). Because `.claude/` is gitignored, that update exists only on this machine: it isn't in the branch history, and a fresh clone or another operator gets the old (now wrong) recipe or none.
- Options + trade-offs:
  1. Track the skill: add `!.claude/skills/` (or just `!.claude/skills/verify/`) to .gitignore and commit it. Shares the safe-run recipe with the repo; must stay free of secrets (it is today — it only names Anvil test keys by index and reads ALCHEMY_KEY from .env without printing it).
  2. Keep it local-only. No risk of committing other `.claude/` state, but the recipe drifts from the code and is lost with the machine.
- Recommendation: 1, tracking only `.claude/skills/` so settings and local state stay ignored.

### [engine/dashboard] Persist operator controls across restarts (Pause, Dry Run switch, pair toggles)
- Where: engine/src/api.rs `engine_pause` / `engine_dry_run` / pair toggle (flags live only in `EngineState`); engine_restart exit(75); dashboard.jsx Controls + Tokens tab
- What / why unsure: Review D1-3. Every restart (dashboard Restart, crash, 5-min block-stall exit 1) comes back unpaused, in DRY_RUN from the env file, with every pair enabled. The dashboard now says so ("until restart"), but an operator who pauses or disables a misbehaving pair and then restarts gets it trading again. Making state durable is a product decision.
- Options + trade-offs:
  1. Leave in-memory (today, now documented in the UI). Restarts always return to the env-file state. Predictable, and a restart is a deliberate reset.
  2. Persist pause + disabled pairs in SQLite (not dry_run). Survives restarts. Risk: a persisted pause leaves a bot silently idle after a deploy or upgrade (the dashboard now shows PAUSED, but uptime monitors don't page on it; /health says `paused`).
  3. Also persist dry_run=false. Rejected direction: the API flip would then skip the startup router-allowlist check on every restart (see the Go Live entry below).
  4. Start paused after a non-clean exit (code ≠ 75). Stops a crash loop from trading unattended, but a block stall at night (exit 1 → auto-restart) would then need a human to resume.
- Recommendation: 1 for the first mainnet runs. Revisit 2 (pause + pairs only) if operators rely on pair toggles in production.

### [dashboard] "Go Live" button skips the startup router-allowlist check
- Where: dashboard.jsx Controls "Go Live" → POST /engine/dry-run {enabled:false} (engine/src/api.rs `engine_dry_run`); PRODUCTION_DEPLOYMENT_GUIDE.md "Prefer [env + restart] over POST /engine/dry-run"
- What / why unsure: Review D1-4. The flip goes live without the startup router-allowlist check that DRY_RUN=false + restart runs. A router missing from the allowlist then costs one reverted tx before `router_not_allowed` auto-pauses the chain (which the dashboard now shows as AUTO-PAUSED). The confirm text now points to env + restart. Whether to keep the button is a workflow decision; it is handy on Base Sepolia.
- Options + trade-offs:
  1. Keep the button with the new confirm text (today). Fast for testnet; one possible reverted tx on mainnet.
  2. Hide/disable Go Live when the chain id is Base mainnet (8453) and point to env + restart. Keeps testnet convenience; mainnet go-live always passes the router check. Small dashboard change.
  3. Engine: run the router-allowlist check inside `engine_dry_run` before accepting enabled:false (reject with an error the dashboard now displays). Keeps the button safe everywhere; needs an RPC call in the handler and a test.
  4. Remove the button. Simplest; loses the testnet convenience.
- Recommendation: 3 if the button stays long-term, else 2.

### [engine] Flashblocks HTTP `pending` lagging the pendingLogs feed (QuoteCache staleness)
- Where: engine/src/strategy/mod.rs `QuoteCache` (keyed on the pool's cached sqrtP + liquidity); chain.preconf_http_rpc = https://mainnet.base.org at `pending`; scripts/preconf-lag.mjs
- What / why unsure: Review R1-HOT-3. A scan triggered by a pre-confirmed log quotes QuoterV2 at `pending` on preconf_http_rpc. If that backend hasn't applied the flashblock yet, the quote reflects the old state but is cached under the new (sqrtP, liquidity) key and served for up to the cache age. Measured from a laptop (RTT ~130 ms): 0 lag in 297 samples, 7–8 % already "ahead". A co-located engine (RTT ~5 ms) reaches the backend ~60 ms sooner, so a short lag there can't be ruled out from here.
- Procedure: on the production host run `node scripts/preconf-lag.mjs 120` (read-only, keyless; Node ≥ 22). It reports sync / lag / ahead counts and RTT.
- Options + trade-offs:
  1. Do nothing if lag = 0 there.
  2. If lag > 0: skip `QuoteCache::insert` for scans triggered by pre-confirmed logs. Cheapest; costs ~1 extra quoter RTT on the next scan of that pool.
  3. Precise: add slot0() to the same Multicall3 batch and key the entry by the returned state. Small extra calldata per batch.
- Note: never point eth_getLogs at preconf_http_rpc / https://mainnet.base.org (or the preconf HTTP endpoint): they answer -32011 "request limit reached" even for 1-block ranges. eth_call / eth_blockNumber are fine. Today get_logs uses the primary provider only.
- Recommendation: run the probe from the production host before going live; 2 if it shows lag.
- Resolved 2026-10-25: on the AWS us-east-1 host, 120 s on mainnet.base.org gave 86 calls: sync 63, **lag 16 (19%)**, ahead 7, err 0; every lag caught up by +100 ms (RTT p50 39 ms). Fixed with a time-based variant of option 2: `QuoteCache::insert` skips pools whose local state changed less than `tuning.quote_cache_settle_ms` (500) ago. Sealed and pre-confirmed swap events share one channel, so the scan can't tell which feed triggered it; pool-state age covers both.
