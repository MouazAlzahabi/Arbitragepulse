import type { PublicClient, WalletClient } from "viem";
import { log } from "./logger";

/**
 * Nonce Manager — handles concurrent transaction submissions.
 *
 * Problem: If two arbs trigger within 100ms, both fetch the same nonce
 * from the RPC, and one will fail with "nonce too low".
 *
 * Solution: Track nonce locally, increment optimistically, and only
 * re-sync from chain when we detect an error.
 */
export class NonceManager {
  private currentNonce: number = -1;
  private lock: boolean = false;
  private queue: Array<() => void> = [];

  constructor(
    private publicClient: PublicClient,
    private walletAddress: `0x${string}`
  ) {}

  /** Initialize nonce from chain */
  async init() {
    this.currentNonce = await this.publicClient.getTransactionCount({
      address: this.walletAddress,
      blockTag: "pending",
    });
    log("debug", `Nonce initialized: ${this.currentNonce}`);
  }

  /** Get next nonce (auto-increments) */
  async acquire(): Promise<number> {
    // Wait for lock
    if (this.lock) {
      await new Promise<void>((resolve) => this.queue.push(resolve));
    }

    this.lock = true;

    if (this.currentNonce === -1) {
      await this.init();
    }

    const nonce = this.currentNonce;
    this.currentNonce++;

    return nonce;
  }

  /** Release lock for next consumer */
  release() {
    this.lock = false;
    const next = this.queue.shift();
    if (next) next();
  }

  /** Re-sync with chain after error */
  async resync() {
    this.currentNonce = await this.publicClient.getTransactionCount({
      address: this.walletAddress,
      blockTag: "pending",
    });
    log("warn", `Nonce resynced to ${this.currentNonce}`);
  }

  /** Handle nonce-related errors */
  async handleError(error: unknown) {
    const msg = error instanceof Error ? error.message : String(error);
    if (
      msg.includes("nonce too low") ||
      msg.includes("nonce has already been used") ||
      msg.includes("replacement transaction underpriced")
    ) {
      await this.resync();
      return true; // Was a nonce error
    }
    return false;
  }
}
