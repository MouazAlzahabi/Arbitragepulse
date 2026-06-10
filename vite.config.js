import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Dev dashboard proxies API + WebSocket to the engine so DEFAULT_WS can be
// ws://localhost:5173/ws (same origin as Vite). Override if your engine listens elsewhere.
const engineTarget = process.env.ENGINE_PROXY_TARGET || "http://127.0.0.1:3000";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/ws": { target: engineTarget, ws: true, changeOrigin: true },
      "/stats": { target: engineTarget, changeOrigin: true },
      "/stats/pairs": { target: engineTarget, changeOrigin: true },
      "/stats/reset": { target: engineTarget, changeOrigin: true },
      "/trades": { target: engineTarget, changeOrigin: true },
      "/engine": { target: engineTarget, changeOrigin: true },
      "/tokens": { target: engineTarget, changeOrigin: true },
      "/pair-scan": { target: engineTarget, changeOrigin: true },
      "/health": { target: engineTarget, changeOrigin: true },
      "/metrics": { target: engineTarget, changeOrigin: true },
    },
  },
});
