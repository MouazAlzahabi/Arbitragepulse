import { config } from "dotenv";
import type { Address, Hex } from "viem";

config();

// ============================================================
// Environment
// ============================================================

export const ANVIL_URL = process.env.ANVIL_URL || "http://127.0.0.1:8545";
export const TEST_KEY = (process.env.TEST_PRIVATE_KEY ||
  "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80") as Hex;
export const FORK_CHAIN = process.env.FORK_CHAIN || "optimism";
export const CONTRACT_ARTIFACT_PATH =
  process.env.CONTRACT_ARTIFACT_PATH ||
  "../contract/out/ArbitrageExecutor.sol/ArbitrageExecutor.json";

// ============================================================
// Chain-specific addresses
// ============================================================

export interface ChainAddresses {
  chainId: number;
  weth: Address;
  usdc: Address;
  usdt: Address;
  /** Known address with large USDC balance (for impersonation) */
  usdcWhale: Address;
  /** Known address with large WETH balance */
  wethWhale: Address;
  /** Two V2-compatible routers to arb between */
  routerA: { name: string; address: Address };
  routerB: { name: string; address: Address };
}

export const CHAINS: Record<string, ChainAddresses> = {
  optimism: {
    chainId: 10,
    weth: "0x4200000000000000000000000000000000000006",
    usdc: "0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85",
    usdt: "0x94b008aA00579c1307B0EF2c499aD98a8ce58e58",
    // Optimism bridge / large holder — has USDC
    usdcWhale: "0xacD03D601e5bB1B275Bb94076fF46ED9D753435A",
    // Large WETH holder on Optimism
    wethWhale: "0x4200000000000000000000000000000000000006", // WETH contract itself (always has balance from deposits)
    routerA: {
      name: "Velodrome V2",
      address: "0xa062aE8A9c5e11aaA026fc2670B0D65cCc8B2858",
    },
    routerB: {
      name: "Uniswap V2",
      address: "0x4A7b5Da61326A6379179b40d00F57E5bbDC962c2",
    },
  },
  base: {
    chainId: 8453,
    weth: "0x4200000000000000000000000000000000000006",
    usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    usdt: "0xfde4C96c8593536E31F229EA8f37b2ADa2699bb2",
    usdcWhale: "0x4c80E24119CFB836cdF0a6b53dc23F04F7e652CA",
    wethWhale: "0x4200000000000000000000000000000000000006",
    routerA: {
      name: "Aerodrome",
      address: "0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43",
    },
    routerB: {
      name: "Uniswap V2",
      address: "0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24",
    },
  },
  gnosis: {
    chainId: 100,
    weth: "0x6A023CCd1ff6F2045C3309768eAd9E68F978f6e1",
    usdc: "0xDDAfbb505ad214D7b80b1f830fcCc89B60fb7A83",
    usdt: "0x4ECaBa5870353805a9F068101A40E0f32ed605C6",
    usdcWhale: "0xBA12222222228d8Ba445958a75a0704d566BF2C8", // Balancer vault
    wethWhale: "0x6A023CCd1ff6F2045C3309768eAd9E68F978f6e1",
    routerA: {
      name: "SushiSwap",
      address: "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506",
    },
    routerB: {
      name: "Honeyswap",
      address: "0x1C232F01118CB8B424793ae03F870aa7D0ac7f77",
    },
  },
};

export function getChain(): ChainAddresses {
  const chain = CHAINS[FORK_CHAIN];
  if (!chain) {
    throw new Error(`Unknown chain: ${FORK_CHAIN}. Use: optimism | base | gnosis`);
  }
  return chain;
}

// ============================================================
// Common ABIs (minimal, just what scripts need)
// ============================================================

export const ERC20_ABI = [
  { type: "function", name: "balanceOf", inputs: [{ name: "account", type: "address" }], outputs: [{ name: "", type: "uint256" }], stateMutability: "view" },
  { type: "function", name: "transfer", inputs: [{ name: "to", type: "address" }, { name: "amount", type: "uint256" }], outputs: [{ name: "", type: "bool" }], stateMutability: "nonpayable" },
  { type: "function", name: "approve", inputs: [{ name: "spender", type: "address" }, { name: "amount", type: "uint256" }], outputs: [{ name: "", type: "bool" }], stateMutability: "nonpayable" },
  { type: "function", name: "decimals", inputs: [], outputs: [{ name: "", type: "uint8" }], stateMutability: "view" },
  { type: "function", name: "symbol", inputs: [], outputs: [{ name: "", type: "string" }], stateMutability: "view" },
] as const;

export const ROUTER_ABI = [
  {
    type: "function", name: "swapExactTokensForTokens",
    inputs: [
      { name: "amountIn", type: "uint256" },
      { name: "amountOutMin", type: "uint256" },
      { name: "path", type: "address[]" },
      { name: "to", type: "address" },
      { name: "deadline", type: "uint256" },
    ],
    outputs: [{ name: "amounts", type: "uint256[]" }],
    stateMutability: "nonpayable",
  },
  {
    type: "function", name: "getAmountsOut",
    inputs: [
      { name: "amountIn", type: "uint256" },
      { name: "path", type: "address[]" },
    ],
    outputs: [{ name: "amounts", type: "uint256[]" }],
    stateMutability: "view",
  },
] as const;

export const EXECUTOR_ABI = [
  {
    type: "function", name: "executeArbitrage",
    inputs: [
      { name: "tokenIn", type: "address" },
      { name: "tokenOut", type: "address" },
      { name: "amountIn", type: "uint256" },
      { name: "routerA", type: "address" },
      { name: "routerB", type: "address" },
      { name: "minProfit", type: "uint256" },
      { name: "deadline", type: "uint256" },
    ],
    outputs: [],
    stateMutability: "nonpayable",
  },
  {
    type: "function", name: "getStats",
    inputs: [{ name: "token", type: "address" }],
    outputs: [
      { name: "_totalProfit", type: "uint256" },
      { name: "_totalTrades", type: "uint256" },
      { name: "_contractBalance", type: "uint256" },
    ],
    stateMutability: "view",
  },
  {
    type: "function", name: "withdrawToken",
    inputs: [{ name: "token", type: "address" }, { name: "amount", type: "uint256" }],
    outputs: [],
    stateMutability: "nonpayable",
  },
  {
    type: "function", name: "owner",
    inputs: [],
    outputs: [{ name: "", type: "address" }],
    stateMutability: "view",
  },
  {
    type: "function", name: "paused",
    inputs: [],
    outputs: [{ name: "", type: "bool" }],
    stateMutability: "view",
  },
  {
    type: "function", name: "totalTrades",
    inputs: [],
    outputs: [{ name: "", type: "uint256" }],
    stateMutability: "view",
  },
] as const;
