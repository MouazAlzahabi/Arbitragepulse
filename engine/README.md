# ⚡ ArbitragePulse Engine (ARB-002)

Multi-chain TypeScript MEV engine built with Bun + Viem. Monitors DEX prices across multiple chains simultaneously, detects cross-DEX arbitrage, and executes via the on-chain ArbitrageExecutor contract (ARB-001).

## Architecture

```
                         ┌─────────────────────┐
                         │   tokens.yaml        │  ← You curate this
                         │   (Token Allowlist)   │
                         └──────────┬────────────┘
                                    │  auto-generates pairs
  ┌──────────┐           ┌──────────▼────────────┐
  │ Chain 1  │──────────▶│     Engine Pipeline    │
  │ (OP WS)  │           │                        │
  ├──────────┤           │  Listener → Strategy   │
  │ Chain 2  │──────────▶│     → Executor         │──── TX to chain
  │ (GNO WS) │           │                        │
  ├──────────┤           └──────────┬─────────────┘
  │ Chain N  │──────────▶           │
  └──────────┘           ┌──────────▼─────────────┐
                         │   Dashboard / API       │
                         │   WS /ws  •  GET /health│
                         │   Token CRUD /tokens    │
                         └─────────────────────────┘
```

**Flow per chain:** Swap Event → Multicall all router quotes → Find best buy/sell combo → Profitable after gas? → Execute on-chain (atomic) → Log to Dashboard

## Quick Start

```bash
# Install
bun install

# Configure
cp .env.example .env          # Add your private key
cp config.example.yaml config.yaml   # Add RPC URLs + contract addresses
cp tokens.example.yaml tokens.yaml   # Review token allowlist

# Run (dry-run by default — no real trades)
bun run start

# Development (auto-reload on file changes)
bun run dev
```

## Configuration: 3 Files

### 1. `.env` — Secrets only
```
PRIVATE_KEY=0x...    # Same key across all chains
PORT=3000
LOG_LEVEL=info
```

### 2. `config.yaml` — Chains + Routers
Add any EVM chain by providing chain ID, RPCs, and your deployed contract address. Add any Uniswap V2-compatible DEX by providing the router address.

```yaml
chains:
  - id: 42161           # Arbitrum — just add a block, no code changes
    name: "Arbitrum"
    enabled: true
    ws_rpc: "wss://..."
    http_rpc: "https://..."
    contract_address: "0x..."

routers:
  - id: "camelot"
    name: "Camelot"
    chain_id: 42161
    address: "0x..."
    type: "v2"
```

### 3. `tokens.yaml` — Token Allowlist (Layer 1 + 2)

This is where you control WHAT the bot trades. Only tokens marked `trusted: true` generate trading pairs. The engine auto-generates all valid pair combinations.

```yaml
optimism:
  - symbol: "WETH"
    address: "0x4200000000000000000000000000000000000006"
    decimals: 18
    trusted: true          # ← actively traded
    category: "blue_chip"  # ← determines trade size

  - symbol: "AAVE"
    address: "0x76FB31fb..."
    decimals: 18
    trusted: false          # ← monitored only, not traded yet
    category: "defi"
```

**Math:** 8 trusted tokens on a chain = 48 directional pairs (8 × 7, minus stable↔stable). Add one token → 14 new pairs instantly.

**Categories & trade sizes:**
| Category | Default Trade Size | Example Tokens |
|----------|-------------------|----------------|
| `stable` | $150 | USDC, USDT, DAI |
| `blue_chip` | 0.05 ETH (~$150) | WETH, OP, WBTC |
| `defi` | 0.03 ETH (~$90) | LINK, SNX, AAVE |
| `meme` | 0.01 ETH (~$30) | VELO, DEGEN |

## API Endpoints

### Monitoring
| Endpoint | Description |
|----------|-------------|
| `GET /health` | Uptime, balances, per-chain stats |
| `GET /stats` | Execution statistics |
| `WS /ws` | Live log stream (JSON) |

### Token Management (Layer 3 — for Dashboard)
| Endpoint | Description |
|----------|-------------|
| `GET /tokens` | List all tokens (`?chain_id=10&trusted_only=true`) |
| `POST /tokens` | Add token: `{ symbol, address, decimals, chain_id, category }` |
| `PATCH /tokens/:chainId/:address` | Update: `{ trusted: true }` |
| `DELETE /tokens/:chainId/:address` | Remove token |
| `GET /tokens/pairs` | Preview auto-generated pairs (`?chain_id=10`) |

### Example: Add a token via API
```bash
curl -X POST http://localhost:3000/tokens \
  -H "Content-Type: application/json" \
  -d '{"symbol":"LINK","address":"0x350a...","decimals":18,"chain_id":10,"category":"defi"}'

# Then trust it:
curl -X PATCH http://localhost:3000/tokens/10/0x350a... \
  -H "Content-Type: application/json" \
  -d '{"trusted":true}'
```

### WebSocket Messages
```json
{ "type": "info", "timestamp": 1234, "message": "..." }
{ "type": "opportunity", "timestamp": 1234, "message": "ARB FOUND", "data": { "profitUsd": "0.42", "buyOn": "Velodrome", "sellOn": "SushiSwap" } }
{ "type": "trade", "timestamp": 1234, "message": "TX confirmed", "data": { "txHash": "0x...", "profit": "$0.42" } }
{ "type": "error", "timestamp": 1234, "message": "RPC timeout" }
```

## How V2 vs V3 Monitoring Works

The engine supports both pool types but with different roles:

- **V2 routers** → used for both **price checking** and **execution** (matches ARB-001 contract's `swapExactTokensForTokens`)
- **V3 routers** → used for **price monitoring only** (detects large swaps that create V2 lagging)

When a large V3 swap happens, V2 pools often haven't caught up. The bot detects this mismatch and executes on V2, capturing the lag.

## Going Live

1. Run in dry-run for 24+ hours — check logs for opportunities
2. Verify simulated P&L looks reasonable
3. In `src/index.ts`, change `DRY_RUN = true` → `DRY_RUN = false`
4. Start with small trade sizes in `tokens.yaml` defaults
5. Monitor via `GET /health` or WebSocket dashboard
6. Scale up trade sizes gradually as you gain confidence

## Safety Rules

- Only `trusted: true` tokens are traded — everything else is ignored
- Stable↔stable pairs are skipped (USDC↔USDT arb is negligible on V2)
- The on-chain contract has its own atomic check: if the trade isn't profitable, it reverts
- You can pause the bot via the contract's `pause()` function
- Withdrawals always work even when paused
