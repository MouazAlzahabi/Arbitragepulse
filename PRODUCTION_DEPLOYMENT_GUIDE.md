# Production — Base mainnet

Production is one engine instance on **Base mainnet** (chain id 8453), run by systemd on
a Linux server, with `engine/config.yaml`. Each additional chain is another instance of
the same binary (see [Running another chain](#running-another-chain)).

Run the [testing stage](TESTNET_DEPLOYMENT_GUIDE.md) first.

## 1. RPC

The config expects Alchemy (`${ALCHEMY_KEY}` is substituted from the instance env):
- `ws_rpc` — Alchemy Base WebSocket (block/log subscriptions and the pending-tx monitor
  need a WS endpoint that exposes the mempool)
- `http_rpc`, `ws_rpc_fallbacks`, `submission_rpcs` — public Base endpoints; signed txs
  are broadcast to all submission RPCs at once (Base sequencer is first-come-first-served)

Size `chain.rpc_concurrency` to your Alchemy plan (3 for the free tier, 10–12 for Growth).

## 2. Server

Any small Linux VPS (2 vCPU / 4 GB is plenty) close to the Base sequencer region (US).

```bash
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
curl -L https://foundry.paradigm.xyz | bash && foundryup     # for cast/forge
sudo adduser botadmin
```

The scripts assume the repo lives at `/home/botadmin/Arbitragepulse` and runs as
`botadmin`; change `SCRIPTS_DIR`/`SERVICE_USER` in `scripts/server/setup.sh` and the
paths in `scripts/server/arbitragepulse@.service` otherwise.

## 3. Executor contract

The current Base executor is configured in `engine/config.yaml`
(`contract_address`). To deploy a new one:

```bash
cd contract
forge script script/Deploy.s.sol --rpc-url base --broadcast --verify   # BASE_RPC + ETHERSCAN_API_KEY in contract/.env
```

Whitelist every active router (types: `0` = V2, `1` = V3, `4` = Aerodrome):

| Router | Address | Type |
|---|---|---|
| Uniswap V3 SwapRouter02 | `0x2626664c2603336E57B271c5C0b26F421741e481` | 1 |
| PancakeSwap V3 SmartRouter | `0x678Aa4bF4E210cf2166753e054d5b7c31cc7fa86` | 1 |
| Aerodrome V2 Router | `0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43` | 4 |

```bash
cast send $EXECUTOR "setRouter(address,uint8)" $ROUTER $TYPE --private-key $PRIVATE_KEY --rpc-url https://mainnet.base.org
```

Fund the executor with the pairs' `token_in` (mostly USDC) and the wallet with a little
ETH for gas (`chain.min_native_balance`). Withdraw with `withdrawToken(token)` /
`withdrawETH()`; `pause()` freezes the contract.

Failed trades are logged with the decoded contract error — which leg, which router and the
router's own reason (see [contract/README.md](contract/README.md#readable-failures)).

## 4. Instance environment

```bash
cp engine/base.env.example engine/base.env   # on the server
chmod 600 engine/base.env
```

Fill in `PRIVATE_KEY` (the executor owner), `ALCHEMY_KEY`, `API_KEY`. Keep
`CONFIG_PATH=engine/config.yaml`, `PORT=3000`, `DB_PATH=trades-base.db`,
`DRY_RUN=true` for the first run. `*.env` files are gitignored and are not synced by
`deploy.sh` — they live only on the server.

## 5. Install and deploy

From your machine (rsync + build on the server + restart + health check):

```bash
./scripts/deploy.sh                    # default instance: base
```

On the server, once:

```bash
sudo bash /home/botadmin/Arbitragepulse/scripts/server/setup.sh base
```

This installs the systemd template `arbitragepulse@.service`, log rotation, and enables
`arbitragepulse@base` (env `engine/base.env`, log `/var/log/arbitragepulse/base.log`).
It also removes the old single-unit `arbitragepulse.service` if present.

Go live after a clean dry-run period: set `DRY_RUN=false` in `engine/base.env` and
`sudo systemctl restart arbitragepulse@base` (or `POST /engine/dry-run {"enabled":false}`).

## 6. Monitoring

```bash
systemctl status arbitragepulse@base
journalctl -fu arbitragepulse@base
curl -s -H "Authorization: Bearer $API_KEY" localhost:3000/health | python3 -m json.tool
```

- `/health` returns **503** when the RPC is lost or the chain is in **RPC backoff**
  (`rpc_backoff: true`: node hanging, down or too slow to simulate on — detection
  continues, execution is off until the monitor sees the node healthy again). Point
  uptime checks at the status code.
- The heartbeat line (`♥ scans=… | last_scan=…s ago`) logs at WARN if no scan ran for
  `tuning.heartbeat_stall_warn_ms`.
- The circuit breaker pauses the chain after `tuning.max_consecutive_failures`
  execution failures (RPC outages don't count); resume with `POST /engine/resume`.
- The dashboard (`dist/`, served by your web server or `npm run dashboard` locally,
  e.g. through an SSH tunnel to port 3000) shows the live feed, stats and backoff state.

Tuning (`tuning:` in the config) needs a restart; routers, pairs and profit floors
hot-reload.

## Running another chain

One instance per chain, on the same server or another:

1. Write a config for the chain: copy `engine/config.yaml`, set the `chain:` block (id,
   RPCs, `block_time_ms`, executor, wrapped native), its routers and pairs, and any
   `tuning:` overrides. Block-relative timings follow `block_time_ms` automatically.
2. Deploy and whitelist an executor on that chain.
3. Create `engine/<name>.env` with its own `CONFIG_PATH`, `PORT`, `DB_PATH` and wallet.
4. `sudo bash scripts/server/setup.sh <name>` → `arbitragepulse@<name>` runs alongside.

The engine refuses to start if the RPC's chain id differs from `chain.id`, so a config
can't accidentally run against the wrong network.
