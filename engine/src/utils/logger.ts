import pino from "pino";
import { ENV } from "../config/env";

// ── Pino logger ──
export const logger = pino({
  level: ENV.LOG_LEVEL,
  transport: {
    target: "pino-pretty",
    options: {
      colorize: true,
      translateTime: "HH:MM:ss.l",
      ignore: "pid,hostname",
    },
  },
});

// ── WebSocket broadcast ──
// The API layer registers ws clients here, and any module can broadcast logs.

export type LogLevel = "debug" | "info" | "warn" | "error" | "trade" | "opportunity";

export interface LogEntry {
  type: LogLevel;
  timestamp: number;
  message: string;
  data?: Record<string, unknown>;
}

type WsBroadcastFn = (entry: LogEntry) => void;

let _broadcast: WsBroadcastFn = () => {}; // no-op until API registers

export function registerBroadcast(fn: WsBroadcastFn) {
  _broadcast = fn;
}

/**
 * Log + broadcast to connected WS clients.
 * Use this instead of logger.info() when you want the dashboard to see it.
 */
export function log(level: LogLevel, message: string, data?: Record<string, unknown>) {
  const entry: LogEntry = {
    type: level,
    timestamp: Date.now(),
    message,
    data,
  };

  // Pino
  switch (level) {
    case "debug":
      logger.debug(data, message);
      break;
    case "info":
    case "opportunity":
      logger.info(data, message);
      break;
    case "trade":
      logger.info(data, `💰 ${message}`);
      break;
    case "warn":
      logger.warn(data, message);
      break;
    case "error":
      logger.error(data, message);
      break;
  }

  // Broadcast to WS
  _broadcast(entry);
}
