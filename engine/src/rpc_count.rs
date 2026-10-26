//! Per-method request counter for the main RPC provider (Alchemy in the shipped config):
//! a tower layer on the alloy client. The heartbeat prints and resets the counts, so RPC
//! spend can be traced to a method without the provider's dashboard.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::TransportError;
use tower::{Layer, Service};

#[derive(Clone, Default)]
pub struct RpcCounter(Arc<Mutex<BTreeMap<String, u64>>>);

impl RpcCounter {
    fn record(&self, method: &str) {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *m.entry(method.to_string()).or_insert(0) += 1;
    }

    /// Counts since the last call, as `method:n,…` (most frequent first); empty if none.
    pub fn take_summary(&self) -> String {
        let counts = std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()));
        let mut v: Vec<_> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.iter().map(|(m, n)| format!("{m}:{n}")).collect::<Vec<_>>().join(",")
    }
}

#[derive(Clone)]
pub struct CountLayer(pub RpcCounter);

impl<S> Layer<S> for CountLayer {
    type Service = CountService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        CountService { inner, counter: self.0.clone() }
    }
}

#[derive(Clone)]
pub struct CountService<S> {
    inner: S,
    counter: RpcCounter,
}

impl<S> Service<RequestPacket> for CountService<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        for m in req.method_names() {
            self.counter.record(m);
        }
        self.inner.call(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_sorts_by_count_and_resets() {
        let c = RpcCounter::default();
        for m in ["eth_call", "eth_call", "eth_getBalance", "eth_call"] {
            c.record(m);
        }
        assert_eq!(c.take_summary(), "eth_call:3,eth_getBalance:1");
        assert_eq!(c.take_summary(), "");
    }
}
