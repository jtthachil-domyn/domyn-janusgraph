//! HTTP/REST API for Domyn Nexus.
//!
//! Endpoints:
//!   POST /cypher       -- execute a Cypher query (tenant-aware)
//!   GET  /status       -- server status
//!   GET  /schema       -- graph schema (labels, property keys)
//!   GET  /health       -- load-balancer health check
//!   GET  /tenants      -- list registered tenants
//!   POST /tenants      -- register a new tenant
//!   POST /admin/backup -- create a hot backup for a durable engine
//!   POST /admin/compact -- compact tombstones and snapshot the graph
//!   GET  /vectors      -- list named vector indexes
//!   POST /vectors/{name} -- create/load a named vector index
//!   POST /vectors/{name}/upsert -- upsert an embedding
//!   POST /vectors/{name}/search -- nearest-neighbor search
//!   DELETE /vectors/{name}/{vertex_id} -- remove an embedding
//!   GET/PUT/DELETE /collections/{collection}/documents/{key} -- JSON documents
//!   POST /tx/batch -- one atomic graph+document+vector write batch

use crate::engine::{
    DocumentMutation, EngineBuilder, MultiTenantEngine, NexusEngine, VectorMutation,
    VectorSearchMetrics,
};
use crate::tls::{TlsConfig, TlsListener};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, FromRequest, Path, Query, Request, State, rejection::JsonRejection,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use nexus_core::graph::{GraphCompactionPressure, GraphCompactionStats};
use nexus_core::transaction::TxError;
use nexus_core::types::{Value, VertexId};
use nexus_cypher::ast::Statement;
use nexus_cypher::parser::Parser;
use nexus_parser::{NexusParser, ParsedStatement};
use nexus_storage::persistence::{
    BackupManifest, DocumentFullTextQuery, DocumentFullTextRanking, DocumentIndexKind,
    DocumentIndexQuery, DocumentRecord, DocumentSearchHit, StoreOptions,
};
use nexus_storage::wal::WalOptions;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Duration, timeout};

#[derive(Deserialize)]
pub struct CypherRequest {
    pub query: String,
    pub tenant: Option<String>,
    pub params: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Clone, Serialize)]
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

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    pub details: serde_json::Value,
    /// Compatibility alias for earlier beta clients. New clients should use
    /// `message`; this field will stay populated through Internal Beta.
    pub error: String,
}

impl ErrorResponse {
    fn coded(code: &'static str, error: impl Into<String>) -> Self {
        let message = error.into();
        Self::coded_with_details(code, message, serde_json::json!({}))
    }

    fn coded_with_details(
        code: &'static str,
        error: impl Into<String>,
        details: serde_json::Value,
    ) -> Self {
        let message = error.into();
        Self {
            code: Some(code.into()),
            message: message.clone(),
            details,
            error: message,
        }
    }
}

struct ProductJson<T>(T);

impl<T, S> FromRequest<S> for ProductJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<ErrorResponse>);

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(value)| ProductJson(value))
            .map_err(product_json_rejection)
    }
}

fn product_json_rejection(rejection: JsonRejection) -> (StatusCode, Json<ErrorResponse>) {
    let status = rejection.status();
    let code = match status {
        StatusCode::PAYLOAD_TOO_LARGE => "REQUEST_BODY_TOO_LARGE",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "JSON_CONTENT_TYPE_REQUIRED",
        _ => "JSON_REQUEST_INVALID",
    };
    (
        status,
        Json(ErrorResponse::coded_with_details(
            code,
            rejection.to_string(),
            serde_json::json!({
                "status": status.as_u16()
            }),
        )),
    )
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

#[derive(Deserialize)]
pub struct CompactRequest {
    pub tenant: Option<String>,
    pub threshold: Option<usize>,
}

#[derive(Deserialize)]
pub struct BackupRequest {
    pub tenant: Option<String>,
    pub path: String,
}

#[derive(Deserialize)]
pub struct TenantQuery {
    pub tenant: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateVectorIndexRequest {
    pub tenant: Option<String>,
    pub dimension: usize,
}

#[derive(Deserialize)]
pub struct VectorUpsertRequest {
    pub tenant: Option<String>,
    pub vertex_id: u64,
    pub embedding: Vec<f32>,
}

#[derive(Deserialize)]
pub struct VectorSearchRequest {
    pub tenant: Option<String>,
    pub query: Vec<f32>,
    pub k: Option<usize>,
}

#[derive(Deserialize)]
pub struct VectorCompactRequest {
    pub tenant: Option<String>,
}

#[derive(Deserialize)]
pub struct DocumentUpsertRequest {
    pub tenant: Option<String>,
    pub document: serde_json::Value,
}

#[derive(Deserialize)]
pub struct DocumentListQuery {
    pub tenant: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct DocumentIndexCreateRequest {
    pub tenant: Option<String>,
    pub path: String,
    pub kind: Option<DocumentIndexKind>,
}

#[derive(Deserialize)]
pub struct DocumentIndexSearchRequest {
    pub tenant: Option<String>,
    pub path: String,
    pub value: Option<serde_json::Value>,
    pub prefix: Option<String>,
    pub text: Option<String>,
    pub phrase: Option<bool>,
    pub fuzzy_distance: Option<u8>,
    pub stem: Option<bool>,
    pub ranking: Option<DocumentFullTextRanking>,
    pub snippets: Option<bool>,
    pub explain: Option<bool>,
    pub gte: Option<serde_json::Value>,
    pub lte: Option<serde_json::Value>,
    pub limit: Option<usize>,
}

#[derive(Serialize, Deserialize)]
pub struct DocumentCollectionsResponse {
    pub collections: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct DocumentIndexResponse {
    pub collection: String,
    pub path: String,
    pub kind: DocumentIndexKind,
}

#[derive(Serialize, Deserialize)]
pub struct DocumentIndexListResponse {
    pub indexes: Vec<DocumentIndexResponse>,
}

#[derive(Serialize, Deserialize)]
pub struct DocumentRecordResponse {
    pub collection: String,
    pub key: String,
    pub document: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct DocumentListResponse {
    pub documents: Vec<DocumentRecordResponse>,
}

fn document_record_response(record: DocumentRecord) -> DocumentRecordResponse {
    DocumentRecordResponse {
        collection: record.collection,
        key: record.key,
        document: record.document,
        score: None,
        snippet: None,
        explanation: None,
    }
}

fn document_search_hit_response(hit: DocumentSearchHit) -> DocumentRecordResponse {
    DocumentRecordResponse {
        collection: hit.record.collection,
        key: hit.record.key,
        document: hit.record.document,
        score: Some(hit.score),
        snippet: hit.snippet,
        explanation: hit.explanation,
    }
}

#[derive(Serialize, Deserialize)]
pub struct DocumentAckResponse {
    pub ok: bool,
}

#[derive(Serialize, Deserialize)]
pub struct DocumentDeleteResponse {
    pub deleted: bool,
}

#[derive(Deserialize)]
pub struct TxBatchCypherRequest {
    pub query: String,
    pub params: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TxBatchDocumentOperation {
    Upsert {
        collection: String,
        key: String,
        document: serde_json::Value,
    },
    Delete {
        collection: String,
        key: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TxBatchVectorOperation {
    Upsert {
        index: String,
        vertex_id: u64,
        embedding: Vec<f32>,
    },
    Remove {
        index: String,
        vertex_id: u64,
    },
}

#[derive(Deserialize)]
pub struct TxBatchRequest {
    pub tenant: Option<String>,
    pub cypher: Option<TxBatchCypherRequest>,
    #[serde(default)]
    pub cypher_statements: Vec<TxBatchCypherRequest>,
    #[serde(default)]
    pub documents: Vec<TxBatchDocumentOperation>,
    #[serde(default)]
    pub vectors: Vec<TxBatchVectorOperation>,
}

#[derive(Serialize)]
pub struct TxBatchResponse {
    pub committed: bool,
    pub document_ops: usize,
    pub vector_ops: usize,
    pub cypher: Option<CypherResponse>,
    pub cypher_results: Vec<CypherResponse>,
    pub time_ms: f64,
}

#[derive(Serialize, Deserialize)]
pub struct VectorIndexListResponse {
    pub indexes: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct VectorAckResponse {
    pub ok: bool,
}

#[derive(Serialize, Deserialize)]
pub struct VectorRemoveResponse {
    pub removed: bool,
}

#[derive(Serialize, Deserialize)]
pub struct VectorSearchHit {
    pub vertex_id: u64,
    pub distance: f32,
}

#[derive(Serialize, Deserialize)]
pub struct VectorSearchResponse {
    pub results: Vec<VectorSearchHit>,
}

#[derive(Serialize, Deserialize)]
pub struct CompactResponse {
    pub compacted: bool,
    pub pressure_before: CompactionPressureResponse,
    pub pressure_after: CompactionPressureResponse,
    pub stats: Option<CompactionStatsResponse>,
}

#[derive(Serialize, Deserialize)]
pub struct BackupResponse {
    pub path: String,
    pub manifest: BackupManifest,
}

#[derive(Serialize, Deserialize)]
pub struct CompactionPressureResponse {
    pub deleted_vertices: usize,
    pub tombstoned_edges: usize,
    pub delta_edges: usize,
}

#[derive(Serialize, Deserialize)]
pub struct CompactionStatsResponse {
    pub deleted_vertices_cleared: usize,
    pub deleted_edges_cleared: usize,
    pub tombstoned_edge_meta_removed: usize,
    pub delta_edges_compacted: usize,
    pub live_edges_after: usize,
}

#[derive(Serialize, Deserialize)]
pub struct TenantListResponse {
    pub tenants: Vec<String>,
}

const QUERY_TIMEOUT_SECS: u64 = 30;
const MAX_BODY_BYTES: usize = 1024 * 1024; // 1 MiB

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VectorIndexMode {
    Exact,
    Hnsw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthRole {
    ReadOnly,
    ReadWrite,
    Admin,
}

impl AuthRole {
    fn allows(self, required: AuthRole) -> bool {
        match self {
            AuthRole::Admin => true,
            AuthRole::ReadWrite => matches!(required, AuthRole::ReadOnly | AuthRole::ReadWrite),
            AuthRole::ReadOnly => matches!(required, AuthRole::ReadOnly),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPrincipalConfig {
    pub name: String,
    pub token: String,
    pub role: AuthRole,
    #[serde(default)]
    pub tenants: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Principal {
    name: String,
    role: AuthRole,
    tenants: Vec<String>,
}

impl Principal {
    fn anonymous() -> Self {
        Self {
            name: "anonymous".into(),
            role: AuthRole::Admin,
            tenants: Vec::new(),
        }
    }

    fn legacy_admin() -> Self {
        Self {
            name: "legacy-auth-token".into(),
            role: AuthRole::Admin,
            tenants: Vec::new(),
        }
    }

    fn from_config(config: &AuthPrincipalConfig) -> Self {
        Self {
            name: config.name.clone(),
            role: config.role,
            tenants: config.tenants.clone(),
        }
    }

    fn can_access_tenant(&self, tenant: Option<&str>) -> bool {
        if self.tenants.is_empty() || self.tenants.iter().any(|t| t == "*") {
            return true;
        }

        let tenant = tenant.unwrap_or("default");
        self.tenants.iter().any(|allowed| allowed == tenant)
    }
}

#[derive(Clone)]
pub struct ServerConfig {
    pub query_timeout_secs: u64,
    pub max_body_bytes: usize,
    pub max_concurrent_queries: usize,
    pub max_query_rate_per_sec: u64,
    pub max_query_rate_per_tenant_per_sec: u64,
    pub slow_query_ms: u64,
    pub auth_token: Option<String>,
    pub auth_principals: Vec<AuthPrincipalConfig>,
    pub wal_segment_bytes: u64,
    pub wal_retention_segments: usize,
    pub snapshot_retention: usize,
    pub compaction_threshold: usize,
    pub query_memory_budget_bytes: usize,
    pub process_memory_budget_bytes: usize,
    #[cfg(test)]
    pub process_memory_budget_sample_bytes: Option<u64>,
    pub default_query_limit: usize,
    pub vector_index_mode: VectorIndexMode,
    pub backup_root: Option<String>,
    pub audit_log_path: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            query_timeout_secs: QUERY_TIMEOUT_SECS,
            max_body_bytes: MAX_BODY_BYTES,
            max_concurrent_queries: 128,
            max_query_rate_per_sec: 0,
            max_query_rate_per_tenant_per_sec: 0,
            slow_query_ms: 1000,
            auth_token: None,
            auth_principals: Vec::new(),
            wal_segment_bytes: 64 * 1024 * 1024,
            wal_retention_segments: 8,
            snapshot_retention: 2,
            compaction_threshold: 4,
            query_memory_budget_bytes: 512 * 1024 * 1024,
            process_memory_budget_bytes: 0,
            #[cfg(test)]
            process_memory_budget_sample_bytes: None,
            default_query_limit: 10_000,
            vector_index_mode: VectorIndexMode::Exact,
            backup_root: None,
            audit_log_path: None,
        }
    }
}

impl ServerConfig {
    pub fn wal_options(&self) -> WalOptions {
        WalOptions {
            max_segment_bytes: (self.wal_segment_bytes > 0).then_some(self.wal_segment_bytes),
            retained_archived_segments: self.wal_retention_segments,
            ..WalOptions::default()
        }
    }

    pub fn store_options(&self) -> StoreOptions {
        StoreOptions {
            wal: self.wal_options(),
            snapshot_retention: self.snapshot_retention,
        }
    }
}

#[derive(Clone)]
struct AppState {
    engine: Arc<MultiTenantEngine>,
    config: Arc<ServerConfig>,
    compaction: Arc<CompactionRuntime>,
    backup: Arc<BackupRuntime>,
    queries: Arc<QueryRuntime>,
    query_permits: Option<Arc<Semaphore>>,
    rate_limit: Arc<RateLimitRuntime>,
    audit: Arc<AuditRuntime>,
}

#[derive(Default)]
struct CompactionRuntime {
    active: AtomicBool,
    scheduled_total: AtomicU64,
    completed_total: AtomicU64,
    failed_total: AtomicU64,
    skipped_active_total: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CompactionRuntimeSnapshot {
    active: bool,
    scheduled_total: u64,
    completed_total: u64,
    failed_total: u64,
    skipped_active_total: u64,
}

impl CompactionRuntime {
    fn snapshot(&self) -> CompactionRuntimeSnapshot {
        CompactionRuntimeSnapshot {
            active: self.active.load(Ordering::Acquire),
            scheduled_total: self.scheduled_total.load(Ordering::Relaxed),
            completed_total: self.completed_total.load(Ordering::Relaxed),
            failed_total: self.failed_total.load(Ordering::Relaxed),
            skipped_active_total: self.skipped_active_total.load(Ordering::Relaxed),
        }
    }
}

#[derive(Default)]
struct BackupRuntime {
    started_total: AtomicU64,
    completed_total: AtomicU64,
    failed_total: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct BackupRuntimeSnapshot {
    started_total: u64,
    completed_total: u64,
    failed_total: u64,
}

impl BackupRuntime {
    fn snapshot(&self) -> BackupRuntimeSnapshot {
        BackupRuntimeSnapshot {
            started_total: self.started_total.load(Ordering::Relaxed),
            completed_total: self.completed_total.load(Ordering::Relaxed),
            failed_total: self.failed_total.load(Ordering::Relaxed),
        }
    }
}

#[derive(Default)]
struct AuditRuntime {
    events_total: AtomicU64,
    failed_total: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AuditRuntimeSnapshot {
    events_total: u64,
    failed_total: u64,
}

impl AuditRuntime {
    fn snapshot(&self) -> AuditRuntimeSnapshot {
        AuditRuntimeSnapshot {
            events_total: self.events_total.load(Ordering::Relaxed),
            failed_total: self.failed_total.load(Ordering::Relaxed),
        }
    }
}

#[derive(Serialize)]
struct AuditEvent<'a> {
    timestamp_unix_seconds: u64,
    principal: String,
    action: &'a str,
    outcome: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Default)]
struct QueryRuntime {
    active: AtomicU64,
    total: AtomicU64,
    succeeded_total: AtomicU64,
    failed_total: AtomicU64,
    timed_out_total: AtomicU64,
    limited_total: AtomicU64,
    rejected_total: AtomicU64,
    rate_limited_total: AtomicU64,
    memory_rejected_total: AtomicU64,
    slow_total: AtomicU64,
    rows_returned_total: AtomicU64,
    bytes_returned_total: AtomicU64,
    elapsed_ms_total: AtomicU64,
    latency_le_1_ms: AtomicU64,
    latency_le_5_ms: AtomicU64,
    latency_le_10_ms: AtomicU64,
    latency_le_50_ms: AtomicU64,
    latency_le_100_ms: AtomicU64,
    latency_le_500_ms: AtomicU64,
    latency_le_1000_ms: AtomicU64,
    latency_le_5000_ms: AtomicU64,
    latency_inf: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct QueryRuntimeSnapshot {
    active: u64,
    total: u64,
    succeeded_total: u64,
    failed_total: u64,
    timed_out_total: u64,
    limited_total: u64,
    rejected_total: u64,
    rate_limited_total: u64,
    memory_rejected_total: u64,
    slow_total: u64,
    rows_returned_total: u64,
    bytes_returned_total: u64,
    elapsed_ms_total: u64,
    latency_le_1_ms: u64,
    latency_le_5_ms: u64,
    latency_le_10_ms: u64,
    latency_le_50_ms: u64,
    latency_le_100_ms: u64,
    latency_le_500_ms: u64,
    latency_le_1000_ms: u64,
    latency_le_5000_ms: u64,
    latency_inf: u64,
}

impl QueryRuntime {
    fn begin(self: &Arc<Self>) -> QueryRuntimeGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
        QueryRuntimeGuard {
            runtime: self.clone(),
            finished: false,
        }
    }

    fn snapshot(&self) -> QueryRuntimeSnapshot {
        QueryRuntimeSnapshot {
            active: self.active.load(Ordering::Acquire),
            total: self.total.load(Ordering::Relaxed),
            succeeded_total: self.succeeded_total.load(Ordering::Relaxed),
            failed_total: self.failed_total.load(Ordering::Relaxed),
            timed_out_total: self.timed_out_total.load(Ordering::Relaxed),
            limited_total: self.limited_total.load(Ordering::Relaxed),
            rejected_total: self.rejected_total.load(Ordering::Relaxed),
            rate_limited_total: self.rate_limited_total.load(Ordering::Relaxed),
            memory_rejected_total: self.memory_rejected_total.load(Ordering::Relaxed),
            slow_total: self.slow_total.load(Ordering::Relaxed),
            rows_returned_total: self.rows_returned_total.load(Ordering::Relaxed),
            bytes_returned_total: self.bytes_returned_total.load(Ordering::Relaxed),
            elapsed_ms_total: self.elapsed_ms_total.load(Ordering::Relaxed),
            latency_le_1_ms: self.latency_le_1_ms.load(Ordering::Relaxed),
            latency_le_5_ms: self.latency_le_5_ms.load(Ordering::Relaxed),
            latency_le_10_ms: self.latency_le_10_ms.load(Ordering::Relaxed),
            latency_le_50_ms: self.latency_le_50_ms.load(Ordering::Relaxed),
            latency_le_100_ms: self.latency_le_100_ms.load(Ordering::Relaxed),
            latency_le_500_ms: self.latency_le_500_ms.load(Ordering::Relaxed),
            latency_le_1000_ms: self.latency_le_1000_ms.load(Ordering::Relaxed),
            latency_le_5000_ms: self.latency_le_5000_ms.load(Ordering::Relaxed),
            latency_inf: self.latency_inf.load(Ordering::Relaxed),
        }
    }

    fn reject_over_capacity(&self) {
        self.rejected_total.fetch_add(1, Ordering::Relaxed);
    }

    fn reject_rate_limited(&self) {
        self.rate_limited_total.fetch_add(1, Ordering::Relaxed);
    }

    fn reject_memory_pressure(&self) {
        self.rejected_total.fetch_add(1, Ordering::Relaxed);
        self.memory_rejected_total.fetch_add(1, Ordering::Relaxed);
    }

    fn record_slow_query(&self) {
        self.slow_total.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct RateLimitRuntime {
    windows: Mutex<HashMap<String, RateLimitWindow>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RateLimitWindow {
    start_secs: u64,
    count: u64,
}

impl RateLimitRuntime {
    fn admit(&self, key: &str, limit_per_sec: u64) -> bool {
        if limit_per_sec == 0 {
            return true;
        }

        let now = unix_secs();
        let mut windows = self.windows.lock().unwrap_or_else(|err| err.into_inner());
        let window = windows.entry(key.to_string()).or_default();
        if window.start_secs != now {
            window.start_secs = now;
            window.count = 0;
        }

        if window.count >= limit_per_sec {
            return false;
        }

        window.count += 1;
        true
    }
}

struct QueryRuntimeGuard {
    runtime: Arc<QueryRuntime>,
    finished: bool,
}

impl QueryRuntimeGuard {
    fn finish_success(mut self, rows: usize, bytes: usize, elapsed: Duration) {
        self.runtime.succeeded_total.fetch_add(1, Ordering::Relaxed);
        self.record_elapsed(elapsed);
        self.runtime
            .rows_returned_total
            .fetch_add(rows as u64, Ordering::Relaxed);
        self.runtime
            .bytes_returned_total
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.runtime
            .elapsed_ms_total
            .fetch_add(elapsed.as_millis() as u64, Ordering::Relaxed);
        self.finish();
    }

    fn finish_failure(mut self, elapsed: Duration) {
        self.runtime.failed_total.fetch_add(1, Ordering::Relaxed);
        self.record_elapsed(elapsed);
        self.finish();
    }

    fn finish_timeout(mut self, elapsed: Duration) {
        self.runtime.timed_out_total.fetch_add(1, Ordering::Relaxed);
        self.record_elapsed(elapsed);
        self.finish();
    }

    fn finish_limited(mut self, elapsed: Duration) {
        self.runtime.limited_total.fetch_add(1, Ordering::Relaxed);
        self.record_elapsed(elapsed);
        self.finish();
    }

    fn record_elapsed(&self, elapsed: Duration) {
        let ms = elapsed.as_millis() as u64;
        if ms <= 1 {
            self.runtime.latency_le_1_ms.fetch_add(1, Ordering::Relaxed);
        }
        if ms <= 5 {
            self.runtime.latency_le_5_ms.fetch_add(1, Ordering::Relaxed);
        }
        if ms <= 10 {
            self.runtime
                .latency_le_10_ms
                .fetch_add(1, Ordering::Relaxed);
        }
        if ms <= 50 {
            self.runtime
                .latency_le_50_ms
                .fetch_add(1, Ordering::Relaxed);
        }
        if ms <= 100 {
            self.runtime
                .latency_le_100_ms
                .fetch_add(1, Ordering::Relaxed);
        }
        if ms <= 500 {
            self.runtime
                .latency_le_500_ms
                .fetch_add(1, Ordering::Relaxed);
        }
        if ms <= 1000 {
            self.runtime
                .latency_le_1000_ms
                .fetch_add(1, Ordering::Relaxed);
        }
        if ms <= 5000 {
            self.runtime
                .latency_le_5000_ms
                .fetch_add(1, Ordering::Relaxed);
        }
        self.runtime.latency_inf.fetch_add(1, Ordering::Relaxed);
    }

    fn finish(&mut self) {
        if !self.finished {
            self.runtime.active.fetch_sub(1, Ordering::Relaxed);
            self.finished = true;
        }
    }
}

impl Drop for QueryRuntimeGuard {
    fn drop(&mut self) {
        self.finish();
    }
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
    let query_permits = (config.max_concurrent_queries > 0)
        .then(|| Arc::new(Semaphore::new(config.max_concurrent_queries)));
    let state = Arc::new(AppState {
        engine,
        config: Arc::new(config),
        compaction: Arc::new(CompactionRuntime::default()),
        backup: Arc::new(BackupRuntime::default()),
        queries: Arc::new(QueryRuntime::default()),
        query_permits,
        rate_limit: Arc::new(RateLimitRuntime::default()),
        audit: Arc::new(AuditRuntime::default()),
    });

    Router::new()
        .route("/cypher", post(handle_cypher))
        .route("/tx/batch", post(handle_tx_batch))
        .route("/status", get(handle_status))
        .route("/schema", get(handle_schema))
        .route("/health", get(handle_health))
        .route("/ready", get(handle_ready))
        .route("/metrics", get(handle_metrics))
        .route("/admin/compact", post(handle_compact))
        .route("/admin/backup", post(handle_backup))
        .route("/vectors", get(handle_list_vector_indexes))
        .route("/vectors/{name}", post(handle_create_vector_index))
        .route("/vectors/{name}/upsert", post(handle_upsert_vector))
        .route("/vectors/{name}/search", post(handle_vector_search))
        .route("/vectors/{name}/compact", post(handle_compact_vector_index))
        .route("/vectors/{name}/{vertex_id}", delete(handle_remove_vector))
        .route("/collections", get(handle_list_document_collections))
        .route(
            "/collections/{collection}/documents",
            get(handle_list_documents),
        )
        .route(
            "/collections/{collection}/documents/search",
            post(handle_search_documents_by_index),
        )
        .route(
            "/collections/{collection}/indexes",
            get(handle_list_document_indexes).post(handle_create_document_index),
        )
        .route(
            "/collections/{collection}/documents/{key}",
            get(handle_get_document)
                .put(handle_upsert_document)
                .delete(handle_delete_document),
        )
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

/// Start the HTTPS server with graceful shutdown support.
pub async fn run_https_server(
    router: Router,
    bind_addr: &str,
    tls: &TlsConfig,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    let listener = TlsListener::bind(bind_addr, tls).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
}

fn schedule_compaction_if_needed(
    engine: Arc<NexusEngine>,
    threshold: usize,
    runtime: Arc<CompactionRuntime>,
) -> bool {
    if threshold == 0 || engine.compaction_pressure().total() < threshold {
        return false;
    }

    if runtime.active.swap(true, Ordering::AcqRel) {
        runtime.skipped_active_total.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    runtime.scheduled_total.fetch_add(1, Ordering::Relaxed);
    tokio::task::spawn_blocking(move || {
        let outcome = engine.compact_storage_if_needed(threshold);
        match outcome {
            Ok(_) => {
                runtime.completed_total.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => {
                runtime.failed_total.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %err, "background compaction failed");
            }
        }
        runtime.active.store(false, Ordering::Release);
    });

    true
}

async fn handle_cypher(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ProductJson(req): ProductJson<CypherRequest>,
) -> Result<Json<CypherResponse>, (StatusCode, Json<ErrorResponse>)> {
    let tenant = req.tenant.clone();
    let query = req.query.clone();
    let audit_write = http_query_is_write(&query);
    let principal = authorize(
        &state.config,
        &headers,
        if audit_write {
            AuthRole::ReadWrite
        } else {
            AuthRole::ReadOnly
        },
        tenant.as_deref(),
    )?;
    enforce_query_rate_limit(&state, tenant.as_deref().unwrap_or("default"))?;
    enforce_process_memory_budget(&state)?;

    let engine = state.engine.resolve(tenant.as_deref()).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::coded("TENANT_NOT_FOUND", e)),
        )
    })?;
    let _permit = acquire_query_permit(&state)?;

    let params: HashMap<String, Value> = req
        .params
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, json_to_value(v)))
        .collect();
    let start = std::time::Instant::now();
    let query_engine = engine.clone();
    let query_runtime = state.queries.begin();
    let cancellation = Arc::new(AtomicBool::new(false));
    let query_cancellation = cancellation.clone();
    let row_budget =
        (state.config.default_query_limit > 0).then_some(state.config.default_query_limit);
    let byte_budget = (state.config.query_memory_budget_bytes > 0)
        .then_some(state.config.query_memory_budget_bytes);

    let result = timeout(
        Duration::from_secs(state.config.query_timeout_secs),
        tokio::task::spawn_blocking(move || {
            query_engine.execute_cypher_with_params_cancellation_and_limits(
                &query,
                params,
                Some(query_cancellation),
                row_budget,
                byte_budget,
            )
        }),
    )
    .await;

    match result {
        Ok(Ok(Ok(qr))) => {
            if audit_write {
                record_audit_event(
                    &state,
                    &principal,
                    "cypher.write",
                    tenant.as_deref(),
                    "success",
                    Some(format!("rows={}", qr.rows.len())),
                );
            }
            schedule_compaction_if_needed(
                engine,
                state.config.compaction_threshold,
                state.compaction.clone(),
            );
            let estimated_bytes = estimate_query_result_bytes(&qr);
            if let Err(err) = enforce_result_limits(&qr, &state.config, estimated_bytes) {
                let elapsed = start.elapsed();
                record_slow_query_if_needed(&state, elapsed, "limited");
                query_runtime.finish_limited(elapsed);
                return Err(err);
            }
            let elapsed = start.elapsed();
            record_slow_query_if_needed(&state, elapsed, "success");
            query_runtime.finish_success(qr.rows.len(), estimated_bytes, elapsed);
            let rows: Vec<Vec<serde_json::Value>> = qr
                .rows
                .iter()
                .map(|row| row.iter().map(value_to_json).collect())
                .collect();

            Ok(Json(CypherResponse {
                columns: qr.columns,
                rows,
                time_ms: elapsed.as_secs_f64() * 1000.0,
            }))
        }
        Ok(Ok(Err(e))) => {
            if audit_write {
                record_audit_event(
                    &state,
                    &principal,
                    "cypher.write",
                    tenant.as_deref(),
                    "error",
                    Some(e.to_string()),
                );
            }
            let elapsed = start.elapsed();
            if is_row_budget_error(&e) {
                record_slow_query_if_needed(&state, elapsed, "limited");
                query_runtime.finish_limited(elapsed);
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(ErrorResponse::coded(
                        "RESULT_ROW_LIMIT_EXCEEDED",
                        format!(
                            "query exceeded configured default_query_limit {}: {e}",
                            state.config.default_query_limit
                        ),
                    )),
                ));
            }
            if is_byte_budget_error(&e) {
                record_slow_query_if_needed(&state, elapsed, "limited");
                query_runtime.finish_limited(elapsed);
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(ErrorResponse::coded(
                        "RESULT_MEMORY_BUDGET_EXCEEDED",
                        format!(
                            "query exceeded configured query_memory_budget_bytes {}: {e}",
                            state.config.query_memory_budget_bytes
                        ),
                    )),
                ));
            }
            record_slow_query_if_needed(&state, elapsed, "error");
            query_runtime.finish_failure(elapsed);
            Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::coded("CYPHER_ERROR", e.to_string())),
            ))
        }
        Ok(Err(e)) => {
            if audit_write {
                record_audit_event(
                    &state,
                    &principal,
                    "cypher.write",
                    tenant.as_deref(),
                    "panic",
                    Some(e.to_string()),
                );
            }
            let elapsed = start.elapsed();
            record_slow_query_if_needed(&state, elapsed, "panic");
            query_runtime.finish_failure(elapsed);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::coded(
                    "QUERY_TASK_PANIC",
                    format!("query task panicked: {e}"),
                )),
            ))
        }
        Err(_) => {
            cancellation.store(true, Ordering::Relaxed);
            if audit_write {
                record_audit_event(
                    &state,
                    &principal,
                    "cypher.write",
                    tenant.as_deref(),
                    "timeout",
                    Some(format!(
                        "query timed out after {}s",
                        state.config.query_timeout_secs
                    )),
                );
            }
            let elapsed = start.elapsed();
            record_slow_query_if_needed(&state, elapsed, "timeout");
            query_runtime.finish_timeout(elapsed);
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                Json(ErrorResponse::coded(
                    "QUERY_TIMEOUT",
                    format!("query timed out after {}s", state.config.query_timeout_secs),
                )),
            ))
        }
    }
}

async fn handle_tx_batch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ProductJson(req): ProductJson<TxBatchRequest>,
) -> Result<Json<TxBatchResponse>, (StatusCode, Json<ErrorResponse>)> {
    let tenant = req.tenant.clone();
    let principal = authorize(
        &state.config,
        &headers,
        AuthRole::ReadWrite,
        tenant.as_deref(),
    )?;
    enforce_query_rate_limit(&state, tenant.as_deref().unwrap_or("default"))?;
    enforce_process_memory_budget(&state)?;

    if req.cypher.is_some() && !req.cypher_statements.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::coded(
                "BATCH_CYPHER_AMBIGUOUS",
                "use either cypher or cypher_statements, not both",
            )),
        ));
    }

    if req.cypher.is_none()
        && req.cypher_statements.is_empty()
        && req.documents.is_empty()
        && req.vectors.is_empty()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::coded(
                "BATCH_EMPTY",
                "batch requires at least one cypher, document, or vector operation",
            )),
        ));
    }

    let engine = state.engine.resolve(tenant.as_deref()).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::coded("TENANT_NOT_FOUND", e)),
        )
    })?;
    let _permit = acquire_query_permit(&state)?;

    let cypher_statements: Vec<_> = req
        .cypher
        .into_iter()
        .chain(req.cypher_statements)
        .map(|cypher| {
            let params: HashMap<String, Value> = cypher
                .params
                .unwrap_or_default()
                .into_iter()
                .map(|(key, value)| (key, json_to_value(value)))
                .collect();
            (cypher.query, params)
        })
        .collect();
    let document_mutations: Vec<DocumentMutation> = req
        .documents
        .into_iter()
        .map(|op| match op {
            TxBatchDocumentOperation::Upsert {
                collection,
                key,
                document,
            } => DocumentMutation::Upsert {
                collection,
                key,
                document,
            },
            TxBatchDocumentOperation::Delete { collection, key } => {
                DocumentMutation::Delete { collection, key }
            }
        })
        .collect();
    let vector_mutations: Vec<VectorMutation> = req
        .vectors
        .into_iter()
        .map(|op| match op {
            TxBatchVectorOperation::Upsert {
                index,
                vertex_id,
                embedding,
            } => VectorMutation::Upsert {
                index,
                vertex: VertexId(vertex_id),
                embedding,
            },
            TxBatchVectorOperation::Remove { index, vertex_id } => VectorMutation::Remove {
                index,
                vertex: VertexId(vertex_id),
            },
        })
        .collect();

    let start = std::time::Instant::now();
    let query_runtime = state.queries.begin();
    let query_engine = engine.clone();
    let cancellation = Arc::new(AtomicBool::new(false));
    let query_cancellation = cancellation.clone();
    let row_budget =
        (state.config.default_query_limit > 0).then_some(state.config.default_query_limit);
    let byte_budget = (state.config.query_memory_budget_bytes > 0)
        .then_some(state.config.query_memory_budget_bytes);

    let result = timeout(
        Duration::from_secs(state.config.query_timeout_secs),
        tokio::task::spawn_blocking(move || {
            query_engine.execute_cross_model_batch_multi(
                cypher_statements,
                document_mutations,
                vector_mutations,
                Some(query_cancellation),
                row_budget,
                byte_budget,
            )
        }),
    )
    .await;

    match result {
        Ok(Ok(Ok(batch))) => {
            record_audit_event(
                &state,
                &principal,
                "tx.batch",
                tenant.as_deref(),
                "success",
                Some(format!(
                    "document_ops={} vector_ops={}",
                    batch.document_ops, batch.vector_ops
                )),
            );
            schedule_compaction_if_needed(
                engine,
                state.config.compaction_threshold,
                state.compaction.clone(),
            );
            let elapsed = start.elapsed();
            let mut cypher_results = Vec::new();
            for qr in batch.cypher_results {
                let estimated_bytes = estimate_query_result_bytes(&qr);
                if let Err(err) = enforce_result_limits(&qr, &state.config, estimated_bytes) {
                    record_slow_query_if_needed(&state, elapsed, "limited");
                    query_runtime.finish_limited(elapsed);
                    return Err(err);
                }
                cypher_results.push(CypherResponse {
                    columns: qr.columns,
                    rows: qr
                        .rows
                        .iter()
                        .map(|row| row.iter().map(value_to_json).collect())
                        .collect(),
                    time_ms: elapsed.as_secs_f64() * 1000.0,
                });
            }
            let cypher = cypher_results.last().cloned();
            let rows = cypher_results.iter().map(|qr| qr.rows.len()).sum();
            let bytes = serde_json::to_vec(&cypher_results)
                .map(|bytes| bytes.len())
                .unwrap_or(0);
            record_slow_query_if_needed(&state, elapsed, "success");
            query_runtime.finish_success(rows, bytes, elapsed);
            Ok(Json(TxBatchResponse {
                committed: true,
                document_ops: batch.document_ops,
                vector_ops: batch.vector_ops,
                cypher,
                cypher_results,
                time_ms: elapsed.as_secs_f64() * 1000.0,
            }))
        }
        Ok(Ok(Err(e))) => {
            record_audit_event(
                &state,
                &principal,
                "tx.batch",
                tenant.as_deref(),
                "error",
                Some(e.to_string()),
            );
            let elapsed = start.elapsed();
            if is_row_budget_error(&e) {
                record_slow_query_if_needed(&state, elapsed, "limited");
                query_runtime.finish_limited(elapsed);
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(ErrorResponse::coded(
                        "RESULT_ROW_LIMIT_EXCEEDED",
                        format!(
                            "batch exceeded configured default_query_limit {}: {e}",
                            state.config.default_query_limit
                        ),
                    )),
                ));
            }
            if is_byte_budget_error(&e) {
                record_slow_query_if_needed(&state, elapsed, "limited");
                query_runtime.finish_limited(elapsed);
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(ErrorResponse::coded(
                        "RESULT_MEMORY_BUDGET_EXCEEDED",
                        format!(
                            "batch exceeded configured query_memory_budget_bytes {}: {e}",
                            state.config.query_memory_budget_bytes
                        ),
                    )),
                ));
            }
            record_slow_query_if_needed(&state, elapsed, "error");
            query_runtime.finish_failure(elapsed);
            Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::coded("BATCH_ERROR", e.to_string())),
            ))
        }
        Ok(Err(e)) => {
            record_audit_event(
                &state,
                &principal,
                "tx.batch",
                tenant.as_deref(),
                "panic",
                Some(e.to_string()),
            );
            let elapsed = start.elapsed();
            record_slow_query_if_needed(&state, elapsed, "panic");
            query_runtime.finish_failure(elapsed);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::coded(
                    "BATCH_TASK_PANIC",
                    format!("batch task panicked: {e}"),
                )),
            ))
        }
        Err(_) => {
            cancellation.store(true, Ordering::Relaxed);
            record_audit_event(
                &state,
                &principal,
                "tx.batch",
                tenant.as_deref(),
                "timeout",
                Some(format!(
                    "batch timed out after {}s",
                    state.config.query_timeout_secs
                )),
            );
            let elapsed = start.elapsed();
            record_slow_query_if_needed(&state, elapsed, "timeout");
            query_runtime.finish_timeout(elapsed);
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                Json(ErrorResponse::coded(
                    "QUERY_TIMEOUT",
                    format!("batch timed out after {}s", state.config.query_timeout_secs),
                )),
            ))
        }
    }
}

fn enforce_query_rate_limit(
    state: &Arc<AppState>,
    tenant: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if !state
        .rate_limit
        .admit("__global__", state.config.max_query_rate_per_sec)
    {
        state.queries.reject_rate_limited();
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::coded(
                "QUERY_RATE_LIMITED",
                format!(
                    "query rate limit exceeded; configured max_query_rate_per_sec is {}",
                    state.config.max_query_rate_per_sec
                ),
            )),
        ));
    }

    let tenant_key = format!("tenant:{tenant}");
    if !state
        .rate_limit
        .admit(&tenant_key, state.config.max_query_rate_per_tenant_per_sec)
    {
        state.queries.reject_rate_limited();
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::coded(
                "TENANT_QUERY_RATE_LIMITED",
                format!(
                    "query rate limit exceeded for tenant {tenant}; configured max_query_rate_per_tenant_per_sec is {}",
                    state.config.max_query_rate_per_tenant_per_sec
                ),
            )),
        ));
    }

    Ok(())
}

fn enforce_process_memory_budget(
    state: &Arc<AppState>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let budget = state.config.process_memory_budget_bytes;
    if budget == 0 {
        return Ok(());
    }

    let rss = process_resident_memory_bytes_for_state(state);
    if let Some(err) = process_memory_budget_error(budget, rss) {
        state.queries.reject_memory_pressure();
        return Err(err);
    }
    Ok(())
}

fn process_resident_memory_bytes_for_state(_state: &Arc<AppState>) -> u64 {
    #[cfg(test)]
    {
        if let Some(rss) = _state.config.process_memory_budget_sample_bytes {
            return rss;
        }
    }
    process_resident_memory_bytes()
}

fn process_memory_budget_error(
    budget: usize,
    rss: u64,
) -> Option<(StatusCode, Json<ErrorResponse>)> {
    let budget = budget as u64;
    if budget == 0 || rss == 0 || rss <= budget {
        return None;
    }

    Some((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse::coded(
            "PROCESS_MEMORY_BUDGET_EXCEEDED",
            format!(
                "process RSS {rss} bytes exceeds configured process_memory_budget_bytes {budget}"
            ),
        )),
    ))
}

fn record_slow_query_if_needed(state: &Arc<AppState>, elapsed: Duration, status: &'static str) {
    let threshold = state.config.slow_query_ms;
    if threshold == 0 || elapsed.as_millis() < threshold as u128 {
        return;
    }
    state.queries.record_slow_query();
    tracing::warn!(
        status,
        elapsed_ms = elapsed.as_millis() as u64,
        threshold_ms = threshold,
        "slow cypher query"
    );
}

fn acquire_query_permit(
    state: &Arc<AppState>,
) -> Result<Option<OwnedSemaphorePermit>, (StatusCode, Json<ErrorResponse>)> {
    let Some(semaphore) = &state.query_permits else {
        return Ok(None);
    };

    match semaphore.clone().try_acquire_owned() {
        Ok(permit) => Ok(Some(permit)),
        Err(_) => {
            state.queries.reject_over_capacity();
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::coded(
                    "QUERY_OVER_CAPACITY",
                    format!(
                        "too many concurrent queries; configured max_concurrent_queries is {}",
                        state.config.max_concurrent_queries
                    ),
                )),
            ))
        }
    }
}

async fn handle_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(&state.config, &headers, AuthRole::ReadOnly, Some("default"))?;

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
    authorize(&state.config, &headers, AuthRole::ReadOnly, Some("default"))?;

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

async fn handle_compact(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ProductJson(req): ProductJson<CompactRequest>,
) -> Result<Json<CompactResponse>, (StatusCode, Json<ErrorResponse>)> {
    let tenant = req.tenant.clone();
    let principal = authorize(&state.config, &headers, AuthRole::Admin, tenant.as_deref())?;

    let engine = state.engine.resolve(tenant.as_deref()).map_err(|e| {
        record_audit_event(
            &state,
            &principal,
            "admin.compact",
            tenant.as_deref(),
            "error",
            Some(e.clone()),
        );
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::coded("TENANT_NOT_FOUND", e)),
        )
    })?;
    let pressure_before = engine.compaction_pressure();

    if state.compaction.active.swap(true, Ordering::AcqRel) {
        state
            .compaction
            .skipped_active_total
            .fetch_add(1, Ordering::Relaxed);
        record_audit_event(
            &state,
            &principal,
            "admin.compact",
            tenant.as_deref(),
            "conflict",
            Some("compaction already active".into()),
        );
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::coded(
                "COMPACTION_ACTIVE",
                "compaction already active",
            )),
        ));
    }

    state
        .compaction
        .scheduled_total
        .fetch_add(1, Ordering::Relaxed);

    let compact_engine = engine.clone();
    let threshold = req.threshold;
    let result = tokio::task::spawn_blocking(move || match threshold {
        Some(threshold) => compact_engine
            .compact_storage_if_needed(threshold)
            .map(|stats| (stats.is_some(), stats)),
        None => compact_engine
            .compact_storage()
            .map(|stats| (true, Some(stats))),
    })
    .await;

    state.compaction.active.store(false, Ordering::Release);

    let (compacted, stats) = match result {
        Ok(Ok(outcome)) => {
            state
                .compaction
                .completed_total
                .fetch_add(1, Ordering::Relaxed);
            record_audit_event(
                &state,
                &principal,
                "admin.compact",
                tenant.as_deref(),
                "success",
                Some(format!("compacted={}", outcome.0)),
            );
            outcome
        }
        Ok(Err(err)) => {
            state
                .compaction
                .failed_total
                .fetch_add(1, Ordering::Relaxed);
            record_audit_event(
                &state,
                &principal,
                "admin.compact",
                tenant.as_deref(),
                "error",
                Some(err.to_string()),
            );
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::coded("COMPACTION_FAILED", err.to_string())),
            ));
        }
        Err(err) => {
            state
                .compaction
                .failed_total
                .fetch_add(1, Ordering::Relaxed);
            record_audit_event(
                &state,
                &principal,
                "admin.compact",
                tenant.as_deref(),
                "panic",
                Some(err.to_string()),
            );
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::coded(
                    "COMPACTION_TASK_PANIC",
                    format!("compaction task panicked: {err}"),
                )),
            ));
        }
    };

    Ok(Json(CompactResponse {
        compacted,
        pressure_before: pressure_response(pressure_before),
        pressure_after: pressure_response(engine.compaction_pressure()),
        stats: stats.map(stats_response),
    }))
}

async fn handle_backup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ProductJson(req): ProductJson<BackupRequest>,
) -> Result<Json<BackupResponse>, (StatusCode, Json<ErrorResponse>)> {
    let tenant = req.tenant.clone();
    let principal = authorize(&state.config, &headers, AuthRole::Admin, tenant.as_deref())?;

    let engine = state.engine.resolve(tenant.as_deref()).map_err(|e| {
        record_audit_event(
            &state,
            &principal,
            "admin.backup",
            tenant.as_deref(),
            "error",
            Some(e.clone()),
        );
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::coded("TENANT_NOT_FOUND", e)),
        )
    })?;
    let backup_path = match resolve_backup_path(&state.config, &req.path) {
        Ok(path) => path,
        Err(err) => {
            record_audit_event(
                &state,
                &principal,
                "admin.backup",
                tenant.as_deref(),
                "rejected",
                err.1.0.code.clone(),
            );
            return Err(err);
        }
    };
    let path = backup_path.to_string_lossy().into_owned();
    state.backup.started_total.fetch_add(1, Ordering::Relaxed);
    let result = tokio::task::spawn_blocking(move || engine.backup_to(&backup_path)).await;

    let manifest = match result {
        Ok(Ok(manifest)) => {
            state.backup.completed_total.fetch_add(1, Ordering::Relaxed);
            record_audit_event(
                &state,
                &principal,
                "admin.backup",
                tenant.as_deref(),
                "success",
                Some(path.clone()),
            );
            manifest
        }
        Ok(Err(err)) => {
            state.backup.failed_total.fetch_add(1, Ordering::Relaxed);
            let message = err.to_string();
            let (status, code) = if message.contains("no durable store") {
                (StatusCode::BAD_REQUEST, "BACKUP_UNAVAILABLE")
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, "BACKUP_FAILED")
            };
            record_audit_event(
                &state,
                &principal,
                "admin.backup",
                tenant.as_deref(),
                "error",
                Some(message.clone()),
            );
            return Err((status, Json(ErrorResponse::coded(code, message))));
        }
        Err(err) => {
            state.backup.failed_total.fetch_add(1, Ordering::Relaxed);
            record_audit_event(
                &state,
                &principal,
                "admin.backup",
                tenant.as_deref(),
                "panic",
                Some(err.to_string()),
            );
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::coded(
                    "BACKUP_TASK_PANIC",
                    format!("backup task panicked: {err}"),
                )),
            ));
        }
    };

    Ok(Json(BackupResponse { path, manifest }))
}

async fn handle_list_vector_indexes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<TenantQuery>,
) -> Result<Json<VectorIndexListResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadOnly,
        query.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, query.tenant.as_deref())?;
    Ok(Json(VectorIndexListResponse {
        indexes: engine.vector_index_names(),
    }))
}

async fn handle_create_vector_index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    ProductJson(req): ProductJson<CreateVectorIndexRequest>,
) -> Result<(StatusCode, Json<VectorAckResponse>), (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadWrite,
        req.tenant.as_deref(),
    )?;
    if req.dimension == 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::coded(
                "VECTOR_BAD_DIMENSION",
                "vector index dimension must be greater than zero",
            )),
        ));
    }
    let engine = resolve_request_engine(&state, req.tenant.as_deref())?;
    let result =
        tokio::task::spawn_blocking(move || engine.create_vector_index(&name, req.dimension)).await;
    match result {
        Ok(Ok(())) => Ok((StatusCode::CREATED, Json(VectorAckResponse { ok: true }))),
        Ok(Err(err)) => Err(vector_error_response(err)),
        Err(err) => Err(vector_task_error_response("VECTOR_CREATE_TASK_PANIC", err)),
    }
}

async fn handle_upsert_vector(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    ProductJson(req): ProductJson<VectorUpsertRequest>,
) -> Result<Json<VectorAckResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadWrite,
        req.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, req.tenant.as_deref())?;
    let result = tokio::task::spawn_blocking(move || {
        engine.upsert_vector(&name, VertexId(req.vertex_id), req.embedding)
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(Json(VectorAckResponse { ok: true })),
        Ok(Err(err)) => Err(vector_error_response(err)),
        Err(err) => Err(vector_task_error_response("VECTOR_UPSERT_TASK_PANIC", err)),
    }
}

async fn handle_vector_search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    ProductJson(req): ProductJson<VectorSearchRequest>,
) -> Result<Json<VectorSearchResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadOnly,
        req.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, req.tenant.as_deref())?;
    let k = req.k.unwrap_or(10);
    let result =
        tokio::task::spawn_blocking(move || engine.vector_search(&name, &req.query, k)).await;
    match result {
        Ok(Ok(results)) => Ok(Json(VectorSearchResponse {
            results: results
                .into_iter()
                .map(|(vertex, distance)| VectorSearchHit {
                    vertex_id: vertex.0,
                    distance,
                })
                .collect(),
        })),
        Ok(Err(err)) => Err(vector_error_response(err)),
        Err(err) => Err(vector_task_error_response("VECTOR_SEARCH_TASK_PANIC", err)),
    }
}

async fn handle_remove_vector(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, vertex_id)): Path<(String, u64)>,
    Query(query): Query<TenantQuery>,
) -> Result<Json<VectorRemoveResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadWrite,
        query.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, query.tenant.as_deref())?;
    let result =
        tokio::task::spawn_blocking(move || engine.remove_vector(&name, VertexId(vertex_id))).await;
    match result {
        Ok(Ok(removed)) => Ok(Json(VectorRemoveResponse { removed })),
        Ok(Err(err)) => Err(vector_error_response(err)),
        Err(err) => Err(vector_task_error_response("VECTOR_REMOVE_TASK_PANIC", err)),
    }
}

async fn handle_compact_vector_index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    ProductJson(req): ProductJson<VectorCompactRequest>,
) -> Result<Json<VectorAckResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadWrite,
        req.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, req.tenant.as_deref())?;
    let result = tokio::task::spawn_blocking(move || engine.compact_vector_index(&name)).await;
    match result {
        Ok(Ok(())) => Ok(Json(VectorAckResponse { ok: true })),
        Ok(Err(err)) => Err(vector_error_response(err)),
        Err(err) => Err(vector_task_error_response("VECTOR_COMPACT_TASK_PANIC", err)),
    }
}

async fn handle_list_document_collections(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<TenantQuery>,
) -> Result<Json<DocumentCollectionsResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadOnly,
        query.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, query.tenant.as_deref())?;
    let result = tokio::task::spawn_blocking(move || engine.list_document_collections()).await;
    match result {
        Ok(Ok(collections)) => Ok(Json(DocumentCollectionsResponse { collections })),
        Ok(Err(err)) => Err(document_error_response(err)),
        Err(err) => Err(document_task_error_response(
            "DOCUMENT_LIST_TASK_PANIC",
            err,
        )),
    }
}

async fn handle_list_documents(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(collection): Path<String>,
    Query(query): Query<DocumentListQuery>,
) -> Result<Json<DocumentListResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadOnly,
        query.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, query.tenant.as_deref())?;
    let result =
        tokio::task::spawn_blocking(move || engine.list_documents(&collection, query.limit)).await;
    match result {
        Ok(Ok(documents)) => Ok(Json(DocumentListResponse {
            documents: documents
                .into_iter()
                .map(document_record_response)
                .collect(),
        })),
        Ok(Err(err)) => Err(document_error_response(err)),
        Err(err) => Err(document_task_error_response(
            "DOCUMENT_LIST_TASK_PANIC",
            err,
        )),
    }
}

async fn handle_create_document_index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(collection): Path<String>,
    ProductJson(req): ProductJson<DocumentIndexCreateRequest>,
) -> Result<Json<DocumentIndexResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadWrite,
        req.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, req.tenant.as_deref())?;
    let kind = req.kind.unwrap_or(DocumentIndexKind::Scalar);
    let result = tokio::task::spawn_blocking(move || {
        engine.create_document_index_with_kind(&collection, &req.path, kind)
    })
    .await;
    match result {
        Ok(Ok(index)) => Ok(Json(DocumentIndexResponse {
            collection: index.collection,
            path: index.path,
            kind: index.kind,
        })),
        Ok(Err(err)) => Err(document_error_response(err)),
        Err(err) => Err(document_task_error_response(
            "DOCUMENT_INDEX_CREATE_TASK_PANIC",
            err,
        )),
    }
}

async fn handle_list_document_indexes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(collection): Path<String>,
    Query(query): Query<TenantQuery>,
) -> Result<Json<DocumentIndexListResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadOnly,
        query.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, query.tenant.as_deref())?;
    let result =
        tokio::task::spawn_blocking(move || engine.list_document_indexes(Some(&collection))).await;
    match result {
        Ok(Ok(indexes)) => Ok(Json(DocumentIndexListResponse {
            indexes: indexes
                .into_iter()
                .map(|index| DocumentIndexResponse {
                    collection: index.collection,
                    path: index.path,
                    kind: index.kind,
                })
                .collect(),
        })),
        Ok(Err(err)) => Err(document_error_response(err)),
        Err(err) => Err(document_task_error_response(
            "DOCUMENT_INDEX_LIST_TASK_PANIC",
            err,
        )),
    }
}

async fn handle_search_documents_by_index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(collection): Path<String>,
    ProductJson(req): ProductJson<DocumentIndexSearchRequest>,
) -> Result<Json<DocumentListResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadOnly,
        req.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, req.tenant.as_deref())?;
    let has_exact = req.value.is_some();
    let has_prefix = req.prefix.is_some();
    let has_text = req.text.is_some();
    let has_range = req.gte.is_some() || req.lte.is_some();
    if [has_exact, has_prefix, has_text, has_range]
        .into_iter()
        .filter(|present| *present)
        .count()
        != 1
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::coded(
                "DOCUMENT_INDEX_QUERY_INVALID",
                "document index search requires exactly one of value, prefix, text, or gte/lte",
            )),
        ));
    }
    let is_full_text = has_text;
    let query = if let Some(value) = req.value {
        DocumentIndexQuery::Exact(value)
    } else if let Some(prefix) = req.prefix {
        DocumentIndexQuery::Prefix(prefix)
    } else if let Some(text) = req.text {
        DocumentIndexQuery::FullTextAdvanced(DocumentFullTextQuery {
            text,
            phrase: req.phrase.unwrap_or(false),
            fuzzy_distance: req.fuzzy_distance,
            stem: req.stem.unwrap_or(false),
            ranking: req.ranking.unwrap_or_default(),
            include_snippets: req.snippets.unwrap_or(false),
            include_explanations: req.explain.unwrap_or(false),
        })
    } else {
        DocumentIndexQuery::Range {
            gte: req.gte,
            lte: req.lte,
        }
    };
    if is_full_text {
        let result = tokio::task::spawn_blocking(move || {
            engine.query_document_hits_by_index_with(&collection, &req.path, query, req.limit)
        })
        .await;
        match result {
            Ok(Ok(documents)) => Ok(Json(DocumentListResponse {
                documents: documents
                    .into_iter()
                    .map(document_search_hit_response)
                    .collect(),
            })),
            Ok(Err(err)) => Err(document_error_response(err)),
            Err(err) => Err(document_task_error_response(
                "DOCUMENT_INDEX_SEARCH_TASK_PANIC",
                err,
            )),
        }
    } else {
        let result = tokio::task::spawn_blocking(move || {
            engine.query_documents_by_index_with(&collection, &req.path, query, req.limit)
        })
        .await;
        match result {
            Ok(Ok(documents)) => Ok(Json(DocumentListResponse {
                documents: documents
                    .into_iter()
                    .map(document_record_response)
                    .collect(),
            })),
            Ok(Err(err)) => Err(document_error_response(err)),
            Err(err) => Err(document_task_error_response(
                "DOCUMENT_INDEX_SEARCH_TASK_PANIC",
                err,
            )),
        }
    }
}

async fn handle_get_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((collection, key)): Path<(String, String)>,
    Query(query): Query<TenantQuery>,
) -> Result<Json<DocumentRecordResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadOnly,
        query.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, query.tenant.as_deref())?;
    let collection_for_response = collection.clone();
    let key_for_response = key.clone();
    let result = tokio::task::spawn_blocking(move || engine.load_document(&collection, &key)).await;
    match result {
        Ok(Ok(Some(document))) => Ok(Json(DocumentRecordResponse {
            collection: collection_for_response,
            key: key_for_response,
            document,
            score: None,
            snippet: None,
            explanation: None,
        })),
        Ok(Ok(None)) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::coded(
                "DOCUMENT_NOT_FOUND",
                "document not found",
            )),
        )),
        Ok(Err(err)) => Err(document_error_response(err)),
        Err(err) => Err(document_task_error_response("DOCUMENT_GET_TASK_PANIC", err)),
    }
}

async fn handle_upsert_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((collection, key)): Path<(String, String)>,
    ProductJson(req): ProductJson<DocumentUpsertRequest>,
) -> Result<Json<DocumentAckResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadWrite,
        req.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, req.tenant.as_deref())?;
    let result = tokio::task::spawn_blocking(move || {
        engine.upsert_document(&collection, &key, req.document)
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(Json(DocumentAckResponse { ok: true })),
        Ok(Err(err)) => Err(document_error_response(err)),
        Err(err) => Err(document_task_error_response(
            "DOCUMENT_UPSERT_TASK_PANIC",
            err,
        )),
    }
}

async fn handle_delete_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((collection, key)): Path<(String, String)>,
    Query(query): Query<TenantQuery>,
) -> Result<Json<DocumentDeleteResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(
        &state.config,
        &headers,
        AuthRole::ReadWrite,
        query.tenant.as_deref(),
    )?;
    let engine = resolve_request_engine(&state, query.tenant.as_deref())?;
    let result =
        tokio::task::spawn_blocking(move || engine.delete_document(&collection, &key)).await;
    match result {
        Ok(Ok(deleted)) => Ok(Json(DocumentDeleteResponse { deleted })),
        Ok(Err(err)) => Err(document_error_response(err)),
        Err(err) => Err(document_task_error_response(
            "DOCUMENT_DELETE_TASK_PANIC",
            err,
        )),
    }
}

async fn handle_metrics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    authorize(&state.config, &headers, AuthRole::Admin, None)?;

    let (vertices, edges, deleted_vertices, tombstoned_edges, delta_edges) = state
        .engine
        .default_engine()
        .map(|e| {
            let pressure = e.compaction_pressure();
            (
                e.vertex_count(),
                e.edge_count(),
                pressure.deleted_vertices,
                pressure.tombstoned_edges,
                pressure.delta_edges,
            )
        })
        .unwrap_or((0, 0, 0, 0, 0));
    let tenants = state.engine.list_tenants().len();
    let compaction = state.compaction.snapshot();
    let backup = state.backup.snapshot();
    let audit = state.audit.snapshot();
    let queries = state.queries.snapshot();
    let vector_search = state
        .engine
        .default_engine()
        .map(|engine| engine.vector_search_metrics())
        .unwrap_or_default();
    let vector_search_by_index = state
        .engine
        .default_engine()
        .map(|engine| engine.vector_search_metrics_by_index())
        .unwrap_or_default();
    let storage = state
        .engine
        .default_engine()
        .and_then(|engine| engine.storage_metrics())
        .unwrap_or_default();
    let graph_memory_estimate_bytes = state
        .engine
        .default_engine()
        .map(|engine| engine.graph_memory_estimate_bytes())
        .unwrap_or(0);
    let index_memory_estimate_bytes = state
        .engine
        .default_engine()
        .map(|engine| engine.index_memory_estimate_bytes())
        .unwrap_or(0);
    let vector_index_memory_estimate_bytes = state
        .engine
        .default_engine()
        .map(|engine| engine.vector_index_memory_estimate_bytes())
        .unwrap_or(0);
    let total_memory_estimate_bytes = graph_memory_estimate_bytes
        .saturating_add(index_memory_estimate_bytes)
        .saturating_add(vector_index_memory_estimate_bytes);
    let vector_index_memory_by_name = state
        .engine
        .default_engine()
        .map(|engine| engine.vector_index_memory_estimates_by_name())
        .unwrap_or_default();
    let query_timeout_secs = state.config.query_timeout_secs;
    let max_body_bytes = state.config.max_body_bytes;
    let max_concurrent_queries = state.config.max_concurrent_queries;
    let max_query_rate_per_sec = state.config.max_query_rate_per_sec;
    let max_query_rate_per_tenant_per_sec = state.config.max_query_rate_per_tenant_per_sec;
    let slow_query_ms = state.config.slow_query_ms;
    let query_memory_budget_bytes = state.config.query_memory_budget_bytes;
    let process_memory_budget_bytes = state.config.process_memory_budget_bytes;
    let default_query_limit = state.config.default_query_limit;
    let snapshot_modified_unix_seconds = storage.snapshot_modified_unix_seconds;
    let snapshot_age_seconds = if snapshot_modified_unix_seconds == 0 {
        0
    } else {
        unix_secs().saturating_sub(snapshot_modified_unix_seconds)
    };
    let process_resident_memory_bytes = process_resident_memory_bytes();
    let mut body = format!(
        "# TYPE domyn_nexus_vertices gauge\n\
         domyn_nexus_vertices {vertices}\n\
         # TYPE domyn_nexus_edges gauge\n\
         domyn_nexus_edges {edges}\n\
         # TYPE domyn_nexus_compaction_deleted_vertices gauge\n\
         domyn_nexus_compaction_deleted_vertices {deleted_vertices}\n\
         # TYPE domyn_nexus_compaction_tombstoned_edges gauge\n\
         domyn_nexus_compaction_tombstoned_edges {tombstoned_edges}\n\
         # TYPE domyn_nexus_compaction_delta_edges gauge\n\
         domyn_nexus_compaction_delta_edges {delta_edges}\n\
         # TYPE domyn_nexus_compaction_active gauge\n\
         domyn_nexus_compaction_active {}\n\
         # TYPE domyn_nexus_compaction_scheduled_total counter\n\
         domyn_nexus_compaction_scheduled_total {}\n\
         # TYPE domyn_nexus_compaction_completed_total counter\n\
         domyn_nexus_compaction_completed_total {}\n\
         # TYPE domyn_nexus_compaction_failed_total counter\n\
         domyn_nexus_compaction_failed_total {}\n\
         # TYPE domyn_nexus_compaction_skipped_active_total counter\n\
         domyn_nexus_compaction_skipped_active_total {}\n\
         # TYPE domyn_nexus_backup_started_total counter\n\
         domyn_nexus_backup_started_total {}\n\
         # TYPE domyn_nexus_backup_completed_total counter\n\
         domyn_nexus_backup_completed_total {}\n\
         # TYPE domyn_nexus_backup_failed_total counter\n\
         domyn_nexus_backup_failed_total {}\n\
         # TYPE domyn_nexus_audit_events_total counter\n\
         domyn_nexus_audit_events_total {}\n\
         # TYPE domyn_nexus_audit_failed_total counter\n\
         domyn_nexus_audit_failed_total {}\n\
         # TYPE domyn_nexus_config_query_timeout_seconds gauge\n\
         domyn_nexus_config_query_timeout_seconds {query_timeout_secs}\n\
         # TYPE domyn_nexus_config_max_body_bytes gauge\n\
         domyn_nexus_config_max_body_bytes {max_body_bytes}\n\
         # TYPE domyn_nexus_config_max_concurrent_queries gauge\n\
         domyn_nexus_config_max_concurrent_queries {max_concurrent_queries}\n\
         # TYPE domyn_nexus_config_max_query_rate_per_sec gauge\n\
         domyn_nexus_config_max_query_rate_per_sec {max_query_rate_per_sec}\n\
         # TYPE domyn_nexus_config_max_query_rate_per_tenant_per_sec gauge\n\
         domyn_nexus_config_max_query_rate_per_tenant_per_sec {max_query_rate_per_tenant_per_sec}\n\
         # TYPE domyn_nexus_config_slow_query_ms gauge\n\
         domyn_nexus_config_slow_query_ms {slow_query_ms}\n\
         # TYPE domyn_nexus_config_query_memory_budget_bytes gauge\n\
         domyn_nexus_config_query_memory_budget_bytes {query_memory_budget_bytes}\n\
         # TYPE domyn_nexus_config_process_memory_budget_bytes gauge\n\
         domyn_nexus_config_process_memory_budget_bytes {process_memory_budget_bytes}\n\
         # TYPE domyn_nexus_config_default_query_limit gauge\n\
         domyn_nexus_config_default_query_limit {default_query_limit}\n\
         # TYPE domyn_nexus_process_resident_memory_bytes gauge\n\
         domyn_nexus_process_resident_memory_bytes {process_resident_memory_bytes}\n\
         # TYPE domyn_nexus_graph_memory_estimate_bytes gauge\n\
         domyn_nexus_graph_memory_estimate_bytes {graph_memory_estimate_bytes}\n\
         # TYPE domyn_nexus_index_memory_estimate_bytes gauge\n\
         domyn_nexus_index_memory_estimate_bytes {index_memory_estimate_bytes}\n\
         # TYPE domyn_nexus_vector_index_memory_estimate_bytes gauge\n\
         domyn_nexus_vector_index_memory_estimate_bytes {vector_index_memory_estimate_bytes}\n\
         # TYPE domyn_nexus_total_memory_estimate_bytes gauge\n\
         domyn_nexus_total_memory_estimate_bytes {total_memory_estimate_bytes}\n\
         # TYPE domyn_nexus_queries_active gauge\n\
         domyn_nexus_queries_active {}\n\
         # TYPE domyn_nexus_queries_total counter\n\
         domyn_nexus_queries_total {}\n\
         # TYPE domyn_nexus_queries_succeeded_total counter\n\
         domyn_nexus_queries_succeeded_total {}\n\
         # TYPE domyn_nexus_queries_failed_total counter\n\
         domyn_nexus_queries_failed_total {}\n\
         # TYPE domyn_nexus_queries_timed_out_total counter\n\
         domyn_nexus_queries_timed_out_total {}\n\
         # TYPE domyn_nexus_queries_limited_total counter\n\
         domyn_nexus_queries_limited_total {}\n\
         # TYPE domyn_nexus_queries_rejected_total counter\n\
         domyn_nexus_queries_rejected_total {}\n\
         # TYPE domyn_nexus_queries_rate_limited_total counter\n\
         domyn_nexus_queries_rate_limited_total {}\n\
         # TYPE domyn_nexus_queries_memory_rejected_total counter\n\
         domyn_nexus_queries_memory_rejected_total {}\n\
         # TYPE domyn_nexus_queries_slow_total counter\n\
         domyn_nexus_queries_slow_total {}\n\
         # TYPE domyn_nexus_query_rows_returned_total counter\n\
         domyn_nexus_query_rows_returned_total {}\n\
         # TYPE domyn_nexus_query_bytes_returned_total counter\n\
         domyn_nexus_query_bytes_returned_total {}\n\
         # TYPE domyn_nexus_query_elapsed_ms_total counter\n\
         domyn_nexus_query_elapsed_ms_total {}\n\
         # TYPE domyn_nexus_query_elapsed_ms_bucket counter\n\
         domyn_nexus_query_elapsed_ms_bucket{{le=\"1\"}} {}\n\
         domyn_nexus_query_elapsed_ms_bucket{{le=\"5\"}} {}\n\
         domyn_nexus_query_elapsed_ms_bucket{{le=\"10\"}} {}\n\
         domyn_nexus_query_elapsed_ms_bucket{{le=\"50\"}} {}\n\
         domyn_nexus_query_elapsed_ms_bucket{{le=\"100\"}} {}\n\
         domyn_nexus_query_elapsed_ms_bucket{{le=\"500\"}} {}\n\
         domyn_nexus_query_elapsed_ms_bucket{{le=\"1000\"}} {}\n\
         domyn_nexus_query_elapsed_ms_bucket{{le=\"5000\"}} {}\n\
         domyn_nexus_query_elapsed_ms_bucket{{le=\"+Inf\"}} {}\n\
         # TYPE domyn_nexus_vector_searches_total counter\n\
         domyn_nexus_vector_searches_total {}\n\
         # TYPE domyn_nexus_vector_search_results_returned_total counter\n\
         domyn_nexus_vector_search_results_returned_total {}\n\
         # TYPE domyn_nexus_vector_search_exact_candidates_total counter\n\
         domyn_nexus_vector_search_exact_candidates_total {}\n\
         # TYPE domyn_nexus_vector_search_exact_overlap_total counter\n\
         domyn_nexus_vector_search_exact_overlap_total {}\n\
         # TYPE domyn_nexus_storage_wal_sequence gauge\n\
         domyn_nexus_storage_wal_sequence {}\n\
         # TYPE domyn_nexus_storage_wal_active_bytes gauge\n\
         domyn_nexus_storage_wal_active_bytes {}\n\
         # TYPE domyn_nexus_storage_wal_live_segments gauge\n\
         domyn_nexus_storage_wal_live_segments {}\n\
         # TYPE domyn_nexus_storage_wal_live_segment_bytes gauge\n\
         domyn_nexus_storage_wal_live_segment_bytes {}\n\
         # TYPE domyn_nexus_storage_wal_archived_segments gauge\n\
         domyn_nexus_storage_wal_archived_segments {}\n\
         # TYPE domyn_nexus_storage_wal_archived_segment_bytes gauge\n\
         domyn_nexus_storage_wal_archived_segment_bytes {}\n\
         # TYPE domyn_nexus_storage_wal_recoverable_bytes gauge\n\
         domyn_nexus_storage_wal_recoverable_bytes {}\n\
         # TYPE domyn_nexus_storage_snapshot_bytes gauge\n\
         domyn_nexus_storage_snapshot_bytes {}\n\
         # TYPE domyn_nexus_storage_snapshot_modified_unix_seconds gauge\n\
         domyn_nexus_storage_snapshot_modified_unix_seconds {}\n\
         # TYPE domyn_nexus_storage_snapshot_age_seconds gauge\n\
         domyn_nexus_storage_snapshot_age_seconds {}\n\
         # TYPE domyn_nexus_storage_snapshot_archives gauge\n\
         domyn_nexus_storage_snapshot_archives {}\n\
         # TYPE domyn_nexus_storage_snapshot_archive_bytes gauge\n\
         domyn_nexus_storage_snapshot_archive_bytes {}\n\
         # TYPE domyn_nexus_storage_vector_snapshots gauge\n\
         domyn_nexus_storage_vector_snapshots {}\n\
         # TYPE domyn_nexus_storage_vector_snapshot_bytes gauge\n\
         domyn_nexus_storage_vector_snapshot_bytes {}\n\
         # TYPE domyn_nexus_storage_documents gauge\n\
         domyn_nexus_storage_documents {}\n\
         # TYPE domyn_nexus_storage_document_bytes gauge\n\
         domyn_nexus_storage_document_bytes {}\n\
         # TYPE domyn_nexus_storage_document_indexes gauge\n\
         domyn_nexus_storage_document_indexes {}\n\
         # TYPE domyn_nexus_storage_document_index_entries gauge\n\
         domyn_nexus_storage_document_index_entries {}\n\
         # TYPE domyn_nexus_storage_catalog_bytes gauge\n\
         domyn_nexus_storage_catalog_bytes {}\n\
         # TYPE domyn_nexus_tenants gauge\n\
         domyn_nexus_tenants {tenants}\n",
        u8::from(compaction.active),
        compaction.scheduled_total,
        compaction.completed_total,
        compaction.failed_total,
        compaction.skipped_active_total,
        backup.started_total,
        backup.completed_total,
        backup.failed_total,
        audit.events_total,
        audit.failed_total,
        queries.active,
        queries.total,
        queries.succeeded_total,
        queries.failed_total,
        queries.timed_out_total,
        queries.limited_total,
        queries.rejected_total,
        queries.rate_limited_total,
        queries.memory_rejected_total,
        queries.slow_total,
        queries.rows_returned_total,
        queries.bytes_returned_total,
        queries.elapsed_ms_total,
        queries.latency_le_1_ms,
        queries.latency_le_5_ms,
        queries.latency_le_10_ms,
        queries.latency_le_50_ms,
        queries.latency_le_100_ms,
        queries.latency_le_500_ms,
        queries.latency_le_1000_ms,
        queries.latency_le_5000_ms,
        queries.latency_inf,
        vector_search.searches_total,
        vector_search.results_returned_total,
        vector_search.exact_candidates_total,
        vector_search.exact_overlap_total,
        storage.wal_sequence,
        storage.wal_active_bytes,
        storage.wal_live_segment_count,
        storage.wal_live_segment_bytes,
        storage.wal_archived_segment_count,
        storage.wal_archived_segment_bytes,
        storage.wal_recoverable_bytes(),
        storage.snapshot_bytes,
        snapshot_modified_unix_seconds,
        snapshot_age_seconds,
        storage.snapshot_archive_count,
        storage.snapshot_archive_bytes,
        storage.vector_snapshot_count,
        storage.vector_snapshot_bytes,
        storage.document_count,
        storage.document_bytes,
        storage.document_index_count,
        storage.document_index_entries,
        storage.catalog_bytes,
    );
    append_vector_index_metrics(&mut body, &vector_search_by_index);
    append_vector_index_memory_metrics(&mut body, &vector_index_memory_by_name);

    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response())
}

fn append_vector_index_metrics(body: &mut String, by_index: &HashMap<String, VectorSearchMetrics>) {
    let mut names: Vec<_> = by_index.keys().collect();
    names.sort();
    for name in names {
        if let Some(metrics) = by_index.get(name) {
            let label = prom_label_escape(name);
            body.push_str(&format!(
                "domyn_nexus_vector_searches_total{{index=\"{label}\"}} {}\n\
                 domyn_nexus_vector_search_results_returned_total{{index=\"{label}\"}} {}\n\
                 domyn_nexus_vector_search_exact_candidates_total{{index=\"{label}\"}} {}\n\
                 domyn_nexus_vector_search_exact_overlap_total{{index=\"{label}\"}} {}\n",
                metrics.searches_total,
                metrics.results_returned_total,
                metrics.exact_candidates_total,
                metrics.exact_overlap_total,
            ));
        }
    }
}

fn append_vector_index_memory_metrics(body: &mut String, by_index: &HashMap<String, usize>) {
    let mut names: Vec<_> = by_index.keys().collect();
    names.sort();
    for name in names {
        if let Some(bytes) = by_index.get(name) {
            let label = prom_label_escape(name);
            body.push_str(&format!(
                "domyn_nexus_vector_index_memory_estimate_bytes{{index=\"{label}\"}} {bytes}\n",
            ));
        }
    }
}

fn prom_label_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

async fn handle_list_tenants(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<TenantListResponse>, (StatusCode, Json<ErrorResponse>)> {
    authorize(&state.config, &headers, AuthRole::Admin, None)?;

    Ok(Json(TenantListResponse {
        tenants: state.engine.list_tenants(),
    }))
}

async fn handle_create_tenant(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ProductJson(req): ProductJson<CreateTenantRequest>,
) -> Result<(StatusCode, Json<TenantListResponse>), (StatusCode, Json<ErrorResponse>)> {
    authorize(&state.config, &headers, AuthRole::Admin, None)?;

    if state.engine.get_engine(&req.tenant_id).is_some() {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::coded(
                "TENANT_EXISTS",
                format!("tenant already exists: {}", req.tenant_id),
            )),
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
    required_role: AuthRole,
    tenant: Option<&str>,
) -> Result<Principal, (StatusCode, Json<ErrorResponse>)> {
    if config.auth_token.is_none() && config.auth_principals.is_empty() {
        return Ok(Principal::anonymous());
    }

    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    let presented = bearer.or(api_key);

    let Some(token) = presented else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::coded("UNAUTHORIZED", "unauthorized")),
        ));
    };

    let principal = if config.auth_token.as_deref() == Some(token) {
        Principal::legacy_admin()
    } else if let Some(configured) = config.auth_principals.iter().find(|p| p.token == token) {
        Principal::from_config(configured)
    } else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::coded("UNAUTHORIZED", "unauthorized")),
        ));
    };

    if !principal.role.allows(required_role) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::coded(
                "FORBIDDEN",
                format!(
                    "principal {} with role {:?} cannot perform {:?}",
                    principal.name, principal.role, required_role
                ),
            )),
        ));
    }

    if !principal.can_access_tenant(tenant) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::coded(
                "TENANT_FORBIDDEN",
                format!(
                    "principal {} is not authorized for tenant {}",
                    principal.name,
                    tenant.unwrap_or("default")
                ),
            )),
        ));
    }

    Ok(principal)
}

fn resolve_backup_path(
    config: &ServerConfig,
    requested: &str,
) -> Result<PathBuf, (StatusCode, Json<ErrorResponse>)> {
    if requested.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::coded(
                "BACKUP_PATH_INVALID",
                "backup path must not be empty",
            )),
        ));
    }

    let requested = PathBuf::from(requested);
    let Some(root) = config.backup_root.as_deref() else {
        return Ok(requested);
    };

    if root.trim().is_empty() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::coded(
                "BACKUP_ROOT_INVALID",
                "configured backup_root must not be empty",
            )),
        ));
    }

    if requested.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::coded(
                "BACKUP_PATH_FORBIDDEN",
                "absolute backup paths are not allowed when backup_root is configured",
            )),
        ));
    }

    for component in requested.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::coded(
                        "BACKUP_PATH_FORBIDDEN",
                        "backup path must stay under configured backup_root",
                    )),
                ));
            }
        }
    }

    Ok(PathBuf::from(root).join(requested))
}

fn record_audit_event(
    state: &Arc<AppState>,
    principal: &Principal,
    action: &'static str,
    tenant: Option<&str>,
    outcome: &'static str,
    detail: Option<String>,
) {
    let Some(path) = state.config.audit_log_path.as_deref() else {
        return;
    };

    let event = AuditEvent {
        timestamp_unix_seconds: unix_secs(),
        principal: principal.name.clone(),
        action,
        outcome,
        tenant,
        detail,
    };

    match append_audit_event(path, &event) {
        Ok(()) => {
            state.audit.events_total.fetch_add(1, Ordering::Relaxed);
        }
        Err(err) => {
            state.audit.failed_total.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(error = %err, action, outcome, "audit log write failed");
        }
    }
}

fn append_audit_event(path: &str, event: &AuditEvent<'_>) -> Result<(), String> {
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| err.to_string())?;
    serde_json::to_writer(&mut file, event).map_err(|err| err.to_string())?;
    file.write_all(b"\n").map_err(|err| err.to_string())?;
    file.flush().map_err(|err| err.to_string())
}

fn http_query_is_write(query: &str) -> bool {
    match Parser::parse(query) {
        Ok(Statement::Write(_)) => return true,
        Ok(Statement::Read(_)) => return false,
        Err(_) => {}
    }

    matches!(
        NexusParser::parse_statement_kyu(query),
        Ok(ParsedStatement::Write(_))
    )
}

fn resolve_request_engine(
    state: &AppState,
    tenant: Option<&str>,
) -> Result<Arc<NexusEngine>, (StatusCode, Json<ErrorResponse>)> {
    state.engine.resolve(tenant).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::coded("TENANT_NOT_FOUND", e)),
        )
    })
}

fn vector_error_response(err: TxError) -> (StatusCode, Json<ErrorResponse>) {
    let message = err.to_string();
    let (status, code) = if message.contains("not found") {
        (StatusCode::NOT_FOUND, "VECTOR_INDEX_NOT_FOUND")
    } else {
        (StatusCode::BAD_REQUEST, "VECTOR_ERROR")
    };
    (status, Json(ErrorResponse::coded(code, message)))
}

fn document_error_response(err: TxError) -> (StatusCode, Json<ErrorResponse>) {
    let message = err.to_string();
    let (status, code) = if message.contains("no durable store") {
        (StatusCode::BAD_REQUEST, "DOCUMENT_STORE_UNAVAILABLE")
    } else if message.contains("invalid collection") || message.contains("invalid document key") {
        (StatusCode::BAD_REQUEST, "DOCUMENT_BAD_NAME")
    } else {
        (StatusCode::BAD_REQUEST, "DOCUMENT_ERROR")
    };
    (status, Json(ErrorResponse::coded(code, message)))
}

fn is_row_budget_error(err: &nexus_cypher::error::CypherError) -> bool {
    err.to_string().contains("query row budget exceeded")
}

fn is_byte_budget_error(err: &nexus_cypher::error::CypherError) -> bool {
    err.to_string().contains("query byte budget exceeded")
}

fn vector_task_error_response(
    code: &'static str,
    err: tokio::task::JoinError,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::coded(
            code,
            format!("vector task panicked: {err}"),
        )),
    )
}

fn document_task_error_response(
    code: &'static str,
    err: tokio::task::JoinError,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::coded(
            code,
            format!("document task panicked: {err}"),
        )),
    )
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

fn pressure_response(pressure: GraphCompactionPressure) -> CompactionPressureResponse {
    CompactionPressureResponse {
        deleted_vertices: pressure.deleted_vertices,
        tombstoned_edges: pressure.tombstoned_edges,
        delta_edges: pressure.delta_edges,
    }
}

fn stats_response(stats: GraphCompactionStats) -> CompactionStatsResponse {
    CompactionStatsResponse {
        deleted_vertices_cleared: stats.deleted_vertices_cleared,
        deleted_edges_cleared: stats.deleted_edges_cleared,
        tombstoned_edge_meta_removed: stats.tombstoned_edge_meta_removed,
        delta_edges_compacted: stats.delta_edges_compacted,
        live_edges_after: stats.live_edges_after,
    }
}

fn enforce_result_limits(
    qr: &nexus_cypher::executor::QueryResult,
    config: &ServerConfig,
    estimated_bytes: usize,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if config.default_query_limit > 0 && qr.rows.len() > config.default_query_limit {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse::coded(
                "RESULT_ROW_LIMIT_EXCEEDED",
                format!(
                    "query returned {} rows, exceeding configured default_query_limit {}",
                    qr.rows.len(),
                    config.default_query_limit
                ),
            )),
        ));
    }

    if config.query_memory_budget_bytes > 0 && estimated_bytes > config.query_memory_budget_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse::coded(
                "RESULT_MEMORY_BUDGET_EXCEEDED",
                format!(
                    "query result estimate {estimated_bytes} bytes exceeds configured query_memory_budget_bytes {}",
                    config.query_memory_budget_bytes
                ),
            )),
        ));
    }

    Ok(())
}

fn estimate_query_result_bytes(qr: &nexus_cypher::executor::QueryResult) -> usize {
    let column_bytes: usize = qr.columns.iter().map(|c| c.len()).sum();
    let row_bytes: usize = qr
        .rows
        .iter()
        .flat_map(|row| row.iter())
        .map(estimate_value_bytes)
        .sum();
    column_bytes + row_bytes
}

fn estimate_value_bytes(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int64(_) | Value::Float64(_) => 8,
        Value::String(s) => s.len(),
        Value::Bytes(bytes) => bytes.len(),
        Value::List(items) => items.iter().map(estimate_value_bytes).sum(),
        Value::Map(entries) => entries
            .iter()
            .map(|(key, value)| key.len() + estimate_value_bytes(value))
            .sum(),
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn process_resident_memory_bytes() -> u64 {
    process_resident_memory_bytes_impl().unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn process_resident_memory_bytes_impl() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("VmRSS:") else {
            continue;
        };
        let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
        return kb.checked_mul(1024);
    }
    None
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_resident_memory_bytes_impl() -> Option<u64> {
    let pid = std::process::id().to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let kb = text.split_whitespace().next()?.parse::<u64>().ok()?;
    kb.checked_mul(1024)
}

#[cfg(not(unix))]
fn process_resident_memory_bytes_impl() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineBuilder;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use nexus_core::graph::Graph;
    use nexus_core::properties::PropertyType;
    use nexus_core::types::VertexId;
    use nexus_storage::persistence::NexusStore;
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

    fn durable_document_router() -> (Router, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = NexusStore::open(dir.path().join("db")).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = Arc::new(NexusEngine::with_store(graph, store));
        let mt = Arc::new(MultiTenantEngine::new_with_arc("default", engine));
        (create_multi_tenant_router(mt), dir)
    }

    async fn read_error_response(response: Response) -> ErrorResponse {
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert!(error.code.as_deref().is_some_and(|code| !code.is_empty()));
        assert!(!error.message.is_empty());
        assert_eq!(error.error, error.message);
        assert!(error.details.is_object());
        error
    }

    async fn assert_error_response_shape(
        response: Response,
        expected_status: StatusCode,
        expected_code: &str,
    ) {
        assert_eq!(response.status(), expected_status);
        let error = read_error_response(response).await;
        assert_eq!(error.code.as_deref(), Some(expected_code));
    }

    async fn call_json(
        app: Router,
        method: Method,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap();
        (status, json)
    }

    async fn call_empty(app: Router, method: Method, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap();
        (status, json)
    }

    fn sorted_object_keys(value: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<_> = value
            .as_object()
            .expect("response must be a JSON object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    fn assert_object_keys(value: &serde_json::Value, expected: &[&str]) {
        let expected: Vec<String> = expected.iter().map(|key| (*key).to_string()).collect();
        assert_eq!(sorted_object_keys(value), expected);
    }

    #[test]
    fn server_config_exposes_production_hardening_knobs() {
        let config = ServerConfig::default();
        assert_eq!(config.query_timeout_secs, QUERY_TIMEOUT_SECS);
        assert_eq!(config.max_body_bytes, MAX_BODY_BYTES);
        assert_eq!(config.max_concurrent_queries, 128);
        assert_eq!(config.max_query_rate_per_sec, 0);
        assert_eq!(config.max_query_rate_per_tenant_per_sec, 0);
        assert_eq!(config.slow_query_ms, 1000);
        assert_eq!(config.wal_retention_segments, 8);
        assert_eq!(config.snapshot_retention, 2);
        assert_eq!(config.compaction_threshold, 4);
        assert_eq!(config.query_memory_budget_bytes, 512 * 1024 * 1024);
        assert_eq!(config.process_memory_budget_bytes, 0);
        assert_eq!(config.default_query_limit, 10_000);
        assert_eq!(config.vector_index_mode, VectorIndexMode::Exact);
        assert_eq!(config.backup_root, None);
        assert_eq!(config.audit_log_path, None);

        let wal_options = config.wal_options();
        assert_eq!(wal_options.max_segment_bytes, Some(64 * 1024 * 1024));
        assert_eq!(wal_options.retained_archived_segments, 8);

        let store_options = config.store_options();
        assert_eq!(store_options.snapshot_retention, 2);
        assert_eq!(store_options.wal.max_segment_bytes, Some(64 * 1024 * 1024));
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
    async fn contract_cypher_endpoint_response_shape() {
        let engine = test_engine();
        let app = create_router(engine);

        let (status, json) = call_json(
            app,
            Method::POST,
            "/cypher",
            serde_json::json!({
                "query": "MATCH (n:Entity) WHERE n.name = $name RETURN n.name AS name",
                "params": { "name": "Apple" }
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_object_keys(&json, &["columns", "rows", "time_ms"]);
        assert_eq!(json["columns"], serde_json::json!(["name"]));
        assert_eq!(json["rows"], serde_json::json!([["Apple"]]));
        assert!(
            json["time_ms"].as_f64().is_some(),
            "time_ms must remain a numeric field"
        );
    }

    #[tokio::test]
    async fn contract_tenants_endpoint_response_shapes() {
        let mt = test_mt_engine();
        let app = create_multi_tenant_router(mt);

        let (status, json) = call_empty(app.clone(), Method::GET, "/tenants").await;
        assert_eq!(status, StatusCode::OK);
        assert_object_keys(&json, &["tenants"]);
        assert!(
            json["tenants"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("default"))
        );

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/tenants",
            serde_json::json!({
                "tenant_id": "AAPL",
                "vertex_capacity": 8,
                "edge_capacity": 8
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_object_keys(&json, &["tenants"]);
        assert!(
            json["tenants"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("AAPL"))
        );

        let (status, json) = call_json(
            app,
            Method::POST,
            "/tenants",
            serde_json::json!({ "tenant_id": "AAPL" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_object_keys(&json, &["code", "details", "error", "message"]);
        assert_eq!(json["code"], "TENANT_EXISTS");
        assert_eq!(json["message"], json["error"]);
        assert!(json["details"].is_object());
    }

    #[tokio::test]
    async fn test_cypher_endpoint_enforces_default_query_limit() {
        let engine = test_engine();
        let app = create_multi_tenant_router_with_config(
            Arc::new(MultiTenantEngine::new_with_arc("default", engine)),
            ServerConfig {
                default_query_limit: 1,
                ..ServerConfig::default()
            },
        );

        let body = serde_json::json!({ "query": "MATCH (n:Entity) RETURN n.name" });
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

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code.as_deref(), Some("RESULT_ROW_LIMIT_EXCEEDED"));
        assert!(error.error.contains("default_query_limit"));
    }

    #[tokio::test]
    async fn test_cypher_endpoint_enforces_query_memory_budget() {
        let engine = test_engine();
        let app = create_multi_tenant_router_with_config(
            Arc::new(MultiTenantEngine::new_with_arc("default", engine)),
            ServerConfig {
                query_memory_budget_bytes: 8,
                ..ServerConfig::default()
            },
        );

        let body = serde_json::json!({ "query": "MATCH (n:Entity) RETURN n.name" });
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

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code.as_deref(), Some("RESULT_MEMORY_BUDGET_EXCEEDED"));
        assert!(error.error.contains("query_memory_budget_bytes"));
    }

    #[tokio::test]
    async fn test_cypher_endpoint_rejects_when_process_memory_budget_is_exceeded() {
        let engine = test_engine();
        let app = create_multi_tenant_router_with_config(
            Arc::new(MultiTenantEngine::new_with_arc("default", engine)),
            ServerConfig {
                process_memory_budget_bytes: 1,
                process_memory_budget_sample_bytes: Some(2),
                ..ServerConfig::default()
            },
        );

        let body = serde_json::json!({ "query": "MATCH (n:Entity) RETURN n.name" });
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

        assert_error_response_shape(
            response,
            StatusCode::SERVICE_UNAVAILABLE,
            "PROCESS_MEMORY_BUDGET_EXCEEDED",
        )
        .await;
    }

    #[test]
    fn process_memory_budget_error_is_reported_when_rss_exceeds_budget() {
        let Some((status, Json(error))) = process_memory_budget_error(100, 101) else {
            panic!("expected process memory budget error");
        };

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error.code.as_deref(),
            Some("PROCESS_MEMORY_BUDGET_EXCEEDED")
        );
        assert!(error.error.contains("process_memory_budget_bytes 100"));
        assert!(process_memory_budget_error(100, 100).is_none());
        assert!(process_memory_budget_error(100, 0).is_none());
        assert!(process_memory_budget_error(0, 101).is_none());
    }

    #[test]
    fn query_permit_limit_rejects_over_capacity_without_starting_query() {
        let state = Arc::new(AppState {
            engine: test_mt_engine(),
            config: Arc::new(ServerConfig {
                max_concurrent_queries: 1,
                ..ServerConfig::default()
            }),
            compaction: Arc::new(CompactionRuntime::default()),
            backup: Arc::new(BackupRuntime::default()),
            queries: Arc::new(QueryRuntime::default()),
            query_permits: Some(Arc::new(Semaphore::new(1))),
            rate_limit: Arc::new(RateLimitRuntime::default()),
            audit: Arc::new(AuditRuntime::default()),
        });

        let first = acquire_query_permit(&state).unwrap();
        let second = acquire_query_permit(&state);
        assert!(second.is_err());
        let (status, body) = second.err().unwrap();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.0.code.as_deref(), Some("QUERY_OVER_CAPACITY"));
        assert_eq!(state.queries.snapshot().rejected_total, 1);
        assert_eq!(state.queries.snapshot().total, 0);

        drop(first);
        assert!(acquire_query_permit(&state).unwrap().is_some());
    }

    #[test]
    fn global_query_rate_limit_rejects_over_budget_without_starting_query() {
        let state = Arc::new(AppState {
            engine: test_mt_engine(),
            config: Arc::new(ServerConfig {
                max_query_rate_per_sec: 1,
                ..ServerConfig::default()
            }),
            compaction: Arc::new(CompactionRuntime::default()),
            backup: Arc::new(BackupRuntime::default()),
            queries: Arc::new(QueryRuntime::default()),
            query_permits: None,
            rate_limit: Arc::new(RateLimitRuntime::default()),
            audit: Arc::new(AuditRuntime::default()),
        });

        state.rate_limit.windows.lock().unwrap().insert(
            "__global__".into(),
            RateLimitWindow {
                start_secs: unix_secs(),
                count: 1,
            },
        );

        let rejected = enforce_query_rate_limit(&state, "default");
        assert!(rejected.is_err());
        let (status, body) = rejected.err().unwrap();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body.0.code.as_deref(), Some("QUERY_RATE_LIMITED"));
        assert!(body.0.error.contains("max_query_rate_per_sec"));

        let snapshot = state.queries.snapshot();
        assert_eq!(snapshot.rate_limited_total, 1);
        assert_eq!(snapshot.total, 0);
    }

    #[test]
    fn tenant_query_rate_limit_rejects_only_matching_tenant() {
        let state = Arc::new(AppState {
            engine: test_mt_engine(),
            config: Arc::new(ServerConfig {
                max_query_rate_per_tenant_per_sec: 1,
                ..ServerConfig::default()
            }),
            compaction: Arc::new(CompactionRuntime::default()),
            backup: Arc::new(BackupRuntime::default()),
            queries: Arc::new(QueryRuntime::default()),
            query_permits: None,
            rate_limit: Arc::new(RateLimitRuntime::default()),
            audit: Arc::new(AuditRuntime::default()),
        });

        state.rate_limit.windows.lock().unwrap().insert(
            "tenant:AAPL".into(),
            RateLimitWindow {
                start_secs: unix_secs(),
                count: 1,
            },
        );

        let rejected = enforce_query_rate_limit(&state, "AAPL");
        assert!(rejected.is_err());
        let (status, body) = rejected.err().unwrap();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body.0.code.as_deref(), Some("TENANT_QUERY_RATE_LIMITED"));
        assert!(body.0.error.contains("max_query_rate_per_tenant_per_sec"));

        assert!(enforce_query_rate_limit(&state, "MSFT").is_ok());
        let snapshot = state.queries.snapshot();
        assert_eq!(snapshot.rate_limited_total, 1);
        assert_eq!(snapshot.total, 0);
    }

    #[test]
    fn query_runtime_records_latency_buckets() {
        let runtime = Arc::new(QueryRuntime::default());
        runtime
            .begin()
            .finish_success(2, 32, Duration::from_millis(42));
        runtime.record_slow_query();

        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.total, 1);
        assert_eq!(snapshot.succeeded_total, 1);
        assert_eq!(snapshot.slow_total, 1);
        assert_eq!(snapshot.latency_le_10_ms, 0);
        assert_eq!(snapshot.latency_le_50_ms, 1);
        assert_eq!(snapshot.latency_le_5000_ms, 1);
        assert_eq!(snapshot.latency_inf, 1);
    }

    #[tokio::test]
    async fn test_query_metrics_track_success_and_limited_results() {
        let engine = test_engine();
        let app = create_multi_tenant_router_with_config(
            Arc::new(MultiTenantEngine::new_with_arc("default", engine)),
            ServerConfig {
                default_query_limit: 1,
                ..ServerConfig::default()
            },
        );

        let one_row = serde_json::json!({
            "query": "MATCH (n:Entity) WHERE n.name = 'Apple' RETURN n.name"
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&one_row).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let two_rows = serde_json::json!({ "query": "MATCH (n:Entity) RETURN n.name" });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&two_rows).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

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
        assert!(text.contains("domyn_nexus_queries_active 0"));
        assert!(text.contains("domyn_nexus_queries_total 2"));
        assert!(text.contains("domyn_nexus_queries_succeeded_total 1"));
        assert!(text.contains("domyn_nexus_queries_limited_total 1"));
        assert!(text.contains("domyn_nexus_queries_rejected_total 0"));
        assert!(text.contains("domyn_nexus_queries_rate_limited_total 0"));
        assert!(text.contains("domyn_nexus_queries_slow_total"));
        assert!(text.contains("domyn_nexus_query_elapsed_ms_bucket"));
    }

    #[tokio::test]
    async fn test_cypher_endpoint_runs_threshold_compaction() {
        let engine = test_engine();
        let app = create_multi_tenant_router_with_config(
            Arc::new(MultiTenantEngine::new_with_arc("default", engine.clone())),
            ServerConfig {
                compaction_threshold: 1,
                ..ServerConfig::default()
            },
        );

        let body =
            serde_json::json!({ "query": "MATCH (n:Entity) WHERE n.name = 'Apple' DELETE n" });
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
        for _ in 0..50 {
            if engine.compaction_pressure().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(engine.compaction_pressure().is_empty());
        let graph = engine.graph().read();
        assert!(graph.get_vertex_property(VertexId(0), "name").is_null());
    }

    #[tokio::test]
    async fn test_manual_compaction_endpoint_compacts_default_engine() {
        let engine = test_engine();
        let app = create_multi_tenant_router_with_config(
            Arc::new(MultiTenantEngine::new_with_arc("default", engine.clone())),
            ServerConfig {
                compaction_threshold: 0,
                ..ServerConfig::default()
            },
        );

        let body =
            serde_json::json!({ "query": "MATCH (n:Entity) WHERE n.name = 'Apple' DELETE n" });
        let response = app
            .clone()
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
        assert!(!engine.compaction_pressure().is_empty());

        let body = serde_json::json!({});
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/compact")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let response: CompactResponse = serde_json::from_slice(&body).unwrap();
        assert!(response.compacted);
        assert_eq!(response.pressure_before.deleted_vertices, 1);
        assert_eq!(response.pressure_after.deleted_vertices, 0);
        assert!(engine.compaction_pressure().is_empty());
    }

    #[tokio::test]
    async fn test_admin_backup_endpoint_creates_restorable_backup() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let backup_path = dir.path().join("backup");
        let restore_path = dir.path().join("restore");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = nexus_core::graph::Graph::new(0, 0);
        graph.build();
        let engine = Arc::new(NexusEngine::with_store(graph, store));
        let app = create_router(engine.clone());

        engine
            .execute_cypher("CREATE (n:Entity {name: 'backup-proof'}) RETURN n.name")
            .unwrap();

        let body = serde_json::json!({
            "path": backup_path.to_string_lossy(),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/backup")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let response: BackupResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response.path, backup_path.to_string_lossy());
        assert!(
            response
                .manifest
                .files
                .iter()
                .any(|file| file == "graph.wal")
        );
        assert!(backup_path.join("backup-manifest.json").exists());

        NexusStore::restore_backup(&backup_path, &restore_path).unwrap();
        let restored = NexusStore::open(&restore_path).unwrap();
        let graph = restored.load_graph(4, 4).unwrap();
        assert_eq!(
            graph.get_vertex_property(VertexId(0), "name"),
            Value::String("backup-proof".into())
        );
    }

    #[tokio::test]
    async fn test_admin_backup_endpoint_resolves_relative_path_under_configured_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let backup_root = dir.path().join("allowed-backups");
        let expected_backup_path = backup_root.join("daily").join("snapshot");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = nexus_core::graph::Graph::new(0, 0);
        graph.build();
        let engine = Arc::new(NexusEngine::with_store(graph, store));
        let app = create_multi_tenant_router_with_config(
            Arc::new(MultiTenantEngine::new_with_arc("default", engine.clone())),
            ServerConfig {
                backup_root: Some(backup_root.to_string_lossy().into_owned()),
                ..ServerConfig::default()
            },
        );

        engine
            .execute_cypher("CREATE (n:Entity {name: 'root-confined'}) RETURN n.name")
            .unwrap();

        let body = serde_json::json!({ "path": "daily/snapshot" });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/backup")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let response: BackupResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response.path, expected_backup_path.to_string_lossy());
        assert!(expected_backup_path.join("backup-manifest.json").exists());
    }

    #[tokio::test]
    async fn test_admin_backup_endpoint_rejects_paths_outside_configured_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let backup_root = dir.path().join("allowed-backups");
        let app = create_multi_tenant_router_with_config(
            Arc::new(MultiTenantEngine::new_with_arc("default", test_engine())),
            ServerConfig {
                backup_root: Some(backup_root.to_string_lossy().into_owned()),
                ..ServerConfig::default()
            },
        );

        let body = serde_json::json!({ "path": "../escape" });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/backup")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let error: ErrorResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error.code.as_deref(), Some("BACKUP_PATH_FORBIDDEN"));

        let absolute_path = dir.path().join("absolute-backup");
        let body = serde_json::json!({ "path": absolute_path.to_string_lossy() });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/backup")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let error: ErrorResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error.code.as_deref(), Some("BACKUP_PATH_FORBIDDEN"));
    }

    #[tokio::test]
    async fn test_admin_backup_endpoint_rejects_memory_only_engine() {
        let dir = tempfile::TempDir::new().unwrap();
        let backup_path = dir.path().join("backup");
        let app = create_router(test_engine());

        let body = serde_json::json!({
            "path": backup_path.to_string_lossy(),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/backup")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let error: ErrorResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error.code.as_deref(), Some("BACKUP_UNAVAILABLE"));
    }

    #[tokio::test]
    async fn test_http_write_appends_audit_event_when_configured() {
        let dir = tempfile::TempDir::new().unwrap();
        let audit_log_path = dir.path().join("audit").join("audit.jsonl");
        let app = create_multi_tenant_router_with_config(
            test_mt_engine(),
            ServerConfig {
                auth_principals: vec![AuthPrincipalConfig {
                    name: "writer".into(),
                    token: "writer-token".into(),
                    role: AuthRole::ReadWrite,
                    tenants: vec!["default".into()],
                }],
                audit_log_path: Some(audit_log_path.to_string_lossy().into_owned()),
                ..ServerConfig::default()
            },
        );

        let body = serde_json::json!({
            "query": "CREATE (n:Entity {name: 'Audited'}) RETURN n.name",
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("content-type", "application/json")
                    .header("x-api-key", "writer-token")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let log = std::fs::read_to_string(&audit_log_path).unwrap();
        let event: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(event["action"], "cypher.write");
        assert_eq!(event["outcome"], "success");
        assert_eq!(event["principal"], "writer");
        assert_eq!(event["detail"], "rows=1");
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
    async fn test_product_error_shape_across_core_endpoints() {
        let dir = tempfile::TempDir::new().unwrap();
        let backup_root = dir.path().join("backups");
        let app = create_multi_tenant_router_with_config(
            test_mt_engine(),
            ServerConfig {
                auth_token: Some("admin-token".into()),
                backup_root: Some(backup_root.to_string_lossy().into_owned()),
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
        assert_error_response_shape(response, StatusCode::UNAUTHORIZED, "UNAUTHORIZED").await;

        let body = serde_json::json!({ "query": "MATCH (n RETURN n" });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_error_response_shape(response, StatusCode::BAD_REQUEST, "CYPHER_ERROR").await;

        let body = serde_json::json!({ "path": "../escape" });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/backup")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_error_response_shape(response, StatusCode::BAD_REQUEST, "BACKUP_PATH_FORBIDDEN")
            .await;

        let body = serde_json::json!({ "query": [1.0, 0.0], "k": 2 });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vectors/missing/search")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_error_response_shape(response, StatusCode::NOT_FOUND, "VECTOR_INDEX_NOT_FOUND")
            .await;
    }

    #[tokio::test]
    async fn test_json_extractor_rejections_use_product_error_shape() {
        let app = create_multi_tenant_router(test_mt_engine());

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .body(Body::from(r#"{"query":"MATCH (n) RETURN n"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_error_response_shape(
            response,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "JSON_CONTENT_TYPE_REQUIRED",
        )
        .await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"MATCH (n) RETURN n""#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_error_response_shape(response, StatusCode::BAD_REQUEST, "JSON_REQUEST_INVALID")
            .await;

        let small_body_app = create_multi_tenant_router_with_config(
            test_mt_engine(),
            ServerConfig {
                max_body_bytes: 16,
                ..ServerConfig::default()
            },
        );
        let response = small_body_app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"query":"MATCH (n) RETURN n","params":{"large":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_error_response_shape(
            response,
            StatusCode::PAYLOAD_TOO_LARGE,
            "REQUEST_BODY_TOO_LARGE",
        )
        .await;
    }

    #[tokio::test]
    async fn test_read_only_principal_can_read_but_cannot_write() {
        let app = create_multi_tenant_router_with_config(
            test_mt_engine(),
            ServerConfig {
                auth_principals: vec![AuthPrincipalConfig {
                    name: "reader".into(),
                    token: "reader-token".into(),
                    role: AuthRole::ReadOnly,
                    tenants: vec!["default".into()],
                }],
                ..ServerConfig::default()
            },
        );

        let read_body = serde_json::json!({
            "query": "MATCH (n:Entity) RETURN n.name",
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer reader-token")
                    .body(Body::from(serde_json::to_vec(&read_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let write_body = serde_json::json!({
            "query": "CREATE (n:Entity {name: 'blocked'})",
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer reader-token")
                    .body(Body::from(serde_json::to_vec(&write_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let error: ErrorResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error.code.as_deref(), Some("FORBIDDEN"));
    }

    #[tokio::test]
    async fn test_principal_tenant_scope_is_enforced() {
        let app = create_multi_tenant_router_with_config(
            test_mt_engine(),
            ServerConfig {
                auth_principals: vec![AuthPrincipalConfig {
                    name: "nvda-reader".into(),
                    token: "nvda-token".into(),
                    role: AuthRole::ReadOnly,
                    tenants: vec!["NVDA".into()],
                }],
                ..ServerConfig::default()
            },
        );

        let body = serde_json::json!({
            "tenant": "AAPL",
            "query": "MATCH (n:Entity) RETURN n.name",
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cypher")
                    .header("content-type", "application/json")
                    .header("x-api-key", "nvda-token")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let error: ErrorResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error.code.as_deref(), Some("TENANT_FORBIDDEN"));
    }

    #[tokio::test]
    async fn test_non_admin_principal_cannot_enumerate_tenants() {
        let app = create_multi_tenant_router_with_config(
            test_mt_engine(),
            ServerConfig {
                auth_principals: vec![AuthPrincipalConfig {
                    name: "writer".into(),
                    token: "writer-token".into(),
                    role: AuthRole::ReadWrite,
                    tenants: vec!["*".into()],
                }],
                ..ServerConfig::default()
            },
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/tenants")
                    .header("authorization", "Bearer writer-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_vector_http_lifecycle() {
        let engine = test_engine();
        let app = create_router(engine);

        let body = serde_json::json!({ "dimension": 2 });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vectors/entities")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/vectors")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: VectorIndexListResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(list.indexes, vec!["entities".to_string()]);

        for (vertex_id, embedding) in [(0_u64, vec![1.0, 0.0]), (1, vec![0.0, 1.0])] {
            let body = serde_json::json!({
                "vertex_id": vertex_id,
                "embedding": embedding,
            });
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/vectors/entities/upsert")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let body = serde_json::json!({ "query": [1.0, 0.0], "k": 2 });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vectors/entities/search")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let search: VectorSearchResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(search.results.len(), 2);
        assert_eq!(search.results[0].vertex_id, 0);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/vectors/entities/0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let removed: VectorRemoveResponse = serde_json::from_slice(&body).unwrap();
        assert!(removed.removed);

        let body = serde_json::json!({ "query": [1.0, 0.0], "k": 2 });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vectors/entities/search")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let search: VectorSearchResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(search.results.len(), 1);
        assert_eq!(search.results[0].vertex_id, 1);

        let body = serde_json::json!({});
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vectors/entities/compact")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn contract_vector_endpoint_response_shapes() {
        let engine = test_engine();
        let app = create_router(engine);

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/vectors/entities",
            serde_json::json!({ "dimension": 2 }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_object_keys(&json, &["ok"]);
        assert_eq!(json["ok"], true);

        let (status, json) = call_empty(app.clone(), Method::GET, "/vectors").await;
        assert_eq!(status, StatusCode::OK);
        assert_object_keys(&json, &["indexes"]);
        assert_eq!(json["indexes"], serde_json::json!(["entities"]));

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/vectors/entities/upsert",
            serde_json::json!({
                "vertex_id": 0,
                "embedding": [1.0, 0.0]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_object_keys(&json, &["ok"]);
        assert_eq!(json["ok"], true);

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/vectors/entities/search",
            serde_json::json!({
                "query": [1.0, 0.0],
                "k": 1
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_object_keys(&json, &["results"]);
        let first = &json["results"].as_array().unwrap()[0];
        assert_object_keys(first, &["distance", "vertex_id"]);
        assert_eq!(first["vertex_id"], 0);
        assert!(first["distance"].as_f64().is_some());

        let (status, json) = call_empty(app.clone(), Method::DELETE, "/vectors/entities/0").await;
        assert_eq!(status, StatusCode::OK);
        assert_object_keys(&json, &["removed"]);
        assert_eq!(json["removed"], true);

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/vectors/entities/compact",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_object_keys(&json, &["ok"]);
        assert_eq!(json["ok"], true);

        let (status, json) = call_json(
            app,
            Method::POST,
            "/vectors/missing/search",
            serde_json::json!({
                "query": [1.0, 0.0],
                "k": 1
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_object_keys(&json, &["code", "details", "error", "message"]);
        assert_eq!(json["code"], "VECTOR_INDEX_NOT_FOUND");
        assert_eq!(json["message"], json["error"]);
        assert!(json["details"].is_object());
    }

    #[tokio::test]
    async fn test_document_http_lifecycle() {
        let (app, _dir) = durable_document_router();

        let (status, json) = call_json(
            app.clone(),
            Method::PUT,
            "/collections/filings/documents/nvda-2024",
            serde_json::json!({
                "document": {
                    "ticker": "NVDA",
                    "year": 2024,
                    "tags": ["10-K", "risk"]
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);

        let (status, json) = call_empty(app.clone(), Method::GET, "/collections").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["collections"], serde_json::json!(["filings"]));

        let (status, json) = call_empty(
            app.clone(),
            Method::GET,
            "/collections/filings/documents?limit=10",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["documents"].as_array().unwrap().len(), 1);
        assert_eq!(json["documents"][0]["key"], "nvda-2024");

        let (status, json) = call_empty(
            app.clone(),
            Method::GET,
            "/collections/filings/documents/nvda-2024",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["document"]["ticker"], "NVDA");

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/indexes",
            serde_json::json!({ "path": "ticker" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["collection"], "filings");
        assert_eq!(json["path"], "ticker");

        let (status, json) =
            call_empty(app.clone(), Method::GET, "/collections/filings/indexes").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["indexes"],
            serde_json::json!([{ "collection": "filings", "path": "ticker", "kind": "scalar" }])
        );

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({ "path": "ticker", "value": "NVDA", "limit": 10 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["documents"].as_array().unwrap().len(), 1);
        assert_eq!(json["documents"][0]["key"], "nvda-2024");

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/cypher",
            serde_json::json!({
                "query": "RETURN document('filings', 'nvda-2024').ticker AS ticker"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["columns"], serde_json::json!(["ticker"]));
        assert_eq!(json["rows"], serde_json::json!([["NVDA"]]));

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/cypher",
            serde_json::json!({
                "query": "UNWIND documentsBy('filings', 'ticker', 'NVDA', 10) AS doc RETURN doc.key AS key"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["columns"], serde_json::json!(["key"]));
        assert_eq!(json["rows"], serde_json::json!([["nvda-2024"]]));

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/cypher",
            serde_json::json!({
                "query": "MATCH DOCUMENT doc IN filings WHERE doc.document.ticker = 'NVDA' RETURN doc.key AS key"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["columns"], serde_json::json!(["key"]));
        assert_eq!(json["rows"], serde_json::json!([["nvda-2024"]]));

        let (status, json) = call_empty(
            app.clone(),
            Method::DELETE,
            "/collections/filings/documents/nvda-2024",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["deleted"], true);

        let (status, json) = call_empty(
            app.clone(),
            Method::GET,
            "/collections/filings/documents/nvda-2024",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["code"], "DOCUMENT_NOT_FOUND");
    }

    #[tokio::test]
    async fn test_document_http_fulltext_index_search() {
        let (app, _dir) = durable_document_router();

        for (key, body) in [
            (
                "nvda-2024",
                "Revenue growth accelerated while supply-chain risk remained visible.",
            ),
            (
                "aapl-2024",
                "Services margin expanded with stable device demand.",
            ),
        ] {
            let (status, _) = call_json(
                app.clone(),
                Method::PUT,
                &format!("/collections/filings/documents/{key}"),
                serde_json::json!({ "document": { "body": body } }),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/indexes",
            serde_json::json!({ "path": "body", "kind": "full_text" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["kind"], "full_text");

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({ "path": "body", "text": "revenue risk", "limit": 10 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["documents"].as_array().unwrap().len(), 1);
        assert_eq!(json["documents"][0]["key"], "nvda-2024");
        assert!(json["documents"][0]["score"].as_f64().unwrap() >= 1.0);

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({
                "path": "body",
                "text": "supply-chain risk",
                "phrase": true,
                "snippets": true,
                "explain": true,
                "limit": 10
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["documents"][0]["key"], "nvda-2024");
        assert!(
            json["documents"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("supply-chain risk")
        );
        assert!(
            json["documents"][0]["explanation"]
                .as_str()
                .unwrap()
                .contains("matched_terms")
        );

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({
                "path": "body",
                "text": "reveneu",
                "fuzzy_distance": 2,
                "limit": 10
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["documents"][0]["key"], "nvda-2024");

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({
                "path": "body",
                "text": "service",
                "stem": true,
                "limit": 10
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["documents"][0]["key"], "aapl-2024");

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({
                "path": "body",
                "text": "revenue risk",
                "ranking": "bm25",
                "explain": true,
                "limit": 10
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["documents"][0]["key"], "nvda-2024");
        assert!(
            json["documents"][0]["explanation"]
                .as_str()
                .unwrap()
                .contains("ranking=Bm25")
        );

        let (status, _) = call_json(
            app.clone(),
            Method::PUT,
            "/collections/filings/documents/nvda-2024",
            serde_json::json!({
                "document": { "body": "Cash flow improved with stable margins." }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({ "path": "body", "text": "revenue", "limit": 10 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["documents"].as_array().unwrap().is_empty());

        let (status, json) = call_json(
            app,
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({ "path": "body", "text": "cash margins", "limit": 10 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["documents"][0]["key"], "nvda-2024");
    }

    #[tokio::test]
    async fn test_document_http_index_prefix_and_range_search() {
        let (app, _dir) = durable_document_router();

        for (key, ticker, year) in [
            ("nvda-2024", "NVDA", 2024),
            ("nflx-2022", "NFLX", 2022),
            ("aapl-2023", "AAPL", 2023),
        ] {
            let (status, _) = call_json(
                app.clone(),
                Method::PUT,
                &format!("/collections/filings/documents/{key}"),
                serde_json::json!({
                    "document": {
                        "ticker": ticker,
                        "year": year
                    }
                }),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }

        for path in ["ticker", "year"] {
            let (status, _) = call_json(
                app.clone(),
                Method::POST,
                "/collections/filings/indexes",
                serde_json::json!({ "path": path }),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({ "path": "ticker", "prefix": "N", "limit": 10 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let keys: Vec<_> = json["documents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, vec!["nflx-2022", "nvda-2024"]);

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({
                "path": "year",
                "gte": 2023,
                "lte": 2024,
                "limit": 10
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let keys: Vec<_> = json["documents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, vec!["aapl-2023", "nvda-2024"]);

        let (status, json) = call_json(
            app,
            Method::POST,
            "/collections/filings/documents/search",
            serde_json::json!({ "path": "ticker", "value": "NVDA", "prefix": "N" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "DOCUMENT_INDEX_QUERY_INVALID");
    }

    #[tokio::test]
    async fn test_tx_batch_commits_graph_and_document_together() {
        let (app, _dir) = durable_document_router();

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/vectors/entities",
            serde_json::json!({ "dimension": 2 }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json["ok"], true);

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/tx/batch",
            serde_json::json!({
                "cypher": {
                    "query": "CREATE (n:Document {name: $name}) RETURN n.name AS name",
                    "params": { "name": "NVDA filing" }
                },
                "documents": [
                    {
                        "op": "upsert",
                        "collection": "filings",
                        "key": "nvda-2024",
                        "document": {
                            "ticker": "NVDA",
                            "graph_key": "NVDA filing"
                        }
                    }
                ],
                "vectors": [
                    {
                        "op": "upsert",
                        "index": "entities",
                        "vertex_id": 0,
                        "embedding": [1.0, 0.0]
                    }
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["committed"], true);
        assert_eq!(json["document_ops"], 1);
        assert_eq!(json["vector_ops"], 1);
        assert_eq!(json["cypher"]["columns"], serde_json::json!(["name"]));
        assert_eq!(json["cypher"]["rows"], serde_json::json!([["NVDA filing"]]));

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/cypher",
            serde_json::json!({
                "query": "MATCH (n:Document) RETURN n.name AS name"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["rows"], serde_json::json!([["NVDA filing"]]));

        let (status, json) = call_empty(
            app.clone(),
            Method::GET,
            "/collections/filings/documents/nvda-2024",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["document"]["ticker"], "NVDA");
        assert_eq!(json["document"]["graph_key"], "NVDA filing");

        let (status, json) = call_json(
            app,
            Method::POST,
            "/vectors/entities/search",
            serde_json::json!({
                "query": [1.0, 0.0],
                "k": 1
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["results"][0]["vertex_id"], 0);
    }

    #[tokio::test]
    async fn test_tx_batch_commits_multiple_cypher_statements() {
        let (app, _dir) = durable_document_router();

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/tx/batch",
            serde_json::json!({
                "cypher_statements": [
                    {
                        "query": "CREATE (n:Document {name: $name}) RETURN n.name AS name",
                        "params": { "name": "NVDA filing" }
                    },
                    {
                        "query": "MATCH (n:Document) WHERE n.name = $name SET n.status = 'indexed' RETURN n.status AS status",
                        "params": { "name": "NVDA filing" }
                    }
                ],
                "documents": [
                    {
                        "op": "upsert",
                        "collection": "filings",
                        "key": "nvda-2024",
                        "document": { "ticker": "NVDA" }
                    }
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["committed"], true);
        assert_eq!(json["cypher_results"].as_array().unwrap().len(), 2);
        assert_eq!(
            json["cypher_results"][0]["rows"],
            serde_json::json!([["NVDA filing"]])
        );
        assert_eq!(json["cypher"]["rows"], serde_json::json!([["indexed"]]));

        let (status, json) = call_json(
            app.clone(),
            Method::POST,
            "/cypher",
            serde_json::json!({
                "query": "MATCH (n:Document) RETURN n.name AS name, n.status AS status"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["rows"],
            serde_json::json!([["NVDA filing", "indexed"]])
        );

        let (status, json) =
            call_empty(app, Method::GET, "/collections/filings/documents/nvda-2024").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["document"]["ticker"], "NVDA");
    }

    #[tokio::test]
    async fn test_tx_batch_rejects_ambiguous_cypher_shapes() {
        let (app, _dir) = durable_document_router();

        let (status, json) = call_json(
            app,
            Method::POST,
            "/tx/batch",
            serde_json::json!({
                "cypher": { "query": "CREATE (:Entity {name: 'legacy'})" },
                "cypher_statements": [
                    { "query": "CREATE (:Entity {name: 'new'})" }
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "BATCH_CYPHER_AMBIGUOUS");
    }

    #[tokio::test]
    async fn test_tx_batch_rejects_read_only_cypher() {
        let (app, _dir) = durable_document_router();

        let (status, json) = call_json(
            app,
            Method::POST,
            "/tx/batch",
            serde_json::json!({
                "cypher": {
                    "query": "MATCH (n) RETURN count(n) AS count"
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "BATCH_ERROR");
        assert!(
            json["message"]
                .as_str()
                .unwrap()
                .contains("must be a write statement")
        );
    }

    #[tokio::test]
    async fn test_vector_http_unknown_tenant_returns_404() {
        let mt = test_mt_engine();
        let app = create_multi_tenant_router(mt);

        let body = serde_json::json!({
            "tenant": "MISSING",
            "dimension": 2,
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vectors/entities")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_vector_http_dimension_mismatch_returns_bad_request() {
        let engine = test_engine();
        let app = create_router(engine);

        let body = serde_json::json!({ "dimension": 2 });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vectors/entities")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let body = serde_json::json!({
            "vertex_id": 0,
            "embedding": [1.0],
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vectors/entities/upsert")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code.as_deref(), Some("VECTOR_ERROR"));
        assert!(error.error.contains("dimension mismatch"));
    }

    #[tokio::test]
    async fn test_vector_search_metrics_report_recall_overlap() {
        let engine = test_engine();
        let app = create_router(engine);

        let body = serde_json::json!({ "dimension": 2 });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vectors/entities")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        for (vertex_id, embedding) in [(0_u64, vec![1.0, 0.0]), (1, vec![0.0, 1.0])] {
            let body = serde_json::json!({
                "vertex_id": vertex_id,
                "embedding": embedding,
            });
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/vectors/entities/upsert")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let body = serde_json::json!({ "query": [1.0, 0.0], "k": 2 });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vectors/entities/search")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

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
        assert!(text.contains("domyn_nexus_vector_searches_total 1"));
        assert!(text.contains("domyn_nexus_vector_search_results_returned_total 2"));
        assert!(text.contains("domyn_nexus_vector_search_exact_candidates_total 2"));
        assert!(text.contains("domyn_nexus_vector_search_exact_overlap_total 2"));
        assert!(text.contains("domyn_nexus_vector_searches_total{index=\"entities\"} 1"));
        assert!(
            text.contains("domyn_nexus_vector_search_results_returned_total{index=\"entities\"} 2")
        );
        assert!(
            text.contains("domyn_nexus_vector_search_exact_candidates_total{index=\"entities\"} 2")
        );
        assert!(
            text.contains("domyn_nexus_vector_search_exact_overlap_total{index=\"entities\"} 2")
        );
        assert!(
            text.contains("domyn_nexus_vector_index_memory_estimate_bytes{index=\"entities\"}")
        );
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
        assert!(text.contains("domyn_nexus_compaction_deleted_vertices"));
        assert!(text.contains("domyn_nexus_compaction_tombstoned_edges"));
        assert!(text.contains("domyn_nexus_compaction_delta_edges"));
        assert!(text.contains("domyn_nexus_compaction_active"));
        assert!(text.contains("domyn_nexus_compaction_scheduled_total"));
        assert!(text.contains("domyn_nexus_compaction_completed_total"));
        assert!(text.contains("domyn_nexus_compaction_failed_total"));
        assert!(text.contains("domyn_nexus_backup_started_total"));
        assert!(text.contains("domyn_nexus_backup_completed_total"));
        assert!(text.contains("domyn_nexus_backup_failed_total"));
        assert!(text.contains("domyn_nexus_audit_events_total"));
        assert!(text.contains("domyn_nexus_audit_failed_total"));
        assert!(text.contains("domyn_nexus_tenants"));
        assert!(text.contains("domyn_nexus_config_query_timeout_seconds"));
        assert!(text.contains("domyn_nexus_config_max_body_bytes"));
        assert!(text.contains("domyn_nexus_config_max_concurrent_queries"));
        assert!(text.contains("domyn_nexus_config_max_query_rate_per_sec"));
        assert!(text.contains("domyn_nexus_config_max_query_rate_per_tenant_per_sec"));
        assert!(text.contains("domyn_nexus_config_slow_query_ms"));
        assert!(text.contains("domyn_nexus_config_query_memory_budget_bytes"));
        assert!(text.contains("domyn_nexus_config_process_memory_budget_bytes"));
        assert!(text.contains("domyn_nexus_config_default_query_limit"));
        assert!(text.contains("domyn_nexus_process_resident_memory_bytes"));
        assert!(text.contains("domyn_nexus_graph_memory_estimate_bytes"));
        assert!(text.contains("domyn_nexus_index_memory_estimate_bytes"));
        assert!(text.contains("domyn_nexus_vector_index_memory_estimate_bytes"));
        assert!(text.contains("domyn_nexus_total_memory_estimate_bytes"));
        assert!(text.contains("domyn_nexus_queries_active"));
        assert!(text.contains("domyn_nexus_queries_total"));
        assert!(text.contains("domyn_nexus_queries_succeeded_total"));
        assert!(text.contains("domyn_nexus_queries_failed_total"));
        assert!(text.contains("domyn_nexus_queries_timed_out_total"));
        assert!(text.contains("domyn_nexus_queries_limited_total"));
        assert!(text.contains("domyn_nexus_queries_rejected_total"));
        assert!(text.contains("domyn_nexus_queries_rate_limited_total"));
        assert!(text.contains("domyn_nexus_queries_memory_rejected_total"));
        assert!(text.contains("domyn_nexus_queries_slow_total"));
        assert!(text.contains("domyn_nexus_query_rows_returned_total"));
        assert!(text.contains("domyn_nexus_query_bytes_returned_total"));
        assert!(text.contains("domyn_nexus_query_elapsed_ms_total"));
        assert!(text.contains("domyn_nexus_query_elapsed_ms_bucket"));
        assert!(text.contains("domyn_nexus_vector_searches_total"));
        assert!(text.contains("domyn_nexus_vector_search_results_returned_total"));
        assert!(text.contains("domyn_nexus_vector_search_exact_candidates_total"));
        assert!(text.contains("domyn_nexus_vector_search_exact_overlap_total"));
        assert!(text.contains("domyn_nexus_storage_wal_sequence"));
        assert!(text.contains("domyn_nexus_storage_wal_active_bytes"));
        assert!(text.contains("domyn_nexus_storage_wal_live_segments"));
        assert!(text.contains("domyn_nexus_storage_wal_live_segment_bytes"));
        assert!(text.contains("domyn_nexus_storage_wal_archived_segments"));
        assert!(text.contains("domyn_nexus_storage_wal_archived_segment_bytes"));
        assert!(text.contains("domyn_nexus_storage_wal_recoverable_bytes"));
        assert!(text.contains("domyn_nexus_storage_snapshot_bytes"));
        assert!(text.contains("domyn_nexus_storage_snapshot_modified_unix_seconds"));
        assert!(text.contains("domyn_nexus_storage_snapshot_age_seconds"));
        assert!(text.contains("domyn_nexus_storage_snapshot_archives"));
        assert!(text.contains("domyn_nexus_storage_snapshot_archive_bytes"));
        assert!(text.contains("domyn_nexus_storage_vector_snapshots"));
        assert!(text.contains("domyn_nexus_storage_vector_snapshot_bytes"));
        assert!(text.contains("domyn_nexus_storage_documents"));
        assert!(text.contains("domyn_nexus_storage_document_bytes"));
        assert!(text.contains("domyn_nexus_storage_document_indexes"));
        assert!(text.contains("domyn_nexus_storage_document_index_entries"));
        assert!(text.contains("domyn_nexus_storage_catalog_bytes"));
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
