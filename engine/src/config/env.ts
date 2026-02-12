import { config } from "dotenv";
import type { Address, Hex } from "viem";

config();

function requireEnv(key: string): string {
  const val = process.env[key];
  if (!val || val.startsWith("YOUR_")) {
    throw new Error(`Missing required env var: ${key}. Check your .env file.`);
  }
  return val;
}

function optionalEnv(key: string, fallback: string): string {
  const val = process.env[key];
  if (!val || val.startsWith("YOUR_")) return fallback;
  return val;
}

export const ENV = {
  PRIVATE_KEY: requireEnv("PRIVATE_KEY") as Hex,
  CONTRACT_ADDRESS: requireEnv("CONTRACT_ADDRESS") as Address,
  CONFIG_PATH: optionalEnv("CONFIG_PATH", "config.yaml"),
  PORT: parseInt(optionalEnv("PORT", "3000")),
  LOG_LEVEL: optionalEnv("LOG_LEVEL", "info") as "debug" | "info" | "warn" | "error",
  /** API key for dashboard auth. If empty, auth is disabled (local dev). */
  API_KEY: optionalEnv("API_KEY", ""),
} as const;
