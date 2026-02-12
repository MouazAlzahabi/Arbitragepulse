import {
  type PublicClient,
  type Address,
  formatUnits,
  parseUnits,
} from "viem";
import { UniswapV2RouterABI } from "../abi";
import type { RouterConfig, PairConfig, ChainConfig } from "../config/chains";
import type { SwapEvent } from "../listeners";
import { log } from "../utils/logger";

// ── Types ──

export interface ArbOpportunity {
  id: string;
  chainId: number;
  pair: PairConfig;
  tokenIn: Address;
  tokenOut: Address;
  amountIn: bigint;
  expectedOut: bigint;
  profit: bigint;
  profitPct: number;
  profitUsd: number;
  routerA: RouterConfig; // Buy here (cheaper — gives most tokenOut)
  routerB: RouterConfig; // Sell here (more expensive — gives most tokenIn back)
  gasEstimate: bigint;
  gasCostWei: bigint;
  gasCostUsd: number;
  netProfitable: boolean;
  timestamp: number;
}

// ── Strategy ──

export class ArbitrageStrategy {
  private nativePriceUsd: number = 3000;

  constructor(
    private client: PublicClient,
    private chain: ChainConfig,
    private v2Routers: RouterConfig[],
    private v3Routers: RouterConfig[],
    private pairs: PairConfig[]
  ) {}

  async init() {
    log("info", `[${this.chain.name}] Strategy init: ${this.v2Routers.length} V2 routers, ${this.v3Routers.length} V3 routers, ${this.pairs.length} pairs`);

    // Verify each router can quote each pair
    for (const pair of this.pairs) {
      const available: string[] = [];
      for (const router of this.v2Routers) {
        try {
          const amounts = await this.client.readContract({
            address: router.address,
            abi: UniswapV2RouterABI,
            functionName: "getAmountsOut",
            args: [
              parseUnits(pair.tradeAmount, pair.tokenInDecimals),
              [pair.tokenIn, pair.tokenOut],
            ],
          });
          if (amounts[1] > 0n) {
            available.push(router.name);
          }
        } catch {
          // Pair not available on this router
        }
      }

      if (available.length >= 2) {
        log("info", `[${this.chain.name}] ${pair.tokenInSymbol}/${pair.tokenOutSymbol}: available on ${available.join(", ")}`);
      } else if (available.length === 1) {
        log("warn", `[${this.chain.name}] ${pair.tokenInSymbol}/${pair.tokenOutSymbol}: only on ${available[0]} — need 2+ routers for arb`);
      } else {
        log("warn", `[${this.chain.name}] ${pair.tokenInSymbol}/${pair.tokenOutSymbol}: not found on any router`);
      }
    }

    // Get native token price
    await this.updateNativePrice();
    setInterval(() => this.updateNativePrice(), 60_000);
  }

  /**
   * Main evaluation: called on every swap event or periodic tick.
   * Checks all pairs on all router combinations.
   */
  async evaluate(swap: SwapEvent): Promise<ArbOpportunity | null> {
    const start = performance.now();
    let bestOpp: ArbOpportunity | null = null;

    for (const pair of this.pairs) {
      const opp = await this.evaluatePair(pair);
      if (opp && opp.netProfitable) {
        if (!bestOpp || opp.profitUsd > bestOpp.profitUsd) {
          bestOpp = opp;
        }
      }
    }

    const ms = (performance.now() - start).toFixed(1);

    if (bestOpp) {
      log("opportunity", `[${this.chain.name}] 🎯 ARB in ${ms}ms: ${bestOpp.pair.tokenInSymbol}/${bestOpp.pair.tokenOutSymbol}`, {
        buyOn: bestOpp.routerA.name,
        sellOn: bestOpp.routerB.name,
        profitUsd: `$${bestOpp.profitUsd.toFixed(4)}`,
        profitPct: `${bestOpp.profitPct.toFixed(4)}%`,
        gasCostUsd: `$${bestOpp.gasCostUsd.toFixed(4)}`,
      });
    } else {
      log("debug", `[${this.chain.name}] No opportunity (${ms}ms, ${this.pairs.length} pairs)`);
    }

    return bestOpp;
  }

  /**
   * Evaluate a single pair across all V2 router combos.
   *
   * How V3 monitoring helps here:
   * A large V3 swap moves the V3 price. V2 pools are slower to react.
   * So when we detect a V3 swap event, we immediately check V2 prices —
   * if V2 pools haven't arbitraged yet, we can capture the difference.
   * But we only EXECUTE on V2 routers (matching ARB-001 contract).
   */
  private async evaluatePair(pair: PairConfig): Promise<ArbOpportunity | null> {
    const amountIn = parseUnits(pair.tradeAmount, pair.tokenInDecimals);
    if (amountIn === 0n) return null;

    // Step 1: Get forward quotes from all V2 routers (tokenIn → tokenOut)
    const forwardQuotes = await this.multicallQuotes(
      pair.tokenIn,
      pair.tokenOut,
      amountIn,
      this.v2Routers
    );

    if (forwardQuotes.length < 2) return null;

    // Step 2: For each router that gives us tokenOut, check reverse on all others
    let bestProfit = 0n;
    let bestOpp: ArbOpportunity | null = null;

    for (const fwd of forwardQuotes) {
      if (fwd.amountOut === 0n) continue;

      // Get reverse quotes for the amount of tokenOut we'd receive
      const reverseQuotes = await this.multicallQuotes(
        pair.tokenOut,
        pair.tokenIn,
        fwd.amountOut,
        this.v2Routers.filter((r) => r.id !== fwd.router.id) // Exclude buy router
      );

      for (const rev of reverseQuotes) {
        if (rev.amountOut === 0n) continue;

        const profit = rev.amountOut - amountIn;
        if (profit <= 0n) continue;
        if (profit <= bestProfit) continue;

        // Calculate USD values
        const profitFloat = parseFloat(formatUnits(profit, pair.tokenInDecimals));
        const amountInFloat = parseFloat(formatUnits(amountIn, pair.tokenInDecimals));
        const profitPct = (profitFloat / amountInFloat) * 100;

        let profitUsd: number;
        if (["USDC", "USDT", "DAI", "BUSD"].includes(pair.tokenInSymbol)) {
          profitUsd = profitFloat;
        } else if (["WETH", "ETH"].includes(pair.tokenInSymbol)) {
          profitUsd = profitFloat * this.nativePriceUsd;
        } else {
          profitUsd = profitFloat; // Best guess
        }

        // Gas estimation
        const gasEstimate = 350_000n;
        const gasPrice = await this.client.getGasPrice();
        const gasCostWei = gasEstimate * gasPrice;
        const gasCostUsd = parseFloat(formatUnits(gasCostWei, 18)) * this.nativePriceUsd;

        const netProfitable = profitUsd > gasCostUsd + this.chain.minProfitUsd;

        bestProfit = profit;
        bestOpp = {
          id: `${pair.id}-${fwd.router.id}-${rev.router.id}-${Date.now()}`,
          chainId: this.chain.id,
          pair,
          tokenIn: pair.tokenIn,
          tokenOut: pair.tokenOut,
          amountIn,
          expectedOut: rev.amountOut,
          profit,
          profitPct,
          profitUsd,
          routerA: fwd.router,  // Buy leg
          routerB: rev.router,  // Sell leg
          gasEstimate,
          gasCostWei,
          gasCostUsd,
          netProfitable,
          timestamp: Date.now(),
        };
      }
    }

    return bestOpp;
  }

  /**
   * Fetch quotes from multiple routers via multicall (single RPC call).
   */
  private async multicallQuotes(
    tokenIn: Address,
    tokenOut: Address,
    amountIn: bigint,
    routers: RouterConfig[]
  ): Promise<Array<{ router: RouterConfig; amountOut: bigint }>> {
    const results: Array<{ router: RouterConfig; amountOut: bigint }> = [];

    if (routers.length === 0) return results;

    const calls = routers.map((router) => ({
      address: router.address as Address,
      abi: UniswapV2RouterABI,
      functionName: "getAmountsOut" as const,
      args: [amountIn, [tokenIn, tokenOut]] as const,
    }));

    try {
      const multicallResults = await this.client.multicall({
        contracts: calls,
        allowFailure: true,
      });

      for (let i = 0; i < multicallResults.length; i++) {
        const res = multicallResults[i];
        if (res.status === "success" && res.result) {
          const amounts = res.result as bigint[];
          results.push({
            router: routers[i],
            amountOut: amounts[amounts.length - 1],
          });
        }
      }
    } catch (err) {
      log("error", `[${this.chain.name}] Multicall error`, { error: String(err) });
    }

    return results;
  }

  /** Update native token price from DEX quote */
  private async updateNativePrice() {
    // Find a WETH/USDC or WETH/USDT pair
    const stablePair = this.pairs.find(
      (p) =>
        ["WETH", "ETH"].includes(p.tokenInSymbol) &&
        ["USDC", "USDT", "DAI"].includes(p.tokenOutSymbol)
    );

    if (!stablePair) return;

    for (const router of this.v2Routers) {
      try {
        const amounts = await this.client.readContract({
          address: router.address,
          abi: UniswapV2RouterABI,
          functionName: "getAmountsOut",
          args: [parseUnits("1", 18), [stablePair.tokenIn, stablePair.tokenOut]],
        });
        const price = parseFloat(formatUnits(amounts[1], stablePair.tokenOutDecimals));
        if (price > 0) {
          this.nativePriceUsd = price;
          log("debug", `[${this.chain.name}] ${this.chain.nativeCurrency} price: $${price.toFixed(2)}`);
          return;
        }
      } catch {
        continue;
      }
    }
  }
}
