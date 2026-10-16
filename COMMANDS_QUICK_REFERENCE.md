# Commands — quick reference

Instances: `base` (Base mainnet, `engine/config.yaml`, port 3000) and `base-sepolia`
(testing stage, `engine/config-testnet.yaml`, port 3001). Replace as needed.

## Build and test

```bash
cargo build --release -p engine          # engine binary: target/release/engine
cargo test -p engine --bin engine        # engine unit tests
(cd contract && forge test)              # contract tests
npm run build                            # dashboard → dist/
```

## Local end-to-end (Anvil fork of Base)

```bash
anvil --fork-url https://base-mainnet.g.alchemy.com/v2/$ALCHEMY_KEY --chain-id 8453 --block-time 2
cargo run -p lab -- setup
cargo run -p lab -- scenario
cargo run -p lab -- test-reverts
```

## Contracts

```bash
cd contract
forge script script/Deploy.s.sol      --rpc-url $RPC --broadcast   # executor
forge script script/DeployMocks.s.sol --rpc-url $RPC --broadcast   # testnet mock tokens + pools

# Router types: 0 = V2, 1 = V3, 4 = Aerodrome
cast send $EXECUTOR "setAllowedRouter(address,bool)" $ROUTER true  --private-key $PRIVATE_KEY --rpc-url $RPC
cast send $EXECUTOR "setRouterType(address,uint8)"   $ROUTER $TYPE --private-key $PRIVATE_KEY --rpc-url $RPC
cast call $EXECUTOR "allowedRouters(address)(bool)" $ROUTER --rpc-url $RPC

cast send $EXECUTOR "withdrawToken(address)" $TOKEN --private-key $PRIVATE_KEY --rpc-url $RPC
cast send $EXECUTOR "withdrawETH()"                 --private-key $PRIVATE_KEY --rpc-url $RPC
cast send $EXECUTOR "pause()"                       --private-key $PRIVATE_KEY --rpc-url $RPC
```

## Deploy and service

```bash
./scripts/deploy.sh                          # sync + build + restart arbitragepulse@base
./scripts/deploy.sh --instance=base-sepolia  # same for the testing stage
./scripts/deploy.sh --code-only              # config/script change, no rebuild

sudo bash scripts/server/setup.sh base       # once per instance, on the server
sudo systemctl restart arbitragepulse@base
systemctl status arbitragepulse@base
journalctl -fu arbitragepulse@base
tail -f /var/log/arbitragepulse/base.log
```

## Engine API

```bash
H="Authorization: Bearer $API_KEY"; E=http://localhost:3000
curl -s -H "$H" $E/health        # 503 = RPC lost or in backoff (see rpc_backoff)
curl -s -H "$H" $E/stats
curl -s -H "$H" $E/trades
curl -s -X POST -H "$H" $E/engine/pause
curl -s -X POST -H "$H" $E/engine/resume
curl -s -X POST -H "$H" -H 'Content-Type: application/json' -d '{"enabled":true}' $E/engine/dry-run
curl -s -H "$H" $E/pair-scan
curl -s -X POST -H "$H" $E/pair-scan/base-usdc-weth/toggle
```

## Dashboard over SSH

```bash
ssh -L 3000:localhost:3000 botadmin@SERVER   # then open the dashboard locally
```
