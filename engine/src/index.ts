/**
 * ArbitragePulse Engine — Multi-Chain Entry Point
 *
 * Spawns one monitoring + execution pipeline per enabled chain.
 * All chains share the same wallet (different contract address per chain).
 *
 * Usage:
 *   bun run src/index.ts
 *   CONFIG_PATH=my-config.yaml bun run src/index.ts
 */

import {
  createPublicClient,
  createWalletClient,
  http,
  webSocket,
  formatEther,
  defineChain,
  type Chain,
} from "viem";
import { privateKeyToAccount } from "viem/accounts";

import {
  ENV,
  loadConfig,
  getChainRouters,
  getChainPairs,
  getV2Routers,
  getV3Routers,
  TokenRegistry,
  type ChainConfig,
  type AppConfig,
} from "./config";
import { SwapListener } from "./listeners";
import { ArbitrageStrategy } from "./strategies";
import { ArbExecutor } from "./executor";
import { ApiServer } from "./api";
import { log } from "./utils";

// ── Dynamic chain definition ──

function toViemChain(cfg: ChainConfig): Chain {
  return defineChain({
    id: cfg.id,
    name: cfg.name,
    nativeCurrency: {
      name: cfg.nativeCurrency,
      symbol: cfg.nativeCurrency,
      decimals: 18,
    },
    rpcUrls: {
      default: { http: [cfg.httpRpc] },
    },
  });
}

// ── Per-chain engine ──

interface ChainEngine {
  chain: ChainConfig;
  listener: SwapListener;
  strategy: ArbitrageStrategy;
  executor: ArbExecutor;
  swapCount: number;
  oppCount: number;
}

// ── Main ──

async function main() {
  console.log(`
  ⚡ ═══════════════════════════════════════════ ⚡
     A R B I T R A G E   P U L S E   E N G I N E
              M U L T I - C H A I N
  ⚡ ═══════════════════════════════════════════ ⚡
  `);

  // ─── 1. Load token registry + config ───

  const tokenRegistry = new TokenRegistry("tokens.yaml");
  tokenRegistry.load();

  const config = loadConfig(ENV.CONFIG_PATH, tokenRegistry);
  const enabledChains = config.chains.filter((c) => c.enabled);

  log("info", `Loaded config: ${enabledChains.length} chain(s), ${config.routers.length} router(s), ${config.pairs.length} pair(s) (auto-generated from ${tokenRegistry.getTrusted().length} trusted tokens)`);

  if (enabledChains.length === 0) {
    log("error", "No enabled chains. Enable at least one chain in config.yaml");
    process.exit(1);
  }

  // ─── 2. Shared wallet ───

  const account = privateKeyToAccount(ENV.PRIVATE_KEY);
  log("info", `Wallet: ${account.address}`);

  // ─── 3. Start API server ───

  const api = new ApiServer();
  api.setTokenRegistry(tokenRegistry);
  await api.init();

  // ─── 4. Spin up one engine per chain ───

  const engines: ChainEngine[] = [];
  const startTime = Date.now();

  // ⚠️ DRY RUN by default — toggle from dashboard or set to false for live
  let DRY_RUN = true;
  let GLOBAL_PAUSED = false;

  for (const chainCfg of enabledChains) {
    log("info", `\n━━━ Initializing ${chainCfg.name} (chain ${chainCfg.id}) ━━━`);

    try {
      const viemChain = toViemChain(chainCfg);

      // Clients
      const wsClient = createPublicClient({
        chain: viemChain,
        transport: webSocket(chainCfg.wsRpc, { retryCount: 5, retryDelay: 2000 }),
      });

      const httpClient = createPublicClient({
        chain: viemChain,
        transport: http(chainCfg.httpRpc),
      });

      const walletClient = createWalletClient({
        chain: viemChain,
        transport: http(chainCfg.httpRpc),
        account,
      });

      // Verify connection
      const blockNumber = await httpClient.getBlockNumber();
      const ethBalance = await httpClient.getBalance({ address: account.address });

      log("info", `[${chainCfg.name}] Block: ${blockNumber} | Balance: ${formatEther(ethBalance)} ${chainCfg.nativeCurrency}`);

      if (ethBalance < BigInt(Math.floor(chainCfg.minNativeBalance * 1e18))) {
        log("warn", `[${chainCfg.name}] Low balance! Min: ${chainCfg.minNativeBalance} ${chainCfg.nativeCurrency}`);
      }

      // Routers and pairs for this chain
      const v2Routers = getV2Routers(config, chainCfg.id);
      const v3Routers = getV3Routers(config, chainCfg.id);
      const pairs = getChainPairs(config, chainCfg.id);

      if (pairs.length === 0) {
        log("warn", `[${chainCfg.name}] No pairs configured — skipping`);
        continue;
      }

      if (v2Routers.length < 2) {
        log("warn", `[${chainCfg.name}] Need at least 2 V2 routers for arb. Found: ${v2Routers.length}`);
      }

      // Executor
      const executor = new ArbExecutor(httpClient, walletClient, chainCfg, account.address);
      await executor.init();

      // Strategy
      const strategy = new ArbitrageStrategy(httpClient, chainCfg, v2Routers, v3Routers, pairs);
      await strategy.init();

      // Listener
      const listener = new SwapListener(wsClient);

      const engine: ChainEngine = {
        chain: chainCfg,
        listener,
        strategy,
        executor,
        swapCount: 0,
        oppCount: 0,
      };

      // Wire up: swap event → evaluate → execute
      listener.onSwap(async (swap) => {
        engine.swapCount++;

        if (GLOBAL_PAUSED) return;

        try {
          const opportunity = await strategy.evaluate(swap);

          if (opportunity && opportunity.netProfitable) {
            engine.oppCount++;

            const result = await executor.execute(opportunity, DRY_RUN);

            if (result.success) {
              log("trade", `[${chainCfg.name}] ${DRY_RUN ? "🧪 DRY" : "💰 LIVE"} trade succeeded`, {
                profit: `$${opportunity.profitUsd.toFixed(4)}`,
                txHash: result.txHash,
              });
            }
          }
        } catch (err) {
          log("error", `[${chainCfg.name}] Handler error: ${String(err)}`);
        }
      });

      // Start watching pools
      const v2Pools = pairs.flatMap((p) => p.watchPools);
      if (v2Pools.length > 0) {
        await listener.watchV2Pools(v2Pools);
      }

      // Fallback: periodic scanning
      const scanInterval = chainCfg.blockTimeMs * 2;
      setInterval(async () => {
        const fakeSwap = {
          pool: "0x0000000000000000000000000000000000000000" as `0x${string}`,
          type: "v2" as const,
          token0Amount: 0n,
          token1Amount: 0n,
          blockNumber: 0n,
          txHash: "periodic-scan",
          timestamp: Date.now(),
        };

        try {
          const opp = await strategy.evaluate(fakeSwap);
          if (opp && opp.netProfitable && !GLOBAL_PAUSED) {
            engine.oppCount++;
            await executor.execute(opp, DRY_RUN);
          }
        } catch (err) {
          log("debug", `[${chainCfg.name}] Scan error: ${String(err)}`);
        }
      }, scanInterval);

      // Also watch V3 pools (for price movement detection → triggers V2 check)
      // V3 swap events tell us "price just moved" which is when V2 arbs appear
      const v3Pools = pairs
        .flatMap((p) => p.watchPools)
        .filter(() => v3Routers.length > 0); // Only if V3 routers exist

      if (v3Pools.length > 0) {
        await listener.watchV3Pools(v3Pools);
      }

      listener.startFallbackPolling(v2Pools, scanInterval);
      engines.push(engine);

      log("info", `[${chainCfg.name}] ✅ Engine running (${v2Routers.length} V2 + ${v3Routers.length} V3 routers, ${pairs.length} pairs)`);

    } catch (err) {
      log("error", `[${chainCfg.name}] Init failed: ${err instanceof Error ? err.message : String(err)}`);
      log("error", `[${chainCfg.name}] Skipping this chain — fix config and restart`);
    }
  }

  if (engines.length === 0) {
    log("error", "No engines started! Check your config.yaml and RPC endpoints.");
    process.exit(1);
  }

  // ─── 5. Register engine controls ───

  api.setControls({
    pause: (chainId) => {
      if (chainId) {
        const eng = engines.find((e) => e.chain.id === chainId);
        if (eng) { eng.executor.pause(); }
      } else {
        GLOBAL_PAUSED = true;
        engines.forEach((e) => e.executor.pause());
        log("warn", "⏸ ALL chains paused from dashboard");
      }
    },
    resume: (chainId) => {
      if (chainId) {
        const eng = engines.find((e) => e.chain.id === chainId);
        if (eng) { eng.executor.resume(); }
      } else {
        GLOBAL_PAUSED = false;
        engines.forEach((e) => e.executor.resume());
        log("info", "▶ ALL chains resumed from dashboard");
      }
    },
    setDryRun: (enabled) => {
      const prev = DRY_RUN;
      DRY_RUN = enabled;
      if (prev && !enabled) {
        log("warn", "🔴 LIVE TRADING ENABLED from dashboard");
      } else if (!prev && enabled) {
        log("info", "🧪 Dry-run mode re-enabled from dashboard");
      }
    },
    getState: () => ({
      paused: GLOBAL_PAUSED,
      dryRun: DRY_RUN,
      chains: engines.map((e) => ({
        name: e.chain.name,
        chainId: e.chain.id,
        paused: false, // Individual pause state would need to be tracked in executor
      })),
    }),
  });

  // ─── 6. Health endpoint ───

  api.onHealth(async () => {
    const chainStats: Record<string, unknown> = {};

    for (const eng of engines) {
      chainStats[eng.chain.name] = {
        swapsDetected: eng.swapCount,
        opportunitiesFound: eng.oppCount,
        ...eng.executor.getStats(),
      };
    }

    return {
      uptime: Date.now() - startTime,
      startedAt: startTime,
      wsClients: 0,
      stats: {
        chains: engines.map((e) => e.chain.name),
        wallet: account.address,
        dryRun: DRY_RUN,
        paused: GLOBAL_PAUSED,
        ...chainStats,
      },
    };
  });

  // ─── 7. Start server ───

  await api.start();

  log("info", `\n🚀 ArbitragePulse running on ${engines.length} chain(s)!`);
  log("info", `   Chains: ${engines.map((e) => e.chain.name).join(", ")}`);
  log("info", `   API: http://localhost:${ENV.PORT}/health`);
  log("info", `   WS:  ws://localhost:${ENV.PORT}/ws`);
  log("info", `   ⚠️  DRY RUN MODE — toggle from dashboard or POST /engine/dry-run {"enabled":false}`);

  // ─── 8. Shutdown ───

  const shutdown = async (signal: string) => {
    log("info", `${signal} received, shutting down...`);
    for (const eng of engines) eng.listener.stop();
    await api.stop();
    log("info", "Shutdown complete");
    process.exit(0);
  };

  process.on("SIGINT", () => shutdown("SIGINT"));
  process.on("SIGTERM", () => shutdown("SIGTERM"));
}

main().catch((err) => {
  console.error("Fatal:", err);
  process.exit(1);
});
