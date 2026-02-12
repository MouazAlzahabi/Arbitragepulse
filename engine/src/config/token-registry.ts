import { readFileSync, writeFileSync, existsSync } from "fs";
import { parse, stringify } from "yaml";
import type { Address } from "viem";
import type { PairConfig } from "./chains";
import { log } from "../utils/logger";

// ============================================================
// Types
// ============================================================

export type TokenCategory = "stable" | "blue_chip" | "defi" | "meme" | "other";

export interface TokenEntry {
  symbol: string;
  address: Address;
  decimals: number;
  trusted: boolean;
  category: TokenCategory;
  chainId: number;
  chainName: string;
  /** Optional: override trade amount for this specific token */
  maxTrade?: string;
}

export interface TradeSizeDefaults {
  stable: string;
  blue_chip: string;
  defi: string;
  meme: string;
  other: string;
}

interface TokenRegistryData {
  tokens: TokenEntry[];
  tradeSizes: TradeSizeDefaults;
  minSwapSizes: TradeSizeDefaults;
}

// ── Chain name → chain ID mapping ──

const CHAIN_NAME_TO_ID: Record<string, number> = {
  optimism: 10,
  gnosis: 100,
  base: 8453,
  arbitrum: 42161,
  polygon: 137,
  avalanche: 43114,
  ethereum: 1,
  bsc: 56,
  zksync: 324,
  scroll: 534352,
  linea: 59144,
  mantle: 5000,
  blast: 81457,
  mode: 34443,
  celo: 42220,
};

// ============================================================
// Token Registry
// ============================================================

export class TokenRegistry {
  private tokens: TokenEntry[] = [];
  private tradeSizes: TradeSizeDefaults = {
    stable: "150",
    blue_chip: "0.05",
    defi: "0.03",
    meme: "0.01",
    other: "0.02",
  };
  private minSwapSizes: TradeSizeDefaults = {
    stable: "10",
    blue_chip: "0.005",
    defi: "0.005",
    meme: "0.002",
    other: "0.005",
  };
  private filePath: string;

  constructor(filePath: string = "tokens.yaml") {
    this.filePath = filePath;
  }

  // ── Load ──

  load(): TokenRegistryData {
    if (!existsSync(this.filePath)) {
      log("warn", `Token file not found: ${this.filePath}. Using empty registry.`);
      return { tokens: this.tokens, tradeSizes: this.tradeSizes, minSwapSizes: this.minSwapSizes };
    }

    const raw = readFileSync(this.filePath, "utf-8");
    const data = parse(raw) as any;

    // Load defaults
    if (data.defaults?.trade_sizes) {
      this.tradeSizes = { ...this.tradeSizes, ...data.defaults.trade_sizes };
    }
    if (data.defaults?.min_swap_sizes) {
      this.minSwapSizes = { ...this.minSwapSizes, ...data.defaults.min_swap_sizes };
    }

    // Load tokens from all chain sections
    this.tokens = [];

    for (const [chainName, chainTokens] of Object.entries(data)) {
      if (chainName === "defaults") continue;
      if (!Array.isArray(chainTokens)) continue;

      const chainId = CHAIN_NAME_TO_ID[chainName.toLowerCase()];
      if (!chainId) {
        log("warn", `Unknown chain name in tokens.yaml: "${chainName}". Add it to CHAIN_NAME_TO_ID.`);
        continue;
      }

      for (const t of chainTokens as any[]) {
        this.tokens.push({
          symbol: t.symbol,
          address: t.address as Address,
          decimals: t.decimals,
          trusted: t.trusted !== false,
          category: (t.category || "other") as TokenCategory,
          chainId,
          chainName,
          maxTrade: t.max_trade,
        });
      }
    }

    log("info", `Token registry loaded: ${this.tokens.length} tokens across ${this.getChainIds().length} chain(s)`);

    // Per-chain summary
    for (const chainId of this.getChainIds()) {
      const chainTokens = this.getChainTokens(chainId);
      const trusted = chainTokens.filter((t) => t.trusted);
      log("info", `  Chain ${chainId}: ${trusted.length} trusted / ${chainTokens.length} total tokens`);
    }

    return { tokens: this.tokens, tradeSizes: this.tradeSizes, minSwapSizes: this.minSwapSizes };
  }

  // ── Query methods ──

  getAll(): TokenEntry[] {
    return this.tokens;
  }

  getTrusted(chainId?: number): TokenEntry[] {
    const list = chainId
      ? this.tokens.filter((t) => t.chainId === chainId)
      : this.tokens;
    return list.filter((t) => t.trusted);
  }

  getChainTokens(chainId: number): TokenEntry[] {
    return this.tokens.filter((t) => t.chainId === chainId);
  }

  getChainIds(): number[] {
    return [...new Set(this.tokens.map((t) => t.chainId))];
  }

  getToken(chainId: number, address: string): TokenEntry | undefined {
    return this.tokens.find(
      (t) => t.chainId === chainId && t.address.toLowerCase() === address.toLowerCase()
    );
  }

  getTokenBySymbol(chainId: number, symbol: string): TokenEntry | undefined {
    return this.tokens.find(
      (t) => t.chainId === chainId && t.symbol.toUpperCase() === symbol.toUpperCase()
    );
  }

  isAllowed(chainId: number, address: string): boolean {
    const token = this.getToken(chainId, address);
    return token?.trusted === true;
  }

  // ── Mutation (for API/dashboard) ──

  addToken(token: Omit<TokenEntry, "chainName">): boolean {
    // Check for duplicate
    const existing = this.getToken(token.chainId, token.address);
    if (existing) {
      log("warn", `Token ${token.symbol} already exists on chain ${token.chainId}`);
      return false;
    }

    const chainName = Object.entries(CHAIN_NAME_TO_ID).find(
      ([_, id]) => id === token.chainId
    )?.[0] || "unknown";

    this.tokens.push({ ...token, chainName });
    log("info", `Token added: ${token.symbol} on chain ${token.chainId}`);
    return true;
  }

  removeToken(chainId: number, address: string): boolean {
    const idx = this.tokens.findIndex(
      (t) => t.chainId === chainId && t.address.toLowerCase() === address.toLowerCase()
    );
    if (idx === -1) return false;

    const removed = this.tokens.splice(idx, 1)[0];
    log("info", `Token removed: ${removed.symbol} from chain ${chainId}`);
    return true;
  }

  setTrusted(chainId: number, address: string, trusted: boolean): boolean {
    const token = this.getToken(chainId, address);
    if (!token) return false;

    token.trusted = trusted;
    log("info", `Token ${token.symbol} on chain ${chainId}: trusted = ${trusted}`);
    return true;
  }

  /** Save current state back to YAML file */
  save(): void {
    const output: Record<string, any> = {
      defaults: {
        trade_sizes: this.tradeSizes,
        min_swap_sizes: this.minSwapSizes,
      },
    };

    // Group by chain
    for (const token of this.tokens) {
      if (!output[token.chainName]) {
        output[token.chainName] = [];
      }
      output[token.chainName].push({
        symbol: token.symbol,
        address: token.address,
        decimals: token.decimals,
        trusted: token.trusted,
        category: token.category,
        ...(token.maxTrade ? { max_trade: token.maxTrade } : {}),
      });
    }

    writeFileSync(this.filePath, stringify(output, { lineWidth: 120 }));
    log("info", `Token registry saved to ${this.filePath}`);
  }

  // ── Pair Generation ──

  /**
   * Auto-generate all valid pair combinations from trusted tokens.
   *
   * Rules:
   *   - Only trusted tokens are included
   *   - Both directions: A→B and B→A
   *   - Skip same-category-to-same for stables (USDC→USDT arb is rare and tiny)
   *   - Trade size determined by tokenIn category
   *   - Returns PairConfig[] compatible with the strategy engine
   */
  generatePairs(chainId: number): PairConfig[] {
    const trusted = this.getTrusted(chainId);
    const pairs: PairConfig[] = [];

    if (trusted.length < 2) {
      log("warn", `Chain ${chainId}: need at least 2 trusted tokens to generate pairs. Found: ${trusted.length}`);
      return pairs;
    }

    for (let i = 0; i < trusted.length; i++) {
      for (let j = 0; j < trusted.length; j++) {
        if (i === j) continue;

        const tokenIn = trusted[i];
        const tokenOut = trusted[j];

        // Skip stable→stable (USDC→USDT, DAI→USDC etc.)
        // These pairs rarely have meaningful arb on V2
        if (tokenIn.category === "stable" && tokenOut.category === "stable") {
          continue;
        }

        // Determine trade size from tokenIn category
        const tradeAmount =
          tokenIn.maxTrade ||
          this.tradeSizes[tokenIn.category] ||
          this.tradeSizes.other;

        const minSwapSize =
          this.minSwapSizes[tokenIn.category] ||
          this.minSwapSizes.other;

        const id = `${chainId}-${tokenIn.symbol}-${tokenOut.symbol}`.toLowerCase();

        pairs.push({
          id,
          chainId,
          tokenIn: tokenIn.address,
          tokenOut: tokenOut.address,
          tokenInSymbol: tokenIn.symbol,
          tokenOutSymbol: tokenOut.symbol,
          tokenInDecimals: tokenIn.decimals,
          tokenOutDecimals: tokenOut.decimals,
          watchPools: [],
          tradeAmount,
          minSwapSize,
        });
      }
    }

    log("info", `Chain ${chainId}: generated ${pairs.length} pairs from ${trusted.length} trusted tokens`);
    return pairs;
  }

  /**
   * Generate pairs for ALL chains.
   */
  generateAllPairs(): PairConfig[] {
    const allPairs: PairConfig[] = [];
    for (const chainId of this.getChainIds()) {
      allPairs.push(...this.generatePairs(chainId));
    }
    return allPairs;
  }
}
