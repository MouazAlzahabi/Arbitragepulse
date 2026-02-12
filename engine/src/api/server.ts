import Fastify from "fastify";
import websocket from "@fastify/websocket";
import type { WebSocket } from "ws";
import { ENV } from "../config/env";
import type { TokenRegistry, TokenEntry, TokenCategory } from "../config/token-registry";
import { log, registerBroadcast, type LogEntry } from "../utils/logger";

// ── Types ──

interface HealthData {
  uptime: number;
  startedAt: number;
  wsClients: number;
  stats: Record<string, unknown>;
  ethBalance?: string;
  contractBalance?: string;
}

// ── API Server ──

export class ApiServer {
  private app = Fastify({ logger: false });
  private clients: Set<WebSocket> = new Set();
  private startedAt = Date.now();
  private healthProvider: (() => Promise<HealthData>) | null = null;
  private tokenRegistry: TokenRegistry | null = null;

  /** Engine control callbacks — registered by index.ts */
  private controlCallbacks: {
    pause?: (chainId?: number) => void;
    resume?: (chainId?: number) => void;
    setDryRun?: (dryRun: boolean) => void;
    getState?: () => { paused: boolean; dryRun: boolean; chains: Array<{ name: string; chainId: number; paused: boolean }> };
  } = {};

  /** Register token registry for API management */
  setTokenRegistry(registry: TokenRegistry) {
    this.tokenRegistry = registry;
  }

  /** Register engine control callbacks */
  setControls(callbacks: typeof this.controlCallbacks) {
    this.controlCallbacks = callbacks;
  }

  async init() {
    await this.app.register(websocket);

    const apiKey = ENV.API_KEY;
    const authEnabled = apiKey.length > 0;

    if (authEnabled) {
      log("info", "API auth enabled — all requests require valid token");
    } else {
      log("warn", "API auth DISABLED — set API_KEY in .env for production!");
    }

    // ── CORS + Auth ──
    this.app.addHook("onRequest", (req, reply, done) => {
      // CORS headers
      reply.header("Access-Control-Allow-Origin", "*");
      reply.header("Access-Control-Allow-Methods", "GET, POST, PATCH, DELETE, OPTIONS");
      reply.header("Access-Control-Allow-Headers", "Content-Type, Authorization");

      if (req.method === "OPTIONS") {
        reply.code(204).send();
        return;
      }

      // Skip auth if disabled
      if (!authEnabled) { done(); return; }

      // WebSocket upgrade — auth via query param ?token=
      if (req.headers.upgrade?.toLowerCase() === "websocket") {
        const url = new URL(req.url, `http://${req.headers.host || "localhost"}`);
        const token = url.searchParams.get("token");
        if (token === apiKey) { done(); return; }
        reply.code(401).send({ error: "Unauthorized — pass ?token=YOUR_API_KEY on WS connect" });
        return;
      }

      // HTTP — auth via Authorization: Bearer <key>
      const authHeader = req.headers.authorization;
      if (authHeader) {
        const token = authHeader.startsWith("Bearer ") ? authHeader.slice(7) : authHeader;
        if (token === apiKey) { done(); return; }
      }

      reply.code(401).send({ error: "Unauthorized — set Authorization: Bearer YOUR_API_KEY" });
    });

    // ── WebSocket endpoint ──
    this.app.get("/ws", { websocket: true }, (socket, req) => {
      this.clients.add(socket);

      log("info", `WS client connected (total: ${this.clients.size})`);

      // Send welcome message
      socket.send(
        JSON.stringify({
          type: "info",
          timestamp: Date.now(),
          message: "Connected to ArbitragePulse Engine",
          data: { clients: this.clients.size },
        })
      );

      socket.on("message", (data) => {
        try {
          const msg = JSON.parse(data.toString());

          // Handle commands from dashboard
          if (msg.command === "ping") {
            socket.send(JSON.stringify({ type: "pong", timestamp: Date.now() }));
          }
          if (msg.command === "status") {
            this.sendHealth(socket);
          }
          if (msg.command === "state" && this.controlCallbacks.getState) {
            const state = this.controlCallbacks.getState();
            socket.send(JSON.stringify({ type: "state", timestamp: Date.now(), data: state }));
          }
          if (msg.command === "pause" && this.controlCallbacks.pause) {
            this.controlCallbacks.pause(msg.chain_id);
            socket.send(JSON.stringify({ type: "info", timestamp: Date.now(), message: msg.chain_id ? `Chain ${msg.chain_id} paused` : "All chains paused" }));
          }
          if (msg.command === "resume" && this.controlCallbacks.resume) {
            this.controlCallbacks.resume(msg.chain_id);
            socket.send(JSON.stringify({ type: "info", timestamp: Date.now(), message: msg.chain_id ? `Chain ${msg.chain_id} resumed` : "All chains resumed" }));
          }
          if (msg.command === "set_dry_run" && this.controlCallbacks.setDryRun) {
            this.controlCallbacks.setDryRun(msg.enabled !== false);
            socket.send(JSON.stringify({ type: "info", timestamp: Date.now(), message: `Dry-run: ${msg.enabled !== false ? "ON" : "OFF — LIVE"}` }));
          }
        } catch {
          // Ignore malformed messages
        }
      });

      socket.on("close", () => {
        this.clients.delete(socket);
        log("debug", `WS client disconnected (total: ${this.clients.size})`);
      });

      socket.on("error", () => {
        this.clients.delete(socket);
      });
    });

    // ── Health endpoint ──
    this.app.get("/health", async (req, reply) => {
      const health = this.healthProvider
        ? await this.healthProvider()
        : {
            uptime: Date.now() - this.startedAt,
            startedAt: this.startedAt,
            wsClients: this.clients.size,
            stats: {},
          };

      reply.send({
        status: "ok",
        ...health,
      });
    });

    // ── Stats endpoint ──
    this.app.get("/stats", async (req, reply) => {
      if (this.healthProvider) {
        const health = await this.healthProvider();
        reply.send(health.stats);
      } else {
        reply.send({});
      }
    });

    // ═══════════════════════════════════════════════════════
    // TOKEN MANAGEMENT API (Layer 3 — for dashboard)
    // ═══════════════════════════════════════════════════════

    // GET /tokens — list all tokens, optionally filter by chain
    this.app.get("/tokens", async (req, reply) => {
      if (!this.tokenRegistry) {
        return reply.code(503).send({ error: "Token registry not loaded" });
      }

      const query = req.query as { chain_id?: string; trusted_only?: string };
      const chainId = query.chain_id ? parseInt(query.chain_id) : undefined;
      const trustedOnly = query.trusted_only === "true";

      let tokens: TokenEntry[];
      if (trustedOnly) {
        tokens = this.tokenRegistry.getTrusted(chainId);
      } else if (chainId) {
        tokens = this.tokenRegistry.getChainTokens(chainId);
      } else {
        tokens = this.tokenRegistry.getAll();
      }

      reply.send({
        count: tokens.length,
        tokens,
      });
    });

    // POST /tokens — add a new token to the allowlist
    this.app.post("/tokens", async (req, reply) => {
      if (!this.tokenRegistry) {
        return reply.code(503).send({ error: "Token registry not loaded" });
      }

      const body = req.body as {
        symbol: string;
        address: string;
        decimals: number;
        chain_id: number;
        trusted?: boolean;
        category?: TokenCategory;
      };

      if (!body.symbol || !body.address || !body.decimals || !body.chain_id) {
        return reply.code(400).send({
          error: "Missing required fields: symbol, address, decimals, chain_id",
        });
      }

      const success = this.tokenRegistry.addToken({
        symbol: body.symbol.toUpperCase(),
        address: body.address as `0x${string}`,
        decimals: body.decimals,
        chainId: body.chain_id,
        trusted: body.trusted ?? false, // Default to untrusted (monitor first)
        category: body.category || "other",
      });

      if (!success) {
        return reply.code(409).send({ error: "Token already exists" });
      }

      // Auto-save
      this.tokenRegistry.save();

      reply.code(201).send({
        message: `Token ${body.symbol} added`,
        note: "Restart engine to activate new pairs, or set trusted=true via PATCH",
      });
    });

    // PATCH /tokens/:chainId/:address — update a token (trust/untrust)
    this.app.patch("/tokens/:chainId/:address", async (req, reply) => {
      if (!this.tokenRegistry) {
        return reply.code(503).send({ error: "Token registry not loaded" });
      }

      const params = req.params as { chainId: string; address: string };
      const body = req.body as { trusted?: boolean; category?: TokenCategory };
      const chainId = parseInt(params.chainId);

      const token = this.tokenRegistry.getToken(chainId, params.address);
      if (!token) {
        return reply.code(404).send({ error: "Token not found" });
      }

      if (body.trusted !== undefined) {
        this.tokenRegistry.setTrusted(chainId, params.address, body.trusted);
      }
      if (body.category) {
        token.category = body.category;
      }

      this.tokenRegistry.save();

      reply.send({
        message: `Token ${token.symbol} updated`,
        token,
        note: "Restart engine to regenerate pairs with updated tokens",
      });
    });

    // DELETE /tokens/:chainId/:address — remove a token
    this.app.delete("/tokens/:chainId/:address", async (req, reply) => {
      if (!this.tokenRegistry) {
        return reply.code(503).send({ error: "Token registry not loaded" });
      }

      const params = req.params as { chainId: string; address: string };
      const chainId = parseInt(params.chainId);

      const success = this.tokenRegistry.removeToken(chainId, params.address);
      if (!success) {
        return reply.code(404).send({ error: "Token not found" });
      }

      this.tokenRegistry.save();
      reply.send({ message: "Token removed" });
    });

    // GET /tokens/pairs — preview auto-generated pairs
    this.app.get("/tokens/pairs", async (req, reply) => {
      if (!this.tokenRegistry) {
        return reply.code(503).send({ error: "Token registry not loaded" });
      }

      const query = req.query as { chain_id?: string };
      const chainId = query.chain_id ? parseInt(query.chain_id) : undefined;

      const pairs = chainId
        ? this.tokenRegistry.generatePairs(chainId)
        : this.tokenRegistry.generateAllPairs();

      reply.send({
        count: pairs.length,
        pairs: pairs.map((p) => ({
          id: p.id,
          chain: p.chainId,
          path: `${p.tokenInSymbol} → ${p.tokenOutSymbol}`,
          tradeAmount: p.tradeAmount,
        })),
      });
    });

    // ═══════════════════════════════════════════════════════
    // ENGINE CONTROLS (pause / resume / dry-run toggle)
    // ═══════════════════════════════════════════════════════

    // GET /engine/state — current engine state
    this.app.get("/engine/state", async (req, reply) => {
      if (!this.controlCallbacks.getState) {
        return reply.code(503).send({ error: "Engine controls not registered" });
      }
      reply.send(this.controlCallbacks.getState());
    });

    // POST /engine/pause — pause all chains or a specific chain
    this.app.post("/engine/pause", async (req, reply) => {
      if (!this.controlCallbacks.pause) {
        return reply.code(503).send({ error: "Engine controls not registered" });
      }
      const body = req.body as { chain_id?: number } | null;
      this.controlCallbacks.pause(body?.chain_id);
      reply.send({ message: body?.chain_id ? `Chain ${body.chain_id} paused` : "All chains paused" });
    });

    // POST /engine/resume — resume all chains or a specific chain
    this.app.post("/engine/resume", async (req, reply) => {
      if (!this.controlCallbacks.resume) {
        return reply.code(503).send({ error: "Engine controls not registered" });
      }
      const body = req.body as { chain_id?: number } | null;
      this.controlCallbacks.resume(body?.chain_id);
      reply.send({ message: body?.chain_id ? `Chain ${body.chain_id} resumed` : "All chains resumed" });
    });

    // POST /engine/dry-run — toggle dry-run mode
    this.app.post("/engine/dry-run", async (req, reply) => {
      if (!this.controlCallbacks.setDryRun) {
        return reply.code(503).send({ error: "Engine controls not registered" });
      }
      const body = req.body as { enabled: boolean };
      if (body.enabled === undefined) {
        return reply.code(400).send({ error: "Missing field: enabled (boolean)" });
      }
      this.controlCallbacks.setDryRun(body.enabled);
      reply.send({ message: `Dry-run mode: ${body.enabled ? "ON" : "OFF — LIVE TRADING"}` });
    });

    // Register WS broadcast function with logger
    registerBroadcast((entry: LogEntry) => {
      this.broadcast(entry);
    });
  }

  /** Register a function that provides health data */
  onHealth(provider: () => Promise<HealthData>) {
    this.healthProvider = provider;
  }

  /** Broadcast a log entry to all connected WS clients */
  broadcast(entry: LogEntry) {
    if (this.clients.size === 0) return;

    const message = JSON.stringify(entry);

    for (const client of this.clients) {
      try {
        if (client.readyState === 1) {
          // OPEN
          client.send(message);
        }
      } catch {
        this.clients.delete(client);
      }
    }
  }

  /** Send health data to a specific client */
  private async sendHealth(socket: WebSocket) {
    const health = this.healthProvider
      ? await this.healthProvider()
      : { uptime: Date.now() - this.startedAt, wsClients: this.clients.size };

    socket.send(
      JSON.stringify({
        type: "info",
        timestamp: Date.now(),
        message: "Health status",
        data: health,
      })
    );
  }

  /** Start the server */
  async start() {
    await this.app.listen({ port: ENV.PORT, host: "0.0.0.0" });
    log("info", `API server started on port ${ENV.PORT}`, {
      health: `http://localhost:${ENV.PORT}/health`,
      ws: `ws://localhost:${ENV.PORT}/ws`,
    });
  }

  /** Stop the server */
  async stop() {
    for (const client of this.clients) {
      try {
        client.close();
      } catch {}
    }
    await this.app.close();
  }
}
