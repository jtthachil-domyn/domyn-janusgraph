//! HTTP/REST API for Domyn Nexus.
//!
//! Endpoints:
//!   POST /cypher       -- execute a Cypher query (tenant-aware)
//!   GET  /status       -- server status
//!   GET  /schema       -- graph schema (labels, property keys)
//!   GET  /health       -- load-balancer health check
//!   GET  /tenants      -- list registered tenants
//!   POST /tenants      -- register a new tenant

use crate::engine::{EngineBuilder, MultiTenantEngine, NexusEngine};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use nexus_core::types::Value;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{Duration, timeout};

#[derive(Deserialize)]
pub struct CypherRequest {
    pub query: String,
    pub tenant: Option<String>,
    pub params: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Serialize)]
pub struct CypherResponse {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub time_ms: f64,
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub engine: String,
    pub version: String,
    pub vertices: usize,
    pub edges: u64,
}

#[derive(Serialize)]
pub struct SchemaResponse {
    pub vertex_labels: Vec<String>,
    pub edge_labels: Vec<String>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
}

#[derive(Deserialize)]
pub struct CreateTenantRequest {
    pub tenant_id: String,
    pub vertex_capacity: Option<usize>,
    pub edge_capacity: Option<usize>,
}

#[derive(Serialize, Deserialize)]
pub struct TenantListResponse {
    pub tenants: Vec<String>,
}

const QUERY_TIMEOUT_SECS: u64 = 30;
const MAX_BODY_BYTES: usize = 1024 * 1024; // 1 MiB

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorIndexMode {
    Exact,
    Hnsw,
}

#[derive(Clone)]
pub struct ServerConfig {
    pub query_timeout_secs: u64,
    pub max_body_bytes: usize,
    pub auth_token: Option<String>,
    pub wal_segment_bytes: u64,
    pub wal_retention_segments: usize,
    pub snapshot_retention: usize,
    pub compaction_threshold: usize,
    pub query_memory_budget_bytes: usize,
    pub default_query_limit: usize,
    pub vector_index_mode: VectorIndexMode,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            query_timeout_secs: QUERY_TIMEOUT_SECS,
            max_body_bytes: MAX_BODY_BYTES,
            auth_token: None,
            wal_segment_bytes: 64 * 1024 * 1024,
            wal_retention_segments: 8,
            snapshot_retention: 2,
            compaction_threshold: 4,
            query_memory_budget_bytes: 512 * 1024 * 1024,
            default_query_limit: 10_000,
            vector_index_mode: VectorIndexMode::Exact,
        }
    }
}

#[derive(Clone)]
struct AppState {
    engine: Arc<MultiTenantEngine>,
    config: Arc<ServerConfig>,
}

/// Build a single-tenant router (backwards-compatible).
///
/// Wraps the given engine as the "default" tenant inside a `MultiTenantEngine`,
/// so all existing callers continue to work without changes.
pub fn create_router(engine: Arc<NexusEngine>) -> Router {
    let mt = MultiTenantEngine::new_with_arc("default", engine);
    create_multi_tenant_router(Arc::new(mt))
}

/// Build a multi-tenant router with all endpoints.
pub fn create_multi_tenant_router(engine: Arc<MultiTenantEngine>) -> Router {
    create_multi_tenant_router_with_config(engine, ServerConfig::default())
}

pub fn create_multi_tenant_router_with_config(
    engine: Arc<MultiTenantEngine>,
    config: ServerConfig,
) -> Router {
    let max_body_bytes = config.max_body_bytes;
    let state = Arc::new(AppState {
        engine,
        config: Arc::new(config),
    });

    Router::new()
        .route("/cypher", post(handle_cypher))
        .route("/status", get(handle_status))
        .route("/schema", get(handle_schema))
        .route("/health", get(handle_health))
        .route("/ready", get(handle_ready))
        .route("/metrics", get(handle_metrics))
        .route(
            "/tenants",
            get(handle_list_tenants).post(handle_create_tenant),
        )
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}

/// Start the HTTP server with graceful shutdown support.
pub async fn run_http_server(
    router: Router,
    bind_addr: &str,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
}

async fn handle_cypher(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CypherRequest>,
) -> Result<Json<CypherResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(&state.config, &headers)?;

    let engine = state
        .engine
        .resolve(req.tenant.as_deref())
        .map_err(|e| (StatusCode::NOT_FOUND, Json(ErrorResponse { error: e })))?;

    let query = req.query.clone();
    let params: HashMap<String, Value> = req
        .params
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, json_to_value(v)))
        .collect();
    let start = std::time::Instant::now();

    let result = timeout(
        Duration::from_secs(state.config.query_timeout_secs),
        tokio::task::spawn_blocking(move || engine.execute_cypher_with_params(&query, params)),
    )
    .await;

    match result {
        Ok(Ok(Ok(qr))) => {
            let rows: Vec<Vec<serde_json::Value>> = qr
                .rows
                .iter()
                .map(|row| row.iter().map(value_to_json).collect())
                .collect();

            Ok(Json(CypherResponse {
                columns: qr.columns,
                rows,
                time_ms: start.elapsed().as_secs_f64() * 1000.0,
            }))
        }
        Ok(Ok(Err(e))) => Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("query task panicked: {e}"),
            }),
        )),
        Err(_) => Err((
            StatusCode::GATEWAY_TIMEOUT,
            Json(ErrorResponse {
                error: format!("query timed out after {}s", state.config.query_timeout_secs),
            }),
        )),
    }
}

async fn handle_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(&state.config, &headers)?;

    let (vertices, edges) = state
        .engine
        .default_engine()
        .map(|e| (e.vertex_count(), e.edge_count()))
        .unwrap_or((0, 0));

    Ok(Json(StatusResponse {
        engine: "Domyn Nexus".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        vertices,
        edges,
    }))
}

async fn handle_schema(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<SchemaResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(&state.config, &headers)?;

    let (vertex_labels, edge_labels) = state
        .engine
        .default_engine()
        .map(|e| (e.vertex_labels(), e.edge_labels()))
        .unwrap_or_default();

    Ok(Json(SchemaResponse {
        vertex_labels,
        edge_labels,
    }))
}

async fn handle_health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
    })
}

async fn handle_ready(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    if state.engine.default_engine().is_some() || !state.engine.list_tenants().is_empty() {
        Json(HealthResponse {
            status: "ready".into(),
        })
    } else {
        Json(HealthResponse {
            status: "empty".into(),
        })
    }
}

async fn handle_metrics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    authorize(&state.config, &headers)?;

    let (vertices, edges) = state
        .engine
        .default_engine()
        .map(|e| (e.vertex_count(), e.edge_count()))
        .unwrap_or((0, 0));
    let tenants = state.engine.list_tenants().len();
    let body = format!(
        "# TYPE domyn_nexus_vertices gauge\n\
         domyn_nexus_vertices {vertices}\n\
         # TYPE domyn_nexus_edges gauge\n\
         domyn_nexus_edges {edges}\n\
         # TYPE domyn_nexus_tenants gauge\n\
         domyn_nexus_tenants {tenants}\n"
    );

    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response())
}

async fn handle_list_tenants(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<TenantListResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(&state.config, &headers)?;

    Ok(Json(TenantListResponse {
        tenants: state.engine.list_tenants(),
    }))
}

async fn handle_create_tenant(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateTenantRequest>,
) -> Result<(StatusCode, Json<TenantListResponse>), (StatusCode, Json<ErrorResponse>)> {
    authorize(&state.config, &headers)?;

    if state.engine.get_engine(&req.tenant_id).is_some() {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: format!("tenant already exists: {}", req.tenant_id),
            }),
        ));
    }

    let vcap = req.vertex_capacity.unwrap_or(1024);
    let ecap = req.edge_capacity.unwrap_or(1024);
    let builder = EngineBuilder::new(vcap, ecap);
    state
        .engine
        .register_tenant(&req.tenant_id, builder.build());

    Ok((
        StatusCode::CREATED,
        Json(TenantListResponse {
            tenants: state.engine.list_tenants(),
        }),
    ))
}

fn authorize(
    config: &ServerConfig,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let Some(expected) = config.auth_token.as_deref() else {
        return Ok(());
    };

    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());

    if bearer == Some(expected) || api_key == Some(expected) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".into(),
            }),
        ))
    }
}

fn json_to_value(v: serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int64(i)
            } else {
                Value::Float64(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(arr) => Value::List(arr.into_iter().map(json_to_value).collect()),
        serde_json::Value::Object(map) => Value::Map(
            map.into_iter()
                .map(|(k, v)| (k, json_to_value(v)))
                .collect(),
        ),
    }
}

fn value_to_json(v: &nexus_core::types::Value) -> serde_json::Value {
    use nexus_core::types::Value;
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int64(i) => serde_json::json!(i),
        Value::Float64(f) => serde_json::json!(f),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(b) => serde_json::json!(format!("<{} bytes>", b.len())),
        Value::List(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Map(entries) => {
            let map: serde_json::Map<String, serde_json::Value> = entries
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineBuilder;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use nexus_core::properties::PropertyType;
    use tower::ServiceExt;

    fn test_engine() -> Arc<NexusEngine> {
        let mut builder = EngineBuilder::new(4, 4);
        builder.register_vertex_property("name", PropertyType::String, true, false);

        let v1 = builder.add_vertex("Entity");
        builder.set_vertex_property(v1, "name", "Apple".into());

        let v2 = builder.add_vertex("Entity");
        builder.set_vertex_property(v2, "name", "Google".into());

        Arc::new(builder.build())
    }

    fn test_mt_engine() -> Arc<MultiTenantEngine> {
        let mut builder = EngineBuilder::new(4, 4);
        builder.register_vertex_property("name", PropertyType::String, true, false);

        let v1 = builder.add_vertex("Entity");
        builder.set_vertex_property(v1, "name", "Apple".into());

        let v2 = builder.add_vertex("Entity");
        builder.set_vertex_property(v2, "name", "Google".into());

        Arc::new(MultiTenantEngine::with_default("default", builder.build()))
    }

    #[test]
    fn server_config_exposes_production_hardening_knobs() {
        let config = ServerConfig::default();
        assert_eq!(config.query_timeout_secs, QUERY_TIMEOUT_SECS);
        assert_eq!(config.max_body_bytes, MAX_BODY_BYTES);
        assert_eq!(config.wal_retention_segments, 8);
        assert_eq!(config.snapshot_retention, 2);
        assert_eq!(config.compaction_threshold, 4);
        assert_eq!(config.vector_index_mode, VectorIndexMode::Exact);
    }

    #[tokio::test]
    async fn test_status_endpoint() {
        let engine = test_engine();
        let app = create_router(engine);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_cypher_endpoint() {
        let engine = test_engine();
        let app = create_router(engine);

        let body = serde_json::json!({ "query": "MATCH (n:Entity) RETURN n" });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_schema_endpoint() {
        let engine = test_engine();
        let app = create_router(engine);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/schema")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let mt = test_mt_engine();
        let app = create_multi_tenant_router(mt);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let health: HealthResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(health.status, "ok".to_string());
    }

    #[tokio::test]
    async fn test_auth_required_when_configured() {
        let mt = test_mt_engine();
        let app = create_multi_tenant_router_with_config(
            mt,
            ServerConfig {
                auth_token: Some("secret".into()),
                ..ServerConfig::default()
            },
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_metrics_endpoint() {
        let mt = test_mt_engine();
        let app = create_multi_tenant_router(mt);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("domyn_nexus_vertices"));
        assert!(text.contains("domyn_nexus_tenants"));
    }

    #[tokio::test]
    async fn test_list_tenants_endpoint() {
        let mt = test_mt_engine();
        let app = create_multi_tenant_router(mt);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/tenants")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: TenantListResponse = serde_json::from_slice(&body).unwrap();
        assert!(list.tenants.contains(&"default".to_string()));
    }

    #[tokio::test]
    async fn test_create_tenant_endpoint() {
        let mt = test_mt_engine();
        let app = create_multi_tenant_router(mt);

        let body = serde_json::json!({ "tenant_id": "AAPL" });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tenants")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: TenantListResponse = serde_json::from_slice(&body).unwrap();
        assert!(list.tenants.contains(&"AAPL".to_string()));
    }

    #[tokio::test]
    async fn test_cypher_unknown_tenant_returns_404() {
        let mt = test_mt_engine();
        let app = create_multi_tenant_router(mt);

        let body = serde_json::json!({ "query": "MATCH (n) RETURN n", "tenant": "MISSING" });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
