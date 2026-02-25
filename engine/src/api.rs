use axum::{
    extract::{Path, Query, Request, State, WebSocketUpgrade},
    extract::ws::{Message, WebSocket},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing::info;

use crate::db::Database;
use crate::metrics::Metrics;

// ─── Log entry (broadcasted to WS clients, mirrors TS engine) ─────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: String,
    pub message: String,
    pub timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ─── Token (stored in-memory, seeded from config pairs) ──────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub symbol: String,
    pub address: String,
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    pub decimals: u8,
    pub trusted: bool,
    pub category: String,
}

// ─── Engine state (shared across axum handlers) ───────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ChainStats {
    pub chain_id: u64,
    pub chain_name: String,
    /// Number of times the engine evaluated prices (scanned for opportunities).
    pub total_scans: u64,
    /// Number of times an execution was attempted (opportunity found + profitable).
    pub total_attempts: u64,
    pub total_success: u64,
    pub total_failed: u64,
    pub total_profit_usd: f64,
    pub dry_run: bool,
    pub paused: bool,
    /// True if the chain's RPC connection was healthy at last check.
    pub rpc_ok: bool,
    /// Last block number seen from the block-header subscription (0 = not yet seen).
    pub last_block: u64,
}

#[derive(Debug, Default)]
pub struct EngineState {
    pub chains: Vec<ChainStats>,
    pub dry_run: bool,
    pub paused: bool,
    pub uptime_start: u64,
    pub tokens: Vec<Token>,
    /// Pairs stored as (id, path) for the dashboard's /tokens/pairs endpoint
    pub pairs: Vec<(String, String)>,
}

pub type SharedState = Arc<RwLock<EngineState>>;
pub type LogBroadcaster = broadcast::Sender<LogEntry>;

// ─── API server ───────────────────────────────────────────────────────────────

pub struct ApiServer {
    pub port: u16,
    pub api_key: String,
    pub state: SharedState,
    pub log_tx: LogBroadcaster,
    pub db: Option<Arc<Database>>,
    pub metrics: Arc<Metrics>,
}

impl ApiServer {
    pub fn new(port: u16, api_key: String, db: Option<Arc<Database>>, metrics: Arc<Metrics>, dry_run: bool) -> Self {
        let (log_tx, _) = broadcast::channel(1024);
        Self {
            port,
            api_key,
            state: Arc::new(RwLock::new(EngineState {
                uptime_start: now_secs(),
                dry_run,
                ..Default::default()
            })),
            log_tx,
            db,
            metrics,
        }
    }

    pub fn log_sender(&self) -> LogBroadcaster {
        self.log_tx.clone()
    }

    pub fn shared_state(&self) -> SharedState {
        self.state.clone()
    }

    pub async fn run(self) {
        let addr = format!("0.0.0.0:{}", self.port);
        let state = Arc::new(AppState {
            engine: self.state,
            log_tx: self.log_tx,
            api_key: self.api_key,
            db: self.db,
            metrics: self.metrics,
        });

        let app = Router::new()
            .route("/health", get(health))
            .route("/metrics", get(metrics_handler))
            .route("/stats", get(stats))
            .route("/stats/pairs", get(pair_stats))
            .route("/trades", get(trades_list))
            .route("/ws", get(ws_handler))
            .route("/engine/pause", post(engine_pause))
            .route("/engine/resume", post(engine_resume))
            .route("/engine/dry-run", post(engine_dry_run))
            .route("/tokens", get(tokens_list).post(token_add))
            .route("/tokens/pairs", get(tokens_pairs))
            .route("/tokens/{chain_id}/{address}", patch(token_trust).delete(token_remove))
            .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
            .fallback_service(ServeDir::new("dist").append_index_html_on_directories(true))
            .layer(CorsLayer::permissive())
            .with_state(state);

        info!("API server listening on {}", addr);
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    }
}

// ─── App state ────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    engine: SharedState,
    log_tx: LogBroadcaster,
    api_key: String,
    db: Option<Arc<Database>>,
    metrics: Arc<Metrics>,
}

// ─── Auth middleware ───────────────────────────────────────────────────────────

/// Bearer token auth. Skipped if api_key is empty (local dev).
/// Accepts the key in two ways:
///   1. `Authorization: Bearer <key>` header (REST clients, curl)
///   2. `?token=<key>` query param (browsers — WebSocket cannot set custom headers)
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if state.api_key.is_empty() {
        return next.run(req).await;
    }

    // Check Authorization header
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    // Check ?token= query param (used by browser WebSocket which can't set headers)
    let query_token = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&').find_map(|kv| {
                let mut parts = kv.splitn(2, '=');
                if parts.next()? == "token" {
                    parts.next().map(|v| urlencoding_decode(v))
                } else {
                    None
                }
            })
        });

    let provided = bearer.map(str::to_owned).or(query_token);

    if provided.as_deref() != Some(state.api_key.as_str()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Unauthorized" })),
        )
            .into_response();
    }

    next.run(req).await
}

/// Minimal percent-decode for URL query params (`%20` → ` `, `+` → ` `).
/// Handles the common case without pulling in an extra crate.
fn urlencoding_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '+' {
            out.push(' ');
        } else if c == '%' {
            let hi = chars.next();
            let lo = chars.next();
            if let (Some(h), Some(l)) = (hi, lo) {
                if let Ok(byte) = u8::from_str_radix(&format!("{h}{l}"), 16) {
                    out.push(byte as char);
                    continue;
                }
            }
            out.push('%');
        } else {
            out.push(c);
        }
    }
    out
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let engine = state.engine.read().await;
    let uptime = now_secs() - engine.uptime_start;
    let all_rpc_ok = !engine.chains.is_empty() && engine.chains.iter().all(|c| c.rpc_ok);
    let status = if engine.chains.is_empty() || all_rpc_ok { "ok" } else { "degraded" };
    let chain_health: Vec<_> = engine.chains.iter().map(|c| serde_json::json!({
        "chain_id":   c.chain_id,
        "chain_name": c.chain_name,
        "rpc_ok":     c.rpc_ok,
        "last_block": c.last_block,
        "paused":     c.paused,
    })).collect();
    let code = if all_rpc_ok || engine.chains.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(serde_json::json!({
        "status":         status,
        "uptime_seconds": uptime,
        "dry_run":        engine.dry_run,
        "paused":         engine.paused,
        "chains":         chain_health,
    }))).into_response()
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let body = state.metrics.gather();
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}

async fn stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let engine = state.engine.read().await;
    Json(serde_json::json!({
        "chains": engine.chains,
        "dry_run": engine.dry_run,
        "paused": engine.paused,
        "uptime_seconds": now_secs() - engine.uptime_start,
    }))
}

async fn pair_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(db) = state.db.clone() else {
        return Json(serde_json::json!({ "pairs": [] })).into_response();
    };
    match tokio::task::spawn_blocking(move || db.get_pair_stats()).await {
        Ok(Ok(records)) => Json(serde_json::json!({ "pairs": records })).into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "db error" })),
        )
            .into_response(),
    }
}

// ─── Trades endpoint ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TradesQuery {
    limit: Option<i64>,
}

async fn trades_list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TradesQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(500).min(5000);
    let Some(db) = state.db.clone() else {
        return Json(serde_json::json!({ "trades": [] })).into_response();
    };
    match tokio::task::spawn_blocking(move || db.get_trades(limit)).await {
        Ok(Ok(records)) => Json(serde_json::json!({ "trades": records })).into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "db error" })),
        )
            .into_response(),
    }
}

async fn engine_pause(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut engine = state.engine.write().await;
    engine.paused = true;
    Json(serde_json::json!({ "paused": true }))
}

async fn engine_resume(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut engine = state.engine.write().await;
    engine.paused = false;
    Json(serde_json::json!({ "paused": false }))
}

#[derive(Deserialize)]
struct DryRunBody {
    enabled: bool,
}

async fn engine_dry_run(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DryRunBody>,
) -> impl IntoResponse {
    let mut engine = state.engine.write().await;
    engine.dry_run = body.enabled;
    Json(serde_json::json!({ "dry_run": body.enabled }))
}

// ─── Token endpoints ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ChainFilter {
    chain_id: Option<u64>,
}

async fn tokens_list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ChainFilter>,
) -> impl IntoResponse {
    let engine = state.engine.read().await;
    let tokens: Vec<&Token> = engine.tokens.iter()
        .filter(|t| q.chain_id.map_or(true, |cid| t.chain_id == cid))
        .collect();
    Json(serde_json::json!({ "tokens": tokens }))
}

async fn tokens_pairs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ChainFilter>,
) -> impl IntoResponse {
    let engine = state.engine.read().await;
    let pairs: Vec<serde_json::Value> = engine.pairs.iter()
        .filter(|(_id, _)| q.chain_id.map_or(true, |cid| {
            // pair id encodes chain via token symbols; no chain filter needed for demo
            let _ = cid; true
        }))
        .map(|(id, path)| serde_json::json!({ "id": id, "path": path }))
        .collect();
    Json(serde_json::json!({ "pairs": pairs }))
}

#[derive(Deserialize)]
struct NewToken {
    symbol: String,
    address: String,
    decimals: Option<u8>,
    chain_id: u64,
    category: Option<String>,
}

async fn token_add(
    State(state): State<Arc<AppState>>,
    Json(body): Json<NewToken>,
) -> impl IntoResponse {
    let mut engine = state.engine.write().await;
    // Avoid duplicates
    if !engine.tokens.iter().any(|t| t.address.to_lowercase() == body.address.to_lowercase() && t.chain_id == body.chain_id) {
        engine.tokens.push(Token {
            symbol: body.symbol,
            address: body.address,
            chain_id: body.chain_id,
            decimals: body.decimals.unwrap_or(18),
            trusted: false,
            category: body.category.unwrap_or_else(|| "other".into()),
        });
    }
    (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct TrustBody {
    trusted: bool,
}

async fn token_trust(
    State(state): State<Arc<AppState>>,
    Path((chain_id, address)): Path<(u64, String)>,
    Json(body): Json<TrustBody>,
) -> impl IntoResponse {
    let mut engine = state.engine.write().await;
    if let Some(t) = engine.tokens.iter_mut().find(|t| t.chain_id == chain_id && t.address.to_lowercase() == address.to_lowercase()) {
        t.trusted = body.trusted;
    }
    (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
}

async fn token_remove(
    State(state): State<Arc<AppState>>,
    Path((chain_id, address)): Path<(u64, String)>,
) -> impl IntoResponse {
    let mut engine = state.engine.write().await;
    engine.tokens.retain(|t| !(t.chain_id == chain_id && t.address.to_lowercase() == address.to_lowercase()));
    (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
}

// ─── WebSocket ────────────────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    let log_tx = state.log_tx.clone();
    ws.on_upgrade(move |socket| handle_ws(socket, log_tx))
}

async fn handle_ws(mut socket: WebSocket, log_tx: LogBroadcaster) {
    let mut rx = log_tx.subscribe();
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    heartbeat.tick().await; // consume the first immediate tick
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(entry) => {
                        if let Ok(msg) = serde_json::to_string(&entry) {
                            if socket.send(Message::Text(msg.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            _ = heartbeat.tick() => {
                // Send a ping to keep the connection alive and confirm to the browser
                // that the engine is still running even when no log entries are emitted.
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
            // Drain all incoming messages from the browser — we never act on them
            // but MUST read them to prevent the TCP receive buffer from filling up.
            // Without this, Pong frames (responses to our Ping) + any browser-sent
            // commands accumulate → backpressure → connection stalls and drops.
            msg = socket.recv() => {
                match msg {
                    None | Some(Err(_)) => break,           // socket closed / error
                    Some(Ok(Message::Close(_))) => break,   // explicit close frame
                    Some(Ok(_)) => {}                       // discard everything else
                }
            }
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Send a log entry to all WebSocket clients and tracing.
pub fn broadcast_log(tx: &LogBroadcaster, level: &str, message: &str, data: Option<serde_json::Value>) {
    let entry = LogEntry {
        level: level.to_string(),
        message: message.to_string(),
        timestamp: now_secs(),
        data,
    };
    let _ = tx.send(entry); // OK if no subscribers
}
