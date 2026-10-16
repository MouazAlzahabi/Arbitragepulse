# Testing stage — Base Sepolia

The testing stage is a separate engine instance on **Base Sepolia** (chain id 84532),
using the same code path as production. Because Base Sepolia has little real
liquidity, `contract/script/DeployMocks.s.sol` creates two mock tokens and seeds them
on the **real** Uniswap V2 and V3 deployments ~3% apart. The engine finds those pools
through its normal discovery and arbitrages them — no special test code in the engine.

All addresses below were verified on-chain on 2026-10-16.

| Contract | Address |
|---|---|
| Uniswap V3 SwapRouter02 | `0x94cC0AaC535CCDB3C01d6787D6413C739ae12bc4` |
| Uniswap V3 QuoterV2 | `0xC5290058841028F1614F3A6F0F5816cAd0df5E27` |
| Uniswap V3 Factory | `0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24` |
| Uniswap V3 NonfungiblePositionManager | `0x27F971cb582BF9E50F397e4d29a5C7A34f11faA2` |
| Uniswap V2 Router02 | `0x1689E7B1F10000AE47eBfE339a4f69dECd19F602` |
| Uniswap V2 Factory | `0x7Ae58f10f7849cA6F5fB71b7f45CB416c9204b1e` |
| WETH | `0x4200000000000000000000000000000000000006` |
| USDC (Circle) | `0x036CbD53842c5426634e7929541eC2318f3dCF7e` |

## 1. Wallet and RPC

- Create a **testnet-only** key. Never reuse the production key.
- Get Base Sepolia ETH from a faucet (e.g. the Coinbase Developer Platform or Alchemy
  Base Sepolia faucet). ~0.05 ETH covers deployment and many test trades.
- In Alchemy, enable **Base Sepolia** on your app (the config uses
  `wss://base-sepolia.g.alchemy.com/v2/${ALCHEMY_KEY}`).

```bash
export PRIVATE_KEY=0x...          # testnet key
export RPC=https://sepolia.base.org
```

## 2. Deploy the executor

```bash
cd contract
forge script script/Deploy.s.sol --rpc-url $RPC --broadcast
# → "ArbitrageExecutor deployed to: 0x…"   (call it $EXECUTOR)
```

## 3. Deploy and seed the mock tokens

```bash
forge script script/DeployMocks.s.sol --rpc-url $RPC --broadcast
# prints mUSDC, mWETH, the V2 pair (1 mWETH = 2500 mUSDC)
# and the V3 pool (1 mWETH = 2575 mUSDC, 0.3% fee)
```

## 4. Whitelist the routers and fund the executor

Router types: `0` = V2, `1` = V3, `4` = Aerodrome.

```bash
V2=0x1689E7B1F10000AE47eBfE339a4f69dECd19F602
V3=0x94cC0AaC535CCDB3C01d6787D6413C739ae12bc4
cast send $EXECUTOR "setAllowedRouter(address,bool)" $V2 true --private-key $PRIVATE_KEY --rpc-url $RPC
cast send $EXECUTOR "setAllowedRouter(address,bool)" $V3 true --private-key $PRIVATE_KEY --rpc-url $RPC
cast send $EXECUTOR "setRouterType(address,uint8)"   $V2 0    --private-key $PRIVATE_KEY --rpc-url $RPC
cast send $EXECUTOR "setRouterType(address,uint8)"   $V3 1    --private-key $PRIVATE_KEY --rpc-url $RPC

# Trading capital: 50,000 mUSDC (mock tokens have 18 decimals)
cast send $MUSDC "transfer(address,uint256)" $EXECUTOR 50000000000000000000000 \
  --private-key $PRIVATE_KEY --rpc-url $RPC
```

Send each command and let it confirm before the next — they share one nonce sequence.

## 5. Configure the instance

In `engine/config-testnet.yaml`:
- `contract_address:` → `$EXECUTOR`
- uncomment the `base-sepolia-mock-usdc-weth` pair and paste the mUSDC / mWETH addresses

Create `engine/base-sepolia.env` from `engine/base-sepolia.env.example` (testnet key,
`PORT=3001`, `DB_PATH=trades-base-sepolia.db`, `DRY_RUN=true`).

## 6. Run — dry run first

Locally:

```bash
set -a; source engine/base-sepolia.env; set +a
cargo run --release -p engine
```

On the server: `sudo bash scripts/server/setup.sh base-sepolia` (see the production
guide), then `./scripts/deploy.sh --instance=base-sepolia` for later updates.

What to expect:
- startup: `Loaded chain Base Sepolia (id 84532)`, then V2 and V3 pools discovered
- the mock pair produces opportunities each scan; in dry run each ends in
  `DRY-RUN ok | gross=$… gas≈$… net=$…` (the executor simulation passed)
- the real USDC/WETH testnet pools are wildly mispriced and get logged as
  `phantom skipped` — expected; that is the phantom cap doing its job
- `curl -H "Authorization: Bearer $API_KEY" localhost:3001/health` → `"status":"ok"`

Then set `DRY_RUN=false` and restart: trades are sent and confirmed on Base Sepolia.

## 7. Promote to production

The production instance uses the same binary with `engine/config.yaml`. Promote when
the testnet instance has run cleanly: dry runs pass, live trades confirm, no circuit
breaker trips, and `/health` stays `ok`. Then follow
[PRODUCTION_DEPLOYMENT_GUIDE.md](PRODUCTION_DEPLOYMENT_GUIDE.md).

## Troubleshooting

| Symptom | Cause |
|---|---|
| `refusing to start (wrong RPC URL or wrong config file?)` | RPC chain id ≠ `chain.id` — e.g. the mainnet config pointed at a Sepolia RPC |
| `contract_address is not set` | `YOUR_…` placeholder still in the config |
| Simulation reverts `0x118cdaa7` | the engine's key is not the executor owner |
| Simulation reverts on router | router not whitelisted, or wrong `setRouterType` |
| `insufficient token_a balance` | executor holds no mUSDC |
| `/health` 503, `rpc_backoff: true` | RPC hanging, down or slower than the simulation timeout — execution resumes automatically when it recovers |
