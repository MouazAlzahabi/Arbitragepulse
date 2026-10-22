#!/usr/bin/env node
// Read-only, keyless probe (Node >= 22, built-in WebSocket/fetch). Run on the engine host:
//   node scripts/preconf-lag.mjs [seconds=60] [http_url=https://mainnet.base.org]
// Sends only eth_subscribe(pendingLogs) and eth_call(slot0) — no keys, no transactions.
// "lag" > 0 means `pending` state on http_url trails the pendingLogs feed (see
// docs/review-loop/OPEN_QUESTIONS.md, Flashblocks HTTP pending lag).
// READ-ONLY. HOT-3: right after a pendingLogs V3 Swap event (wss://mainnet-preconf.base.org),
// does eth_call slot0() at `pending` on https://mainnet.base.org already reflect that swap?
// Classify the returned sqrtP: = this event (sync), = an earlier event of the pool (LAG),
// = a later event of the pool (ahead), else unknown. Re-check lagging ones at +100/250/500 ms.
const WS = "wss://mainnet-preconf.base.org", HTTP = process.argv[3] || "https://mainnet.base.org";
const secs = +(process.argv[2] || 60);
const SWAP = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";
const PSWAP = "0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83";
const pools = ["0xd0b53D9277642d899DF5C87A3966A349A798F224", "0x6c561B446416E1A00E8E93E221854d6eA4171372",
  "0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59", "0x72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38"].map((a) => a.toLowerCase());
const hist = {}; // pool -> [{sqrt, t, bn, li}]
const stats = { events: 0, calls: 0, sync: 0, lag: 0, ahead: 0, unknown: 0, err: 0, lagRecheck: { 100: 0, 250: 0, 500: 0, still: 0 }, rtt: [] };
const lagEx = [];
const call = async (to) => {
  const t0 = Date.now();
  const r = await fetch(HTTP, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "eth_call", params: [{ to, data: "0x3850c7bd" }, "pending"] }) });
  const j = await r.json(); stats.rtt.push(Date.now() - t0);
  if (j.error) throw new Error(JSON.stringify(j.error));
  return BigInt("0x" + j.result.slice(2, 66)).toString();
};
const classify = (pool, idx, s) => {
  const h = hist[pool];
  if (h[idx].sqrt === s) return "sync";
  for (let i = idx + 1; i < h.length; i++) if (h[i].sqrt === s) return "ahead";
  for (let i = idx - 1; i >= 0; i--) if (h[i].sqrt === s) return "lag";
  return "unknown";
};
const ws = new WebSocket(WS);
ws.onopen = () => ws.send(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "eth_subscribe", params: ["pendingLogs", { address: pools, topics: [[SWAP, PSWAP]] }] }));
ws.onmessage = async (m) => {
  const d = JSON.parse(m.data);
  if (d.id) { console.log("sub", d.error ? "ERR" : "ok"); return; }
  const l = d.params.result, pool = l.address.toLowerCase();
  const sqrt = BigInt("0x" + l.data.slice(2 + 64 * 2, 2 + 64 * 3)).toString(); // amount0, amount1, sqrtPriceX96
  const h = (hist[pool] ??= []); const idx = h.push({ sqrt, t: Date.now(), bn: parseInt(l.blockNumber, 16), li: parseInt(l.logIndex, 16) }) - 1;
  stats.events++;
  if (idx === 0) return; // need a prior state to tell lag from unknown
  try {
    stats.calls++;
    const s = await call(pool);
    const c = classify(pool, idx, s); stats[c]++;
    if (c === "lag") {
      if (lagEx.length < 8) lagEx.push({ pool: pool.slice(0, 10), bn: h[idx].bn, li: h[idx].li, ms: Date.now() - h[idx].t });
      for (const w of [100, 250, 500]) {
        await new Promise((r) => setTimeout(r, w === 100 ? 100 : w === 250 ? 150 : 250));
        const s2 = await call(pool); const c2 = classify(pool, idx, s2);
        if (c2 !== "lag") { stats.lagRecheck[w]++; return; }
      }
      stats.lagRecheck.still++;
    }
  } catch (e) { stats.err++; }
};
setTimeout(() => {
  const r = stats.rtt.sort((a, b) => a - b), q = (p) => r[Math.floor(p * (r.length - 1))];
  stats.rtt = { p10: q(0.1), p50: q(0.5), p90: q(0.9) };
  console.log(JSON.stringify({ HTTP, secs, ...stats, lagEx }, null, 1));
  process.exit(0);
}, secs * 1000);
