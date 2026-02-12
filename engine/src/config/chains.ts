import { readFileSync } from "fs";
import { parse } from "yaml";
import type { Address } from "viem";

// ============================================================
// Types — these are loaded from config.yaml, not hardcoded
// ============================================================

export interface ChainConfig {
  id: number;
  name: string;
  wsRpc: string;
  httpRpc: string;
  nativeCurrency: string;
  /** Native wrapped token (WETH on Optimism, WXDAI on Gnosis, etc.) */
  wrappedNative: Address;
  /** Approximate block time in ms — used for fallback polling interval */
  blockTimeMs: number;
  /** Deployed ArbitrageExecutor address on this chain */
  contractAddress: Address;
  /** Min native balance to keep trading (in ether units) */
  minNativeBalance: number;
  /** Min profit in USD to execute */
  minProfitUsd: number;
  /** Deadline offset in seconds for txns */
  deadlineSeconds: number;
  /** Whether this chain is currently active */
  enabled: boolean;
}

export interface RouterConfig {
  id: string;
  name: string;
  address: Address;
  chainId: number;
  /** v2 or v3 — determines how we read prices and which events to watch */
  type: "v2" | "v3";
  /** Fee in basis points (v2: typically 30 = 0.3%, v3: 100/500/3000/10000) */
  feeBps: number;
  /** Factory address — used for pair discovery */
  factory?: Address;
}

export interface PairConfig {
  id: string;
  chainId: number;
  tokenIn: Address;
  tokenOut: Address;
  tokenInSymbol: string;
  tokenOutSymbol: string;
  tokenInDecimals: number;
  tokenOutDecimals: number;
  /** Pool addresses to watch for Swap events (optional — falls back to polling) */
  watchPools: Address[];
  /** Min swap size to trigger analysis (human-readable, e.g. "0.01") */
  minSwapSize: string;
  /** Trade amount per arb (human-readable, e.g. "0.05") */
  tradeAmount: string;
}

export interface AppConfig {
  chains: ChainConfig[];
  routers: RouterConfig[];
  pairs: PairConfig[];
}

// ============================================================
// Loader
// ============================================================

export function loadConfig(path: string, tokenRegistry?: import("./token-registry").TokenRegistry): AppConfig {
  const raw = readFileSync(path, "utf-8");
  const data = parse(raw) as any;

  // Validate chains
  const chains: ChainConfig[] = (data.chains || []).map((c: any) => ({
    id: c.id,
    name: c.name,
    wsRpc: c.ws_rpc,
    httpRpc: c.http_rpc,
    nativeCurrency: c.native_currency || "ETH",
    wrappedNative: c.wrapped_native as Address,
    blockTimeMs: c.block_time_ms || 2000,
    contractAddress: c.contract_address as Address,
    minNativeBalance: c.min_native_balance || 0.005,
    minProfitUsd: c.min_profit_usd || 0.50,
    deadlineSeconds: c.deadline_seconds || 120,
    enabled: c.enabled !== false,
  }));

  // Validate routers
  const routers: RouterConfig[] = (data.routers || []).map((r: any) => ({
    id: r.id,
    name: r.name,
    address: r.address as Address,
    chainId: r.chain_id,
    type: r.type || "v2",
    feeBps: r.fee_bps || 30,
    factory: r.factory as Address | undefined,
  }));

  // Pairs: auto-generate from token registry if available, otherwise fall back to manual
  let pairs: PairConfig[];

  if (tokenRegistry) {
    // AUTO MODE: generate pairs from trusted tokens
    pairs = tokenRegistry.generateAllPairs();
  } else if (data.pairs && data.pairs.length > 0) {
    // MANUAL MODE: use pairs from config.yaml (backward compatible)
    pairs = (data.pairs || []).map((p: any) => ({
      id: p.id,
      chainId: p.chain_id,
      tokenIn: p.token_in as Address,
      tokenOut: p.token_out as Address,
      tokenInSymbol: p.token_in_symbol,
      tokenOutSymbol: p.token_out_symbol,
      tokenInDecimals: p.token_in_decimals,
      tokenOutDecimals: p.token_out_decimals,
      watchPools: (p.watch_pools || []) as Address[],
      minSwapSize: p.min_swap_size || "0.001",
      tradeAmount: p.trade_amount || "0.01",
    }));
  } else {
    pairs = [];
  }

  // Validation
  if (chains.filter((c) => c.enabled).length === 0) {
    throw new Error("No enabled chains in config. Enable at least one chain.");
  }

  for (const r of routers) {
    if (!chains.find((c) => c.id === r.chainId)) {
      throw new Error(`Router '${r.id}' references unknown chain_id: ${r.chainId}`);
    }
  }

  return { chains, routers, pairs };
}

// ── Helpers ──

export function getChainRouters(config: AppConfig, chainId: number): RouterConfig[] {
  return config.routers.filter((r) => r.chainId === chainId);
}

export function getChainPairs(config: AppConfig, chainId: number): PairConfig[] {
  return config.pairs.filter((p) => p.chainId === chainId);
}

export function getV2Routers(config: AppConfig, chainId: number): RouterConfig[] {
  return config.routers.filter((r) => r.chainId === chainId && r.type === "v2");
}

export function getV3Routers(config: AppConfig, chainId: number): RouterConfig[] {
  return config.routers.filter((r) => r.chainId === chainId && r.type === "v3");
}
