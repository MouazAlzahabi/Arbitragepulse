# ArbitrageExecutor v3 — Foundry

Atomic cross-DEX arbitrage executor for Base. The engine calls one of two entry points:

| Function | Route |
|----------|-------|
| `executeArbitrage(tokenIn, tokenOut, amountIn, routerA, routerB, feeA, feeB, minProfit, deadline)` | `tokenIn → tokenOut` on A, back on B |
| `executeTriangularArbitrage(tokenA, tokenB, tokenC, amountIn, routerAB, routerBC, routerCA, feeAB, feeBC, feeCA, minProfit, deadline)` | `A → B → C → A` |

Both revert unless the contract ends with at least `amountIn + minProfit` of the start token.
The trade uses the contract's own balance; profit stays in the contract until `withdrawToken`.

## Router types

Each router must be whitelisted with a type. Ordinals are part of the engine interface:

| Type | Value | Swap call | `fee` argument |
|------|-------|-----------|----------------|
| V2 | 0 | `swapExactTokensForTokens` | ignored |
| V3 | 1 | SwapRouter02 7-field `exactInputSingle` (Uniswap V3, PancakeSwap V3) | pool fee tier, must be non-zero |
| — | 2, 3 | retired (Solidly 3-field, SyncSwap) — rejected | |
| Aerodrome | 4 | 4-field `Route{from,to,stable,factory}` | `0` volatile pool, non-zero stable pool |

```bash
cast send $EXECUTOR "setRouter(address,uint8)" $ROUTER $TYPE --private-key $PRIVATE_KEY --rpc-url $RPC
cast send $EXECUTOR "revokeRouter(address)" $ROUTER ...                    # stop using it
```

## Readable failures

Every failure path reverts with a custom error the engine decodes (`engine/src/revert.rs`)
into a log line such as `not profitable: balance 1000 → 998, needed +1` or
`leg 1 (V3 router 0x2626…) reverted with no reason (no pool for this pair/fee tier, or wrong
router type — check setRouter)`.

`minProfit` is enforced once, on the final balance: routers get a zero min-out, so a price
move reads as `NotProfitable` with the exact shortfall on every DEX (PancakeSwap's router
reverts on slippage with no reason at all). The transaction is atomic, so this protects
the capital exactly like a router-level minimum.

| Error | Meaning |
|-------|---------|
| `LegFailed(leg, router, rtype, reason)` | a router reverted; `reason` is its raw revert data (string, custom error such as Aerodrome's, or empty). Empty: no pool for that fee tier / stable flag, or the wrong router type |
| `LegExhaustedGas(leg, router, reason)` | the router used all the gas it was given (whatever it reverted with — Uniswap re-raises an inner out-of-gas as "TF"/"STF"): raise the tx gas limit — or, if it persists at a higher limit, an invalid opcode / assert in the router or pool (the EVM can't tell them apart). At very tight limits the executor can't even pay for this revert; the tx then fails with no data and the engine reads out-of-gas from the receipt (gas used ≥ 97% of the limit) |
| `NotProfitable(balanceBefore, balanceAfter, minProfit)` | the route completed but returned less than `amountIn + minProfit` — price moved or spread closed |
| `InsufficientBalance(token, balance, needed)` | the contract does not hold `amountIn` of the start token |
| `NoOutput(leg, router)` | a router returned without paying anything out (or the output balance shrank) |
| `InvalidV3Fee(leg, router, fee)` | V3 fee outside 100..999999 — 0, or a bps value like 30 instead of 3000 |
| `RouterNotAllowed(router)` / `UnsupportedRouterType(router, rtype)` | not whitelisted / retired type |
| `InvalidPath()` | repeated token in the route |
| `DeadlineExpired(deadline, now)`, `ZeroAmount()`, `ZeroAddress()` | input checks |
| `EnforcedPause()`, `OwnableUnauthorizedAccount(caller)` | OpenZeppelin: paused / not owner |
| `NotAContract(router)`, `RenounceDisabled()` | `setRouter` given an address with no code; `renounceOwnership` is disabled (it would lock every withdrawal) |

`setRouter` is the only way to allow a router (it also rejects addresses with no code), so
no router is ever allowed with a forgotten type; call it again to change the type.

Routers are called with a low-level call and their return value is never decoded: each
leg is measured by balance delta, so a router that misreports (or returns nothing) cannot
make the next leg over-spend, and still fails as `NoOutput`/`LegFailed` naming the leg.
Each leg approves exactly its input; a leftover allowance is cleared best-effort (a token
that rejects `approve(0)` is left at 1 wei instead of aborting a finished trade;
`revokeApproval` does the same). If even that fails, `AllowanceNotCleared(token, spender,
remaining)` is emitted and the trade still completes.

Events: `ArbitrageExecuted(tokenIn, tokenOut, routerA, routerB, amountIn, profit)` for 2-hop,
`TriangularArbitrageExecuted(tokenA, tokenB, tokenC, routerAB, routerBC, routerCA, amountIn, profit)`
for triangular.

Fee-on-transfer tokens are **not** supported as a leg input: real V2/Aerodrome pools revert
(`K`) and V3 pools revert (`IIA`) when the input arrives short.

v3 removed `batchExecute`, `executeArbitrageMultiHop` and `estimateArbitrage` (unused by the
engine) and the Solidly/SyncSwap paths. Gas per trade (mocks): ~185k for two legs, ~320k
triangular; real pools add their own swap cost on top.

## Commands

```bash
forge build
forge test                 # no fork needed, all mocks
forge test -vvv            # traces on failure
forge test --gas-report

# Local Anvil fork of Base
anvil --fork-url $BASE_RPC --chain-id 8453 --port 8545
forge script script/Deploy.s.sol --rpc-url local --broadcast

# Base Sepolia (testing stage) — then DeployMocks.s.sol, see ../TESTNET_DEPLOYMENT_GUIDE.md
forge script script/Deploy.s.sol --rpc-url base_sepolia --broadcast --verify

# Base mainnet
forge script script/Deploy.s.sol --rpc-url base --broadcast --verify
```

`.env` (see `.env.example`): `PRIVATE_KEY`, `BASE_RPC`, `BASE_SEPOLIA_RPC`, and
`ETHERSCAN_API_KEY` (Etherscan API V2 — one etherscan.io key verifies on both chains; needs
Foundry ≥ 1.3, where V2 is the default — run `foundryup` if verification hits the old BaseScan API).

The Rust lab (`cargo run -p lab -- setup`) deploys from the compiled artifact
`out/ArbitrageExecutor.sol/ArbitrageExecutor.json`, so run `forge build` first.

## Layout

```
src/
  ArbitrageExecutor.sol      main contract
  mocks/                     MockERC20, MockRouter (V2), MockV3Router, MockAerodromeRouter,
                             MockOddTokens (odd tokens and misbehaving routers: fee-on-transfer, approve
                             quirks, slashing, lying/no-op/silent/partial/gas-burning routers)
script/
  Deploy.s.sol               executor
  DeployMocks.s.sol          Base Sepolia mock tokens seeded on real Uniswap V2/V3 at a spread
  RewidenMocks.s.sol         restore that spread after arbs narrow it
  MockMarket.sol             shared addresses, prices and sqrtPriceX96 math
test/
  ArbitrageExecutor.t.sol    executor (58 tests)
  MockMarket.t.sol           mock-market price math
```
