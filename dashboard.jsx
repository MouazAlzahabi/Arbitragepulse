import { useState, useEffect, useRef, useCallback, useMemo } from "react";
import { AreaChart, Area, XAxis, YAxis, Tooltip, ResponsiveContainer } from "recharts";

// ═══════════════════════════════════════════════════════════
// CONFIG
// ═══════════════════════════════════════════════════════════
const DEFAULT_WS = "ws://localhost:3000/ws";
const DEFAULT_API = "http://localhost:3000";

// ═══════════════════════════════════════════════════════════
// HOOKS
// ═══════════════════════════════════════════════════════════
function useWebSocket(url, apiKey) {
  const [status, setStatus] = useState("disconnected");
  const [logs, setLogs] = useState([]);
  const [stats, setStats] = useState(null);
  const [engineState, setEngineState] = useState(null);
  const [authError, setAuthError] = useState(false);
  const wsRef = useRef(null);
  const reconnRef = useRef(null);
  const attempts = useRef(0);

  const connect = useCallback(() => {
    if (!url) return;
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
          setLogs((p) => [...p.slice(-500), { ...d, _id: Date.now() + Math.random() }]);
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
    clearLogs: () => setLogs([]),
    disconnect: () => { clearTimeout(reconnRef.current); attempts.current = 999; wsRef.current?.close(); setStatus("disconnected"); },
    reconnect: () => { attempts.current = 0; connect(); },
    refreshState: () => send({ command: "state" }),
  };
}

function useApi(baseUrl, apiKey) {
  const [tokens, setTokens] = useState([]);
  const [pairs, setPairs] = useState([]);
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
    tokens, pairs, loading,
    fetchTokens: async (cid) => { setLoading(true); const d = await f(`/tokens${cid ? `?chain_id=${cid}` : ""}`); setTokens(d?.tokens || []); setLoading(false); },
    fetchPairs: async (cid) => { const d = await f(`/tokens/pairs${cid ? `?chain_id=${cid}` : ""}`); setPairs(d?.pairs || []); },
    toggleTrust: async (cid, addr, trusted) => { await f(`/tokens/${cid}/${addr}`, { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ trusted }) }); },
    addToken: async (t) => { const r = await fetch(`${baseUrl}/tokens`, { method: "POST", headers, body: JSON.stringify(t) }); return r?.ok; },
    removeToken: async (cid, addr) => { await f(`/tokens/${cid}/${addr}`, { method: "DELETE" }); },
    pauseEngine: (cid) => f("/engine/pause", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(cid ? { chain_id: cid } : {}) }),
    resumeEngine: (cid) => f("/engine/resume", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(cid ? { chain_id: cid } : {}) }),
    setDryRun: (enabled) => f("/engine/dry-run", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ enabled }) }),
    fetchState: () => f("/engine/state"),
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
        <div style={{ marginTop: 24, fontSize: 10, color: "#1e293b", textAlign: "center", lineHeight: 1.8 }}>Engine not running?<br /><span style={{ color: "#334155" }}>bun run start</span> in your engine directory</div>
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
const TYPE = { info: ["#94a3b8", "INFO"], debug: ["#475569", "DBUG"], warn: ["#fbbf24", "WARN"], error: ["#f87171", "ERR!"], trade: ["#34d399", "TRDE"], opportunity: ["#a78bfa", "OPP!"] };
const CAT_C = { stable: "#22d3ee", blue_chip: "#a78bfa", defi: "#34d399", meme: "#fbbf24", other: "#64748b" };
const btn = { background: "#1e293b", border: "none", borderRadius: 4, padding: "6px 14px", color: "#94a3b8", cursor: "pointer", fontFamily: "'JetBrains Mono', monospace", fontSize: 12 };
const input = { background: "#020617", border: "1px solid #1e293b", borderRadius: 4, padding: "6px 10px", color: "#e2e8f0", fontFamily: "'JetBrains Mono', monospace", fontSize: 12, width: "100%" };

// ═══════════════════════════════════════════════════════════
// COMPONENTS
// ═══════════════════════════════════════════════════════════
function Dot({ status }) {
  const c = { connected: "#34d399", connecting: "#fbbf24", disconnected: "#f87171" };
  return <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: "50%", background: c[status] || "#475569", boxShadow: status === "connected" ? `0 0 8px ${c.connected}` : "none", animation: status === "connecting" ? "pulse 1.5s infinite" : "none" }} />;
}

function Header({ tab, setTab, status, engineUrl, apiKey, onDisconnect, onLogout }) {
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

function StatsRow({ stats, logs, engineState }) {
  const trades = logs.filter((l) => l.type === "trade").length;
  const opps = logs.filter((l) => l.type === "opportunity").length;
  const errors = logs.filter((l) => l.type === "error").length;
  const uptime = stats?.uptime || stats?.startedAt ? formatUptime(Date.now() - (stats?.startedAt || Date.now())) : "—";
  const dryRun = engineState?.dryRun ?? stats?.dryRun ?? true;
  const paused = engineState?.paused ?? stats?.paused ?? false;

  const items = [
    { l: "UPTIME", v: uptime, c: "#94a3b8" },
    { l: "CHAINS", v: stats?.chains?.length || "—", c: "#a78bfa" },
    { l: "OPPORTUNITIES", v: opps, c: "#818cf8" },
    { l: "TRADES", v: trades, c: "#34d399" },
    { l: "ERRORS", v: errors, c: errors > 0 ? "#f87171" : "#334155" },
    { l: "MODE", v: paused ? "⏸ PAUSED" : dryRun ? "?? DRY RUN" : "?? LIVE", c: paused ? "#f87171" : dryRun ? "#fbbf24" : "#ef4444" },
  ];

  return (
    <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(130px, 1fr))", gap: 1, background: "#1e293b" }}>
      {items.map((i) => (
        <div key={i.l} style={{ background: "#0f172a", padding: "12px 14px" }}>
          <div style={{ fontSize: 9, color: "#475569", letterSpacing: 1.5, fontWeight: 700, marginBottom: 3 }}>{i.l}</div>
          <div style={{ fontSize: 18, color: i.c, fontWeight: 700, fontFamily: "inherit" }}>{i.v}</div>
        </div>
      ))}
    </div>
  );
}

function ChainCards({ stats }) {
  const chains = (stats?.chains || []).map((n) => ({ name: n, ...stats[n] })).filter((c) => c.name && c.swapsDetected !== undefined);
  if (chains.length === 0) return null;
  return (
    <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(240px, 1fr))", gap: 10, padding: "10px 14px" }}>
      {chains.map((c) => (
        <div key={c.name} style={{ background: "linear-gradient(135deg, #0f172a, #1e1b4b)", border: "1px solid #1e293b", borderRadius: 8, padding: 14 }}>
          <div style={{ display: "flex", justifyContent: "space-between", marginBottom: 10 }}>
            <span style={{ fontWeight: 700, fontSize: 13 }}>{c.name}</span>
            <span style={{ fontSize: 9, color: "#34d399", background: "#052e16", padding: "2px 8px", borderRadius: 10 }}>ACTIVE</span>
          </div>
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 6, fontSize: 11 }}>
            <div><span style={{ color: "#475569" }}>Swaps </span><span style={{ color: "#94a3b8" }}>{c.swapsDetected}</span></div>
            <div><span style={{ color: "#475569" }}>Opps </span><span style={{ color: "#a78bfa" }}>{c.opportunitiesFound}</span></div>
            <div><span style={{ color: "#475569" }}>OK </span><span style={{ color: "#34d399" }}>{c.totalSuccess || 0}</span></div>
            <div><span style={{ color: "#475569" }}>Fail </span><span style={{ color: (c.totalFailed || 0) > 0 ? "#f87171" : "#334155" }}>{c.totalFailed || 0}</span></div>
          </div>
        </div>
      ))}
    </div>
  );
}

function ProfitChart({ logs }) {
  const data = useMemo(() => {
    let cum = 0;
    const pts = [{ time: "start", profit: 0 }];
    logs.filter((l) => l.type === "trade" && l.data?.profit).forEach((t) => {
      cum += parseFloat(String(t.data.profit).replace("$", "") || "0");
      pts.push({ time: ts(t.timestamp), profit: parseFloat(cum.toFixed(4)) });
    });
    return pts;
  }, [logs]);

  const total = data.length > 1 ? data[data.length - 1].profit : 0;

  return (
    <div style={{ background: "#0f172a", borderRadius: 8, border: "1px solid #1e293b", overflow: "hidden" }}>
      <div style={{ display: "flex", justifyContent: "space-between", padding: "10px 14px", borderBottom: "1px solid #1e293b" }}>
        <span style={{ fontSize: 9, color: "#475569", letterSpacing: 1.5, fontWeight: 700 }}>CUMULATIVE P&L</span>
        <span style={{ fontSize: 14, color: total >= 0 ? "#34d399" : "#f87171", fontWeight: 700 }}>${total.toFixed(4)}</span>
      </div>
      <div style={{ height: 180, padding: "4px 0" }}>
        {data.length < 2 ? (
          <div style={{ display: "flex", alignItems: "center", justifyContent: "center", height: "100%", color: "#1e293b", fontSize: 12, fontStyle: "italic" }}>Awaiting trade data…</div>
        ) : (
          <ResponsiveContainer width="100%" height="100%">
            <AreaChart data={data} margin={{ top: 8, right: 8, left: 0, bottom: 0 }}>
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
          {["Time", "Pair", "Buy On", "Sell On", "Profit", "Gas", ""].map((h) => (
            <th key={h} style={{ textAlign: "left", padding: "6px 8px", color: "#334155", fontSize: 9, letterSpacing: 1.2, textTransform: "uppercase", fontWeight: 700, position: "sticky", top: 0, background: "#0f172a" }}>{h}</th>
          ))}
        </tr></thead>
        <tbody>
          {opps.map((o) => (
            <tr key={o._id} style={{ borderBottom: "1px solid #0a0f1a" }}>
              <td style={{ padding: "5px 8px", color: "#475569" }}>{ts(o.timestamp)}</td>
              <td style={{ padding: "5px 8px", color: "#e2e8f0", fontWeight: 600 }}>{o.data.pair || "—"}</td>
              <td style={{ padding: "5px 8px", color: "#22d3ee" }}>{o.data.buyOn || "—"}</td>
              <td style={{ padding: "5px 8px", color: "#fb923c" }}>{o.data.sellOn || "—"}</td>
              <td style={{ padding: "5px 8px", color: "#34d399", fontWeight: 600 }}>{o.data.profitUsd || o.data.profit || "—"}</td>
              <td style={{ padding: "5px 8px", color: "#475569" }}>{o.data.gasCostUsd || "—"}</td>
              <td style={{ padding: "5px 8px" }}>
                {o.data.netProfitable === true ? <span style={{ color: "#34d399", fontSize: 9 }}>✓ NET</span> : <span style={{ color: "#475569", fontSize: 9 }}>—</span>}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function LogFeed({ logs, filter, setFilter }) {
  const ref = useRef(null);
  const [auto, setAuto] = useState(true);
  useEffect(() => { if (auto && ref.current) ref.current.scrollTop = ref.current.scrollHeight; }, [logs, auto]);

  const filtered = filter === "all" ? logs : logs.filter((l) => l.type === filter);
  const filters = ["all", "trade", "opportunity", "error", "warn", "info"];

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div style={{ display: "flex", gap: 2, padding: "6px 10px", background: "#0f172a", borderBottom: "1px solid #1e293b", flexShrink: 0, flexWrap: "wrap" }}>
        {filters.map((f) => (
          <button key={f} onClick={() => setFilter(f)} style={{ ...btn, fontSize: 9, padding: "2px 8px", letterSpacing: 1, textTransform: "uppercase", background: filter === f ? "#1e293b" : "transparent", color: filter === f ? "#e2e8f0" : "#475569", borderBottom: filter === f ? "2px solid #a78bfa" : "2px solid transparent" }}>{f}</button>
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
        {filtered.length === 0 && <div style={{ color: "#1e293b", fontStyle: "italic", paddingTop: 16, textAlign: "center" }}>{logs.length === 0 ? "Connecting…" : `No ${filter} logs`}</div>}
        {filtered.map((l) => {
          const [clr, badge] = TYPE[l.type] || TYPE.info;
          return (
            <div key={l._id} style={{ display: "flex", gap: 6, color: "#64748b" }}>
              <span style={{ color: "#1e293b", flexShrink: 0 }}>{ts(l.timestamp)}</span>
              <span style={{ color: clr, fontWeight: 700, flexShrink: 0, width: 34, textAlign: "center", background: `${clr}12`, borderRadius: 2, fontSize: 10 }}>{badge}</span>
              <span style={{ color: clr === "#475569" ? "#475569" : "#b0bec5" }}>{l.message}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function ControlPanel({ ws, api, engineState }) {
  const [confirmLive, setConfirmLive] = useState(false);
  const dryRun = engineState?.dryRun ?? true;
  const paused = engineState?.paused ?? false;
  const [soundOn, setSoundOn] = useState(true);

  // Sound on trade
  useEffect(() => {
    if (!soundOn) return;
    const last = ws.logs[ws.logs.length - 1];
    if (last?.type === "trade") playBeep();
  }, [ws.logs.length, soundOn]);

  const handleGoLive = async () => {
    if (!confirmLive) { setConfirmLive(true); return; }
    ws.send({ command: "set_dry_run", enabled: false });
    setConfirmLive(false);
    setTimeout(() => ws.refreshState(), 500);
  };

  const handleGoDry = () => {
    ws.send({ command: "set_dry_run", enabled: true });
    setConfirmLive(false);
    setTimeout(() => ws.refreshState(), 500);
  };

  const handlePause = () => { ws.send({ command: "pause" }); setTimeout(() => ws.refreshState(), 500); };
  const handleResume = () => { ws.send({ command: "resume" }); setTimeout(() => ws.refreshState(), 500); };

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
      <div style={{ background: "#0f172a", border: "1px solid #1e293b", borderRadius: 8, padding: 16, display: "flex", justifyContent: "space-between", alignItems: "center" }}>
        <div>
          <div style={{ fontSize: 11, color: "#475569", letterSpacing: 1, fontWeight: 700 }}>SOUND ALERTS</div>
          <div style={{ fontSize: 12, color: "#64748b", marginTop: 2 }}>Beep on trade execution</div>
        </div>
        <button onClick={() => setSoundOn(!soundOn)} style={{ ...btn, background: soundOn ? "#052e16" : "#1e293b", color: soundOn ? "#34d399" : "#475569", padding: "8px 20px" }}>
          {soundOn ? "?? ON" : "?? OFF"}
        </button>
      </div>
    </div>
  );
}

function TokenManager({ api }) {
  const [chainFilter, setChainFilter] = useState("");
  const [showAdd, setShowAdd] = useState(false);
  const [newTk, setNew] = useState({ symbol: "", address: "", decimals: 18, chain_id: 10, category: "other" });

  useEffect(() => { api.fetchTokens(chainFilter || undefined); }, [chainFilter]);

  const handleAdd = async () => {
    if (await api.addToken(newTk)) { setShowAdd(false); setNew({ symbol: "", address: "", decimals: 18, chain_id: 10, category: "other" }); api.fetchTokens(chainFilter || undefined); }
  };

  const handleToggle = async (t) => { await api.toggleTrust(t.chainId, t.address, !t.trusted); api.fetchTokens(chainFilter || undefined); };
  const handleRemove = async (t) => { await api.removeToken(t.chainId, t.address); api.fetchTokens(chainFilter || undefined); };

  const chainIds = [...new Set(api.tokens.map((t) => t.chainId))];

  return (
    <div style={{ padding: 16 }}>
      <div style={{ display: "flex", gap: 8, marginBottom: 12, alignItems: "center", flexWrap: "wrap" }}>
        <select value={chainFilter} onChange={(e) => setChainFilter(e.target.value)} style={{ ...input, width: "auto" }}>
          <option value="">All Chains</option>
          {chainIds.map((id) => <option key={id} value={id}>Chain {id}</option>)}
        </select>
        <button onClick={() => api.fetchPairs(chainFilter || undefined)} style={{ ...btn, background: "#1e1b4b", color: "#a78bfa" }}>Preview Pairs ({api.pairs.length})</button>
        <span style={{ flex: 1 }} />
        <span style={{ fontSize: 11, color: "#475569" }}>{api.tokens.filter((t) => t.trusted).length} trusted / {api.tokens.length} total</span>
        <button onClick={() => setShowAdd(!showAdd)} style={{ ...btn, background: "#052e16", color: "#34d399" }}>+ Add Token</button>
      </div>

      {showAdd && (
        <div style={{ background: "#0f172a", border: "1px solid #1e293b", borderRadius: 8, padding: 14, marginBottom: 12, display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 8 }}>
          <input placeholder="Symbol" value={newTk.symbol} onChange={(e) => setNew({ ...newTk, symbol: e.target.value })} style={input} />
          <input placeholder="0x..." value={newTk.address} onChange={(e) => setNew({ ...newTk, address: e.target.value })} style={{ ...input, gridColumn: "span 2" }} />
          <input type="number" placeholder="Decimals" value={newTk.decimals} onChange={(e) => setNew({ ...newTk, decimals: +e.target.value })} style={input} />
          <input type="number" placeholder="Chain ID" value={newTk.chain_id} onChange={(e) => setNew({ ...newTk, chain_id: +e.target.value })} style={input} />
          <select value={newTk.category} onChange={(e) => setNew({ ...newTk, category: e.target.value })} style={input}>
            {["stable", "blue_chip", "defi", "meme", "other"].map((c) => <option key={c} value={c}>{c}</option>)}
          </select>
          <div style={{ gridColumn: "span 3", display: "flex", gap: 8, justifyContent: "flex-end" }}>
            <button onClick={() => setShowAdd(false)} style={btn}>Cancel</button>
            <button onClick={handleAdd} style={{ ...btn, background: "#052e16", color: "#34d399" }}>Add (untrusted)</button>
          </div>
        </div>
      )}

      <div style={{ overflow: "auto", maxHeight: 400 }}>
        <table style={{ width: "100%", borderCollapse: "collapse", fontSize: 12 }}>
          <thead><tr style={{ borderBottom: "1px solid #1e293b" }}>
            {["Symbol", "Chain", "Category", "Status", "Address", ""].map((h) => (
              <th key={h} style={{ textAlign: "left", padding: "7px 8px", color: "#334155", fontSize: 9, letterSpacing: 1.2, textTransform: "uppercase", fontWeight: 700, position: "sticky", top: 0, background: "#020617" }}>{h}</th>
            ))}
          </tr></thead>
          <tbody>{api.tokens.map((t) => (
            <tr key={`${t.chainId}-${t.address}`} style={{ borderBottom: "1px solid #0a0f1a" }}>
              <td style={{ padding: "7px 8px", fontWeight: 600 }}>{t.symbol}</td>
              <td style={{ padding: "7px 8px", color: "#475569" }}>{t.chainId}</td>
              <td style={{ padding: "7px 8px" }}><span style={{ fontSize: 10, padding: "2px 8px", borderRadius: 10, background: `${CAT_C[t.category] || CAT_C.other}18`, color: CAT_C[t.category] || CAT_C.other }}>{t.category}</span></td>
              <td style={{ padding: "7px 8px" }}>
                <button onClick={() => handleToggle(t)} style={{ ...btn, fontSize: 10, padding: "3px 10px", background: t.trusted ? "#052e16" : "#1e293b", color: t.trusted ? "#34d399" : "#475569", border: `1px solid ${t.trusted ? "#166534" : "#1e293b"}` }}>
                  {t.trusted ? "✓ TRUSTED" : "UNTRUSTED"}
                </button>
              </td>
              <td style={{ padding: "7px 8px", color: "#1e293b", fontSize: 10 }}>{t.address.slice(0, 8)}…{t.address.slice(-4)}</td>
              <td style={{ padding: "7px 8px" }}><button onClick={() => handleRemove(t)} style={{ ...btn, fontSize: 10, color: "#334155", padding: "2px 6px" }}>✕</button></td>
            </tr>
          ))}</tbody>
        </table>
      </div>

      {api.pairs.length > 0 && (
        <div style={{ marginTop: 12, background: "#0f172a", border: "1px solid #1e293b", borderRadius: 8, padding: 12, maxHeight: 180, overflow: "auto" }}>
          <div style={{ fontSize: 9, color: "#475569", marginBottom: 6, letterSpacing: 1, fontWeight: 700 }}>AUTO-GENERATED PAIRS ({api.pairs.length})</div>
          <div style={{ display: "flex", flexWrap: "wrap", gap: 4 }}>
            {api.pairs.map((p) => (
              <span key={p.id} style={{ fontSize: 10, padding: "3px 8px", borderRadius: 3, background: "#1e1b4b", color: "#a78bfa", border: "1px solid #312e81" }}>{p.path}</span>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

// ═══════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════
export default function Dashboard() {
  const [url, setUrl] = useState(DEFAULT_WS);
  const [apiKey, setApiKey] = useState("");
  const [authenticated, setAuthenticated] = useState(false);
  const apiUrl = url.replace("ws://", "http://").replace("wss://", "https://").replace("/ws", "");
  const ws = useWebSocket(url, apiKey);
  const api = useApi(apiUrl, apiKey);
  const [tab, setTab] = useState("monitor");
  const [logFilter, setLogFilter] = useState("all");

  // Poll engine state every 5s when connected
  useEffect(() => {
    if (ws.status !== "connected") return;
    const iv = setInterval(() => ws.refreshState(), 5000);
    return () => clearInterval(iv);
  }, [ws.status]);

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

      <Header tab={tab} setTab={setTab} status={ws.status} engineUrl={url} apiKey={apiKey} onDisconnect={ws.disconnect} onLogout={handleLogout} />
      <StatsRow stats={ws.stats} logs={ws.logs} engineState={ws.engineState} />

      {tab === "monitor" && (
        <div style={{ flex: 1, display: "flex", flexDirection: "column", overflow: "hidden" }}>
          <ChainCards stats={ws.stats} />
          <div style={{ padding: "0 14px 10px 14px", display: "grid", gridTemplateColumns: "1fr 1fr", gap: 10 }}>
            <ProfitChart logs={ws.logs} />
            <div style={{ background: "#0f172a", borderRadius: 8, border: "1px solid #1e293b", overflow: "hidden" }}>
              <div style={{ padding: "10px 14px", borderBottom: "1px solid #1e293b" }}>
                <span style={{ fontSize: 9, color: "#475569", letterSpacing: 1.5, fontWeight: 700 }}>OPPORTUNITIES</span>
              </div>
              <OpportunityTable logs={ws.logs} />
            </div>
          </div>
          <div style={{ flex: 1, minHeight: 200, margin: "0 14px 14px 14px", background: "#020617", borderRadius: 8, border: "1px solid #1e293b", overflow: "hidden", display: "flex", flexDirection: "column" }}>
            <div style={{ padding: "8px 14px", borderBottom: "1px solid #1e293b", display: "flex", justifyContent: "space-between", flexShrink: 0 }}>
              <span style={{ fontSize: 9, color: "#475569", letterSpacing: 1.5, fontWeight: 700 }}>LIVE FEED</span>
              <button onClick={ws.clearLogs} style={{ ...btn, fontSize: 9, padding: "2px 8px" }}>Clear</button>
            </div>
            <div style={{ flex: 1, minHeight: 0 }}>
              <LogFeed logs={ws.logs} filter={logFilter} setFilter={setLogFilter} />
            </div>
          </div>
        </div>
      )}

      {tab === "tokens" && <div style={{ flex: 1, overflow: "auto" }}><TokenManager api={api} /></div>}
      {tab === "controls" && <div style={{ flex: 1, overflow: "auto" }}><ControlPanel ws={ws} api={api} engineState={ws.engineState} /></div>}
    </div>
  );
}
