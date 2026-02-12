# ArbitragePulse — Complete System Guide

Cross-DEX arbitrage system for EVM chains (Optimism, Base, Gnosis). Four modules that work together: a Solidity contract that executes atomic swaps, a TypeScript engine that detects opportunities, a React dashboard for monitoring, and a local simulation lab for safe testing.

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                         YOUR MACHINE                                │
│                                                                     │
│   ┌──────────────┐    ┌──────────────┐    ┌───────────────────┐    │
│   │  ARB-001     │    │  ARB-002/003 │    │   ARB-003         │    │
│   │  Contract    │◄───│  Engine      │◄───│   Dashboard       │    │
│   │  (Foundry)   │    │  (Bun+Viem)  │    │   (React JSX)     │    │
│   └──────┬───────┘    └──────┬───────┘    └───────────────────┘    │
│          │                   │                                      │
│          │            ┌──────▼───────┐                              │
│          └───────────►│  ARB-004     │                              │
│                       │  Lab (Anvil) │                              │
│                       └──────────────┘                              │
└─────────────────────────────────────────────────────────────────────┘
                               │
                    ┌──────────▼──────────┐
                    │  Live Blockchains   │
                    │  Optimism / Base /  │
                    │  Gnosis             │
                    └─────────────────────┘
```

---

## Prerequisites (Install Once)

```bash
# Foundry (Solidity compiler + Anvil local chain)
curl -L https://foundry.paradigm.xyz | bash
foundryup

# Bun (TypeScript runtime — faster than Node)
curl -fsSL https://bun.sh/install | bash

# Verify
forge --version    # forge 0.2.x
anvil --version    # anvil 0.2.x
bun --version      # 1.x
```

You also need a free RPC endpoint from Alchemy or QuickNode (HTTP URL for forking).

---

## Module 1: ARB-001 — Smart Contract (Foundry)

**Folder:** `contract/`

### What It Contains

The on-chain ArbitrageExecutor contract that performs atomic cross-DEX swaps. If a trade isn't profitable, the entire transaction reverts — you never lose money on a failed arb (only gas).

| File | Purpose |
|------|---------|
| `src/ArbitrageExecutor.sol` | Main contract — 8 security features (deadline, minProfit, reentrancy guard, allowance hygiene, Ownable2Step, batch execution, profit tracking, pausable) |
| `src/mocks/MockERC20.sol` | Test ERC20 token |
| `src/mocks/MockRouter.sol` | Test DEX router with configurable exchange rates |
| `test/ArbitrageExecutor.t.sol` | 46 Forge tests covering every feature |
| `script/Deploy.s.sol` | Deployment script (works for Anvil, Optimism, Base) |
| `foundry.toml` | Compiler config (Solidity 0.8.20, optimizer on, via-IR) |
| `remappings.txt` | Import mapping for OpenZeppelin |

### Setup

```bash
cd contract

# Install Solidity dependencies
forge install OpenZeppelin/openzeppelin-contracts --no-commit
forge install foundry-rs/forge-std --no-commit

# Copy environment file
cp .env.example .env
# Edit .env → add PRIVATE_KEY and RPC URLs
```

### Test

```bash
# Run all 46 tests (fast — no network needed, uses mocks)
forge test

# With full trace output on failures
forge test -vvv

# Run a single test
forge test --match-test test_ExecuteProfitableArb -vvv

# Gas usage report
forge test --gas-report
```

### Compile

```bash
forge build
```

The compiled artifact lands at `out/ArbitrageExecutor.sol/ArbitrageExecutor.json` — the Lab needs this path.

### Deploy

```bash
# To a local Anvil fork (safe, free)
forge script script/Deploy.s.sol \
  --rpc-url http://127.0.0.1:8545 \
  --broadcast

# To Optimism mainnet (costs real ETH for gas)
forge script script/Deploy.s.sol \
  --rpc-url $OPTIMISM_RPC \
  --broadcast --verify

# To Base mainnet
forge script script/Deploy.s.sol \
  --rpc-url $BASE_RPC \
  --broadcast --verify
```

The deploy script prints the contract address. Save it — you need it for the engine config.

### Key Contract Functions

| Function | Who Calls It | What It Does |
|----------|-------------|--------------|
| `executeArbitrage(...)` | Engine (owner only) | Buy on router A, sell on router B, revert if not profitable |
| `batchExecute(...)` | Engine (owner only) | Multiple arbs in one tx, failing ones are skipped |
| `estimateArbitrage(...)` | Anyone (view) | Simulate profit without spending gas |
| `getStats(token)` | Anyone (view) | Total profit, total trades, balance |
| `pause()` / `unpause()` | Owner only | Emergency stop — blocks trading, withdrawals still work |
| `withdrawToken(token)` | Owner only | Pull funds out — always works even when paused |
| `withdrawETH()` | Owner only | Pull ETH out |

---

## Module 2: ARB-002/003 — Engine + API (Bun + Viem)

**Folder:** `engine/`

### What It Contains

The TypeScript bot that monitors DEX prices, detects arbitrage opportunities, and executes trades via the on-chain contract. Includes a REST + WebSocket API for the dashboard.

| File/Folder | Purpose |
|-------------|---------|
| `src/index.ts` | Entry point — boots all chains, starts API server |
| `src/listeners/swap-listener.ts` | Monitors swap events via WebSocket (V2) and polling (V3) |
| `src/strategies/arbitrage.ts` | Compares prices across routers, calculates profit after gas |
| `src/executor/executor.ts` | Submits transactions to the contract |
| `src/config/chains.ts` | Loads YAML config, builds chain clients |
| `src/config/token-registry.ts` | Token allowlist management, auto pair generation |
| `src/api/server.ts` | Fastify server — REST endpoints + WebSocket + auth |
| `src/abi/index.ts` | Contract and router ABIs |
| `src/utils/` | Logger, mutex (prevent double-execution), nonce manager |
| `config.example.yaml` | Chain + router configuration |
| `tokens.example.yaml` | Token allowlist (what the bot trades) |
| `.env.example` | Secrets (private key, API key) |

### Setup

```bash
cd engine
bun install

# Copy and edit all three config files
cp .env.example .env
cp config.example.yaml config.yaml
cp tokens.example.yaml tokens.yaml
```

**`.env` — secrets only:**
```
PRIVATE_KEY=0xYOUR_PRIVATE_KEY
PORT=3000
API_KEY=            # Generate: openssl rand -hex 32 (empty = no auth)
LOG_LEVEL=info
```

**`config.yaml` — chains and routers:**
Add your deployed contract address, RPC URLs, and the DEX routers to monitor. Each chain is independent. Example:
```yaml
chains:
  - id: 10
    name: "Optimism"
    enabled: true
    ws_rpc: "wss://opt-mainnet.g.alchemy.com/v2/YOUR_KEY"
    http_rpc: "https://opt-mainnet.g.alchemy.com/v2/YOUR_KEY"
    contract_address: "0xYOUR_DEPLOYED_CONTRACT"
```

**`tokens.yaml` — what to trade:**
Only tokens marked `trusted: true` generate trading pairs. The engine auto-generates all valid pair combinations. 8 trusted tokens = 48 directional pairs.
```yaml
optimism:
  - symbol: "WETH"
    address: "0x4200000000000000000000000000000000000006"
    decimals: 18
    trusted: true
    category: "blue_chip"    # determines trade size
```

### Run

```bash
# Start in dry-run mode (default — no real trades, just monitoring)
bun run start

# Development mode (auto-reload on file changes)
bun run dev
```

The engine starts in dry-run mode by default. It logs every opportunity it finds but does not submit transactions. When ready to go live, toggle via the dashboard Controls tab or the API.

### API Endpoints

All endpoints require `Authorization: Bearer <API_KEY>` header if `API_KEY` is set in `.env`.

**Monitoring:**

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Uptime, balances, per-chain stats |
| `/stats` | GET | Execution statistics |
| `/ws` | WebSocket | Live log stream (connect with `?token=API_KEY`) |

**Token Management:**

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/tokens` | GET | List all tokens (filter: `?chain_id=10&trusted_only=true`) |
| `/tokens` | POST | Add a token |
| `/tokens/:chainId/:address` | PATCH | Update (e.g., `{ "trusted": true }`) |
| `/tokens/:chainId/:address` | DELETE | Remove a token |
| `/tokens/pairs` | GET | Preview auto-generated pairs |

**Engine Controls:**

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/engine/state` | GET | Current state (paused, dry-run, per-chain status) |
| `/engine/pause` | POST | Pause all chains or one (`{ "chain_id": 10 }`) |
| `/engine/resume` | POST | Resume all or one |
| `/engine/dry-run` | POST | Toggle dry-run (`{ "enabled": false }` = live) |

### Going Live Checklist

1. Run in dry-run for 24+ hours — check logs for real opportunities
2. Verify simulated P&L is positive and consistent
3. Set `API_KEY` in `.env` (never run live without auth)
4. Toggle dry-run off via `POST /engine/dry-run { "enabled": false }` or the Controls tab
5. Start with small trade sizes (configured per-category in `tokens.yaml`)
6. Monitor via dashboard — watch for errors
7. Scale trade sizes gradually

---

## Module 3: ARB-003 — Dashboard (React)

**File:** `dashboard.jsx`

### What It Contains

A single-file React component (579 lines) that renders as a Claude Artifact. Three tabs: Monitor (live feed, profit chart, opportunity table), Tokens (manage the allowlist), and Controls (pause/resume, dry-run toggle).

**Features:**
- Login screen with engine URL + API key input
- WebSocket connection with auto-reconnect and auth error handling
- Real-time log feed with type filtering (info, opportunity, trade, error)
- Profit-over-time area chart (Recharts)
- Opportunity table showing detected arbs
- Per-chain status cards
- Token manager (add, remove, toggle trust)
- Control panel (pause/resume per-chain, dry-run toggle)
- Sound alerts on opportunities
- Lock icon when authenticated

### How to Use

1. Open the `.jsx` file as a Claude Artifact (paste it into a new artifact, or save it as a `.jsx` file and render it)
2. Enter your engine's WebSocket URL: `ws://YOUR_SERVER:3000/ws`
3. Enter your API key (same one from the engine's `.env`)
4. Click Connect

The dashboard connects to the engine's WebSocket and displays real-time data. All controls (pause, resume, dry-run) are available in the Controls tab.

### Deploying Remotely (e.g., Hetzner VPS)

If the engine runs on a remote server:
1. Set up the engine on the VPS
2. Open port 3000 (or use a reverse proxy with HTTPS)
3. Generate an API key: `openssl rand -hex 32`
4. Set it in the engine's `.env` as `API_KEY`
5. In the dashboard, connect to `ws://YOUR_VPS_IP:3000/ws` with the same key

---

## Module 4: ARB-004 — Simulation Lab (Anvil)

**Folder:** `lab/`

### What It Contains

A local testing environment that forks a live blockchain, deploys your contract, and lets you test the entire system without spending real money. Uses Anvil's impersonation feature to "borrow" tokens from whale addresses.

| File | Purpose |
|------|---------|
| `scripts/config.ts` | Chain addresses, whale addresses, router addresses per chain |
| `scripts/helpers.ts` | Viem client factories, impersonation logic, contract deployment |
| `scripts/setup-fork.ts` | Deploy contract + fund wallet via whale impersonation |
| `scripts/create-arb-opportunity.ts` | Whale swap to create a price imbalance between routers |
| `scripts/test-reverts.ts` | Intentionally trigger all 4 revert reasons |
| `scripts/full-scenario.ts` | Complete E2E: deploy → fund → create imbalance → arb → P&L report |

### Setup

```bash
cd lab
bun install

cp .env.example .env
# Edit .env → add FORK_RPC_URL (Alchemy/QuickNode HTTP endpoint)
# The artifact path defaults to ../contract/out/ArbitrageExecutor.sol/ArbitrageExecutor.json
```

Make sure you've compiled the contract first:
```bash
cd ../contract && forge build
```

### Run

**Terminal 1 — start the fork:**
```bash
# Fork Optimism (most DEX liquidity for testing)
bun run fork:optimism

# Or fork Base
bun run fork:base

# Or fork Gnosis
bun run fork:gnosis
```

**Terminal 2 — run the full scenario:**
```bash
bun run scenario
```

This single command does everything:
1. Deploys ArbitrageExecutor to the local fork
2. Impersonates an Optimism USDC whale, transfers 10,000 USDC to your test wallet
3. Wraps ETH into WETH
4. Funds the contract with 3,000 USDC + 1 WETH
5. Records initial balances
6. Whale swaps 80,000 USDC on Router A (creates a price gap vs Router B)
7. Shows before/after prices on both routers
8. Executes the arb on your contract
9. If one direction fails, automatically tries the opposite
10. Prints a P&L report with exact token deltas

### Individual Scripts

```bash
# Just deploy + fund (skip the arb)
bun run setup

# Just create a price imbalance (run repeatedly for new opportunities)
bun run create-arb

# Test all revert reasons
bun run test-reverts
```

### Testing the Engine Against the Fork

After `bun run setup`, the script prints a contract address and engine config. Copy those into your engine's `config.yaml`:

```yaml
chains:
  - id: 10
    name: "Optimism-Fork"
    enabled: true
    ws_rpc: "ws://127.0.0.1:8545"
    http_rpc: "http://127.0.0.1:8545"
    contract_address: "0x..."   # from setup output
```

Then start the engine (`bun run start`) and run `bun run create-arb` in the lab terminal. Watch the dashboard detect and display the opportunity.

---

## Recommended Workflow

### Phase 1: Local Testing (Zero Risk)

```
1. Compile contract     → cd contract && forge build
2. Run unit tests       → forge test
3. Start fork           → cd lab && bun run fork:optimism
4. Full E2E test        → bun run scenario
5. Test reverts         → bun run test-reverts
6. Start engine on fork → cd engine && bun run dev
7. Trigger arbs         → cd lab && bun run create-arb
8. Watch dashboard      → open dashboard.jsx as artifact
```

### Phase 2: Mainnet Dry-Run (Gas Costs Only)

```
1. Deploy contract      → forge script script/Deploy.s.sol --rpc-url $OPTIMISM_RPC --broadcast
2. Fund contract        → send USDC/WETH to the deployed address
3. Update engine config → set contract_address, real RPC URLs
4. Set API_KEY          → openssl rand -hex 32, add to .env
5. Start engine         → bun run start (dry-run is default)
6. Monitor 24-48 hours  → watch dashboard for opportunity volume/quality
```

### Phase 3: Live Trading

```
1. Toggle dry-run off   → POST /engine/dry-run { "enabled": false }
2. Start with small     → keep default trade sizes ($30-$150 per trade)
3. Monitor closely      → check dashboard every few hours
4. Scale gradually      → increase trade sizes in tokens.yaml as confidence grows
5. Emergency stop       → dashboard Controls → Pause All, or call pause() on contract
```

---

## Directory Layout

```
arbitragepulse/
├── contract/        # Module 1: Solidity contract
│   ├── src/
│   │   ├── ArbitrageExecutor.sol
│   │   └── mocks/
│   ├── test/ArbitrageExecutor.t.sol
│   ├── script/Deploy.s.sol
│   ├── foundry.toml
│   └── .env.example
│
├── engine/                  # Module 2+3: TypeScript engine + API
│   ├── src/
│   │   ├── index.ts
│   │   ├── listeners/
│   │   ├── strategies/
│   │   ├── executor/
│   │   ├── api/
│   │   ├── config/
│   │   └── utils/
│   ├── config.example.yaml
│   ├── tokens.example.yaml
│   └── .env.example
│
├── dashboard.jsx        # Module 3: React dashboard
│
└── lab/                     # Module 4: Simulation lab
    ├── scripts/
    │   ├── config.ts
    │   ├── helpers.ts
    │   ├── setup-fork.ts
    │   ├── create-arb-opportunity.ts
    │   ├── test-reverts.ts
    │   └── full-scenario.ts
    ├── package.json
    └── .env.example
```

---

## Safety & Security

- The contract reverts atomically if a trade isn't profitable — you can never lose principal on a failed arb (only gas)
- `Ownable2Step` prevents accidental ownership transfer (requires the new owner to explicitly accept)
- Withdrawals always work even when the contract is paused
- The engine defaults to dry-run mode — no transactions until you explicitly toggle
- API key authentication protects the engine from unauthorized access
- Token allowlist prevents trading unknown or malicious tokens
- Per-chain pause lets you stop one chain without affecting others

---

## Troubleshooting

**`forge build` fails with "file not found"**
→ Run `forge install OpenZeppelin/openzeppelin-contracts --no-commit` and `forge install foundry-rs/forge-std --no-commit`

**Lab says "artifact not found"**
→ Run `cd contract && forge build` first. Check that `.env` has the correct `CONTRACT_ARTIFACT_PATH`.

**Engine can't connect to RPC**
→ Check your Alchemy/QuickNode URL. WebSocket URLs start with `wss://`, HTTP with `https://`.

**Dashboard shows "Authentication failed"**
→ Make sure the API key in the dashboard matches exactly what's in the engine's `.env`. If `API_KEY` is empty in `.env`, leave the dashboard key field empty too.

**`bun run scenario` shows "NotProfitable"**
→ The whale swap amount may not be large enough to create a spread. Try increasing the swap amount in `full-scenario.ts`, or the forked block may have already-aligned prices. Re-fork from a different block.

**Engine finds opportunities but P&L is zero in dry-run**
→ That's expected. Dry-run simulates but doesn't execute. The logged "estimated profit" is what would have happened.
