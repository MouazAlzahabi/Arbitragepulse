# ArbitragePulse

On-chain DEX arbitrage for **Base**: a Rust engine detects price gaps between DEXes
(2-hop and triangular routes) and executes them atomically through an on-chain
`ArbitrageExecutor` contract.

**One chain per instance.** Every running engine serves exactly one chain, from one
config file. Production runs on **Base mainnet** (`engine/config.yaml`); the testing
stage runs on **Base Sepolia** (`engine/config-testnet.yaml`). To trade another chain,
deploy another instance with its own config, `.env`, port and database — no code change.

## Repository layout

```
contract/     Solidity (Foundry): ArbitrageExecutor + mocks, tests, deploy scripts
engine/       Rust engine: detection, verification, execution, HTTP/WS API
lab/          Rust CLI for local end-to-end tests on an Anvil fork of Base
dashboard.jsx React dashboard (Vite) — talks to the engine API
scripts/      deploy.sh (local → server) and server/ (systemd template, logrotate)
```

## How it works

1. **Pool state, locally.** At startup the engine discovers pools for every configured
   pair on every router (Uniswap/PancakeSwap V3, Aerodrome, V2), then keeps their state
   fresh from on-chain `Swap`/`Sync` events. Detection is pure local math — no RPC.
2. **Detection** each block (and on large swaps / pending mempool swaps): 2-hop routes
   across DEXes and triangular A→B→C→A routes.
3. **Verification.** Routes involving V3 are re-quoted live with QuoterV2 (V3 spot math
   ignores tick crossings); a haircut and a phantom cap guard against bad quotes.
4. **Execution.** A signed tx calls `ArbitrageExecutor`, which swaps both legs atomically
   and reverts unless it ends with more of the input token (`minProfit`).
5. **RPC health.** Every RPC call has a timeout. When the node hangs, is down, or is too
   slow to simulate on, the chain goes *degraded*: opportunities are still detected and
   shown, but nothing executes until a monitor task sees the node healthy again.
   `/health` returns 503 while degraded.

Supported DEX types (`type:` in config): `v3` (Uniswap V3 SwapRouter02 / PancakeSwap V3),
`aerodrome` (Aerodrome V2 volatile + stable pools), `v2` (Uniswap V2-style).

## Quick start (local, no funds)

Prerequisites: Rust (`rustup`), Foundry (`foundryup`), Node 18+ for the dashboard.

```bash
# Contracts
cd contract && forge test            # 86 tests

# Engine + lab
cargo build                          # from the repo root
cargo test -p engine --bin engine    # unit tests

# End-to-end on a local Base fork
anvil --fork-url https://base-mainnet.g.alchemy.com/v2/$ALCHEMY_KEY --chain-id 8453 --block-time 2 &
cargo run -p lab -- setup            # deploy executor + mocks on the fork
cargo run -p lab -- scenario         # run an arbitrage on the fork
cargo run -p lab -- test-reverts     # revert scenarios

# Dashboard
npm install && npm run dashboard     # http://localhost:5173
```

> **Never `cargo run -p engine` from the repo root with a real `.env`.** The engine reads
> `.env` from the working directory and ignores CLI flags; with `DRY_RUN=false` and a
> funded key it trades for real. For local runs use an explicit test key, `DRY_RUN=true`
> and a config whose RPCs point at Anvil.

## Configuration

A config file has four sections:

| Section | What it holds |
|---|---|
| `chain:` | id, name, RPC URLs (`${ALCHEMY_KEY}` is substituted from env), block time, executor address, profit floors, submission options |
| `routers:` | DEX routers: `type`, address, factory, V3 fee tiers and quoter |
| `pairs:` | token pairs, trade size |
| `tuning:` | every timing and threshold (gas limits, slippage, haircuts, RPC timeouts, cooldowns, circuit breaker…) — all optional, defaults tuned for Base |

Block-relative timings (simulation timeout, quote-cache age, RPC monitor interval)
default to multiples of `chain.block_time_ms`, so a chain with a different block
interval gets sensible values without overriding them. Unknown `tuning` keys are
rejected. Routers, pairs and profit floors hot-reload; `chain:` and `tuning:` need a
restart.

Safety checks at startup: the RPC's chain id must match `chain.id` (the engine exits
otherwise), and an unset `contract_address` (`YOUR_…`) is rejected.

Environment (`engine/<instance>.env`, see `engine/base.env.example`):
`PRIVATE_KEY`, `ALCHEMY_KEY`, `CONFIG_PATH`, `PORT`, `DB_PATH`, `API_KEY`, `LOG_LEVEL`,
`DRY_RUN` (defaults to `true`).

## Engine API

| Endpoint | Purpose |
|---|---|
| `GET /health` | status (`ok` / `degraded` → 503), RPC and backoff state |
| `GET /stats` | counters, gas, latency, `rpc_backoff` |
| `GET /trades`, `GET /stats/pairs` | trade history, per-pair stats |
| `GET /ws` | live log/opportunity stream |
| `POST /engine/pause`, `/engine/resume`, `/engine/dry-run` | control |
| `GET /pair-scan`, `POST /pair-scan/{id}/toggle` | per-pair scan state, enable/disable |
| `GET /metrics` | Prometheus metrics |

## Guides

- [TESTNET_DEPLOYMENT_GUIDE.md](TESTNET_DEPLOYMENT_GUIDE.md) — Base Sepolia testing stage
- [PRODUCTION_DEPLOYMENT_GUIDE.md](PRODUCTION_DEPLOYMENT_GUIDE.md) — Base mainnet on a server
- [COMMANDS_QUICK_REFERENCE.md](COMMANDS_QUICK_REFERENCE.md) — copy-paste commands
- [contract/README.md](contract/README.md) — contract details
