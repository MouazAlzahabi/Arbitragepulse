# 🧪 ArbitragePulse Lab (ARB-004)

Local simulation environment for testing the entire ArbitragePulse system without spending real money.

## What This Does

Forks a real blockchain (Optimism/Base/Gnosis) locally, deploys your contract, "steals" tokens from real whale addresses, creates artificial price imbalances, and lets you test everything end-to-end.

## Prerequisites

1. **Foundry (Anvil)** — install: `curl -L https://foundry.paradigm.xyz | bash && foundryup`
2. **Bun** — install: `curl -fsSL https://bun.sh/install | bash`
3. **Compiled contract** — run `cd ../contract && forge build`
4. **RPC URL** — free Alchemy/QuickNode HTTP endpoint

## Quick Start

```bash
# Terminal 1: Start the fork
cp .env.example .env  # Add your RPC URL
bun install
bun run fork:optimism

# Terminal 2: Run the full scenario
bun run scenario
```

That's it. One command deploys, funds, creates an arb, executes it, and prints a P&L report.

## Scripts

| Command | What It Does |
|---------|-------------|
| `bun run fork:optimism` | Start Anvil fork of Optimism |
| `bun run fork:base` | Start Anvil fork of Base |
| `bun run fork:gnosis` | Start Anvil fork of Gnosis |
| `bun run setup` | Deploy contract + fund wallet + fund contract |
| `bun run create-arb` | Whale swap to create price imbalance |
| `bun run test-reverts` | Trigger all revert reasons (NotProfitable, Deadline, etc.) |
| `bun run scenario` | Full E2E: deploy → fund → imbalance → arb → P&L |

## Testing Your Engine Against the Fork

After running `bun run setup`, point your engine at Anvil:

**config.yaml:**
```yaml
chains:
  - id: 10
    name: "Optimism-Fork"
    enabled: true
    ws_rpc: "ws://127.0.0.1:8545"
    http_rpc: "http://127.0.0.1:8545"
    contract_address: "0x..."  # from setup output
```

Then in another terminal: `bun run create-arb` to trigger opportunities.

## How Impersonation Works

Anvil can "become" any address on the forked chain. We use this to:
1. Find a known whale (e.g., Optimism Bridge, Balancer Vault)
2. Call `anvil_impersonateAccount` on that address
3. Transfer their tokens to our test wallet
4. Stop impersonating

No real tokens are moved — this only affects the local fork.

## Adjustments From Original Ticket

1. **Multi-chain** — supports Optimism/Base/Gnosis, not just Base
2. **Contract deployment + funding** — ticket didn't mention this critical step
3. **Full E2E scenario** — one-command proof the whole system works
4. **Revert testing** — intentionally trigger every failure mode
5. **Both arb directions** — if USDC→WETH doesn't work, tries WETH→USDC
6. **P&L report** — verifies profit actually landed in the contract
7. **Lab state file** — `.lab-state.json` so scripts share contract address
