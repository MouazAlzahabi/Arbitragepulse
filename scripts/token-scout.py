#!/usr/bin/env python3
"""Token scout: find Base tokens with deep liquidity on the engine's venues and little
arbitrage competition. Stdlib only, no API keys; never reads .env.

Pipeline
  1. Universe   GeckoTerminal top Base pools (pages 1..N), tokens seen in them.
  2. Pools      DexScreener /token-pairs per token (all its pools, with USD liquidity).
  3. Venue      on-chain pool.factory() mapped to the factories in engine/config.yaml
                ("covered") or a known-uncovered list ("nc").
  4. Eligible   >= --min-pools covered pools, each >= --min-pool-usd liquidity.
  5. Compete    GeckoTerminal /pools/{pool}/trades on the token's top covered pools:
                A = share of tx hashes that touch 2+ of the token's pools (arb share),
                B = distinct senders of those txs.
  6. Persist    (optional, --persist-minutes) on-chain mid price of each covered pool every
                30 s; P = share of samples where the max cross-pool gap > the two pools' fees.
  7. Score      log10(L2) * min(V/TVL, 5) * (0.1 + P) / (1 + A * ln(1 + B))
                L2 = USD depth of the 2nd-deepest covered pool, V = 24h volume, TVL = covered TVL.
  8. Halal      scripts/token-scout-denylist.txt: "deny" rows dropped, "flag" rows marked.

Usage: see scripts/TOKEN_SCOUT.md.
"""
import argparse
import json
import math
import os
import re
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
GT = "https://api.geckoterminal.com/api/v2"
DS = "https://api.dexscreener.com"
UA = {"User-Agent": "arbitragepulse-token-scout/1", "Accept": "application/json"}

USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
USDT = "0xfde4c96c8593536e31f229ea8f37b2ada2699bb2"
WETH = "0x4200000000000000000000000000000000000006"
# Quote-side tokens: never candidates themselves, the other side of a candidate's pools.
BASE_TOKENS = {USDC, USDT, WETH, "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf"}  # + cbBTC
STABLES = {USDC, USDT, "0x60a3e35cc302bfa44cb288bc5a4f316fdb1adb42",  # EURC
           "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca", "0x50c5725949a6f0c72e6c4a641f24049a917db0cb"}

# Factories the engine does not cover (yet). Anything else unknown is "other".
UNCOVERED = {
    "0x9cb220581b91b0c83223012fe69688b92ad07a76": "Aero CL",
    "0x8e6a9fcd1d638cc760ffba397605fe3355de76d2": "Aero V2 volatile",
    "0x78589e876edcebdce72ee3436ab3c9ad3a7cef2d": "Aero V2 stable",
    "0xf8f2eb4940cfe7d13603dddd87f123820fc061ef": "Slipstream 3",
    "0x8909dc15e40173ff4699343b6eb8132c65e18ec6": "Uniswap V2",
    "0xc35dadb65012ec5796536bd9864ed8773abc74c4": "Sushi V3",
}

SEL_FACTORY = "0xc45a0155"   # factory()
SEL_FEE = "0xddca3f43"       # fee()          (V3-style, ppm)
SEL_SLOT0 = "0x3850c7bd"     # slot0()
SEL_RESERVES = "0x0902f1ac"  # getReserves()
SEL_TOKEN0 = "0x0dfe1681"    # token0()
SEL_STABLE = "0x22be3de1"    # stable()       (Solidly)
SEL_DECIMALS = "0x313ce567"
SEL_SYMBOL = "0x95d89b41"
EIP1967 = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"


def log(msg):
    print(msg, file=sys.stderr, flush=True)


# ─── HTTP ────────────────────────────────────────────────────────────────────

_last_call = {}


def get_json(url, min_gap, retries=4):
    """GET with a per-host minimum gap between calls and backoff on 429/5xx."""
    host = url.split("/")[2]
    for attempt in range(retries + 1):
        wait = _last_call.get(host, 0) + min_gap - time.time()
        if wait > 0:
            time.sleep(wait)
        _last_call[host] = time.time()
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=30) as r:
                return json.loads(r.read())
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503, 504) and attempt < retries:
                back = min_gap * (2 ** (attempt + 1))
                log(f"  {host} HTTP {e.code}, retrying in {back:.0f}s")
                time.sleep(back)
                continue
            if e.code == 404:
                return None
            raise
        except (urllib.error.URLError, TimeoutError) as e:
            if attempt < retries:
                time.sleep(min_gap * 2)
                continue
            log(f"  {host} unreachable: {e}")
            return None
    return None


class Rpc:
    def __init__(self, url):
        self.url = url
        self.id = 0

    def batch(self, calls):
        """calls: [(to, data)] -> [hex or None], one JSON-RPC batch per 50 calls."""
        out = []
        for i in range(0, len(calls), 50):
            chunk = calls[i:i + 50]
            body = []
            for to, data in chunk:
                self.id += 1
                body.append({"jsonrpc": "2.0", "id": self.id, "method": "eth_call",
                             "params": [{"to": to, "data": data}, "latest"]})
            res = self._post(body) or []
            by_id = {r.get("id"): r.get("result") for r in res if isinstance(r, dict)}
            out.extend(by_id.get(b["id"]) for b in body)
        # The public endpoint drops batch members under load: retry those one by one.
        for i, r in enumerate(out):
            if r is None:
                out[i] = self.call(*calls[i])
        return out

    def call(self, to, data):
        for attempt in range(3):
            self.id += 1
            r = self._post({"jsonrpc": "2.0", "id": self.id, "method": "eth_call",
                            "params": [{"to": to, "data": data}, "latest"]})
            if isinstance(r, dict):
                if "result" in r:
                    return r["result"]
                if "execution reverted" in str(r.get("error", "")).lower() or r.get("error", {}).get("code") == 3:
                    return None  # a real revert (e.g. no fee() on a V2 pool): don't retry
            time.sleep(0.5 * (attempt + 1))
        return None

    def storage(self, addr, slot):
        self.id += 1
        r = self._post({"jsonrpc": "2.0", "id": self.id, "method": "eth_getStorageAt",
                        "params": [addr, slot, "latest"]})
        return (r or {}).get("result")

    def _post(self, body):
        req = urllib.request.Request(self.url, data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json", **UA})
        for attempt in range(4):
            try:
                with urllib.request.urlopen(req, timeout=30) as r:
                    return json.loads(r.read())
            except (urllib.error.URLError, TimeoutError) as e:
                if attempt == 3:
                    log(f"  RPC failed: {e}")
                    return None
                time.sleep(1.5 * (attempt + 1))


def word(h, i=0):
    if not h or h == "0x" or len(h) < 2 + 64 * (i + 1):
        return None
    return int(h[2 + 64 * i: 2 + 64 * (i + 1)], 16)


def addr_word(h):
    w = word(h)
    return None if w is None else "0x%040x" % w


def decode_string(h):
    try:
        b = bytes.fromhex(h[2:])
        if len(b) >= 96:
            n = int.from_bytes(b[32:64], "big")
            return b[64:64 + n].decode(errors="replace")
        return b.rstrip(b"\0").decode(errors="replace")
    except Exception:
        return "?"


# ─── Engine config ───────────────────────────────────────────────────────────

def covered_factories(config_path):
    """factory_address -> router id, read from engine/config.yaml without a YAML dependency."""
    out, rid = {}, None
    in_routers = False
    for line in open(config_path):
        s = line.split("#", 1)[0].rstrip()
        if re.match(r"^routers:", s):
            in_routers = True
            continue
        if in_routers and re.match(r"^\S", s):
            break
        m = re.match(r"^\s*-\s*id:\s*\"?([\w-]+)", s)
        if m:
            rid = m.group(1)
        m = re.search(r"factory_address:\s*\"?(0x[0-9a-fA-F]{40})", s)
        if m and rid:
            out[m.group(1).lower()] = rid
    return out


def configured_tokens(config_path):
    toks = set()
    for line in open(config_path):
        m = re.search(r"token_(?:in|out):\s*\"(0x[0-9a-fA-F]{40})\"", line.split("#", 1)[0])
        if m:
            toks.add(m.group(1).lower())
    return toks


def load_denylist(path):
    rules = {}
    if not os.path.exists(path):
        return rules
    for line in open(path):
        s = line.split("#", 1)
        note = s[1].strip() if len(s) > 1 else ""
        parts = s[0].split()
        if len(parts) >= 3 and parts[1] in ("deny", "flag"):
            rules[parts[0].lower()] = (parts[1], parts[2], note)
    return rules


# ─── Stages ──────────────────────────────────────────────────────────────────

def universe(pages, gap):
    tokens = {}
    for page in range(1, pages + 1):
        log(f"GeckoTerminal top pools page {page}/{pages}")
        d = get_json(f"{GT}/networks/base/pools?page={page}&include=base_token,quote_token", gap)
        if not d:
            break
        for inc in d.get("included", []):
            if inc.get("type") != "token":
                continue
            a = inc["attributes"]
            addr = a.get("address", "").lower()
            if addr and addr not in BASE_TOKENS:
                tokens[addr] = {"symbol": a.get("symbol", "?"), "decimals": a.get("decimals")}
    return tokens


def token_pools(addr, gap):
    d = get_json(f"{DS}/token-pairs/v1/base/{addr}", gap) or []
    pools = []
    for p in d if isinstance(d, list) else []:
        if p.get("chainId") != "base":
            continue
        liq = (p.get("liquidity") or {}).get("usd") or 0
        base = (p.get("baseToken") or {}).get("address", "").lower()
        quote = (p.get("quoteToken") or {}).get("address", "").lower()
        other = quote if base == addr else base
        pools.append({
            "pool": p.get("pairAddress", "").lower(), "dex": p.get("dexId", "?"),
            "liq": float(liq), "vol24": float((p.get("volume") or {}).get("h24") or 0),
            "other": other, "other_symbol": ((p.get("quoteToken") if base == addr else p.get("baseToken")) or {}).get("symbol", "?"),
        })
    return pools


def classify_venues(rpc, pools, covered):
    raw = rpc.batch([(p["pool"], SEL_FACTORY) for p in pools])
    for p, r in zip(pools, raw):
        f = addr_word(r)
        p["factory"] = f
        if f and f in covered:
            p["venue"], p["covered"] = covered[f], True
        elif f and f in UNCOVERED:
            p["venue"], p["covered"] = "nc:" + UNCOVERED[f], False
        else:
            p["venue"], p["covered"] = "other:" + (p["dex"] or "?"), False


def competition(addr, pools, gap, max_pools):
    """A = share of tx hashes touching 2+ of the token's pools, B = distinct senders of those."""
    by_tx, senders = {}, {}
    span_h = []
    for p in pools[:max_pools]:
        d = get_json(f"{GT}/networks/base/pools/{p['pool']}/trades", gap) or {}
        ts = []
        for t in d.get("data", []):
            a = t.get("attributes", {})
            h = a.get("tx_hash")
            if not h:
                continue
            by_tx.setdefault(h, set()).add(p["pool"])
            senders[h] = a.get("tx_from_address")
            ts.append(a.get("block_timestamp", ""))
        if len(ts) >= 2:
            try:
                t0 = time.mktime(time.strptime(min(ts)[:19], "%Y-%m-%dT%H:%M:%S"))
                t1 = time.mktime(time.strptime(max(ts)[:19], "%Y-%m-%dT%H:%M:%S"))
                span_h.append((t1 - t0) / 3600)
            except ValueError:
                pass
    if not by_tx:
        return None, None, None
    multi = [h for h, ps in by_tx.items() if len(ps) >= 2]
    a = len(multi) / len(by_tx)
    b = len({senders[h] for h in multi if senders.get(h)})
    return a, b, (min(span_h) if span_h else None)


def mid_price(rpc, pool, addr, decimals):
    """Price of `addr` in the other token's units (raw, both decimals applied) or None."""
    r = rpc.batch([(pool["pool"], SEL_SLOT0), (pool["pool"], SEL_TOKEN0), (pool["pool"], SEL_RESERVES)])
    t0 = addr_word(r[1])
    if t0 is None:
        return None
    d_self, d_other = decimals.get(addr, 18), decimals.get(pool["other"], 18)
    sp = word(r[0])
    if sp and sp > 0 and sp < 2 ** 160:
        p1_per_0 = (sp / 2 ** 96) ** 2
    else:
        r0, r1 = word(r[2], 0), word(r[2], 1)
        if not r0 or not r1:
            return None
        p1_per_0 = r1 / r0
    if t0 == addr:
        return p1_per_0 * 10 ** (d_self - d_other)
    return (1 / p1_per_0) * 10 ** (d_self - d_other) if p1_per_0 else None


def pool_fee(rpc, pool):
    """Swap fee as a fraction: V3-style fee() in ppm, else 0.3 % (V2) / 0.05 % (stable) guess."""
    r = rpc.batch([(pool["pool"], SEL_FEE), (pool["pool"], SEL_STABLE)])
    f = word(r[0])
    if f is not None and 0 < f < 100_000:
        return f / 1e6
    return 0.0005 if word(r[1]) == 1 else 0.003


def persistence(rpc, addr, pools, decimals, minutes, interval=30):
    """Share of samples where two covered pools quoting the same other token disagree by more
    than their two fees. Only pools against the same quote token are compared."""
    groups = {}
    for p in pools:
        groups.setdefault(p["other"], []).append(p)
    groups = {k: v for k, v in groups.items() if len(v) >= 2}
    if not groups:
        return None
    fees = {p["pool"]: pool_fee(rpc, p) for g in groups.values() for p in g}
    hits = samples = 0
    end = time.time() + minutes * 60
    while time.time() < end:
        for g in groups.values():
            px = [(p, mid_price(rpc, p, addr, decimals)) for p in g]
            px = [(p, v) for p, v in px if v]
            if len(px) < 2:
                continue
            samples += 1
            lo = min(px, key=lambda x: x[1])
            hi = max(px, key=lambda x: x[1])
            if hi[1] / lo[1] - 1 > fees[lo[0]["pool"]] + fees[hi[0]["pool"]]:
                hits += 1
        time.sleep(interval)
    return hits / samples if samples else None


def token_meta(rpc, addr):
    r = rpc.batch([(addr, SEL_SYMBOL), (addr, SEL_DECIMALS)])
    impl = rpc.storage(addr, EIP1967)
    proxy = bool(impl and int(impl, 16) != 0)
    return decode_string(r[0]) if r[0] else "?", word(r[1]), proxy


def score(l2, vol, tvl, a, b, p):
    a = a or 0.0
    b = b or 0
    p = p or 0.0
    turnover = min(vol / tvl, 5) if tvl else 0
    return math.log10(max(l2, 1)) * turnover * (0.1 + p) / (1 + a * math.log(1 + b))


def yaml_pair(sym, addr, dec):
    sid = re.sub(r"[^a-z0-9]", "", sym.lower())
    return (f'  - id: "base-usdc-{sid}"\n'
            f'    token_in: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"\n'
            f'    token_out: "{addr}"\n'
            f'    token_in_symbol: "USDC"\n'
            f'    token_out_symbol: "{sym.upper()}"\n'
            f'    token_in_decimals: 6\n'
            f'    token_out_decimals: {dec}\n'
            f'    trade_amount: "5000"\n'
            f'    max_trade: "5000"\n')


# ─── Main ────────────────────────────────────────────────────────────────────

def analyse(addr, info, args, rpc, covered, deny, configured):
    pools = [p for p in token_pools(addr, args.ds_gap) if p["pool"]]
    if not pools:
        return None
    classify_venues(rpc, pools, covered)
    pools.sort(key=lambda p: -p["liq"])
    cov = [p for p in pools if p["covered"] and p["liq"] >= args.min_pool_usd]
    sym, dec, proxy = token_meta(rpc, addr)
    sym = sym if sym != "?" else info.get("symbol", "?")
    row = {
        "token": addr, "symbol": sym, "decimals": dec, "proxy": proxy,
        "in_config": addr in configured,
        "covered_pools": [{k: p[k] for k in ("pool", "venue", "liq", "other_symbol")} for p in cov],
        "uncovered_pools": [{k: p[k] for k in ("pool", "venue", "liq", "other_symbol")}
                            for p in pools if not p["covered"] and p["liq"] >= args.min_pool_usd],
        "tvl_covered": sum(p["liq"] for p in cov),
        "vol24": sum(p["vol24"] for p in pools),
        "eligible": len(cov) >= args.min_pools,
        "halal": "ok", "halal_note": "",
    }
    rule = deny.get(addr) or deny.get(sym.lower())
    if rule:
        row["halal"], row["halal_note"] = rule[0], f"{rule[1]} {rule[2]}".strip()
    if addr in STABLES:
        row["halal"], row["halal_note"] = "deny", "stablecoin (already a base asset)"
    a = b = span = p = None
    if row["eligible"] and row["halal"] != "deny" and not args.no_trades:
        a, b, span = competition(addr, cov, args.gt_gap, args.trade_pools)
    if row["eligible"] and row["halal"] != "deny" and args.persist_minutes > 0:
        decimals = {addr: dec or 18, USDC: 6, USDT: 6, WETH: 18,
                    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": 8}
        p = persistence(rpc, addr, cov, decimals, args.persist_minutes)
    l2 = cov[1]["liq"] if len(cov) >= 2 else 0
    row.update({"arb_share": a, "arb_senders": b, "trades_window_h": span, "persist": p,
                "score": score(l2, row["vol24"], row["tvl_covered"], a, b, p) if row["eligible"] else 0})
    return row


def fmt_usd(x):
    return f"${x / 1e6:.1f}M" if x >= 1e6 else f"${x / 1e3:.0f}k"


def print_table(rows):
    hdr = f"{'#':>2} {'symbol':<10} {'score':>6} {'covTVL':>8} {'vol24':>8} {'pools':>5} {'arbA':>5} {'arbB':>4} {'P':>5}  halal  notes"
    print(hdr)
    print("-" * len(hdr))
    for i, r in enumerate(rows, 1):
        f = lambda v, s: "-" if v is None else s.format(v)
        notes = []
        if r["in_config"]:
            notes.append("in config")
        if not r["eligible"]:
            notes.append(f"{len(r['covered_pools'])} covered pool(s)")
        if r["uncovered_pools"]:
            notes.append("nc: " + ", ".join(f"{p['venue'].split(':', 1)[1]} {fmt_usd(p['liq'])}" for p in r["uncovered_pools"][:2]))
        if r["proxy"]:
            notes.append("proxy")
        if r["halal_note"]:
            notes.append(r["halal_note"])
        print(f"{i:>2} {r['symbol'][:10]:<10} {r['score']:>6.2f} {fmt_usd(r['tvl_covered']):>8} {fmt_usd(r['vol24']):>8} "
              f"{len(r['covered_pools']):>5} {f(r['arb_share'], '{:.2f}'):>5} {f(r['arb_senders'], '{}'):>4} "
              f"{f(r['persist'], '{:.2f}'):>5}  {r['halal']:<5}  {'; '.join(notes)}")


def main():
    ap = argparse.ArgumentParser(description="Find deep, low-competition Base tokens for the engine's venues.")
    ap.add_argument("--pages", type=int, default=10, help="GeckoTerminal top-pool pages (20 pools each, max 10)")
    ap.add_argument("--tokens", help="comma-separated token addresses: skip the universe scan, analyse these")
    ap.add_argument("--check", metavar="ADDR", help="analyse one token (implies --tokens ADDR) and print details")
    ap.add_argument("--min-pool-usd", type=float, default=200_000)
    ap.add_argument("--min-pools", type=int, default=2, help="covered pools needed to be eligible")
    ap.add_argument("--trade-pools", type=int, default=4, help="pools per token sampled for competition")
    ap.add_argument("--no-trades", action="store_true", help="skip the competition stage (faster)")
    ap.add_argument("--persist-minutes", type=float, default=0, help="on-chain spread sampling per token (0 = off)")
    ap.add_argument("--top", type=int, default=25, help="rows to print")
    ap.add_argument("--all", action="store_true", help="also print ineligible and denied tokens")
    ap.add_argument("--rpc", default="https://mainnet.base.org", help="read-only Base RPC (keyless)")
    ap.add_argument("--config", default=os.path.join(REPO, "engine", "config.yaml"))
    ap.add_argument("--denylist", default=os.path.join(HERE, "token-scout-denylist.txt"))
    ap.add_argument("--json", metavar="FILE", help="write every analysed row as JSON")
    ap.add_argument("--yaml", action="store_true", help="print pairs: YAML for the eligible, allowed tokens")
    ap.add_argument("--gt-gap", type=float, default=6.0, help="seconds between GeckoTerminal calls")
    ap.add_argument("--ds-gap", type=float, default=0.3, help="seconds between DexScreener calls")
    args = ap.parse_args()

    rpc = Rpc(args.rpc)
    covered = covered_factories(args.config)
    if not covered:
        sys.exit(f"no factory_address entries found under routers: in {args.config}")
    deny = load_denylist(args.denylist)
    configured = configured_tokens(args.config)
    log(f"covered venues: {', '.join(sorted(set(covered.values())))}")

    if args.check:
        args.tokens = args.check
    if args.tokens:
        tokens = {t.strip().lower(): {} for t in args.tokens.split(",") if t.strip()}
    else:
        tokens = universe(min(args.pages, 10), args.gt_gap)
    log(f"analysing {len(tokens)} tokens")

    rows = []
    for i, (addr, info) in enumerate(tokens.items(), 1):
        log(f"[{i}/{len(tokens)}] {info.get('symbol', addr)}")
        try:
            r = analyse(addr, info, args, rpc, covered, deny, configured)
        except Exception as e:  # one bad token must not end a 10-minute run
            log(f"  skipped: {e}")
            continue
        if r:
            rows.append(r)

    rows.sort(key=lambda r: (-r["score"], -r["tvl_covered"]))
    shown = rows if (args.all or args.check) else [r for r in rows if r["eligible"] and r["halal"] != "deny"]
    print_table(shown[:args.top])
    if args.check and rows:
        r = rows[0]
        print(f"\n{r['symbol']} {r['token']} decimals={r['decimals']} proxy={r['proxy']}")
        for p in r["covered_pools"]:
            print(f"  covered    {p['venue']:<28} {p['pool']} vs {p['other_symbol']:<6} {fmt_usd(p['liq'])}")
        for p in r["uncovered_pools"]:
            print(f"  uncovered  {p['venue']:<28} {p['pool']} vs {p['other_symbol']:<6} {fmt_usd(p['liq'])}")
        if r["trades_window_h"] is not None:
            print(f"  trades window sampled: {r['trades_window_h']:.1f} h (shortest pool)")
    if args.yaml:
        print("\n# pairs: block (fee-on-transfer not checked — test each on a fork first)")
        for r in shown:
            if r["eligible"] and r["halal"] == "ok" and not r["in_config"] and r["decimals"] is not None:
                print(yaml_pair(r["symbol"], r["token"], r["decimals"]))
    if args.json:
        with open(args.json, "w") as f:
            json.dump(rows, f, indent=2)
        log(f"wrote {len(rows)} rows to {args.json}")


if __name__ == "__main__":
    main()
