# Graph Report - arbitragepulse  (2026-10-15)

## Corpus Check
- 61 files · ~84,410 words
- Verdict: corpus is large enough that graph structure adds value.
- Unclassified: 9 file(s) not represented in the graph (top: (none) 4, .example 2, .lock 1)

## Summary
- 667 nodes · 1456 edges · 28 communities (22 shown, 6 thin omitted)
- Extraction: 95% EXTRACTED · 5% INFERRED · 0% AMBIGUOUS · INFERRED: 67 edges (avg confidence: 0.84)
- Token cost: 161,970 input · 0 output

## Community Hubs (Navigation)
- Trade DB & Shared Types
- Strategy Evaluation Core
- Executor & Router Types
- HTTP/WS API Server
- Deployment & Ops Docs
- Pool Cache AMM Math
- React Dashboard UI
- Revert Classification
- Event Listener
- Scan & Execute Loop
- Mempool Swap Parsing
- Chain Runner & Failover
- Config Loading
- Engine Config Files
- Math & Tuning Followups
- Pool Discovery
- Smart Contract Docs
- Revert Reduction Plan
- Contract Balance State
- Testnet & Mock Routers
- System Guide & Workflow
- Quoter Verification
- Pending Log Drain
- Linea Deploy Script
- Server Deploy Script
- Server Setup Script
- Engine Crate
- Lab Crate

## God Nodes (most connected - your core abstractions)
1. `PoolCache` - 46 edges
2. `Executor` - 32 edges
3. `Strategy` - 31 edges
4. `AppState` - 25 edges
5. `Database` - 20 edges
6. `Main Engine Config` - 20 edges
7. `evaluate_and_execute()` - 19 edges
8. `RouterType` - 19 edges
9. `Dashboard()` - 16 edges
10. `Metrics` - 16 edges

## Surprising Connections (you probably didn't know these)
- `min_profit_usd floor` --shares_data_with--> `Main Engine Config`  [INFERRED]
  .github/ISSUES/revert-reduction-plan.md → engine/config.yaml
- `Non-zero buy-leg amountOutMin` --references--> `ArbitrageExecutor.sol`  [INFERRED]
  .github/ISSUES/revert-reduction-plan.md → contract/README.md
- `Revert Reduction Plan (Base/FCFS)` --references--> `Base chain (8453)`  [INFERRED]
  .github/ISSUES/revert-reduction-plan.md → engine/config.yaml
- `ArbitragePulse System Guide` --references--> `ArbitrageExecutor v2 Foundry README`  [INFERRED]
  README.md → contract/README.md
- `Main Engine Config` --references--> `ArbitrageExecutor.sol`  [INFERRED]
  engine/config.yaml → contract/README.md

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **Revert reduction levers for V3 sell-leg reverts** — github_issues_revert_reduction_plan_p15_quoter_haircut_bps, github_issues_revert_reduction_plan_router_min_out_slack_bps, github_issues_router_min_out_slack_bps_sell_leg_minprofit_mul_bps, github_issues_revert_reduction_plan_too_little_received_revert, github_issues_revert_reduction_plan_phase_1_5_quoterv2 [EXTRACTED 1.00]
- **ArbitragePulse four-module architecture** — readme_arb_001_contract, readme_arb_002_engine, readme_arb_003_dashboard, readme_arb_004_lab [EXTRACTED 1.00]
- **Engine config variants** — engine_config_config, engine_config_production_config, engine_config_testnet_config [INFERRED 0.85]
- **Mainnet Deployment Workflow (deploy, whitelist, fund, dry-run, live)** — commands_quick_reference_router_whitelisting, linea_deployment_guide_linea_chain_config, linea_deployment_guide_dry_run_first_test, commands_quick_reference_engine_control_api, commands_quick_reference_systemd_service [INFERRED 0.85]
- **Zero-RPC Local Detection Stack** — presentation_event_sourced_price_state, presentation_constant_product_amm, presentation_stable_curve_newton_raphson, presentation_scan_pipeline, presentation_block_level_gas_price [INFERRED 0.85]
- **Latency vs Correctness Trade-offs** — presentation_phantom_profits_phase_1_5, presentation_block_queue_racing, presentation_system_design_speed_vs_accuracy_tuning, presentation_system_design_multi_rpc_submission, presentation_pending_tx_mempool_monitor [INFERRED 0.85]

## Communities (28 total, 6 thin omitted)

### Community 0 - "Trade DB & Shared Types"
Cohesion: 0.05
Nodes (28): Database, PairStat, TradeRecord, approve_token(), deploy_contract(), fund_eth(), impersonate_transfer(), read_bytecode() (+20 more)

### Community 1 - "Strategy Evaluation Core"
Cohesion: 0.06
Nodes (25): STABLE_MAX_AGE, VOLATILE_MAX_AGE, Strategy, apply_exec_quote_haircut(), MIN_V3_LIQUIDITY, MULTICALL_CHUNK_SIZE, P15_QUOTER_HAIRCUT_BPS, PairResolved (+17 more)

### Community 2 - "Executor & Router Types"
Cohesion: 0.06
Nodes (20): RouterType, Aerodrome, Solidly, SyncSwap, V2, V3, Executor, ExecutorStats (+12 more)

### Community 3 - "HTTP/WS API Server"
Cohesion: 0.09
Nodes (36): ApiServer, AppState, auth_middleware(), broadcast_log(), ChainFilter, ChainStats, DryRunBody, engine_dry_run() (+28 more)

### Community 4 - "Deployment & Ops Docs"
Cohesion: 0.05
Nodes (42): Quick Command Reference, Engine Control API (health/stats/pause/resume/dry-run), Gnosis Deployment (Honeyswap, SushiSwap), Hetzner Server Initial Setup, Router Whitelisting (setAllowedRouter / setRouterType), Scroll Deployment (Zebra, SyncSwap), SSH Tunnel Dashboard Access, arbitragepulse systemd Service (+34 more)

### Community 5 - "Pool Cache AMM Math"
Cohesion: 0.11
Nodes (19): amount_out_stable(), amount_out_v2(), cap_v3_spot_amount_in(), compute_amount_out(), FEE_DENOM_V2, FEE_DENOM_V3, PoolCache, PoolInfo (+11 more)

### Community 6 - "React Dashboard UI"
Cohesion: 0.07
Nodes (44): btn, CHAIN_NAMES, ChainCards(), chainName(), ControlPanel(), Dashboard(), Dot(), extractChain() (+36 more)

### Community 7 - "Revert Classification"
Cohesion: 0.09
Nodes (20): classify_revert_bytes(), decode_abi_revert(), diagnose_mined_revert(), format_eth_call_transport_error(), revert_bytes_from_transport(), RevertClass, DeadlineExpired, InsufficientOutput (+12 more)

### Community 8 - "Event Listener"
Cohesion: 0.11
Nodes (8): apply_log_to_cache(), Listener, reserve_delta_bps(), sqrtp_delta_bps(), SwapEvent, SwapMagnitude, Large, Normal

### Community 9 - "Scan & Execute Loop"
Cohesion: 0.14
Nodes (7): broadcast_confirmed_trade(), diagnose_and_count_revert(), evaluate_and_execute(), handle_execution_failure(), handle_execution_success(), log_exec_confirm(), Metrics

### Community 10 - "Mempool Swap Parsing"
Cohesion: 0.15
Nodes (12): CMD_V2_SWAP_EXACT_IN, CMD_V3_SWAP_EXACT_IN, parse_address(), parse_swap_calldata(), parse_u256(), PendingSwapEvent, SEL_UNIVERSAL_ROUTER, SEL_V2_SWAP_EXACT_ETH (+4 more)

### Community 11 - "Chain Runner & Failover"
Cohesion: 0.14
Nodes (7): connect_with_fallback(), COOLDOWN_SECS, GAS_REJECT_COOLDOWN_SECS, MAX_CONSECUTIVE_FAILURES, run_chain(), SEND_COOLDOWN_SECS, ChainConfig

### Community 12 - "Config Loading"
Cohesion: 0.16
Nodes (4): Env, load_config(), load_env(), substitute_env_vars()

### Community 13 - "Engine Config Files"
Cohesion: 0.18
Nodes (13): aerodrome-base router, aerodrome-slipstream-base router, base-usdc-weth pair, Main Engine Config, Linea chain (59144), lynex-solidly-linea router, mock-router-b, optimize_size (+5 more)

### Community 14 - "Math & Tuning Followups"
Cohesion: 0.26
Nodes (11): pending_tx_monitoring, Math & Infra Follow-ups (P2/P3), pool_cache.prune_stale(), V3 spot single-tick cap (V3_SPOT_MAX_VR_FRACTION_BPS), VOLATILE_MAX_AGE (120s), P15_QUOTER_HAIRCUT_BPS (15 bps), ROUTER_MIN_OUT_SLACK_BPS, V3 'Too little received' sell-leg revert (+3 more)

### Community 15 - "Pool Discovery"
Cohesion: 0.32
Nodes (9): decode_addr(), DISCOVER_CHUNK, discover_mc(), discover_pools(), discover_v3_pools(), AppConfig, AppConfigRaw, PairConfig (+1 more)

### Community 16 - "Smart Contract Docs"
Cohesion: 0.25
Nodes (10): ArbitrageExecutor.sol, ArbitrageExecutor v2 Foundry README, Ownable2Step, Pausable, forge-std remapping, OpenZeppelin remapping, ARB-001 Smart Contract module, batchExecute (+2 more)

### Community 17 - "Revert Reduction Plan"
Cohesion: 0.25
Nodes (8): Base chain (8453), Non-zero buy-leg amountOutMin, Revert Reduction Plan (Base/FCFS), min_profit_usd floor, Receipt post-mortem replay via eth_call, Revert-aware cooldown (fingerprint + pool key), scan_to_submit_ms re-quote threshold, trades SQLite table (revert_class)

### Community 18 - "Contract Balance State"
Cohesion: 0.36
Nodes (3): check_contract_funding(), refresh_contract_balances(), update_native_price()

### Community 19 - "Testnet & Mock Routers"
Cohesion: 0.33
Nodes (6): MockRouter, mock-router-a, Testnet Engine Config, mock-router-a-linea-sepolia, uniswap-v3-op-sepolia router, ARB-004 Simulation Lab (Anvil)

### Community 20 - "System Guide & Workflow"
Cohesion: 0.60
Nodes (4): ARB-002/003 Engine + API module, ARB-003 Dashboard module, ArbitragePulse System Guide, Dry-run mode

### Community 21 - "Quoter Verification"
Cohesion: 0.50
Nodes (4): optimistic_submission, Strategy::apply_exec_quote_haircut, Phase 1.5 QuoterV2 verification, Triangular V3-leg quoter verify

## Ambiguous Edges - Review These
- `Multi-Chain Capital Allocation (Linea/Scroll/Gnosis)` → `Base L2 as Target Chain`  [AMBIGUOUS]
  presentation-system-design.html · relation: conceptually_related_to

## Knowledge Gaps
- **50 isolated node(s):** `engine`, `V2`, `V3`, `Solidly`, `SyncSwap` (+45 more)
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 185 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)
- **6 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `PoolCache` connect `Pool Cache AMM Math` to `Strategy Evaluation Core`, `Event Listener`, `Chain Runner & Failover`, `Pool Discovery`, `Pending Log Drain`?**
  _High betweenness centrality (0.076) - this node is a cross-community bridge._
- **What connects `engine`, `V2`, `V3` to the rest of the system?**
  _50 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Trade DB & Shared Types` be split into smaller, more focused modules?**
  _Cohesion score 0.05228105228105228 - nodes in this community are weakly interconnected._
- **Why does `Strategy` connect `Strategy Evaluation Core` to `HTTP/WS API Server`, `Pool Cache AMM Math`, `Scan & Execute Loop`, `Chain Runner & Failover`, `Pool Discovery`, `Contract Balance State`?**
  _High betweenness centrality (0.053) - this node is a cross-community bridge._
- **Should `Strategy Evaluation Core` be split into smaller, more focused modules?**
  _Cohesion score 0.058596491228070174 - nodes in this community are weakly interconnected._
- **Why does `Executor` connect `Executor & Router Types` to `Trade DB & Shared Types`, `Scan & Execute Loop`, `Contract Balance State`, `Chain Runner & Failover`?**
  _High betweenness centrality (0.044) - this node is a cross-community bridge._

### Low-confidence Hints
_AMBIGUOUS edges — the extractor was unsure. Verify before acting on these._

- **What is the exact relationship between `Multi-Chain Capital Allocation (Linea/Scroll/Gnosis)` and `Base L2 as Target Chain`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._