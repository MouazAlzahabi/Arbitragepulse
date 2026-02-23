use dashmap::DashMap;
use std::sync::Arc;
use std::time::Instant;

/// Per-router health statistics for monitoring performance and reliability.
#[derive(Debug, Clone)]
pub struct RouterStats {
    pub attempts: u64,
    pub successes: u64,
    pub failures: u64,
    pub total_latency_ms: u64,
    pub last_success: Option<Instant>,
    pub last_failure: Option<Instant>,
}

impl Default for RouterStats {
    fn default() -> Self {
        Self {
            attempts: 0,
            successes: 0,
            failures: 0,
            total_latency_ms: 0,
            last_success: None,
            last_failure: None,
        }
    }
}

impl RouterStats {
    pub fn success_rate(&self) -> f64 {
        if self.attempts == 0 {
            return 0.0;
        }
        self.successes as f64 / self.attempts as f64
    }

    pub fn avg_latency_ms(&self) -> f64 {
        if self.successes == 0 {
            return 0.0;
        }
        self.total_latency_ms as f64 / self.successes as f64
    }
}

/// Router health monitor for tracking performance across all routers.
pub struct RouterHealthMonitor {
    stats: Arc<DashMap<String, RouterStats>>,
}

impl RouterHealthMonitor {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(DashMap::new()),
        }
    }

    /// Record a successful router operation
    pub fn record_success(&self, router_id: &str, latency_ms: u64) {
        let mut stats = self.stats
            .entry(router_id.to_string())
            .or_default();
        stats.attempts += 1;
        stats.successes += 1;
        stats.total_latency_ms += latency_ms;
        stats.last_success = Some(Instant::now());
    }

    /// Record a failed router operation
    pub fn record_failure(&self, router_id: &str) {
        let mut stats = self.stats
            .entry(router_id.to_string())
            .or_default();
        stats.attempts += 1;
        stats.failures += 1;
        stats.last_failure = Some(Instant::now());
    }

    /// Get stats for a specific router
    pub fn get_stats(&self, router_id: &str) -> Option<RouterStats> {
        self.stats.get(router_id).map(|r| r.clone())
    }

    /// Get all router stats
    pub fn get_all_stats(&self) -> Vec<(String, RouterStats)> {
        self.stats
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    /// Check if a router should be considered healthy (>50% success rate, >10 attempts)
    pub fn is_healthy(&self, router_id: &str) -> bool {
        if let Some(stats) = self.get_stats(router_id) {
            if stats.attempts < 10 {
                return true; // Not enough data, assume healthy
            }
            stats.success_rate() > 0.5
        } else {
            true // Unknown router, assume healthy
        }
    }
}
