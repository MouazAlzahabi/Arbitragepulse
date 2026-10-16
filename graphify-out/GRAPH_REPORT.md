# Graph Report - arbitragepulse  (2026-10-16)

## Corpus Check
- 61 files · ~82,143 words
- Verdict: corpus is large enough that graph structure adds value.
- Unclassified: 11 file(s) not represented in the graph (top: (none) 4, .example 4, .lock 1)

## Summary
- 1074 nodes · 2138 edges · 61 communities (42 shown, 19 thin omitted)
- Extraction: 96% EXTRACTED · 4% INFERRED · 0% AMBIGUOUS · INFERRED: 83 edges (avg confidence: 0.85)
- Token cost: 91,925 input · 0 output

## Community Hubs (Navigation)
- Opportunity Evaluation
- Pool Cache & AMM Math
- HTTP/WS API Server
- ABI & Trade Database
- Transaction Executor
- React Dashboard
- RPC Health Monitor
- Revert Decoding
- Executor Test Suite
- Executor Errors & Events
- Scan Loop & Metrics
- Router Swap Interfaces
- Tuning Knobs
- App Config Loading
- Sync Event Listener
- 2-hop Arb Tests
- Chain Runner Setup
- Pending Tx Decoder
- Lab CLI
- Ops Docs & Commands
- Mock Deploy Scripts
- Test Harness Files
- Design Presentation Concepts
- Revert Reduction Plan
- V3 Mint Params
- Mock Market Math
- Pool Discovery
- Contract Funding State
- Quote Haircut & Slack Knobs
- Mock & Odd ERC20s
- Misbehaving Mock Routers
- Forge Script Entrypoints
- Production Rollout
- Mock Router Files
- Mock Aerodrome Router
- Base Routers & Pairs
- Executor Core Flow
- V3 Swap Params
- Readable Failure Tests
- Triangular Arb Tests
- System Design Deck
- On-chain Router Types
- Mock V2 Router
- Log Broadcaster
- Aerodrome Route Struct
- Config Router Types
- Pending Log Drain
- V3 Position Manager
- V2 Pair Interface
- Slashable Token Mock
- Router Admin
- Approval Revocation
- Dashboard Entry HTML
- Server Setup Script
- Forge-std Remapping
- OpenZeppelin Remapping
- Preflight eth_call
- Engine Crate
- Lab Crate

## God Nodes (most connected - your core abstractions)
1. `ArbitrageExecutorTest` - 78 edges
2. `ArbitrageExecutor` - 51 edges
3. `PoolCache` - 49 edges
4. `Tuning` - 33 edges
5. `Executor` - 33 edges
6. `Strategy` - 31 edges
7. `AppState` - 25 edges
8. `ms()` - 25 edges
9. `_arb()` - 24 edges
10. `evaluate_and_execute()` - 23 edges

## Surprising Connections (you probably didn't know these)
- `pool_cache.prune_stale() heartbeat wiring` --semantically_similar_to--> `Pool freshness guards (60s volatile / 120s stable)`  [INFERRED] [semantically similar]
  .github/ISSUES/math-and-infra-followups.md → presentation.html
- `Base mainnet engine config (config.yaml)` --implements--> `Parallel submission RPCs (FCFS sequencer)`  [INFERRED]
  engine/config.yaml → PRODUCTION_DEPLOYMENT_GUIDE.md
- `Executor security properties (ReentrancyGuard, Ownable2Step, Pausable, whitelist)` --conceptually_related_to--> `Router types (0=V2, 1=V3, 4=Aerodrome; 2,3 retired)`  [AMBIGUOUS]
  presentation.html → contract/README.md
- `Pool freshness guards (60s volatile / 120s stable)` --conceptually_related_to--> `Math & Infrastructure Follow-ups (P2/P3)`  [AMBIGUOUS]
  presentation.html → .github/ISSUES/math-and-infra-followups.md
- `V3 spot single-tick cap (~2% vr_in)` --rationale_for--> `Phantom profits (V3 tick boundary)`  [INFERRED]
  .github/ISSUES/math-and-infra-followups.md → presentation.html

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **Phantom-profit / overstated-quote defenses** — presentation_phantom_profits, _github_issues_math_and_infra_followups_p15_quoter_haircut_bps, _github_issues_math_and_infra_followups_v3_spot_single_tick_cap, presentation_quoterv2_verification, testnet_deployment_guide_phantom_cap, presentation_freshness_guards [INFERRED 0.85]
- **Testnet-to-production deployment flow** — testnet_deployment_guide_doc, engine_config_testnet, production_deployment_guide_doc, engine_config, scripts_deploy, production_deployment_guide_systemd_instance_template, production_deployment_guide_dry_run_first [EXTRACTED 1.00]
- **Executor revert taxonomy decoded by engine** — contract_readme_readable_failures, contract_readme_notprofitable, contract_readme_legfailed, contract_readme_routernotallowed, _github_issues_revert_reduction_plan_revert_classification [INFERRED 0.85]

## Communities (61 total, 19 thin omitted)

### Community 0 - "Opportunity Evaluation"
Cohesion: 0.05
Nodes (37): stable_max_age(), volatile_max_age(), triangular_sell_leg_is_v3(), Strategy, apply_exec_quote_haircut(), decode_v3_quote(), encode_v3_quote(), min_v3_liquidity() (+29 more)

### Community 1 - "Pool Cache & AMM Math"
Cohesion: 0.07
Nodes (34): amount_out_stable(), amount_out_v2(), cap_v3_spot_amount_in(), compute_amount_out(), FEE_DENOM_V2, FEE_DENOM_V3, PoolCache, PoolInfo (+26 more)

### Community 2 - "HTTP/WS API Server"
Cohesion: 0.08
Nodes (36): ApiServer, AppState, auth_middleware(), broadcast_log(), ChainFilter, ChainStats, DryRunBody, engine_dry_run() (+28 more)

### Community 3 - "ABI & Trade Database"
Cohesion: 0.07
Nodes (18): Database, PairStat, TradeRecord, approve_token(), deploy_contract(), fund_eth(), impersonate_transfer(), read_bytecode() (+10 more)

### Community 4 - "Transaction Executor"
Cohesion: 0.08
Nodes (12): Executor, ExecutorStats, format_eth_call_transport_error(), gas_fallback_margin_bps(), gas_fallback_max_age(), gas_limit_2hop(), gas_limit_triangular(), router_min_out_slack_bps() (+4 more)

### Community 5 - "React Dashboard"
Cohesion: 0.07
Nodes (44): btn, ChainCards(), ControlPanel(), Dashboard(), Dot(), extractChain(), formatUptime(), GasLatencyBar() (+36 more)

### Community 6 - "RPC Health Monitor"
Cohesion: 0.06
Nodes (27): on_paused(), spawn_rpc_monitor(), addr_key(), classify_rpc_failure(), contains_status_429(), HARD_DOWN_MARKERS, HealthState, next_hold() (+19 more)

### Community 7 - "Revert Decoding"
Cohesion: 0.07
Nodes (31): classify_inner(), classify_revert_bytes(), contract_errors_are_readable(), decode_abi_revert(), describe_revert(), diagnose_mined_revert(), error_string(), format_eth_call_transport_error() (+23 more)

### Community 8 - "Executor Test Suite"
Cohesion: 0.05
Nodes (19): ArbitrageExecutorTest, ArbitrageExecuted, TriangularArbitrageExecuted, aero, AMT, attacker, deadline, executor (+11 more)

### Community 9 - "Executor Errors & Events"
Cohesion: 0.05
Nodes (25): ArbitrageExecutor, DeadlineExpired, InsufficientBalance, InvalidPath, InvalidV3Fee, LegFailed, NoOutput, NotAContract (+17 more)

### Community 10 - "Scan Loop & Metrics"
Cohesion: 0.09
Nodes (16): cooldown_secs(), gas_reject_cooldown_secs(), max_consecutive_failures(), send_cooldown_secs(), broadcast_confirmed_trade(), diagnose_and_count_revert(), evaluate_and_execute(), FailureAction (+8 more)

### Community 11 - "Router Swap Interfaces"
Cohesion: 0.07
Nodes (17): IMockV2Router, IAerodromeRouter, Route, factory, from, stable, to, ISwapRouterV3 (+9 more)

### Community 13 - "App Config Loading"
Cohesion: 0.11
Nodes (12): AppConfig, block_relative_timings_follow_block_time(), empty_tuning_matches_previous_constants(), Env, install_tuning(), load_config(), load_env(), MIN (+4 more)

### Community 14 - "Sync Event Listener"
Cohesion: 0.12
Nodes (7): apply_log_to_cache(), reserve_delta_bps(), sqrtp_delta_bps(), SwapEvent, SwapMagnitude, Large, Normal

### Community 15 - "2-hop Arb Tests"
Cohesion: 0.11
Nodes (21): _arb(), _notProfitable(), test_Aerodrome_StableFlagFollowsFee(), test_Aerodrome_VolatileSell_Profitable(), test_AllowancesClearedAfterTrade(), test_Arb_EmitsEvent(), test_Deadline(), test_GetStats() (+13 more)

### Community 17 - "Pending Tx Decoder"
Cohesion: 0.20
Nodes (12): CMD_V2_SWAP_EXACT_IN, CMD_V3_SWAP_EXACT_IN, parse_address(), parse_swap_calldata(), parse_u256(), PendingSwapEvent, SEL_UNIVERSAL_ROUTER, SEL_V2_SWAP_EXACT_ETH (+4 more)

### Community 18 - "Lab CLI"
Cohesion: 0.20
Nodes (10): ANVIL_RPC, build_provider(), cmd_scenario(), cmd_setup(), cmd_test_reverts(), CONTRACT_DIR, DEFAULT_PRIVATE_KEY, deployer_address() (+2 more)

### Community 19 - "Ops Docs & Commands"
Cohesion: 0.24
Nodes (13): Commands Quick Reference, lab CLI (setup / scenario / test-reverts), ArbitrageExecutor contract, DeployMocks.s.sol / RewidenMocks.s.sol / MockMarket.sol, ArbitrageExecutor v3 README, executeArbitrage (2-hop), executeTriangularArbitrage (A->B->C->A), setRouter(address,uint8) whitelist (+5 more)

### Community 20 - "Mock Deploy Scripts"
Cohesion: 0.18
Nodes (3): IMockV2Factory, IMockV3Factory, IMockV3Pool

### Community 22 - "Design Presentation Concepts"
Cohesion: 0.18
Nodes (11): QuoteCache (shared 2-hop Phase 1.5/1.5b), Constant product curve xy=k, Executor security properties (ReentrancyGuard, Ownable2Step, Pausable, whitelist), ArbitragePulse Technical Presentation, Phantom profits (V3 tick boundary), PoolInfo struct, QuoterV2 verification (Phase 1.5), Scan pipeline (2-hop + triangular detection) (+3 more)

### Community 23 - "Revert Reduction Plan"
Cohesion: 0.20
Nodes (6): Revert Reduction Plan (Base / FCFS), Sell-leg V3 'Too little received' reverts, LegFailed error, NotProfitable error, RouterNotAllowed error, min_triangular_profit_usd = 0.15

### Community 24 - "V3 Mint Params"
Cohesion: 0.17
Nodes (12): MintParams, amount0Desired, amount0Min, amount1Desired, amount1Min, deadline, fee, recipient (+4 more)

### Community 25 - "Mock Market Math"
Cohesion: 0.17
Nodes (10): MockMarket, LIQ_WETH, TICK_SPACING, V2_PRICE, V2_ROUTER, V3_FACTORY, V3_FEE, V3_POSITION_MANAGER (+2 more)

### Community 26 - "Pool Discovery"
Cohesion: 0.33
Nodes (9): decode_addr(), discover_call_timeout(), discover_chunk(), discover_mc(), discover_pools(), discover_v3_pools(), AppConfigRaw, PairConfig (+1 more)

### Community 27 - "Contract Funding State"
Cohesion: 0.24
Nodes (5): check_contract_funding(), refresh_contract_balances(), update_native_price(), ChainConfig, rpc_read_timeout()

### Community 28 - "Quote Haircut & Slack Knobs"
Cohesion: 0.22
Nodes (6): Math & Infrastructure Follow-ups (P2/P3), router-min-out-slack-bps ticket, tuning.router_min_out_slack_bps = 200, ROUTER_MIN_OUT_SLACK_BPS, Raise ROUTER_MIN_OUT_SLACK_BPS Issue, SELL_LEG_MINPROFIT_MUL_BPS (6200)

### Community 29 - "Mock & Odd ERC20s"
Cohesion: 0.18
Nodes (3): MockERC20, MockFeeOnTransferERC20, MockNoZeroApproveERC20

### Community 30 - "Misbehaving Mock Routers"
Cohesion: 0.18
Nodes (5): MockLyingRouter, MockNoOpRouter, MockPartialRouter, MockSilentRouter, MockSlashingRouter

### Community 31 - "Forge Script Entrypoints"
Cohesion: 0.24
Nodes (7): DeployExecutor, DeployMocks, RewidenMocks, _near(), run(), _v2Reserves(), TOLERANCE_BPS

### Community 32 - "Production Rollout"
Cohesion: 0.22
Nodes (5): Circuit breaker (max_consecutive_failures), Production Deployment Guide (Base mainnet), arbitragepulse@.service systemd template, RPC health / degraded mode (503), deploy.sh script

### Community 33 - "Mock Router Files"
Cohesion: 0.22
Nodes (3): MockV3Router, rateDenominator, rateNumerator

### Community 34 - "Mock Aerodrome Router"
Cohesion: 0.22
Nodes (7): MockAerodromeRouter, lastFactory, lastStable, stableDen, stableNum, volDen, volNum

### Community 35 - "Base Routers & Pairs"
Cohesion: 0.39
Nodes (7): Router types (0=V2, 1=V3, 4=Aerodrome; 2,3 retired), Base mainnet engine config (config.yaml), aerodrome-base router, base-usdc-weth pair, pancake-v3-base router, uniswap-v3-base router, tuning: config section

### Community 36 - "Executor Core Flow"
Cohesion: 0.39
Nodes (8): _checkBalance(), _checkRouter(), _clearAllowance(), executeArbitrage(), executeTriangularArbitrage(), _settle(), _swap(), checkDeadline()

### Community 37 - "V3 Swap Params"
Cohesion: 0.25
Nodes (8): ExactInputSingleParams, amountIn, amountOutMinimum, fee, recipient, sqrtPriceLimitX96, tokenIn, tokenOut

### Community 39 - "Readable Failure Tests"
Cohesion: 0.29
Nodes (7): _fund(), _legFailed(), setUp(), test_LeftoverAllowance_IsCleared(), test_RouterMisreportingOutput_UsesRealBalance(), test_RouterRevert_ReportsLegAndRouterReason(), test_V3RouterTypedAsV2_FailsWithEmptyReason()

### Community 40 - "Triangular Arb Tests"
Cohesion: 0.43
Nodes (7): test_Triangular_AllTypes(), test_Triangular_EventNamesAllThreeRouters(), test_Triangular_LossReportsNotProfitable(), test_Triangular_RouterNotAllowed(), test_Triangular_V2_Profitable(), _tri(), _triRouters()

### Community 41 - "System Design Deck"
Cohesion: 0.29
Nodes (4): ArbitragePulse System Design Presentation, Capital Scaling Plan (~$1k to $50-100k), Multi-RPC Broadcast Submission, Observability (exec_send/exec_confirm, SQLite, Prometheus)

### Community 42 - "On-chain Router Types"
Cohesion: 0.33
Nodes (6): RouterType, Aerodrome, RetiredSolidly, RetiredSyncSwap, V2, V3

### Community 43 - "Mock V2 Router"
Cohesion: 0.33
Nodes (3): MockRouter, rateDenominator, rateNumerator

### Community 45 - "Aerodrome Route Struct"
Cohesion: 0.40
Nodes (5): Route, factory, from, stable, to

### Community 46 - "Config Router Types"
Cohesion: 0.40
Nodes (4): RouterType, Aerodrome, V2, V3

## Ambiguous Edges - Review These
- `Math & Infrastructure Follow-ups (P2/P3)` → `Pool freshness guards (60s volatile / 120s stable)`  [AMBIGUOUS]
  .github/ISSUES/math-and-infra-followups.md · relation: conceptually_related_to
- `Router types (0=V2, 1=V3, 4=Aerodrome; 2,3 retired)` → `Executor security properties (ReentrancyGuard, Ownable2Step, Pausable, whitelist)`  [AMBIGUOUS]
  presentation.html · relation: conceptually_related_to

## Knowledge Gaps
- **49 isolated node(s):** `V2`, `V3`, `RetiredSolidly`, `RetiredSyncSwap`, `Aerodrome` (+44 more)
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 382 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)
- **19 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `ArbitrageExecutor` connect `Executor Errors & Events` to `Executor Core Flow`, `On-chain Router Types`, `Router Swap Interfaces`, `Router Admin`, `Approval Revocation`?**
  _High betweenness centrality (0.064) - this node is a cross-community bridge._
- **What connects `V2`, `V3`, `RetiredSolidly` to the rest of the system?**
  _49 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Opportunity Evaluation` be split into smaller, more focused modules?**
  _Cohesion score 0.054392236976506636 - nodes in this community are weakly interconnected._
- **Why does `IMockV2Router` connect `Router Swap Interfaces` to `Mock Deploy Scripts`?**
  _High betweenness centrality (0.057) - this node is a cross-community bridge._
- **Should `Pool Cache & AMM Math` be split into smaller, more focused modules?**
  _Cohesion score 0.07177033492822966 - nodes in this community are weakly interconnected._

### Low-confidence Hints
_AMBIGUOUS edges — the extractor was unsure. Verify before acting on these._

- **What is the exact relationship between `Math & Infrastructure Follow-ups (P2/P3)` and `Pool freshness guards (60s volatile / 120s stable)`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._
- **What is the exact relationship between `Router types (0=V2, 1=V3, 4=Aerodrome; 2,3 retired)` and `Executor security properties (ReentrancyGuard, Ownable2Step, Pausable, whitelist)`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._