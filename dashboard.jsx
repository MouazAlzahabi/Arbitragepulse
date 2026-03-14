import { useState, useEffect, useRef, useCallback, useMemo } from "react";
import { AreaChart, Area, XAxis, YAxis, Tooltip, ResponsiveContainer } from "recharts";

// ═══════════════════════════════════════════════════════════
// CONFIG
// ═══════════════════════════════════════════════════════════
const DEFAULT_WS = location.port === "5174"
  ? "ws://localhost:3000/ws"                                              // local Vite dev
  : `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`; // served from engine

// Known chain ID → display name mapping
const CHAIN_NAMES = {
  10:     "Optimism",
  8453:   "Base",
  42161:  "Arbitrum",
  100:    "Gnosis",
  137:    "Polygon",
  534352: "Scroll",
  59144:  "Linea",
  1:      "Ethereum",
  56:     "BNB Chain",
  43114:  "Avalanche",
};

const chainName = (id) => CHAIN_NAMES[Number(id)] || `Chain ${id}`;

// Extract [ChainName] prefix from log messages like "[Optimism] tx=..."
const extractChain = (msg) => {
  const m = typeof msg === "string" && msg.match(/^\[([^\]]+)\]/);
  return m ? m[1] : null;
};

// ═══════════════════════════════════════════════════════════
// HOOKS
// ═══════════════════════════════════════════════════════════
function useWebSocket(url, apiKey) {
  const [status, setStatus] = useState("disconnected");
  const [logs, setLogs] = useState(() => {
    try { return JSON.parse(localStorage.getItem("ap_logs") || "[]"); } catch { return []; }
  });
  const [stats, setStats] = useState(null);
  const [engineState, setEngineState] = useState(null);
  const [authError, setAuthError] = useState(false);
  const wsRef = useRef(null);
  const reconnRef = useRef(null);
  const attempts = useRef(0);

  const connect = useCallback(() => {
    if (!url) return;
    // Null out old socket's handlers before closing so onclose doesn't
    // schedule a reconnect that races with the new connection (React StrictMode
    // double-mounts trigger this and result in two live WS connections).
    if (wsRef.current) {
      wsRef.current.onopen = null;
      wsRef.current.onmessage = null;
      wsRef.current.onclose = null;
      wsRef.current.onerror = null;
      wsRef.current.close();
      wsRef.current = null;
    }
    clearTimeout(reconnRef.current);
    try {
      const wsUrl = apiKey ? `${url}${url.includes("?") ? "&" : "?"}token=${encodeURIComponent(apiKey)}` : url;
      const ws = new WebSocket(wsUrl);
      wsRef.current = ws;
      setStatus("connecting");
      setAuthError(false);
      ws.onopen = () => {
        setStatus("connected");
        attempts.current = 0;
        ws.send(JSON.stringify({ command: "status" }));
        ws.send(JSON.stringify({ command: "state" }));
      };
      ws.onmessage = (e) => {
        try {
          const d = JSON.parse(e.data);
          if (d.type === "pong") return;
          if (d.type === "state") { setEngineState(d.data); return; }
          if (d.data?.stats) setStats(d.data.stats || d.data);
          // Normalize: Rust engine sends `level`, TS engine sends `type`
          const entry = { ...d, type: d.type || d.level, _id: Date.now() + Math.random() };
          setLogs((p) => {
            const next = [...p.slice(-499), entry];
            try { localStorage.setItem("ap_logs", JSON.stringify(next)); } catch {}
            return next;
          });
        } catch {}
      };
      ws.onclose = (e) => {
        setStatus("disconnected");
        if (e.code === 1008 || e.reason?.includes("401")) { setAuthError(true); return; }
        scheduleReconnect();
      };
      ws.onerror = () => {
        if (wsRef.current?.readyState === WebSocket.CLOSED && apiKey) setAuthError(true);
        ws.close();
      };
    } catch { setStatus("disconnected"); scheduleReconnect(); }
  }, [url, apiKey]);

  const scheduleReconnect = useCallback(() => {
    attempts.current++;
    const delay = Math.min(1000 * Math.pow(2, attempts.current), 15000);
    reconnRef.current = setTimeout(connect, delay);
  }, [connect]);

  const send = useCallback((cmd) => {
    if (wsRef.current?.readyState === 1) wsRef.current.send(JSON.stringify(cmd));
  }, []);

  useEffect(() => { connect(); return () => { clearTimeout(reconnRef.current); wsRef.current?.close(); }; }, [connect]);

  return {
    status, logs, stats, engineState, authError, send,
    clearLogs: () => { setLogs([]); try { localStorage.removeItem("ap_logs"); } catch {} },
    disconnect: () => { clearTimeout(reconnRef.current); attempts.current = 999; wsRef.current?.close(); setStatus("disconnected"); },
    reconnect: () => { attempts.current = 0; connect(); },
    refreshState: () => send({ command: "state" }),
  };
}

function useApi(baseUrl, apiKey) {
  const [tokens, setTokens] = useState([]);
  const [pairs, setPairs] = useState([]);
  const [pairScan, setPairScan] = useState({});  // chain_id → [{pair_id, display_name, ...}]
  const [loading, setLoading] = useState(false);
  const headers = useMemo(() => {
    const h = { "Content-Type": "application/json" };
    if (apiKey) h["Authorization"] = `Bearer ${apiKey}`;
    return h;
  }, [apiKey]);
  const f = useCallback(async (path, opts = {}) => {
    try { const r = await fetch(`${baseUrl}${path}`, { ...opts, headers: { ...headers, ...(opts.headers || {}) } }); if (r.status === 401) return { _authError: true }; return await r.json(); } catch { return null; }
  }, [baseUrl, headers]);

  return {
    tokens, pairs, pairScan, loading,
    fetchTokens: async (cid) => { setLoading(true); const d = await f(`/tokens${cid ? `?chain_id=${cid}` : ""}`); setTokens(d?.tokens || []); setLoading(false); },
    fetchPairs: async (cid) => { const d = await f(`/tokens/pairs${cid ? `?chain_id=${cid}` : ""}`); setPairs(d?.pairs || []); },
    fetchPairScan: async () => {
      const d = await f("/pair-scan");
      if (!d?.chains) return;
      const m = {};
      for (const c of d.chains) m[c.chain_id] = c.pairs;
      setPairScan(m);
    },
    togglePair: async (pairId) => { await f(`/pair-scan/${encodeURIComponent(pairId)}/toggle`, { method: "POST" }); },
    toggleTrust: async (cid, addr, trusted) => { await f(`/tokens/${cid}/${addr}`, { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ trusted }) }); },
    addToken: async (t) => { const r = await fetch(`${baseUrl}/tokens`, { method: "POST", headers, body: JSON.stringify(t) }); return r?.ok; },
    removeToken: async (cid, addr) => { await f(`/tokens/${cid}/${addr}`, { method: "DELETE" }); },
    pauseEngine: (cid) => f("/engine/pause", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(cid ? { chain_id: cid } : {}) }),
    resumeEngine: (cid) => f("/engine/resume", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(cid ? { chain_id: cid } : {}) }),
    setDryRun: (enabled) => f("/engine/dry-run", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ enabled }) }),
    restartEngine: () => f("/engine/restart", { method: "POST", headers: { "Content-Type": "application/json" } }),
    resetDb: () => f("/stats/reset", { method: "POST", headers: { "Content-Type": "application/json" } }),
    fetchStats: () => f("/stats"),
    fetchState: () => f("/engine/state"),
    fetchTrades: (limit) => f(`/trades?limit=${limit || 500}`),
  };
}

// ═══════════════════════════════════════════════════════════
// LOGIN
// ═══════════════════════════════════════════════════════════
function LoginScreen({ engineUrl, setEngineUrl, apiKey, setApiKey, onConnect, error }) {
  const [localUrl, setLocalUrl] = useState(engineUrl);
  const [localKey, setLocalKey] = useState(apiKey);
  const handleSubmit = () => { setEngineUrl(localUrl); setApiKey(localKey); onConnect(); };
  return (
    <div style={{ minHeight: "100vh", background: "#020617", display: "flex", alignItems: "center", justifyContent: "center", fontFamily: "'JetBrains Mono', monospace" }}>
      <div style={{ width: 420, padding: 40 }}>
        <div style={{ textAlign: "center", marginBottom: 36 }}>
          <div style={{ fontSize: 28, fontWeight: 700, color: "#e2e8f0", letterSpacing: -1 }}><span style={{ color: "#a78bfa" }}>{"\u26a1"}</span> ARBITRAGE<span style={{ color: "#475569" }}>PULSE</span></div>
          <div style={{ fontSize: 11, color: "#334155", marginTop: 6, letterSpacing: 2, textTransform: "uppercase" }}>Operator Dashboard</div>
        </div>
        <div style={{ marginBottom: 16 }}>
          <label style={{ display: "block", fontSize: 10, color: "#475569", letterSpacing: 1.2, marginBottom: 6, textTransform: "uppercase", fontWeight: 700 }}>Engine URL</label>
          <input value={localUrl} onChange={(e) => setLocalUrl(e.target.value)} placeholder="ws://localhost:3000/ws" style={{ width: "100%", background: "#0f172a", border: "1px solid #1e293b", borderRadius: 6, color: "#e2e8f0", fontFamily: "inherit", padding: "10px 14px", fontSize: 13 }} />
        </div>
        <div style={{ marginBottom: 24 }}>
          <label style={{ display: "block", fontSize: 10, color: "#475569", letterSpacing: 1.2, marginBottom: 6, textTransform: "uppercase", fontWeight: 700 }}>API Key <span style={{ color: "#334155", fontWeight: 400 }}>(leave empty if auth disabled)</span></label>
          <input value={localKey} onChange={(e) => setLocalKey(e.target.value)} type="password" placeholder="Your API key" style={{ width: "100%", background: "#0f172a", border: "1px solid #1e293b", borderRadius: 6, color: "#e2e8f0", fontFamily: "inherit", padding: "10px 14px", fontSize: 13 }} autoComplete="off" />
        </div>
        {error && <div style={{ background: "#450a0a", border: "1px solid #7f1d1d", borderRadius: 6, padding: "10px 14px", marginBottom: 16, color: "#fca5a5", fontSize: 12 }}>Authentication failed. Check your API key.</div>}
        <button onClick={handleSubmit} style={{ ...btn, width: "100%", padding: "12px 20px", fontSize: 14, fontWeight: 700, background: "linear-gradient(135deg, #4c1d95, #1e1b4b)", color: "#a78bfa", borderRadius: 6 }}>Connect</button>
        <div style={{ marginTop: 24, fontSize: 10, color: "#1e293b", textAlign: "center", lineHeight: 1.8 }}>Engine not running?<br /><span style={{ color: "#334155" }}>cargo run -p engine</span> in your workspace directory</div>
      </div>
    </div>
  );
}

// ═══════════════════════════════════════════════════════════
// UTILITIES
// ═══════════════════════════════════════════════════════════
function formatUptime(ms) {
  const s = Math.floor(ms / 1000); const m = Math.floor(s / 60); const h = Math.floor(m / 60);
  if (h > 0) return `${h}h ${m % 60}m`;
  if (m > 0) return `${m}m ${s % 60}s`;
  return `${s}s`;
}

function ts(d) {
  const t = new Date(d);
  return `${t.getHours().toString().padStart(2, "0")}:${t.getMinutes().toString().padStart(2, "0")}:${t.getSeconds().toString().padStart(2, "0")}`;
}

function playBeep() {
  try {
    const ctx = new AudioContext();
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.connect(gain); gain.connect(ctx.destination);
    osc.frequency.value = 880; gain.gain.value = 0.08;
    osc.start(); osc.stop(ctx.currentTime + 0.1);
  } catch {}
}

// ═══════════════════════════════════════════════════════════
// STYLES
// ═══════════════════════════════════════════════════════════
const TYPE = { heartbeat: ["#334155", "♥ HB"], info: ["#94a3b8", "INFO"], debug: ["#475569", "DBUG"], warn: ["#fbbf24", "WARN"], error: ["#f87171", "ERR!"], trade: ["#34d399", "TRDE"], opportunity: ["#a78bfa", "OPP!"] };
const btn = { background: "#1e293b", border: "none", borderRadius: 4, padding: "6px 14px", color: "#94a3b8", cursor: "pointer", fontFamily: "'JetBrains Mono', monospace", fontSize: 12 };

// ═══════════════════════════════════════════════════════════
// COMPONENTS
// ═══════════════════════════════════════════════════════════
function Dot({ status }) {
  const c = { connected: "#34d399", connecting: "#fbbf24", disconnected: "#f87171" };
  return <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: "50%", background: c[status] || "#475569", boxShadow: status === "connected" ? `0 0 8px ${c.connected}` : "none", animation: status === "connecting" ? "pulse 1.5s infinite" : "none" }} />;
}

function Header({ tab, setTab, status, engineUrl, apiKey, onLogout }) {
  const tabs = ["monitor", "tokens", "controls"];
  return (
    <div style={{ background: "#020617", borderBottom: "1px solid #1e293b" }}>
      <div style={{ display: "flex", alignItems: "center", gap: 12, padding: "10px 16px", flexWrap: "wrap" }}>
        <div style={{ fontSize: 16, fontWeight: 700, letterSpacing: -0.5, whiteSpace: "nowrap" }}>
          <span style={{ color: "#a78bfa" }}>{"\u26a1"}</span> ARBITRAGE<span style={{ color: "#475569" }}>PULSE</span>
        </div>
        <div style={{ display: "flex", gap: 0, marginLeft: 8 }}>
          {tabs.map((t) => (
            <button key={t} onClick={() => setTab(t)} style={{ ...btn, borderRadius: 0, padding: "6px 16px", fontSize: 10, letterSpacing: 1.2, textTransform: "uppercase", background: tab === t ? "#1e293b" : "transparent", color: tab === t ? "#e2e8f0" : "#475569", borderBottom: tab === t ? "2px solid #a78bfa" : "2px solid transparent" }}>
              {t}
            </button>
          ))}
        </div>
        <span style={{ flex: 1 }} />
        <div style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 11 }}>
          <Dot status={status} />
          <span style={{ color: "#334155", fontSize: 10 }}>{engineUrl.replace("ws://", "").replace("/ws", "")}</span>
          {apiKey && <span style={{ color: "#166534", fontSize: 9, background: "#052e16", padding: "2px 6px", borderRadius: 3 }}>{"\ud83d\udd12"}</span>}
          <button onClick={onLogout} style={{ ...btn, fontSize: 10, padding: "4px 10px", color: "#64748b" }} title="Disconnect & change settings">Logout</button>
        </div>
      </div>
    </div>
  );
}

function StatsRow({ stats, logs, engineState, apiStats }) {
  // Use HTTP-polled apiStats as the source of truth for counts — WS log counting
  // is unreliable: the Rust engine emits level="info" (not type="trade"), and the
  // log buffer is capped at 500 entries so counts freeze after ~250 arbs.
  const allChains = apiStats?.chains || [];
  const trades  = allChains.reduce((s, c) => s + (c.total_success  || 0), 0)
                  || logs.filter((l) => l.type === "trade").length;
  const attempts = allChains.reduce((s, c) => s + (c.total_attempts || 0), 0);
  // opportunities = every execution attempt that passed the gas-profit check
  const opps   = allChains.length > 0 ? attempts
                  : logs.filter((l) => l.type === "opportunity").length;
  // errors = attempts that did NOT succeed (sim failures, gas check failures, reverts)
  const errors = allChains.length > 0 ? (attempts - trades)
                  : logs.filter((l) => l.type === "error").length;
  const uptime = apiStats?.uptime_seconds != null ? formatUptime(apiStats.uptime_seconds * 1000) : "—";
  const dryRun = apiStats?.dry_run ?? engineState?.dryRun ?? true;
  const paused = apiStats?.paused ?? engineState?.paused ?? false;
  const ghostProfit = allChains.reduce((s, c) => s + (c.ghost_profit_usd || 0), 0);

  const items = [
    { l: "UPTIME", v: uptime, c: "#94a3b8" },
    { l: "CHAINS", v: apiStats?.chains?.length ?? stats?.chains?.length ?? "—", c: "#a78bfa" },
    { l: "OPPORTUNITIES", v: opps, c: "#818cf8" },
    { l: "TRADES", v: trades, c: "#34d399" },
    { l: "ERRORS", v: errors, c: errors > 0 ? "#f87171" : "#334155" },
    { l: "GHOST PROFIT", v: `$${ghostProfit.toFixed(2)}`, c: "#64748b", title: "Sum of gross profit from trades rejected due to gas cost" },
    { l: "MODE", v: paused ? "⏸ PAUSED" : dryRun ? "DRY RUN" : "⚡ LIVE", c: paused ? "#f87171" : dryRun ? "#fbbf24" : "#ef4444" },
  ];

  return (
    <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(120px, 1fr))", gap: 1, background: "#1e293b" }}>
      {items.map((i) => (
        <div key={i.l} style={{ background: "#0f172a", padding: "12px 14px" }} title={i.title}>
          <div style={{ fontSize: 9, color: "#475569", letterSpacing: 1.5, fontWeight: 700, marginBottom: 3 }}>{i.l}</div>
          <div style={{ fontSize: 18, color: i.c, fontWeight: 700, fontFamily: "inherit" }}>{i.v}</div>
        </div>
      ))}
    </div>
  );
}

function ChainCards({ apiStats }) {
  const chains = apiStats?.chains || [];
  if (chains.length === 0) return null;
  return (
    <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(240px, 1fr))", gap: 10, padding: "10px 14px" }}>
      {chains.map((c) => {
        const failed = c.total_attempts - c.total_success;
        return (
          <div key={c.chain_id} style={{ background: "linear-gradient(135deg, #0f172a, #1e1b4b)", border: "1px solid #1e293b", borderRadius: 8, padding: 14 }}>
            <div style={{ display: "flex", justifyContent: "space-between", marginBottom: 10 }}>
              <span style={{ fontWeight: 700, fontSize: 13 }}>{c.chain_name}</span>
              <span style={{ fontSize: 9, color: "#34d399", background: "#052e16", padding: "2px 8px", borderRadius: 10 }}>ACTIVE</span>
            </div>
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 6, fontSize: 11 }}>
              <div><span style={{ color: "#475569" }}>Scans </span><span style={{ color: "#94a3b8" }}>{c.total_scans || 0}</span></div>
              <div><span style={{ color: "#475569" }}>Executions </span><span style={{ color: "#94a3b8" }}>{c.total_attempts}</span></div>
              <div><span style={{ color: "#475569" }}>Success </span><span style={{ color: "#34d399" }}>{c.total_success}</span></div>
              <div><span style={{ color: "#475569" }}>Profit </span><span style={{ color: "#a78bfa" }}>${(c.total_profit_usd || 0).toFixed(2)}</span></div>
              <div><span style={{ color: "#475569" }}>Fail </span><span style={{ color: failed > 0 ? "#f87171" : "#334155" }}>{failed}</span></div>
            </div>
          </div>
        );
      })}
    </div>
  );
}


function GasLatencyBar({ apiStats, apiLatency }) {
  // Show RPC latency per chain (only chains that have a measurement)
  const rpcChains = (apiStats?.chains || []).filter((c) => c.rpc_latency_ms > 0);
  if (apiLatency == null && rpcChains.length === 0) return null;
  return (
    <div style={{ display: "flex", gap: 24, padding: "4px 16px", background: "#0a0f1a", borderBottom: "1px solid #1e293b", fontSize: 11 }}>
      {rpcChains.map((c) => {
        const ms = Math.round(c.rpc_latency_ms);
        const col = ms < 100 ? "#34d399" : ms < 300 ? "#fbbf24" : "#f87171";
        return (
          <span key={c.chain_id} style={{ color: "#475569" }}>
            📡 RPC ({c.chain_name}): <span style={{ color: col }}>{ms}ms</span>
          </span>
        );
      })}
      {apiLatency != null && (
        <span style={{ color: "#475569" }}>🏓 Ping: <span style={{ color: apiLatency < 100 ? "#34d399" : apiLatency < 500 ? "#fbbf24" : "#f87171" }}>{apiLatency}ms</span></span>
      )}
    </div>
  );
}

// ─── 1. 24h PnL chart (DB-backed, live WS appends) ────────
function ProfitChart({ fetchTrades, wsLogs }) {
  const [series, setSeries] = useState([{ time: "start", profit: 0 }]);
  const [total, setTotal] = useState(0);
  const loadedRef = useRef(false);
  const appendedRef = useRef(0);

  // Load historical trades from DB on mount (survives restarts)
  useEffect(() => {
    if (loadedRef.current || !fetchTrades) return;
    loadedRef.current = true;
    fetchTrades(2000).then((d) => {
      if (!d?.trades) return;
      const cutoff = Date.now() / 1000 - 86400;
      const recent = d.trades
        .filter((t) => t.ts >= cutoff && t.success && !t.dry_run)
        .sort((a, b) => a.ts - b.ts);
      let cum = 0;
      const pts = [{ time: "start", profit: 0 }];
      recent.forEach((t) => {
        cum += t.profit_usd;
        pts.push({ time: ts(t.ts * 1000), profit: parseFloat(cum.toFixed(4)) });
      });
      setSeries(pts);
      setTotal(parseFloat(cum.toFixed(4)));
    });
  }, [fetchTrades]);

  // Append new live WS trades
  useEffect(() => {
    const liveTrades = wsLogs.filter((l) => l.type === "trade" && l.data?.profit_usd != null && !l._historical);
    const newTrades = liveTrades.slice(appendedRef.current);
    if (newTrades.length === 0) return;
    appendedRef.current = liveTrades.length;
    setSeries((prev) => {
      const last = prev[prev.length - 1] || { profit: 0 };
      let cum = last.profit;
      const pts = [...prev];
      newTrades.forEach((l) => {
        cum += parseFloat(l.data.profit_usd || 0);
        pts.push({ time: ts(l.timestamp * 1000), profit: parseFloat(cum.toFixed(4)) });
      });
      setTotal(parseFloat(cum.toFixed(4)));
      return pts;
    });
  }, [wsLogs]);

  return (
    <div style={{ background: "#0f172a", borderRadius: 8, border: "1px solid #1e293b", overflow: "hidden" }}>
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", padding: "10px 14px", borderBottom: "1px solid #1e293b" }}>
        <span style={{ fontSize: 9, color: "#475569", letterSpacing: 1.5, fontWeight: 700 }}>24h PnL</span>
        <span style={{ fontSize: 14, color: total >= 0 ? "#34d399" : "#f87171", fontWeight: 700 }}>${total.toFixed(4)}</span>
      </div>
      <div style={{ height: 160, padding: "4px 0" }}>
        {series.length < 2 ? (
          <div style={{ display: "flex", alignItems: "center", justifyContent: "center", height: "100%", color: "#1e293b", fontSize: 12, fontStyle: "italic" }}>Awaiting trade data…</div>
        ) : (
          <ResponsiveContainer width="100%" height="100%">
            <AreaChart data={series} margin={{ top: 8, right: 8, left: 0, bottom: 0 }}>
              <defs><linearGradient id="pg" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stopColor="#34d399" stopOpacity={0.25} /><stop offset="100%" stopColor="#34d399" stopOpacity={0} /></linearGradient></defs>
              <XAxis dataKey="time" tick={{ fill: "#334155", fontSize: 9 }} axisLine={{ stroke: "#1e293b" }} tickLine={false} />
              <YAxis tick={{ fill: "#334155", fontSize: 9 }} axisLine={false} tickLine={false} tickFormatter={(v) => `$${v}`} />
              <Tooltip contentStyle={{ background: "#1e293b", border: "1px solid #334155", borderRadius: 6, fontSize: 11, color: "#e2e8f0" }} formatter={(v) => [`$${v}`, "Profit"]} />
              <Area type="monotone" dataKey="profit" stroke="#34d399" strokeWidth={2} fill="url(#pg)" dot={false} />
            </AreaChart>
          </ResponsiveContainer>
        )}
      </div>
    </div>
  );
}

function OpportunityTable({ logs }) {
  const opps = useMemo(() =>
    logs.filter((l) => l.type === "opportunity" && l.data).slice(-50).reverse(),
  [logs]);

  if (opps.length === 0) return <div style={{ color: "#1e293b", textAlign: "center", padding: 30, fontSize: 12, fontStyle: "italic" }}>No opportunities detected yet</div>;

  return (
    <div style={{ overflow: "auto", maxHeight: 260 }}>
      <table style={{ width: "100%", borderCollapse: "collapse", fontSize: 11 }}>
        <thead><tr style={{ borderBottom: "1px solid #1e293b" }}>
          {["Time", "Chain", "Pair", "Buy → Sell", "Profit USD"].map((h) => (
            <th key={h} style={{ textAlign: "left", padding: "6px 8px", color: "#334155", fontSize: 9, letterSpacing: 1.2, textTransform: "uppercase", fontWeight: 700, position: "sticky", top: 0, background: "#0f172a" }}>{h}</th>
          ))}
        </tr></thead>
        <tbody>
          {opps.map((o) => (
            <tr key={o._id} style={{ borderBottom: "1px solid #0a0f1a" }}>
              <td style={{ padding: "5px 8px", color: "#475569" }}>{ts(o.timestamp * 1000)}</td>
              <td style={{ padding: "5px 8px", color: "#a78bfa" }}>{o.data.chain || extractChain(o.message) || "—"}</td>
              <td style={{ padding: "5px 8px", color: "#e2e8f0", fontWeight: 600 }}>{o.data.pair_id || o.data.triplet_id || o.data.pair || "—"}</td>
              <td style={{ padding: "5px 8px", color: "#94a3b8", fontSize: 10 }}>
                {o.data.router_ab ? (
                  // Triangular: A→B, B→C, C→A
                  <span><span style={{ color: "#22d3ee" }}>{o.data.router_ab}</span><span style={{ color: "#475569" }}>→</span><span style={{ color: "#fb923c" }}>{o.data.router_bc}</span><span style={{ color: "#475569" }}>→</span><span style={{ color: "#a78bfa" }}>{o.data.router_ca}</span></span>
                ) : (
                  // 2-hop: A → B
                  <span><span style={{ color: "#22d3ee" }}>{o.data.router_a || "—"}</span><span style={{ color: "#475569" }}> → </span><span style={{ color: "#fb923c" }}>{o.data.router_b || "—"}</span></span>
                )}
              </td>
              <td style={{ padding: "5px 8px", color: "#34d399", fontWeight: 600 }}>
                ${parseFloat(o.data.profit_usd || o.data.profit || 0).toFixed(4)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

// ─── 3. Pair leaderboard (DB-backed via /stats/pairs + live OPP! detection counting) ────
function PairLeaderboard({ pairStats, wsLogs, onReload }) {
  // Count live OPP! detections per pair_id from WS logs (this session only, not executions)
  const detectedCounts = useMemo(() => {
    const counts = {};
    wsLogs.forEach((l) => {
      if (l.type === "opportunity" && l.data?.pair_id) {
        counts[l.data.pair_id] = (counts[l.data.pair_id] || 0) + 1;
      }
    });
    return counts;
  }, [wsLogs]);

  const rows = useMemo(() =>
    [...(pairStats || [])].sort((a, b) => b.profit_usd - a.profit_usd),
  [pairStats]);

  return (
    <div style={{ background: "#0f172a", borderRadius: 8, border: "1px solid #1e293b", overflow: "hidden" }}>
      <div style={{ padding: "10px 14px", borderBottom: "1px solid #1e293b", display: "flex", alignItems: "center", justifyContent: "space-between" }}>
        <div>
          <span style={{ fontSize: 9, color: "#475569", letterSpacing: 1.5, fontWeight: 700 }}>PAIR LEADERBOARD</span>
          <span style={{ fontSize: 9, color: "#1e293b", marginLeft: 8 }}>(DB-backed — executed trades only)</span>
        </div>
        {onReload && rows.length === 0 && (
          <button onClick={onReload} style={{ background: "#1e293b", border: "none", color: "#64748b", fontSize: 10, padding: "3px 8px", borderRadius: 4, cursor: "pointer" }}>↺ Reload</button>
        )}
      </div>
      {rows.length === 0 ? (
        <div style={{ color: "#1e293b", textAlign: "center", padding: 24, fontSize: 12, fontStyle: "italic" }}>No executed trade data yet…</div>
      ) : (
        <div style={{ overflow: "auto", maxHeight: 240 }}>
          <table style={{ width: "100%", borderCollapse: "collapse", fontSize: 11 }}>
            <thead>
              <tr style={{ borderBottom: "1px solid #1e293b" }}>
                {["Chain", "Pair", "Detected", "Executed", "Net Profit", "Win Rate"].map((h) => (
                  <th key={h} style={{ textAlign: "left", padding: "6px 10px", color: "#334155", fontSize: 9, letterSpacing: 1.2, textTransform: "uppercase", fontWeight: 700, position: "sticky", top: 0, background: "#0f172a" }}>{h}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => {
                const winRate = r.attempts > 0 ? ((r.successes / r.attempts) * 100).toFixed(0) + "%" : "0%";
                const isZero = (r.profit_usd || 0) <= 0;
                const detected = detectedCounts[r.pair_id] || 0;
                const rowKey = `${r.chain_name || ""}:${r.pair_id}`;
                return (
                  <tr key={rowKey} style={{ borderBottom: "1px solid #0a0f1a" }}>
                    <td style={{ padding: "6px 10px", color: "#a78bfa", fontSize: 10 }}>{r.chain_name || "—"}</td>
                    <td style={{ padding: "6px 10px", color: isZero ? "#334155" : "#e2e8f0", fontWeight: 600 }}>{r.pair_id}</td>
                    <td style={{ padding: "6px 10px", color: detected > 0 ? "#64748b" : "#1e293b" }} title="OPP! detections this session (not executions)">{detected}</td>
                    <td style={{ padding: "6px 10px", color: isZero ? "#334155" : "#94a3b8" }}>{r.attempts}</td>
                    <td style={{ padding: "6px 10px", color: isZero ? "#334155" : "#34d399", fontWeight: 600 }}>${(r.profit_usd || 0).toFixed(4)}</td>
                    <td style={{ padding: "6px 10px", color: isZero ? "#334155" : "#fbbf24" }}>{winRate}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

function LogFeed({ logs, enabledTypes, setEnabledTypes }) {
  const ref = useRef(null);
  const [auto, setAuto] = useState(true);
  useEffect(() => { if (auto && ref.current) ref.current.scrollTop = ref.current.scrollHeight; }, [logs, auto]);

  const isSkipped = (l) =>
    (l.type === "info" || l.type === "debug") &&
    (l.message?.includes("negative after gas") || l.message?.includes("Skipped") || l.message?.includes("skipped"));

  const filtered = logs.filter((l) => {
    if (l.type === "heartbeat") return enabledTypes.has("heartbeat");
    if (isSkipped(l)) return enabledTypes.has("skipped");
    if (l.type === "trade") return enabledTypes.has("trade");
    if (l.type === "opportunity") return enabledTypes.has("opportunity");
    if (l.type === "error" || l.type === "warn") return enabledTypes.has("errors");
    if (l.type === "info") return enabledTypes.has("info");
    return false;
  });

  const toggle = (key) => setEnabledTypes((prev) => {
    const next = new Set(prev);
    if (next.has(key)) next.delete(key); else next.add(key);
    return next;
  });

  const checkboxes = [
    { key: "trade", label: "TRDE", color: "#34d399" },
    { key: "opportunity", label: "OPP!", color: "#a78bfa" },
    { key: "info", label: "Info", color: "#94a3b8" },
    { key: "heartbeat", label: "♥ HB", color: "#334155" },
    { key: "skipped", label: "Skipped", color: "#475569" },
    { key: "errors", label: "Errors/Warn", color: "#f87171" },
  ];

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div style={{ display: "flex", gap: 12, padding: "6px 10px", background: "#0f172a", borderBottom: "1px solid #1e293b", flexShrink: 0, flexWrap: "wrap", alignItems: "center" }}>
        {checkboxes.map(({ key, label, color }) => (
          <label key={key} style={{ display: "flex", alignItems: "center", gap: 5, cursor: "pointer", userSelect: "none" }}>
            <input
              type="checkbox"
              checked={enabledTypes.has(key)}
              onChange={() => toggle(key)}
              style={{ accentColor: color, width: 12, height: 12, cursor: "pointer" }}
            />
            <span style={{ fontSize: 9, fontWeight: 700, letterSpacing: 1, color: enabledTypes.has(key) ? color : "#334155" }}>{label}</span>
          </label>
        ))}
        <span style={{ flex: 1 }} />
        <button onClick={() => setAuto(!auto)} style={{ ...btn, fontSize: 9, padding: "2px 8px", color: auto ? "#34d399" : "#475569" }}>{auto ? "⬇ AUTO" : "⏸ STOP"}</button>
      </div>
      <div ref={ref} onScroll={(e) => {
        const el = e.target;
        const atBot = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
        if (!atBot && auto) setAuto(false);
        if (atBot && !auto) setAuto(true);
      }} style={{ flex: 1, overflow: "auto", padding: "6px 10px", fontSize: 11, lineHeight: 1.8, background: "#020617" }}>
        {filtered.length === 0 && <div style={{ color: "#1e293b", fontStyle: "italic", paddingTop: 16, textAlign: "center" }}>{logs.length === 0 ? "Connecting…" : "No matching logs"}</div>}
        {filtered.map((l) => {
          const [clr, badge] = TYPE[l.type] || TYPE.info;
          return (
            <div key={l._id} style={{ display: "flex", gap: 6, color: "#64748b" }}>
              <span style={{ color: "#1e293b", flexShrink: 0 }}>{ts(l.timestamp * 1000)}</span>
              <span style={{ color: clr, fontWeight: 700, flexShrink: 0, width: 34, textAlign: "center", background: `${clr}12`, borderRadius: 2, fontSize: 10 }}>{badge}</span>
              <span style={{ color: clr === "#475569" ? "#475569" : "#b0bec5" }}>{l.message}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function ControlPanel({ ws, api, apiStats, clearAll, onDbReset }) {
  const [confirmLive, setConfirmLive] = useState(false);
  const dryRun = apiStats?.dry_run ?? true;
  const paused = apiStats?.paused ?? false;
  const [soundOn, setSoundOn] = useState(true);
  const [confirmRestart, setConfirmRestart] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const [confirmDbReset, setConfirmDbReset] = useState(false);
  const [dbResetting, setDbResetting] = useState(false);

  // Sound on trade
  useEffect(() => {
    if (!soundOn) return;
    const last = ws.logs[ws.logs.length - 1];
    if (last?.type === "trade") playBeep();
  }, [ws.logs.length, soundOn]);

  const handleGoLive = async () => {
    if (!confirmLive) { setConfirmLive(true); return; }
    await api.setDryRun(false);
    setConfirmLive(false);
  };

  const handleGoDry = async () => {
    await api.setDryRun(true);
    setConfirmLive(false);
  };

  const handlePause = () => api.pauseEngine();
  const handleResume = () => api.resumeEngine();

  const handleRestart = async () => {
    if (!confirmRestart) { setConfirmRestart(true); return; }
    setRestarting(true);
    setConfirmRestart(false);
    await api.restartEngine();
    // Give process time to exit and restart before re-enabling button
    setTimeout(() => setRestarting(false), 5000);
  };

  return (
    <div style={{ padding: 20, maxWidth: 700 }}>
      <h2 style={{ fontSize: 14, color: "#475569", letterSpacing: 1.5, marginBottom: 20, textTransform: "uppercase", fontWeight: 700 }}>Engine Controls</h2>

      {/* Pause / Resume */}
      <div style={{ background: "#0f172a", border: "1px solid #1e293b", borderRadius: 8, padding: 20, marginBottom: 16 }}>
        <div style={{ fontSize: 11, color: "#475569", letterSpacing: 1, marginBottom: 12, fontWeight: 700 }}>EXECUTION STATE</div>
        <div style={{ display: "flex", gap: 12, alignItems: "center", flexWrap: "wrap" }}>
          <div style={{ fontSize: 22, fontWeight: 700, color: paused ? "#f87171" : "#34d399" }}>
            {paused ? "⏸ PAUSED" : "▶ RUNNING"}
          </div>
          <span style={{ flex: 1 }} />
          {paused ? (
            <button onClick={handleResume} style={{ ...btn, background: "#052e16", color: "#34d399", padding: "10px 24px", fontSize: 14, fontWeight: 700, borderRadius: 6 }}>▶ Resume All Chains</button>
          ) : (
            <button onClick={handlePause} style={{ ...btn, background: "#450a0a", color: "#f87171", padding: "10px 24px", fontSize: 14, fontWeight: 700, borderRadius: 6 }}>⏸ Pause All Chains</button>
          )}
        </div>
      </div>

      {/* Dry Run / Live */}
      <div style={{ background: dryRun ? "#0f172a" : "#1a0a0a", border: `1px solid ${dryRun ? "#1e293b" : "#7f1d1d"}`, borderRadius: 8, padding: 20, marginBottom: 16, transition: "all 0.3s" }}>
        <div style={{ fontSize: 11, color: "#475569", letterSpacing: 1, marginBottom: 12, fontWeight: 700 }}>TRADING MODE</div>
        <div style={{ display: "flex", gap: 12, alignItems: "center", flexWrap: "wrap" }}>
          <div style={{ fontSize: 22, fontWeight: 700, color: dryRun ? "#fbbf24" : "#ef4444" }}>
            {dryRun ? "?? DRY RUN" : "?? LIVE TRADING"}
          </div>
          <span style={{ flex: 1 }} />
          {dryRun ? (
            <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
              {confirmLive && <span style={{ color: "#f87171", fontSize: 12, fontWeight: 600 }}>Are you sure? Real money!</span>}
              <button onClick={handleGoLive} style={{
                ...btn, padding: "10px 24px", fontSize: 14, fontWeight: 700, borderRadius: 6,
                background: confirmLive ? "#7f1d1d" : "#422006", color: confirmLive ? "#fecaca" : "#fbbf24",
                animation: confirmLive ? "pulse 1s infinite" : "none",
              }}>
                {confirmLive ? "⚠ CONFIRM GO LIVE" : "Go Live"}
              </button>
              {confirmLive && <button onClick={() => setConfirmLive(false)} style={{ ...btn, fontSize: 11 }}>Cancel</button>}
            </div>
          ) : (
            <button onClick={handleGoDry} style={{ ...btn, background: "#422006", color: "#fbbf24", padding: "10px 24px", fontSize: 14, fontWeight: 700, borderRadius: 6 }}>Switch to Dry Run</button>
          )}
        </div>
        {!dryRun && (
          <div style={{ marginTop: 12, padding: 10, background: "#450a0a", borderRadius: 6, color: "#fca5a5", fontSize: 11, lineHeight: 1.6 }}>
            ⚠ Live trading is active. Real transactions will be sent. The on-chain contract still has its atomic safety check — unprofitable trades will revert (costing only gas). Monitor closely and pause if anything looks wrong.
          </div>
        )}
      </div>

      {/* Sound */}
      <div style={{ background: "#0f172a", border: "1px solid #1e293b", borderRadius: 8, padding: 16, display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 16 }}>
        <div>
          <div style={{ fontSize: 11, color: "#475569", letterSpacing: 1, fontWeight: 700 }}>SOUND ALERTS</div>
          <div style={{ fontSize: 12, color: "#64748b", marginTop: 2 }}>Beep on trade execution</div>
        </div>
        <button onClick={() => setSoundOn(!soundOn)} style={{ ...btn, background: soundOn ? "#052e16" : "#1e293b", color: soundOn ? "#34d399" : "#475569", padding: "8px 20px" }}>
          {soundOn ? "?? ON" : "?? OFF"}
        </button>
      </div>

      {/* Restart Engine */}
      <div style={{ background: "#0f172a", border: "1px solid #1e293b", borderRadius: 8, padding: 16, display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 16 }}>
        <div>
          <div style={{ fontSize: 11, color: "#475569", letterSpacing: 1, fontWeight: 700 }}>RESTART ENGINE</div>
          <div style={{ fontSize: 12, color: "#64748b", marginTop: 2 }}>Send SIGTERM — systemd/supervisor will restart the process</div>
        </div>
        <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
          {confirmRestart && <span style={{ color: "#fbbf24", fontSize: 12, fontWeight: 600 }}>Confirm restart?</span>}
          <button
            onClick={handleRestart}
            disabled={restarting}
            style={{ ...btn, background: confirmRestart ? "#422006" : "#1e293b", color: confirmRestart ? "#fbbf24" : "#94a3b8", padding: "8px 20px", opacity: restarting ? 0.5 : 1 }}
          >
            {restarting ? "↺ Restarting…" : confirmRestart ? "⚠ Confirm" : "↺ Restart"}
          </button>
          {confirmRestart && <button onClick={() => setConfirmRestart(false)} style={{ ...btn, fontSize: 11 }}>Cancel</button>}
        </div>
      </div>

      {/* Clear Monitor */}
      <div style={{ background: "#0f172a", border: "1px solid #1e293b", borderRadius: 8, padding: 16, display: "flex", justifyContent: "space-between", alignItems: "center" }}>
        <div>
          <div style={{ fontSize: 11, color: "#475569", letterSpacing: 1, fontWeight: 700 }}>MONITOR</div>
          <div style={{ fontSize: 12, color: "#64748b", marginTop: 2 }}>Clear logs + leaderboard display for fresh monitoring (does not affect DB)</div>
        </div>
        <button onClick={clearAll} style={{ ...btn, background: "#1e293b", color: "#94a3b8", padding: "8px 20px" }}>
          ✕ Clear
        </button>
      </div>

      {/* Reset Database */}
      <div style={{ background: "#0f172a", border: "1px solid #450a0a", borderRadius: 8, padding: 16, display: "flex", justifyContent: "space-between", alignItems: "center" }}>
        <div>
          <div style={{ fontSize: 11, color: "#7f1d1d", letterSpacing: 1, fontWeight: 700 }}>RESET DATABASE</div>
          <div style={{ fontSize: 12, color: "#64748b", marginTop: 2 }}>Permanently delete all trade records from the database — irreversible</div>
        </div>
        <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
          <button
            onClick={async () => {
              if (!confirmDbReset) { setConfirmDbReset(true); return; }
              setDbResetting(true);
              setConfirmDbReset(false);
              await api.resetDb();
              clearAll();
              onDbReset?.();
              setDbResetting(false);
            }}
            disabled={dbResetting}
            style={{ ...btn, background: confirmDbReset ? "#450a0a" : "#1e293b", color: confirmDbReset ? "#fca5a5" : "#94a3b8", padding: "8px 20px", opacity: dbResetting ? 0.5 : 1 }}
          >
            {dbResetting ? "Deleting…" : confirmDbReset ? "⚠ Confirm Delete" : "Delete DB"}
          </button>
          {confirmDbReset && <button onClick={() => setConfirmDbReset(false)} style={{ ...btn, fontSize: 11 }}>Cancel</button>}
        </div>
      </div>
    </div>
  );
}

// ─── Pair Manager (replaces TokenManager) ─────────────────
function PairManager({ api }) {
  const chainIds = Object.keys(api.pairScan).map(Number).sort();
  const [selectedChain, setSelectedChain] = useState(null);
  const [toggling, setToggling] = useState({});

  // Auto-select first chain when data arrives
  useEffect(() => {
    if (chainIds.length > 0 && selectedChain === null) setSelectedChain(chainIds[0]);
  }, [chainIds.length]);

  // Poll pair scan every 30s
  useEffect(() => {
    api.fetchPairScan();
    const t = setInterval(() => api.fetchPairScan(), 30000);
    return () => clearInterval(t);
  }, []);

  const activeChain = selectedChain ?? chainIds[0] ?? null;
  const allRows = activeChain != null ? (api.pairScan[activeChain] ?? []) : [];
  const twoHopRows = allRows.filter((r) => !r.display_name.startsWith("▲"));
  const triRows = allRows.filter((r) => r.display_name.startsWith("▲"));
  const enabledCount = twoHopRows.filter((r) => !r.disabled).length;
  const quotedCount = allRows.filter((r) => r.was_quoted).length;

  const handleToggle = async (pair) => {
    setToggling((p) => ({ ...p, [pair.pair_id]: true }));
    await api.togglePair(pair.pair_id);
    await api.fetchPairScan();
    setToggling((p) => ({ ...p, [pair.pair_id]: false }));
  };

  const TH = ({ children }) => (
    <th style={{ textAlign: "left", padding: "7px 8px", color: "#334155", fontSize: 9, letterSpacing: 1.2, textTransform: "uppercase", fontWeight: 700, position: "sticky", top: 0, background: "#020617" }}>{children}</th>
  );

  const renderRows = (rows, isTri) => rows.map((p) => (
    <tr key={p.pair_id} style={{ borderBottom: "1px solid #0a0f1a", opacity: p.disabled ? 0.45 : 1 }}>
      {/* Pair name */}
      <td style={{ padding: "7px 8px", fontWeight: 600, whiteSpace: "nowrap" }}>
        {isTri ? (
          <span style={{ color: p.was_quoted ? "#fb923c" : "#475569" }}>{p.display_name}</span>
        ) : (
          <>
            <span style={{ color: p.disabled ? "#334155" : "#e2e8f0" }}>{p.display_name}</span>
            <span style={{ marginLeft: 6, fontSize: 9, color: "#334155" }}>{p.pair_id.replace(/^[^-]+-/, "")}</span>
          </>
        )}
      </td>
      {/* DEXes active */}
      <td style={{ padding: "7px 8px" }}>
        <div style={{ display: "flex", flexWrap: "wrap", gap: 3 }}>
          {p.dex_ids.length === 0 ? (
            <span style={{ fontSize: 9, color: "#334155" }}>—</span>
          ) : p.dex_ids.map((d) => (
            <span key={d} style={{ fontSize: 9, padding: "2px 6px", borderRadius: 3,
              background: isTri ? "#431407" : "#1e1b4b",
              color: isTri ? "#fb923c" : "#a78bfa",
              border: `1px solid ${isTri ? "#7c2d12" : "#312e81"}` }}>{d}</span>
          ))}
        </div>
      </td>
      {/* Paths / Cross count */}
      <td style={{ padding: "7px 8px", color: p.cross_count >= 1 ? (isTri ? "#fb923c" : "#34d399") : "#475569", fontWeight: p.cross_count >= 1 ? 600 : 400 }}>
        {p.cross_count || "—"}
      </td>
      {/* Was quoted badge */}
      <td style={{ padding: "7px 8px" }}>
        <span style={{ fontSize: 9, padding: "2px 7px", borderRadius: 8,
          background: p.was_quoted ? (isTri ? "#431407" : "#052e16") : "#1e293b",
          color: p.was_quoted ? (isTri ? "#fb923c" : "#34d399") : "#334155",
          border: `1px solid ${p.was_quoted ? (isTri ? "#7c2d12" : "#166534") : "#1e293b"}` }}>
          {p.was_quoted ? "✓" : "—"}
        </span>
      </td>
      {/* Opp count */}
      <td style={{ padding: "7px 8px", color: p.opp_count > 0 ? "#fbbf24" : "#334155", fontWeight: p.opp_count > 0 ? 600 : 400 }}>
        {p.opp_count > 0 ? p.opp_count : "—"}
      </td>
      {/* Enable/disable toggle (2-hop only; triangular is auto-managed by pool cache) */}
      <td style={{ padding: "7px 8px" }}>
        {isTri ? (
          <span style={{ fontSize: 9, color: "#334155" }}>auto</span>
        ) : (
          <button
            onClick={() => handleToggle(p)}
            disabled={toggling[p.pair_id]}
            style={{ ...btn, fontSize: 9, padding: "3px 10px",
              background: p.disabled ? "#1e293b" : "#052e16",
              color: p.disabled ? "#475569" : "#34d399",
              border: `1px solid ${p.disabled ? "#1e293b" : "#166534"}`,
              opacity: toggling[p.pair_id] ? 0.5 : 1 }}
          >
            {p.disabled ? "Disabled" : "Enabled"}
          </button>
        )}
      </td>
    </tr>
  ));

  return (
    <div style={{ padding: 16, display: "flex", flexDirection: "column", gap: 12 }}>
      {/* Chain tabs */}
      <div style={{ display: "flex", gap: 6, alignItems: "center", flexWrap: "wrap" }}>
        {chainIds.map((cid) => (
          <button
            key={cid}
            onClick={() => setSelectedChain(cid)}
            style={{ ...btn, padding: "5px 14px", fontSize: 11,
              background: activeChain === cid ? "#1e1b4b" : "#0f172a",
              color: activeChain === cid ? "#a78bfa" : "#475569",
              border: `1px solid ${activeChain === cid ? "#4c1d95" : "#1e293b"}` }}
          >
            {chainName(cid)}
          </button>
        ))}
        <span style={{ flex: 1 }} />
        {activeChain != null && (
          <span style={{ fontSize: 11, color: "#475569" }}>
            {enabledCount}/{twoHopRows.length} pairs enabled · {triRows.length} triplets · {quotedCount} quoted last scan
          </span>
        )}
        <button onClick={() => api.fetchPairScan()} style={{ ...btn, fontSize: 10, padding: "4px 10px", color: "#a78bfa" }}>↺ Refresh</button>
      </div>

      {/* Pair table */}
      {activeChain == null ? (
        <div style={{ color: "#334155", fontSize: 12, padding: 24, textAlign: "center" }}>
          No pair scan data yet — data appears after the first heartbeat (~60s after engine start)
        </div>
      ) : (
        <div style={{ overflow: "auto" }}>
          <table style={{ width: "100%", borderCollapse: "collapse", fontSize: 12 }}>
            <thead>
              <tr style={{ borderBottom: "1px solid #1e293b" }}>
                <TH>Pair</TH><TH>DEXes active</TH><TH>Cross/Paths</TH><TH>Quoted</TH><TH>Opps</TH><TH>Scan</TH>
              </tr>
            </thead>
            <tbody>
              {twoHopRows.length === 0 && triRows.length === 0 ? (
                <tr><td colSpan={6} style={{ padding: "20px 8px", color: "#334155", fontSize: 11, textAlign: "center" }}>No pairs — engine may still be starting up</td></tr>
              ) : (
                <>
                  {renderRows(twoHopRows, false)}
                  {triRows.length > 0 && (
                    <>
                      <tr>
                        <td colSpan={6} style={{ padding: "6px 8px 3px 8px", fontSize: 9, color: "#475569", letterSpacing: 1.2, fontWeight: 700, borderTop: "1px solid #1e293b", background: "#020617" }}>
                          TRIANGULAR ({triRows.length})
                        </td>
                      </tr>
                      {renderRows(triRows, true)}
                    </>
                  )}
                </>
              )}
            </tbody>
          </table>
        </div>
      )}

      <div style={{ fontSize: 10, color: "#334155", marginTop: 4 }}>
        Scan data updates every heartbeat (~60s). Toggle disables a pair or triplet from future scans — takes effect on the next cycle.
      </div>
    </div>
  );
}

// ═══════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════
export default function Dashboard() {
  const [url, setUrl] = useState(() => localStorage.getItem("ap_url") || DEFAULT_WS);
  const [apiKey, setApiKey] = useState(() => localStorage.getItem("ap_key") || "");
  const [authenticated, setAuthenticated] = useState(
    () => !!(localStorage.getItem("ap_url") && localStorage.getItem("ap_key"))
  );
  useEffect(() => { localStorage.setItem("ap_url", url); }, [url]);
  useEffect(() => { localStorage.setItem("ap_key", apiKey); }, [apiKey]);
  const apiUrl = url.replace("ws://", "http://").replace("wss://", "https://").replace("/ws", "");
  const ws = useWebSocket(url, apiKey);
  const api = useApi(apiUrl, apiKey);
  const [tab, setTab] = useState("monitor");
  const [enabledTypes, setEnabledTypes] = useState(() => new Set(["trade", "opportunity", "errors", "info", "heartbeat"]));

  // Poll /stats via HTTP every 3s to keep pause/dry-run state accurate.
  // Measure API latency from each poll.
  const [apiStats, setApiStats] = useState(null);
  const [apiLatency, setApiLatency] = useState(null);
  useEffect(() => {
    if (!authenticated) return;
    const poll = async () => {
      const t0 = Date.now();
      const d = await api.fetchStats();
      if (d && !d._authError) {
        setApiLatency(Date.now() - t0);
        setApiStats(d);
      }
    };
    poll();
    const iv = setInterval(poll, 3000);
    return () => clearInterval(iv);
  }, [authenticated]);

  // Poll /stats/pairs every 5s for the pair leaderboard.
  // pairStatsCleared: when true, show empty until user clicks Reload.
  // The poll always updates pairStatsLive so data is ready the moment they reload.
  const [pairStatsLive, setPairStatsLive] = useState([]);
  const [pairStatsCleared, setPairStatsCleared] = useState(false);
  const pairStats = pairStatsCleared ? [] : pairStatsLive;
  useEffect(() => {
    if (!authenticated) return;
    const headers = apiKey ? { Authorization: `Bearer ${apiKey}` } : {};
    const poll = async () => {
      try {
        const r = await fetch(`${apiUrl}/stats/pairs`, { headers });
        if (r.ok) { const d = await r.json(); setPairStatsLive(d.pairs || []); }
      } catch {}
    };
    poll();
    const iv = setInterval(poll, 5000);
    return () => clearInterval(iv);
  }, [authenticated, apiUrl, apiKey]);

  // Clear all monitoring state for fresh tracking (logs + leaderboard display).
  // apiStats is intentionally kept live — it holds engine state (pause/dry_run/base_fee).
  const clearAll = () => {
    ws.clearLogs();
    setPairStatsCleared(true);
  };

  useEffect(() => { if (ws.status === "connected") setAuthenticated(true); }, [ws.status]);

  const handleConnect = () => setAuthenticated(true);
  const handleLogout = () => { ws.disconnect(); setAuthenticated(false); };

  if (!authenticated || ws.authError) {
    return (
      <>
        <style>{`@import url('https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@300;400;500;600;700&display=swap');*{box-sizing:border-box;margin:0;padding:0}button:hover{filter:brightness(1.15)}input:focus{outline:none;border-color:#a78bfa}`}</style>
        <LoginScreen engineUrl={url} setEngineUrl={setUrl} apiKey={apiKey} setApiKey={setApiKey} onConnect={handleConnect} error={ws.authError} />
      </>
    );
  }

  return (
    <div style={{ minHeight: "100vh", background: "#020617", color: "#e2e8f0", fontFamily: "'JetBrains Mono','Fira Code','SF Mono',monospace", fontSize: 13, display: "flex", flexDirection: "column" }}>
      <style>{`
        @import url('https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@300;400;500;600;700&display=swap');
        *{box-sizing:border-box;margin:0;padding:0}
        ::-webkit-scrollbar{width:5px}::-webkit-scrollbar-track{background:#020617}::-webkit-scrollbar-thumb{background:#1e293b;border-radius:3px}
        @keyframes pulse{0%,100%{opacity:1}50%{opacity:.4}}
        button:hover{filter:brightness(1.15)} input:focus,select:focus{outline:none;border-color:#a78bfa}
      `}</style>

      <Header tab={tab} setTab={setTab} status={ws.status} engineUrl={url} apiKey={apiKey} onLogout={handleLogout} />
      <StatsRow stats={ws.stats} logs={ws.logs} engineState={ws.engineState} apiStats={apiStats} />
      <GasLatencyBar apiStats={apiStats} apiLatency={apiLatency} />

      {tab === "monitor" && (
        <div style={{ flex: 1, display: "flex", flexDirection: "column", overflow: "hidden" }}>
          <ChainCards apiStats={apiStats} />

          {/* Row 1: 24h PnL chart + Opportunities */}
          <div style={{ padding: "0 14px 10px 14px", display: "grid", gridTemplateColumns: "1fr 1fr", gap: 10 }}>
            <ProfitChart fetchTrades={api.fetchTrades} wsLogs={ws.logs} />
            <div style={{ background: "#0f172a", borderRadius: 8, border: "1px solid #1e293b", overflow: "hidden" }}>
              <div style={{ padding: "10px 14px", borderBottom: "1px solid #1e293b" }}>
                <span style={{ fontSize: 9, color: "#475569", letterSpacing: 1.5, fontWeight: 700 }}>OPPORTUNITIES</span>
              </div>
              <OpportunityTable logs={ws.logs} />
            </div>
          </div>

          {/* Row 2: Pair leaderboard (full width) */}
          <div style={{ padding: "0 14px 10px 14px" }}>
            <PairLeaderboard pairStats={pairStats} wsLogs={ws.logs} onReload={() => setPairStatsCleared(false)} />
          </div>

          {/* Row 3: Live feed */}
          <div style={{ flex: 1, minHeight: 200, margin: "0 14px 14px 14px", background: "#020617", borderRadius: 8, border: "1px solid #1e293b", overflow: "hidden", display: "flex", flexDirection: "column" }}>
            <div style={{ padding: "8px 14px", borderBottom: "1px solid #1e293b", display: "flex", justifyContent: "space-between", flexShrink: 0 }}>
              <span style={{ fontSize: 9, color: "#475569", letterSpacing: 1.5, fontWeight: 700 }}>LIVE FEED</span>
              <button onClick={ws.clearLogs} style={{ ...btn, fontSize: 9, padding: "2px 8px" }}>Clear</button>
            </div>
            <div style={{ flex: 1, minHeight: 0 }}>
              <LogFeed logs={ws.logs} enabledTypes={enabledTypes} setEnabledTypes={setEnabledTypes} />
            </div>
          </div>
        </div>
      )}

      {tab === "tokens" && <div style={{ flex: 1, overflow: "auto" }}><PairManager api={api} /></div>}
      {tab === "controls" && <div style={{ flex: 1, overflow: "auto" }}><ControlPanel ws={ws} api={api} apiStats={apiStats} clearAll={clearAll} onDbReset={() => setApiStats(prev => prev ? { ...prev, chains: prev.chains?.map(c => ({ ...c, total_attempts: 0, total_success: 0, total_failed: 0, total_profit_usd: 0, ghost_profit_usd: 0, total_scans: 0 })) } : null)} /></div>}
    </div>
  );
}
