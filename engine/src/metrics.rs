use anyhow::Result;
use prometheus::{CounterVec, GaugeVec, Opts, Registry};
use std::sync::Arc;

// ─── Metrics registry ─────────────────────────────────────────────────────────

pub struct Metrics {
    /// Total opportunities detected, labelled by chain name.
    pub opportunities: CounterVec,
    /// Total execution attempts (dry-run or live), labelled by chain + dry_run.
    pub executed: CounterVec,
    /// Total execution failures (gas check, simulation, tx revert), labelled by chain.
    pub failures: CounterVec,
    /// Running USD profit total from successful live trades, labelled by chain.
    pub profit_usd: GaugeVec,
    /// 1 = RPC is reachable for this chain, 0 = down/unknown.
    pub rpc_connected: GaugeVec,
    /// Last block number seen from the block subscription, labelled by chain.
    pub last_block: GaugeVec,

    registry: Registry,
}

impl Metrics {
    pub fn new() -> Result<Arc<Self>> {
        let registry = Registry::new();

        let opportunities = CounterVec::new(
            Opts::new("arb_opportunities_total", "Arbitrage opportunities detected"),
            &["chain"],
        )?;
        registry.register(Box::new(opportunities.clone()))?;

        let executed = CounterVec::new(
            Opts::new("arb_executed_total", "Arbitrage execution attempts"),
            &["chain", "dry_run"],
        )?;
        registry.register(Box::new(executed.clone()))?;

        let failures = CounterVec::new(
            Opts::new("arb_failures_total", "Arbitrage execution failures"),
            &["chain"],
        )?;
        registry.register(Box::new(failures.clone()))?;

        let profit_usd = GaugeVec::new(
            Opts::new("arb_profit_usd_total", "Cumulative USD profit from live arbs"),
            &["chain"],
        )?;
        registry.register(Box::new(profit_usd.clone()))?;

        let rpc_connected = GaugeVec::new(
            Opts::new("arb_rpc_connected", "RPC health: 1=ok, 0=down"),
            &["chain"],
        )?;
        registry.register(Box::new(rpc_connected.clone()))?;

        let last_block = GaugeVec::new(
            Opts::new("arb_last_block", "Last block number seen per chain"),
            &["chain"],
        )?;
        registry.register(Box::new(last_block.clone()))?;

        Ok(Arc::new(Self {
            opportunities,
            executed,
            failures,
            profit_usd,
            rpc_connected,
            last_block,
            registry,
        }))
    }

    /// Render all metrics in Prometheus text exposition format.
    pub fn gather(&self) -> String {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();
        let mut buf = Vec::new();
        let _ = encoder.encode(&self.registry.gather(), &mut buf);
        String::from_utf8(buf).unwrap_or_default()
    }
}
