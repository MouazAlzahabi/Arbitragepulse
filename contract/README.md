# ArbitrageExecutor v2 — Foundry

Atomic cross-DEX arbitrage executor contract. Migrated from Hardhat to **Foundry** for faster compilation, Solidity-native tests, and native Anvil integration with the ArbitragePulse Lab.

## Prerequisites

Install Foundry:
```bash
curl -L https://foundry.paradigm.xyz | bash
foundryup
```

## Setup

```bash
# Install dependencies (OpenZeppelin + forge-std)
forge install OpenZeppelin/openzeppelin-contracts --no-commit
forge install foundry-rs/forge-std --no-commit

# Copy env
cp .env.example .env
# Edit .env with your keys
```

## Commands

```bash
# Compile
forge build

# Run all tests (fast — no fork needed, uses mocks)
forge test

# Run tests with verbosity (see traces on failure)
forge test -vvv

# Run a specific test
forge test --match-test test_ExecuteProfitableArb -vvv

# Gas report
forge test --gas-report

# Deploy to local Anvil fork
anvil --fork-url $OPTIMISM_RPC --port 8545
forge script script/Deploy.s.sol --rpc-url http://127.0.0.1:8545 --broadcast

# Deploy to Optimism mainnet
forge script script/Deploy.s.sol --rpc-url $OPTIMISM_RPC --broadcast --verify

# Deploy to Base
forge script script/Deploy.s.sol --rpc-url $BASE_RPC --broadcast --verify
```

## Integration with ArbitragePulse Lab

The Lab's `setup-fork.ts` deploys this contract to Anvil using the compiled artifact. After running `forge build`, the artifact is at:

```
out/ArbitrageExecutor.sol/ArbitrageExecutor.json
```

Update the lab's `.env`:
```
CONTRACT_ARTIFACT_PATH=../arb-contract-foundry/out/ArbitrageExecutor.sol/ArbitrageExecutor.json
```

## Project Structure

```
src/
  ArbitrageExecutor.sol    — Main contract
  mocks/
    MockERC20.sol           — Test token
    MockRouter.sol          — Test DEX router
test/
  ArbitrageExecutor.t.sol  — Full test suite (38 tests)
script/
  Deploy.s.sol              — Deployment script
```

## Test Coverage

All original Hardhat tests ported 1:1 to Forge Solidity tests:

| Category | Tests |
|----------|-------|
| ETH deposit/withdraw | 4 |
| Token withdrawal | 5 |
| Arbitrage execution | 5 |
| Access control | 7 |
| #1 Deadline | 3 |
| #2 minProfit | 2 |
| #4 Allowance hygiene | 2 |
| #5 Ownable2Step | 3 |
| #6 Batch execution | 5 |
| #7 Profit accumulator | 4 |
| #8 Pausable | 4 |
| estimateArbitrage | 2 |
| **Total** | **46** |

## Why Foundry over Hardhat?

- **10-50x faster tests** — Solidity tests run in the EVM directly, no JS overhead
- **Native Anvil** — same tool for fork, test, and deploy
- **forge script** — deploy scripts are Solidity too, type-safe
- **Better traces** — `forge test -vvvv` shows full call traces on revert
- **Gas snapshots** — `forge snapshot` for tracking gas regressions
- **No node_modules** — dependencies are git submodules in `lib/`
