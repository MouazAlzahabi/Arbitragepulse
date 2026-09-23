use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::sync::Mutex;

// ─── Trade record ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TradeRecord {
    pub id: i64,
    pub ts: i64,
    pub chain_id: u64,
    pub chain_name: String,
    pub pair_id: String,
    pub router_a: String,
    pub router_b: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub router_c: Option<String>, // For triangular arbs (A→B→C→A)
    pub profit_usd: f64,
    pub success: bool,
    pub tx_hash: String,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_to_submit_ms: Option<u64>,
}

// ─── Per-pair stat (for /stats/pairs endpoint) ────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct PairStat {
    pub chain_name: String,
    pub pair_id: String,
    pub attempts: u64,
    pub successes: u64,
    pub profit_usd: f64,
}

// ─── Database ─────────────────────────────────────────────────────────────────

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS trades (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                ts          INTEGER NOT NULL,
                chain_id    INTEGER NOT NULL,
                chain_name  TEXT    NOT NULL,
                pair_id     TEXT    NOT NULL,
                router_a    TEXT    NOT NULL,
                router_b    TEXT    NOT NULL,
                router_c    TEXT,
                profit_usd  REAL    NOT NULL,
                success     INTEGER NOT NULL,
                tx_hash     TEXT    NOT NULL,
                dry_run     INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_trades_ts    ON trades(ts);
            CREATE INDEX IF NOT EXISTS idx_trades_chain ON trades(chain_id, ts);",
        )?;

        // Migration: Add router_c column if it doesn't exist (for existing databases)
        let _ = conn.execute("ALTER TABLE trades ADD COLUMN router_c TEXT", []);
        let _ = conn.execute("ALTER TABLE trades ADD COLUMN revert_class TEXT", []);
        let _ = conn.execute("ALTER TABLE trades ADD COLUMN scan_to_submit_ms INTEGER", []);

        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn insert_trade(
        &self,
        chain_id: u64,
        chain_name: &str,
        pair_id: &str,
        router_a: &str,
        router_b: &str,
        router_c: Option<&str>, // None for 2-hop, Some(router) for triangular
        profit_usd: f64,
        success: bool,
        tx_hash: &str,
        dry_run: bool,
    ) -> Result<()> {
        self.insert_trade_extended(
            chain_id,
            chain_name,
            pair_id,
            router_a,
            router_b,
            router_c,
            profit_usd,
            success,
            tx_hash,
            dry_run,
            None,
            None,
        )
    }

    pub fn insert_trade_extended(
        &self,
        chain_id: u64,
        chain_name: &str,
        pair_id: &str,
        router_a: &str,
        router_b: &str,
        router_c: Option<&str>,
        profit_usd: f64,
        success: bool,
        tx_hash: &str,
        dry_run: bool,
        revert_class: Option<&str>,
        scan_to_submit_ms: Option<u64>,
    ) -> Result<()> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO trades
             (ts, chain_id, chain_name, pair_id, router_a, router_b, router_c,
              profit_usd, success, tx_hash, dry_run, revert_class, scan_to_submit_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                ts,
                chain_id as i64,
                chain_name,
                pair_id,
                router_a,
                router_b,
                router_c,
                profit_usd,
                success as i64,
                tx_hash,
                dry_run as i64,
                revert_class,
                scan_to_submit_ms.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    /// Returns live (non-dry-run) trades ordered newest-first.
    pub fn get_trades(&self, limit: i64) -> Result<Vec<TradeRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, ts, chain_id, chain_name, pair_id, router_a, router_b, router_c,
                    profit_usd, success, tx_hash, dry_run, revert_class, scan_to_submit_ms
             FROM trades WHERE dry_run=0 ORDER BY ts DESC LIMIT ?1",
        )?;
        let records = stmt
            .query_map(params![limit], |row| {
                Ok(TradeRecord {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    chain_id: row.get::<_, i64>(2)? as u64,
                    chain_name: row.get(3)?,
                    pair_id: row.get(4)?,
                    router_a: row.get(5)?,
                    router_b: row.get(6)?,
                    router_c: row.get(7)?,
                    profit_usd: row.get(8)?,
                    success: row.get::<_, i64>(9)? != 0,
                    tx_hash: row.get(10)?,
                    dry_run: row.get::<_, i64>(11)? != 0,
                    revert_class: row.get(12)?,
                    scan_to_submit_ms: row
                        .get::<_, Option<i64>>(13)?
                        .map(|v| v as u64),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    }

    /// Per-pair cumulative stats for the /stats/pairs endpoint (live trades only).
    pub fn get_pair_stats(&self) -> Result<Vec<PairStat>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT chain_name, pair_id,
                    COUNT(*) as attempts,
                    SUM(success) as successes,
                    SUM(CASE WHEN success=1 THEN profit_usd ELSE 0.0 END) as profit
             FROM trades WHERE dry_run=0
             GROUP BY chain_name, pair_id
             ORDER BY profit DESC",
        )?;
        let records = stmt
            .query_map([], |row| {
                Ok(PairStat {
                    chain_name: row.get(0)?,
                    pair_id: row.get(1)?,
                    attempts: row.get::<_, i64>(2)? as u64,
                    successes: row.get::<_, i64>(3)? as u64,
                    profit_usd: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    }

    /// Delete all trade records from the database (irreversible).
    pub fn clear_all_trades(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute("DELETE FROM trades", [])?;
        Ok(deleted)
    }

    /// Per-chain cumulative stats for seeding ChainStats on restart (live trades only).
    pub fn get_chain_stats(&self) -> Result<Vec<(u64, String, u64, u64, f64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT chain_id, chain_name,
                    COUNT(*) as attempts,
                    SUM(success) as successes,
                    SUM(CASE WHEN success=1 THEN profit_usd ELSE 0.0 END) as profit
             FROM trades WHERE dry_run=0
             GROUP BY chain_id",
        )?;
        let records = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, i64>(3)? as u64,
                    row.get::<_, f64>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    }
}
