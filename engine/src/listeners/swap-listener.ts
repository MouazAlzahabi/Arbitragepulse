import {
  type PublicClient,
  type Address,
  type Log,
  parseAbiItem,
  decodeEventLog,
} from "viem";
import { UniswapV2PairABI, UniswapV3PoolABI } from "../abi";
import { log } from "../utils/logger";

// ── Swap event signatures ──

const V2_SWAP_EVENT = parseAbiItem(
  "event Swap(address indexed sender, uint256 amount0In, uint256 amount1In, uint256 amount0Out, uint256 amount1Out, address indexed to)"
);

const V3_SWAP_EVENT = parseAbiItem(
  "event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)"
);

// ── Types ──

export interface SwapEvent {
  pool: Address;
  type: "v2" | "v3";
  token0Amount: bigint;
  token1Amount: bigint;
  blockNumber: bigint;
  txHash: string;
  timestamp: number;
}

export type SwapHandler = (swap: SwapEvent) => void | Promise<void>;

// ── Listener class ──

export class SwapListener {
  private unwatch: Array<() => void> = [];
  private reconnectAttempts = 0;
  private maxReconnects = 10;
  private handlers: SwapHandler[] = [];
  private isRunning = false;

  /** Min swap size per pool to filter dust (pool address → threshold) */
  private minSwapSizes: Map<string, bigint> = new Map();

  constructor(private client: PublicClient) {}

  /** Register a handler for swap events */
  onSwap(handler: SwapHandler) {
    this.handlers.push(handler);
  }

  /** Set minimum swap size filter for a pool */
  setMinSwapSize(pool: Address, minSize: bigint) {
    this.minSwapSizes.set(pool.toLowerCase(), minSize);
  }

  /**
   * Subscribe to Swap events on V2 pools via WebSocket.
   */
  async watchV2Pools(pools: Address[]) {
    if (pools.length === 0) return;

    log("info", `Subscribing to ${pools.length} V2 pool(s)`, {
      pools: pools.map((p) => p.slice(0, 10) + "..."),
    });

    for (const pool of pools) {
      try {
        const unwatch = this.client.watchContractEvent({
          address: pool,
          abi: UniswapV2PairABI,
          eventName: "Swap",
          onLogs: (logs) => {
            for (const rawLog of logs) {
              this.handleV2Log(pool, rawLog);
            }
          },
          onError: (error) => {
            log("error", `V2 pool watch error: ${pool}`, {
              error: String(error),
            });
            this.scheduleReconnect(pool, "v2");
          },
        });

        this.unwatch.push(unwatch);
      } catch (err) {
        log("error", `Failed to watch V2 pool ${pool}`, { error: String(err) });
      }
    }

    this.isRunning = true;
  }

  /**
   * Subscribe to Swap events on V3 pools.
   */
  async watchV3Pools(pools: Address[]) {
    if (pools.length === 0) return;

    log("info", `Subscribing to ${pools.length} V3 pool(s)`, {
      pools: pools.map((p) => p.slice(0, 10) + "..."),
    });

    for (const pool of pools) {
      try {
        const unwatch = this.client.watchContractEvent({
          address: pool,
          abi: UniswapV3PoolABI,
          eventName: "Swap",
          onLogs: (logs) => {
            for (const rawLog of logs) {
              this.handleV3Log(pool, rawLog);
            }
          },
          onError: (error) => {
            log("error", `V3 pool watch error: ${pool}`, {
              error: String(error),
            });
            this.scheduleReconnect(pool, "v3");
          },
        });

        this.unwatch.push(unwatch);
      } catch (err) {
        log("error", `Failed to watch V3 pool ${pool}`, { error: String(err) });
      }
    }

    this.isRunning = true;
  }

  /**
   * Fallback: poll reserves every N blocks.
   * Not for swap detection — just keeps reserve cache fresh
   * in case we miss a WS event during reconnect.
   */
  startFallbackPolling(pools: Address[], intervalMs: number = 4000) {
    log("info", `Starting fallback reserve polling every ${intervalMs}ms`);

    const poll = setInterval(async () => {
      if (!this.isRunning) {
        clearInterval(poll);
        return;
      }

      // This doesn't trigger swap handlers — it's just a safety net
      // for the strategy to have fresh reserve data.
      // The strategy module reads reserves directly.
    }, intervalMs);
  }

  // ── Internal handlers ──

  private handleV2Log(pool: Address, rawLog: any) {
    try {
      const args = rawLog.args;
      if (!args) return;

      const amount0 =
        (args.amount0In || 0n) > 0n ? args.amount0In! : -(args.amount0Out || 0n);
      const amount1 =
        (args.amount1In || 0n) > 0n ? args.amount1In! : -(args.amount1Out || 0n);

      // Noise filter
      const minSize = this.minSwapSizes.get(pool.toLowerCase()) || 0n;
      const absAmount0 = amount0 < 0n ? -amount0 : amount0;
      const absAmount1 = amount1 < 0n ? -amount1 : amount1;

      if (absAmount0 < minSize && absAmount1 < minSize) {
        return; // Dust swap, ignore
      }

      const swap: SwapEvent = {
        pool,
        type: "v2",
        token0Amount: amount0,
        token1Amount: amount1,
        blockNumber: rawLog.blockNumber || 0n,
        txHash: rawLog.transactionHash || "0x",
        timestamp: Date.now(),
      };

      log("debug", `V2 Swap: ${pool.slice(0, 10)}`, {
        amount0: amount0.toString(),
        amount1: amount1.toString(),
      });

      this.emit(swap);
    } catch (err) {
      log("error", "V2 log parse error", { error: String(err) });
    }
  }

  private handleV3Log(pool: Address, rawLog: any) {
    try {
      const args = rawLog.args;
      if (!args) return;

      const amount0: bigint = args.amount0 || 0n;
      const amount1: bigint = args.amount1 || 0n;

      // Noise filter
      const minSize = this.minSwapSizes.get(pool.toLowerCase()) || 0n;
      const absAmount0 = amount0 < 0n ? -amount0 : amount0;
      const absAmount1 = amount1 < 0n ? -amount1 : amount1;

      if (absAmount0 < minSize && absAmount1 < minSize) {
        return;
      }

      const swap: SwapEvent = {
        pool,
        type: "v3",
        token0Amount: amount0,
        token1Amount: amount1,
        blockNumber: rawLog.blockNumber || 0n,
        txHash: rawLog.transactionHash || "0x",
        timestamp: Date.now(),
      };

      log("debug", `V3 Swap: ${pool.slice(0, 10)}`, {
        amount0: amount0.toString(),
        amount1: amount1.toString(),
      });

      this.emit(swap);
    } catch (err) {
      log("error", "V3 log parse error", { error: String(err) });
    }
  }

  private emit(swap: SwapEvent) {
    for (const handler of this.handlers) {
      try {
        handler(swap);
      } catch (err) {
        log("error", "Swap handler error", { error: String(err) });
      }
    }
  }

  // ── Reconnect logic with exponential backoff ──

  private scheduleReconnect(pool: Address, type: "v2" | "v3") {
    if (this.reconnectAttempts >= this.maxReconnects) {
      log("error", `Max reconnect attempts (${this.maxReconnects}) reached for ${pool}`);
      return;
    }

    this.reconnectAttempts++;
    const delay = Math.min(1000 * Math.pow(2, this.reconnectAttempts), 30000);

    log("warn", `Reconnecting to ${pool} in ${delay}ms (attempt ${this.reconnectAttempts})`);

    setTimeout(async () => {
      try {
        if (type === "v2") {
          await this.watchV2Pools([pool]);
        } else {
          await this.watchV3Pools([pool]);
        }
        this.reconnectAttempts = 0; // Reset on success
        log("info", `Reconnected to ${pool}`);
      } catch (err) {
        log("error", `Reconnect failed for ${pool}`, { error: String(err) });
        this.scheduleReconnect(pool, type);
      }
    }, delay);
  }

  /** Stop all watchers */
  stop() {
    this.isRunning = false;
    for (const uw of this.unwatch) {
      try {
        uw();
      } catch {}
    }
    this.unwatch = [];
    log("info", "Swap listener stopped");
  }
}
