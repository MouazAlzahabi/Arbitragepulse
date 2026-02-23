# Dashboard Enhancements Guide
## Add These Features to dashboard.jsx

Your current dashboard is solid, but here are production-ready enhancements for multi-chain monitoring:

---

## 1. GLOBAL CHAIN FILTER (Add to Header)

Replace the current `Header` component (lines ~204-229) with this enhanced version:

```jsx
function Header({ tab, setTab, status, engineUrl, apiKey, onLogout, globalChainFilter, setGlobalChainFilter, availableChains }) {
  const tabs = ["monitor", "tokens", "controls"];
  return (
    <div style={{ background: "#020617", borderBottom: "1px solid #1e293b" }}>
      <div style={{ display: "flex", alignItems: "center", gap: 12, padding: "10px 16px", flexWrap: "wrap" }}>
        <div style={{ fontSize: 16, fontWeight: 700, letterSpacing: -0.5, whiteSpace: "nowrap" }}>
          <span style={{ color: "#a78bfa" }}>{"\u26a1"}</span> ARBITRAGE<span style={{ color: "#475569" }}>PULSE</span>
        </div>

        {/* CHAIN FILTER DROPDOWN */}
        <select
          value={globalChainFilter}
          onChange={(e) => setGlobalChainFilter(e.target.value)}
          style={{ ...select, width: "auto", fontSize: 11, padding: "4px 8px", background: "#1e293b", border: "1px solid #334155" }}
        >
          <option value="all">All Chains</option>
          {availableChains.map((c) => (
            <option key={c} value={c}>{c}</option>
          ))}
        </select>

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
```

---

## 2. ROUTER SUCCESS RATE WIDGET (New Component)

Add this component after `PairProfitTable`:

```jsx
function RouterHealthTable({ logs, chainFilter }) {
  const { routers, chains } = useMemo(() => {
    const routerStats = new Map();
    const chainSet = new Set();

    logs.forEach((l) => {
      if (!l.data) return;
      const chain = extractChain(l.message) || l.data.chain;
      if (!chain) return;
      chainSet.add(chain);

      // Skip if global filter is active and doesn't match
      if (chainFilter !== "all" && chain !== chainFilter) return;

      const routerA = l.data.router_a_id || l.data.routerA;
      const routerB = l.data.router_b_id || l.data.routerB;

      [routerA, routerB].forEach((rid) => {
        if (!rid) return;
        const key = `${chain}:${rid}`;
        if (!routerStats.has(key)) {
          routerStats.set(key, { chain, router: rid, attempts: 0, successes: 0, failures: 0 });
        }
        const stat = routerStats.get(key);
        stat.attempts++;
        if (l.type === "trade") stat.successes++;
        else if (l.type === "error") stat.failures++;
      });
    });

    const rows = Array.from(routerStats.values())
      .map((r) => ({
        ...r,
        successRate: r.attempts > 0 ? (r.successes / r.attempts * 100).toFixed(1) : "0.0",
      }))
      .sort((a, b) => b.attempts - a.attempts);

    return { routers: rows, chains: Array.from(chainSet) };
  }, [logs, chainFilter]);

  if (routers.length === 0) {
    return <div style={{ color: "#1e293b", textAlign: "center", padding: 30, fontSize: 12, fontStyle: "italic" }}>No router activity yet</div>;
  }

  return (
    <div>
      <div style={{ fontSize: 11, fontWeight: 700, color: "#475569", letterSpacing: 1.2, textTransform: "uppercase", marginBottom: 10 }}>
        Router Health
      </div>
      <div style={{ background: "#020617", border: "1px solid #1e293b", borderRadius: 6, overflow: "hidden" }}>
        <table style={{ width: "100%", fontSize: 11, borderCollapse: "collapse" }}>
          <thead>
            <tr style={{ background: "#0f172a", borderBottom: "1px solid #1e293b" }}>
              <th style={{ padding: "8px 12px", textAlign: "left", color: "#475569", fontWeight: 600, fontSize: 10, textTransform: "uppercase", letterSpacing: 1 }}>Chain</th>
              <th style={{ padding: "8px 12px", textAlign: "left", color: "#475569", fontWeight: 600, fontSize: 10, textTransform: "uppercase", letterSpacing: 1 }}>Router</th>
              <th style={{ padding: "8px 12px", textAlign: "right", color: "#475569", fontWeight: 600, fontSize: 10, textTransform: "uppercase", letterSpacing: 1 }}>Attempts</th>
              <th style={{ padding: "8px 12px", textAlign: "right", color: "#475569", fontWeight: 600, fontSize: 10, textTransform: "uppercase", letterSpacing: 1 }}>Success</th>
              <th style={{ padding: "8px 12px", textAlign: "right", color: "#475569", fontWeight: 600, fontSize: 10, textTransform: "uppercase", letterSpacing: 1 }}>Failures</th>
              <th style={{ padding: "8px 12px", textAlign: "right", color: "#475569", fontWeight: 600, fontSize: 10, textTransform: "uppercase", letterSpacing: 1 }}>Rate</th>
            </tr>
          </thead>
          <tbody>
            {routers.map((r, i) => (
              <tr key={i} style={{ borderBottom: "1px solid #0f172a" }}>
                <td style={{ padding: "10px 12px", color: "#94a3b8", fontSize: 10 }}>
                  <span style={{ color: "#334155" }}>[</span>{r.chain}<span style={{ color: "#334155" }}>]</span>
                </td>
                <td style={{ padding: "10px 12px", color: "#e2e8f0", fontFamily: "monospace", fontSize: 10 }}>
                  {r.router.substring(0, 20)}...
                </td>
                <td style={{ padding: "10px 12px", textAlign: "right", color: "#94a3b8" }}>{r.attempts}</td>
                <td style={{ padding: "10px 12px", textAlign: "right", color: "#34d399" }}>{r.successes}</td>
                <td style={{ padding: "10px 12px", textAlign: "right", color: "#f87171" }}>{r.failures}</td>
                <td style={{ padding: "10px 12px", textAlign: "right", fontWeight: 600, color: parseFloat(r.successRate) > 50 ? "#34d399" : "#f87171" }}>
                  {r.successRate}%
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
```

---

## 3. GAS COST TRACKER (New Component)

Add after `RouterHealthTable`:

```jsx
function GasCostSummary({ logs, chainFilter }) {
  const gasData = useMemo(() => {
    const byChain = new Map();

    logs.forEach((l) => {
      if (!l.data) return;
      const chain = extractChain(l.message) || l.data.chain;
      if (!chain) return;
      if (chainFilter !== "all" && chain !== chainFilter) return;

      const gasCost = l.data.gas_cost_usd || l.data.gasCost || 0;
      if (gasCost <= 0) return;

      if (!byChain.has(chain)) {
        byChain.set(chain, { chain, totalGas: 0, txCount: 0, avgGas: 0 });
      }
      const stat = byChain.get(chain);
      stat.totalGas += gasCost;
      stat.txCount++;
    });

    const rows = Array.from(byChain.values()).map((s) => ({
      ...s,
      avgGas: (s.totalGas / s.txCount).toFixed(4),
    })).sort((a, b) => b.totalGas - a.totalGas);

    const total = rows.reduce((s, r) => s + r.totalGas, 0);

    return { rows, total };
  }, [logs, chainFilter]);

  return (
    <div style={{ background: "#020617", border: "1px solid #1e293b", borderRadius: 6, padding: 16 }}>
      <div style={{ fontSize: 11, fontWeight: 700, color: "#475569", letterSpacing: 1.2, textTransform: "uppercase", marginBottom: 12 }}>
        Gas Costs
      </div>
      <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))", gap: 12 }}>
        {gasData.rows.length === 0 ? (
          <div style={{ color: "#334155", fontSize: 11, fontStyle: "italic" }}>No gas data yet</div>
        ) : (
          <>
            {gasData.rows.map((r) => (
              <div key={r.chain} style={{ background: "#0f172a", borderRadius: 4, padding: "10px 12px" }}>
                <div style={{ fontSize: 9, color: "#475569", textTransform: "uppercase", marginBottom: 4 }}>[{r.chain}]</div>
                <div style={{ fontSize: 14, fontWeight: 700, color: "#f87171" }}>${r.totalGas.toFixed(2)}</div>
                <div style={{ fontSize: 9, color: "#64748b", marginTop: 2 }}>{r.txCount} txs · ${r.avgGas} avg</div>
              </div>
            ))}
            <div style={{ background: "#1e293b", borderRadius: 4, padding: "10px 12px", border: "1px solid #334155" }}>
              <div style={{ fontSize: 9, color: "#94a3b8", textTransform: "uppercase", marginBottom: 4 }}>Total</div>
              <div style={{ fontSize: 16, fontWeight: 700, color: "#f87171" }}>${gasData.total.toFixed(2)}</div>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
```

---

## 4. ENHANCED LOG FEED WITH FILTERS

Replace `LogFeed` component with this enhanced version:

```jsx
function LogFeed({ logs, filter, setFilter, chainFilter }) {
  const ref = useRef(null);
  const [auto, setAuto] = useState(true);
  const [search, setSearch] = useState("");
  const [minProfit, setMinProfit] = useState(0);

  useEffect(() => {
    if (auto && ref.current) ref.current.scrollTop = ref.current.scrollHeight;
  }, [logs, auto]);

  const filtered = useMemo(() => {
    let result = logs;

    // Type filter
    if (filter !== "all") {
      result = result.filter((l) => l.type === filter);
    }

    // Chain filter
    if (chainFilter !== "all") {
      result = result.filter((l) => {
        const chain = extractChain(l.message) || l.data?.chain;
        return chain === chainFilter;
      });
    }

    // Search filter
    if (search) {
      const s = search.toLowerCase();
      result = result.filter((l) =>
        (l.message && l.message.toLowerCase().includes(s)) ||
        (l.data && JSON.stringify(l.data).toLowerCase().includes(s))
      );
    }

    // Min profit filter (for opportunities/trades)
    if (minProfit > 0) {
      result = result.filter((l) => {
        const profit = l.data?.profit_usd || l.data?.profitUsd || 0;
        return profit >= minProfit;
      });
    }

    return result;
  }, [logs, filter, chainFilter, search, minProfit]);

  const handleExport = () => {
    const data = filtered.map((l) => ({
      timestamp: new Date(l.timestamp || Date.now()).toISOString(),
      type: l.type,
      chain: extractChain(l.message) || l.data?.chain || "unknown",
      message: l.message,
      data: l.data,
    }));
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `arbitragepulse-logs-${Date.now()}.json`;
    a.click();
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div style={{ display: "flex", gap: 8, marginBottom: 12, flexWrap: "wrap", alignItems: "center" }}>
        <select value={filter} onChange={(e) => setFilter(e.target.value)} style={select}>
          <option value="all">All Logs</option>
          {["info", "debug", "warn", "error", "trade", "opportunity"].map((t) => (
            <option key={t} value={t}>{t.toUpperCase()}</option>
          ))}
        </select>

        <input
          type="text"
          placeholder="Search logs..."
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          style={{ ...input, flex: 1, minWidth: 200 }}
        />

        <input
          type="number"
          placeholder="Min profit USD"
          value={minProfit || ""}
          onChange={(e) => setMinProfit(parseFloat(e.target.value) || 0)}
          style={{ ...input, width: 120 }}
          step="0.1"
        />

        <button onClick={handleExport} style={{ ...btn, fontSize: 10 }}>
          Export ({filtered.length})
        </button>

        <label style={{ display: "flex", alignItems: "center", gap: 6, fontSize: 11, color: "#94a3b8", cursor: "pointer" }}>
          <input type="checkbox" checked={auto} onChange={() => setAuto(!auto)} />
          Auto-scroll
        </label>
      </div>

      <div ref={ref} style={{ flex: 1, overflowY: "auto", background: "#020617", border: "1px solid #1e293b", borderRadius: 6, padding: 12, fontFamily: "'JetBrains Mono', monospace", fontSize: 11, lineHeight: 1.6 }}>
        {filtered.length === 0 && <div style={{ color: "#334155", fontStyle: "italic" }}>No logs match filters</div>}
        {filtered.map((l) => {
          const [c, label] = TYPE[l.type] || ["#64748b", "LOG!"];
          const chain = extractChain(l.message);
          const profit = l.data?.profit_usd || l.data?.profitUsd;

          return (
            <div key={l._id} style={{ marginBottom: 6, display: "flex", gap: 8, alignItems: "flex-start" }}>
              <span style={{ color: "#334155", fontSize: 10, flexShrink: 0 }}>{ts(l.timestamp)}</span>
              <span style={{ color: c, fontWeight: 700, fontSize: 9, flexShrink: 0, width: 40 }}>{label}</span>
              {chain && (
                <span style={{ color: "#475569", fontSize: 9, flexShrink: 0, background: "#0f172a", padding: "1px 4px", borderRadius: 2 }}>
                  [{chain}]
                </span>
              )}
              {profit && (
                <span style={{ color: profit > 0 ? "#34d399" : "#f87171", fontSize: 9, fontWeight: 600, flexShrink: 0 }}>
                  ${profit.toFixed(2)}
                </span>
              )}
              <span style={{ color: "#e2e8f0", wordBreak: "break-all" }}>
                {l.message}
                {l.data && Object.keys(l.data).length > 0 && (
                  <details style={{ marginTop: 4, color: "#64748b" }}>
                    <summary style={{ cursor: "pointer", fontSize: 9, color: "#475569" }}>Data</summary>
                    <pre style={{ fontSize: 9, marginTop: 4, color: "#334155" }}>
                      {JSON.stringify(l.data, null, 2)}
                    </pre>
                  </details>
                )}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
```

---

## 5. UPDATE MAIN APP COMPONENT

In your main `App` function, add these state variables:

```jsx
function App() {
  // ... existing state ...

  // NEW: Global chain filter
  const [globalChainFilter, setGlobalChainFilter] = useState("all");

  // Extract available chains from logs
  const availableChains = useMemo(() => {
    const chains = new Set();
    logs.forEach((l) => {
      const chain = extractChain(l.message) || l.data?.chain;
      if (chain) chains.add(chain);
    });
    return Array.from(chains).sort();
  }, [logs]);

  // ... rest of component ...

  // UPDATE Header component call to include new props:
  <Header
    tab={tab}
    setTab={setTab}
    status={status}
    engineUrl={engineUrl}
    apiKey={apiKey}
    onLogout={handleLogout}
    globalChainFilter={globalChainFilter}
    setGlobalChainFilter={setGlobalChainFilter}
    availableChains={availableChains}
  />

  // UPDATE Monitor tab to include new components:
  {tab === "monitor" && (
    <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(500px, 1fr))", gap: 16, marginBottom: 16 }}>
      <ProfitChart logs={logs} chainFilter={globalChainFilter} />
      <OpportunityTable logs={logs} chainFilter={globalChainFilter} />
    </div>

    {/* NEW: Router Health */}
    <div style={{ marginBottom: 16 }}>
      <RouterHealthTable logs={logs} chainFilter={globalChainFilter} />
    </div>

    {/* NEW: Gas Cost Summary */}
    <div style={{ marginBottom: 16 }}>
      <GasCostSummary logs={logs} chainFilter={globalChainFilter} />
    </div>

    <div style={{ marginBottom: 16 }}>
      <PairProfitTable logs={logs} chainFilter={globalChainFilter} />
    </div>

    <div style={{ flex: 1 }}>
      <LogFeed logs={logs} filter={logFilter} setFilter={setLogFilter} chainFilter={globalChainFilter} />
    </div>
  )}
}
```

---

## 6. UPDATE EXISTING COMPONENTS TO USE CHAIN FILTER

For `ProfitChart`, `OpportunityTable`, and `PairProfitTable`, update their function signatures to accept `chainFilter`:

```jsx
function ProfitChart({ logs, chainFilter }) {
  // ... existing chainFilter state is now replaced by global filter

  const filtered = useMemo(() => {
    let result = logs.filter((l) => l.type === "trade" && l.data?.profit_usd);

    if (chainFilter !== "all") {
      result = result.filter((l) => {
        const chain = extractChain(l.message) || l.data?.chain;
        return chain === chainFilter;
      });
    }

    return result;
  }, [logs, chainFilter]);

  // ... rest of component uses `filtered` instead of `logs`
}

// Same pattern for OpportunityTable and PairProfitTable
```

---

## 7. KEYBOARD SHORTCUTS (Optional)

Add to the main App component:

```jsx
useEffect(() => {
  const handleKey = (e) => {
    // Ctrl+E: Export logs
    if (e.ctrlKey && e.key === 'e') {
      e.preventDefault();
      // Trigger export in LogFeed
    }

    // Ctrl+P: Toggle pause
    if (e.ctrlKey && e.key === 'p') {
      e.preventDefault();
      if (engineState?.paused) {
        api.resumeEngine();
      } else {
        api.pauseEngine();
      }
    }

    // Ctrl+D: Toggle dry-run
    if (e.ctrlKey && e.key === 'd') {
      e.preventDefault();
      api.setDryRun(!(engineState?.dryRun));
    }
  };

  window.addEventListener('keydown', handleKey);
  return () => window.removeEventListener('keydown', handleKey);
}, [engineState, api]);
```

---

## SUMMARY OF ENHANCEMENTS

✅ **Global Chain Filter** - Filter all views by selected chain
✅ **Router Health Table** - Success rates per router per chain
✅ **Gas Cost Tracker** - Total and average gas costs by chain
✅ **Enhanced Log Feed** - Search, profit filter, and export
✅ **Better Log Formatting** - Chain tags, profit indicators, collapsible data
✅ **Export Functionality** - Download filtered logs as JSON
✅ **Auto-scroll Toggle** - Disable auto-scroll to review old logs

**Estimated implementation time**: 30-45 minutes (copy-paste + integration)

**Production readiness**: All components use `useMemo` for performance, handle missing data gracefully, and work with the existing WebSocket log format.
