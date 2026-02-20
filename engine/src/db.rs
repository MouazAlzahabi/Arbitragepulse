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
    pub profit_usd: f64,
    pub success: bool,
    pub tx_hash: String,
    pub dry_run: bool,
}

// ─── Per-pair stat (for /stats/pairs endpoint) ────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct PairStat {
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
                profit_usd  REAL    NOT NULL,
                success     INTEGER NOT NULL,
                tx_hash     TEXT    NOT NULL,
                dry_run     INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_trades_ts    ON trades(ts);
            CREATE INDEX IF NOT EXISTS idx_trades_chain ON trades(chain_id, ts);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn insert_trade(
        &self,
        chain_id: u64,
        chain_name: &str,
        pair_id: &str,
        router_a: &str,
        router_b: &str,
        profit_usd: f64,
        success: bool,
        tx_hash: &str,
        dry_run: bool,
    ) -> Result<()> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO trades
             (ts, chain_id, chain_name, pair_id, router_a, router_b,
              profit_usd, success, tx_hash, dry_run)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                ts,
                chain_id as i64,
                chain_name,
                pair_id,
                router_a,
                router_b,
                profit_usd,
                success as i64,
                tx_hash,
                dry_run as i64
            ],
        )?;
        Ok(())
    }

    /// Returns trades ordered newest-first.
    pub fn get_trades(&self, limit: i64) -> Result<Vec<TradeRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, ts, chain_id, chain_name, pair_id, router_a, router_b,
                    profit_usd, success, tx_hash, dry_run
             FROM trades ORDER BY ts DESC LIMIT ?1",
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
                    profit_usd: row.get(7)?,
                    success: row.get::<_, i64>(8)? != 0,
                    tx_hash: row.get(9)?,
                    dry_run: row.get::<_, i64>(10)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    }

    /// Per-pair cumulative stats for the /stats/pairs endpoint (live trades only).
    pub fn get_pair_stats(&self) -> Result<Vec<PairStat>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT pair_id,
                    COUNT(*) as attempts,
                    SUM(success) as successes,
                    SUM(CASE WHEN success=1 THEN profit_usd ELSE 0.0 END) as profit
             FROM trades WHERE dry_run=0
             GROUP BY pair_id
             ORDER BY profit DESC",
        )?;
        let records = stmt
            .query_map([], |row| {
                Ok(PairStat {
                    pair_id: row.get(0)?,
                    attempts: row.get::<_, i64>(1)? as u64,
                    successes: row.get::<_, i64>(2)? as u64,
                    profit_usd: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
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
