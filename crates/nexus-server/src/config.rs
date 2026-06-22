use crate::http::{AuthPrincipalConfig, ServerConfig, VectorIndexMode};
use crate::tls::TlsConfig;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NexusServerConfig {
    pub production_mode: bool,
    pub http: HttpConfig,
    pub bolt: BoltConfig,
    pub auth_token: Option<String>,
    pub auth_principals: Vec<AuthPrincipalConfig>,
    pub storage_path: Option<String>,
}

impl Default for NexusServerConfig {
    fn default() -> Self {
        Self {
            production_mode: false,
            http: HttpConfig::default(),
            bolt: BoltConfig::default(),
            auth_token: None,
            auth_principals: Vec::new(),
            storage_path: None,
        }
    }
}

impl NexusServerConfig {
    pub fn from_json_str(input: &str) -> serde_json::Result<Self> {
        serde_json::from_str(input)
    }

    pub fn load_json_file(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let bytes = fs::read(path)?;
        serde_json::from_slice(&bytes)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    }

    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(value) = std::env::var("DOMYN_NEXUS_HTTP_BIND_ADDR") {
            self.http.bind_addr = value;
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_BOLT_BIND_ADDR") {
            self.bolt.bind_addr = value;
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_AUTH_TOKEN") {
            self.auth_token = Some(value);
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_PRODUCTION_MODE") {
            self.production_mode = matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES");
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_BACKUP_ROOT") {
            self.http.backup_root = Some(value);
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_AUDIT_LOG_PATH") {
            self.http.audit_log_path = Some(value);
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_TLS_CERT_PATH") {
            self.http.tls.get_or_insert_with(Default::default).cert_path = value;
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_TLS_KEY_PATH") {
            self.http.tls.get_or_insert_with(Default::default).key_path = value;
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_TLS_CLIENT_CA_PATH") {
            self.http
                .tls
                .get_or_insert_with(Default::default)
                .client_ca_path = Some(value);
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_TLS_REQUIRE_CLIENT_AUTH") {
            self.http
                .tls
                .get_or_insert_with(Default::default)
                .require_client_auth =
                matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES");
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_BOLT_TLS_CERT_PATH") {
            self.bolt.tls.get_or_insert_with(Default::default).cert_path = value;
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_BOLT_TLS_KEY_PATH") {
            self.bolt.tls.get_or_insert_with(Default::default).key_path = value;
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_BOLT_TLS_CLIENT_CA_PATH") {
            self.bolt
                .tls
                .get_or_insert_with(Default::default)
                .client_ca_path = Some(value);
        }
        if let Ok(value) = std::env::var("DOMYN_NEXUS_BOLT_TLS_REQUIRE_CLIENT_AUTH") {
            self.bolt
                .tls
                .get_or_insert_with(Default::default)
                .require_client_auth =
                matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES");
        }
        self
    }

    pub fn validate_for_production(&self) -> Result<(), ConfigValidationError> {
        if !self.production_mode {
            return Ok(());
        }

        let mut issues = Vec::new();
        if self.auth_token.is_none() && self.auth_principals.is_empty() {
            issues.push(ConfigValidationIssue::new(
                "PRODUCTION_AUTH_REQUIRED",
                "auth_principals",
                "production_mode requires auth_token or auth_principals",
                "set auth_principals with read_only/read_write/admin tokens, or set DOMYN_NEXUS_AUTH_TOKEN for a temporary admin token",
            ));
        }
        if self.http.tls.is_none() {
            issues.push(ConfigValidationIssue::new(
                "HTTP_TLS_REQUIRED",
                "http.tls",
                "production_mode requires http.tls",
                "set http.tls.cert_path and http.tls.key_path, or disable production_mode only for local development",
            ));
        }
        if self
            .http
            .audit_log_path
            .as_deref()
            .is_none_or(str::is_empty)
        {
            issues.push(ConfigValidationIssue::new(
                "HTTP_AUDIT_LOG_REQUIRED",
                "http.audit_log_path",
                "production_mode requires http.audit_log_path",
                "set http.audit_log_path to a writable JSONL audit log path",
            ));
        }
        if self.http.backup_root.as_deref().is_none_or(str::is_empty) {
            issues.push(ConfigValidationIssue::new(
                "HTTP_BACKUP_ROOT_REQUIRED",
                "http.backup_root",
                "production_mode requires http.backup_root",
                "set http.backup_root to a writable backup directory outside the data directory",
            ));
        }
        if self.storage_path.as_deref().is_none_or(str::is_empty) {
            issues.push(ConfigValidationIssue::new(
                "STORAGE_PATH_REQUIRED",
                "storage_path",
                "production_mode requires storage_path",
                "set storage_path to a durable data directory",
            ));
        }
        if self.http.query_timeout_secs == 0 {
            issues.push(ConfigValidationIssue::new(
                "HTTP_QUERY_TIMEOUT_REQUIRED",
                "http.query_timeout_secs",
                "production_mode requires http.query_timeout_secs > 0",
                "set http.query_timeout_secs to a non-zero timeout",
            ));
        }
        if self.http.max_concurrent_queries == 0 {
            issues.push(ConfigValidationIssue::new(
                "HTTP_MAX_CONCURRENT_QUERIES_REQUIRED",
                "http.max_concurrent_queries",
                "production_mode requires http.max_concurrent_queries > 0",
                "set http.max_concurrent_queries to a non-zero concurrency limit",
            ));
        }
        if self.http.default_query_limit == 0 {
            issues.push(ConfigValidationIssue::new(
                "HTTP_DEFAULT_QUERY_LIMIT_REQUIRED",
                "http.default_query_limit",
                "production_mode requires http.default_query_limit > 0",
                "set http.default_query_limit to a non-zero row limit",
            ));
        }
        if self.http.query_memory_budget_bytes == 0 {
            issues.push(ConfigValidationIssue::new(
                "HTTP_QUERY_MEMORY_BUDGET_REQUIRED",
                "http.query_memory_budget_bytes",
                "production_mode requires http.query_memory_budget_bytes > 0",
                "set http.query_memory_budget_bytes to a non-zero per-query budget",
            ));
        }
        if self.bolt.enabled {
            if self.bolt.tls.is_none() {
                issues.push(ConfigValidationIssue::new(
                    "BOLT_TLS_REQUIRED",
                    "bolt.tls",
                    "production_mode requires bolt.tls when bolt.enabled",
                    "set bolt.tls.cert_path/key_path or set bolt.enabled=false",
                ));
            }
            if self.bolt.max_connections == 0 {
                issues.push(ConfigValidationIssue::new(
                    "BOLT_MAX_CONNECTIONS_REQUIRED",
                    "bolt.max_connections",
                    "production_mode requires bolt.max_connections > 0 when bolt.enabled",
                    "set bolt.max_connections to a non-zero connection cap",
                ));
            }
            if self.bolt.query_timeout_secs == 0 {
                issues.push(ConfigValidationIssue::new(
                    "BOLT_QUERY_TIMEOUT_REQUIRED",
                    "bolt.query_timeout_secs",
                    "production_mode requires bolt.query_timeout_secs > 0 when bolt.enabled",
                    "set bolt.query_timeout_secs to a non-zero timeout",
                ));
            }
            if self.bolt.default_query_limit == 0 {
                issues.push(ConfigValidationIssue::new(
                    "BOLT_DEFAULT_QUERY_LIMIT_REQUIRED",
                    "bolt.default_query_limit",
                    "production_mode requires bolt.default_query_limit > 0 when bolt.enabled",
                    "set bolt.default_query_limit to a non-zero row limit",
                ));
            }
            if self.bolt.query_memory_budget_bytes == 0 {
                issues.push(ConfigValidationIssue::new(
                    "BOLT_QUERY_MEMORY_BUDGET_REQUIRED",
                    "bolt.query_memory_budget_bytes",
                    "production_mode requires bolt.query_memory_budget_bytes > 0 when bolt.enabled",
                    "set bolt.query_memory_budget_bytes to a non-zero per-query budget",
                ));
            }
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(ConfigValidationError { issues })
        }
    }

    pub fn server_config(&self) -> ServerConfig {
        ServerConfig {
            query_timeout_secs: self.http.query_timeout_secs,
            max_body_bytes: self.http.max_body_bytes,
            max_concurrent_queries: self.http.max_concurrent_queries,
            max_query_rate_per_sec: self.http.max_query_rate_per_sec,
            max_query_rate_per_tenant_per_sec: self.http.max_query_rate_per_tenant_per_sec,
            slow_query_ms: self.http.slow_query_ms,
            auth_token: self.auth_token.clone(),
            auth_principals: self.auth_principals.clone(),
            wal_segment_bytes: self.http.wal_segment_bytes,
            wal_retention_segments: self.http.wal_retention_segments,
            snapshot_retention: self.http.snapshot_retention,
            compaction_threshold: self.http.compaction_threshold,
            query_memory_budget_bytes: self.http.query_memory_budget_bytes,
            process_memory_budget_bytes: self.http.process_memory_budget_bytes,
            #[cfg(test)]
            process_memory_budget_sample_bytes: None,
            default_query_limit: self.http.default_query_limit,
            vector_index_mode: self.http.vector_index_mode,
            backup_root: self.http.backup_root.clone(),
            audit_log_path: self.http.audit_log_path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValidationIssue {
    pub code: &'static str,
    pub field: &'static str,
    pub message: &'static str,
    pub remediation: &'static str,
}

impl ConfigValidationIssue {
    fn new(
        code: &'static str,
        field: &'static str,
        message: &'static str,
        remediation: &'static str,
    ) -> Self {
        Self {
            code,
            field,
            message,
            remediation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValidationError {
    pub issues: Vec<ConfigValidationIssue>,
}

impl fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "configuration is not production-ready")?;
        for issue in &self.issues {
            write!(f, "; {}: {}", issue.code, issue.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigValidationError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct HttpConfig {
    pub bind_addr: String,
    pub tls: Option<TlsConfig>,
    pub query_timeout_secs: u64,
    pub max_body_bytes: usize,
    pub max_concurrent_queries: usize,
    pub max_query_rate_per_sec: u64,
    pub max_query_rate_per_tenant_per_sec: u64,
    pub slow_query_ms: u64,
    pub wal_segment_bytes: u64,
    pub wal_retention_segments: usize,
    pub snapshot_retention: usize,
    pub compaction_threshold: usize,
    pub query_memory_budget_bytes: usize,
    pub process_memory_budget_bytes: usize,
    pub default_query_limit: usize,
    pub vector_index_mode: VectorIndexMode,
    pub backup_root: Option<String>,
    pub audit_log_path: Option<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        let config = ServerConfig::default();
        Self {
            bind_addr: "127.0.0.1:8080".into(),
            tls: None,
            query_timeout_secs: config.query_timeout_secs,
            max_body_bytes: config.max_body_bytes,
            max_concurrent_queries: config.max_concurrent_queries,
            max_query_rate_per_sec: config.max_query_rate_per_sec,
            max_query_rate_per_tenant_per_sec: config.max_query_rate_per_tenant_per_sec,
            slow_query_ms: config.slow_query_ms,
            wal_segment_bytes: config.wal_segment_bytes,
            wal_retention_segments: config.wal_retention_segments,
            snapshot_retention: config.snapshot_retention,
            compaction_threshold: config.compaction_threshold,
            query_memory_budget_bytes: config.query_memory_budget_bytes,
            process_memory_budget_bytes: config.process_memory_budget_bytes,
            default_query_limit: config.default_query_limit,
            vector_index_mode: config.vector_index_mode,
            backup_root: config.backup_root,
            audit_log_path: config.audit_log_path,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct BoltConfig {
    pub bind_addr: String,
    pub enabled: bool,
    pub max_connections: usize,
    pub query_timeout_secs: u64,
    pub default_query_limit: usize,
    pub query_memory_budget_bytes: usize,
    pub tls: Option<TlsConfig>,
}

impl Default for BoltConfig {
    fn default() -> Self {
        let server = ServerConfig::default();
        Self {
            bind_addr: "127.0.0.1:7687".into(),
            enabled: true,
            max_connections: 256,
            query_timeout_secs: server.query_timeout_secs,
            default_query_limit: server.default_query_limit,
            query_memory_budget_bytes: server.query_memory_budget_bytes,
            tls: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_maps_to_server_config() {
        let cfg = NexusServerConfig::from_json_str(
            r#"{
                "auth_token": "secret",
                "auth_principals": [
                    {
                        "name": "analyst",
                        "token": "reader-token",
                        "role": "read_only",
                        "tenants": ["NVDA"]
                    },
                    {
                        "name": "writer",
                        "token": "writer-token",
                        "role": "read_write",
                        "tenants": ["*"]
                    }
                ],
                "http": {
                    "bind_addr": "0.0.0.0:8443",
                    "tls": {
                        "cert_path": "/cert.pem",
                        "key_path": "/key.pem",
                        "client_ca_path": "/client-ca.pem",
                        "require_client_auth": true
                    },
                    "query_timeout_secs": 9,
                    "max_body_bytes": 2048,
                    "max_concurrent_queries": 7,
                    "max_query_rate_per_sec": 100,
                    "max_query_rate_per_tenant_per_sec": 10,
                    "slow_query_ms": 42,
                    "wal_segment_bytes": 4096,
                    "wal_retention_segments": 3,
                    "snapshot_retention": 4,
                    "compaction_threshold": 5,
                    "query_memory_budget_bytes": 8192,
                    "process_memory_budget_bytes": 16384,
                    "default_query_limit": 11,
                    "vector_index_mode": "Exact",
                    "backup_root": "/var/backups/domyn-nexus",
                    "audit_log_path": "/var/log/domyn-nexus/audit.jsonl"
                },
                "bolt": {
                    "bind_addr": "0.0.0.0:7687",
                    "max_connections": 12,
                    "query_timeout_secs": 13,
                    "default_query_limit": 14,
                    "query_memory_budget_bytes": 32768,
                    "tls": {
                        "cert_path": "/bolt-cert.pem",
                        "key_path": "/bolt-key.pem",
                        "client_ca_path": "/bolt-client-ca.pem",
                        "require_client_auth": true
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(cfg.http.bind_addr, "0.0.0.0:8443");
        assert_eq!(cfg.http.tls.as_ref().unwrap().cert_path, "/cert.pem");
        assert_eq!(
            cfg.http.tls.as_ref().unwrap().client_ca_path.as_deref(),
            Some("/client-ca.pem")
        );
        assert!(cfg.http.tls.as_ref().unwrap().require_client_auth);
        assert_eq!(cfg.bolt.max_connections, 12);
        assert_eq!(cfg.bolt.query_timeout_secs, 13);
        assert_eq!(cfg.bolt.default_query_limit, 14);
        assert_eq!(cfg.bolt.query_memory_budget_bytes, 32768);
        assert_eq!(cfg.bolt.tls.as_ref().unwrap().cert_path, "/bolt-cert.pem");
        assert_eq!(
            cfg.bolt.tls.as_ref().unwrap().client_ca_path.as_deref(),
            Some("/bolt-client-ca.pem")
        );
        assert!(cfg.bolt.tls.as_ref().unwrap().require_client_auth);

        let server = cfg.server_config();
        assert_eq!(server.auth_token.as_deref(), Some("secret"));
        assert_eq!(server.auth_principals.len(), 2);
        assert_eq!(server.auth_principals[0].name, "analyst");
        assert_eq!(server.auth_principals[0].tenants, vec!["NVDA"]);
        assert_eq!(server.query_timeout_secs, 9);
        assert_eq!(server.max_query_rate_per_tenant_per_sec, 10);
        assert_eq!(server.process_memory_budget_bytes, 16384);
        assert_eq!(server.default_query_limit, 11);
        assert_eq!(
            server.backup_root.as_deref(),
            Some("/var/backups/domyn-nexus")
        );
        assert_eq!(
            server.audit_log_path.as_deref(),
            Some("/var/log/domyn-nexus/audit.jsonl")
        );
    }

    #[test]
    fn config_loads_from_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nexus.json");
        std::fs::write(
            &path,
            r#"{
                "http": { "bind_addr": "127.0.0.1:9000" },
                "storage_path": "/var/lib/domyn-nexus"
            }"#,
        )
        .unwrap();

        let cfg = NexusServerConfig::load_json_file(&path).unwrap();
        assert_eq!(cfg.http.bind_addr, "127.0.0.1:9000");
        assert_eq!(cfg.storage_path.as_deref(), Some("/var/lib/domyn-nexus"));
    }

    #[test]
    fn production_mode_validation_rejects_insecure_defaults() {
        let cfg = NexusServerConfig {
            production_mode: true,
            ..NexusServerConfig::default()
        };

        let err = cfg.validate_for_production().unwrap_err();
        let codes: Vec<_> = err.issues.iter().map(|issue| issue.code).collect();
        assert!(codes.contains(&"PRODUCTION_AUTH_REQUIRED"));
        assert!(codes.contains(&"HTTP_TLS_REQUIRED"));
        assert!(codes.contains(&"HTTP_AUDIT_LOG_REQUIRED"));
        assert!(codes.contains(&"HTTP_BACKUP_ROOT_REQUIRED"));
        assert!(codes.contains(&"STORAGE_PATH_REQUIRED"));
        assert!(codes.contains(&"BOLT_TLS_REQUIRED"));

        let auth = err
            .issues
            .iter()
            .find(|issue| issue.code == "PRODUCTION_AUTH_REQUIRED")
            .unwrap();
        assert_eq!(auth.field, "auth_principals");
        assert_eq!(
            auth.message,
            "production_mode requires auth_token or auth_principals"
        );
        assert!(auth.remediation.contains("auth_principals"));
    }

    #[test]
    fn production_mode_validation_accepts_hardened_config() {
        let cfg = NexusServerConfig {
            production_mode: true,
            auth_token: Some("secret".into()),
            storage_path: Some("/var/lib/domyn-nexus".into()),
            http: HttpConfig {
                tls: Some(TlsConfig {
                    cert_path: "/cert.pem".into(),
                    key_path: "/key.pem".into(),
                    client_ca_path: None,
                    require_client_auth: false,
                }),
                audit_log_path: Some("/var/log/domyn-nexus/audit.jsonl".into()),
                backup_root: Some("/var/backups/domyn-nexus".into()),
                ..HttpConfig::default()
            },
            bolt: BoltConfig {
                tls: Some(TlsConfig {
                    cert_path: "/bolt-cert.pem".into(),
                    key_path: "/bolt-key.pem".into(),
                    client_ca_path: None,
                    require_client_auth: false,
                }),
                ..BoltConfig::default()
            },
            ..NexusServerConfig::default()
        };

        cfg.validate_for_production().unwrap();
    }

    #[test]
    fn production_example_config_loads_and_validates() {
        let cfg = NexusServerConfig::from_json_str(include_str!(
            "../../../docs/examples/production-single-node.json"
        ))
        .unwrap();

        assert!(cfg.production_mode);
        assert_eq!(cfg.auth_principals.len(), 3);
        assert_eq!(cfg.storage_path.as_deref(), Some("/var/lib/domyn-nexus"));
        assert_eq!(cfg.http.bind_addr, "0.0.0.0:8443");
        assert_eq!(cfg.http.vector_index_mode, VectorIndexMode::Hnsw);
        assert_eq!(cfg.http.default_query_limit, 10000);
        assert_eq!(cfg.http.query_memory_budget_bytes, 67_108_864);
        assert_eq!(
            cfg.http.audit_log_path.as_deref(),
            Some("/var/log/domyn-nexus/audit.jsonl")
        );
        assert!(cfg.bolt.enabled);
        assert_eq!(cfg.bolt.bind_addr, "0.0.0.0:7687");
        assert_eq!(cfg.bolt.max_connections, 256);

        cfg.validate_for_production().unwrap();
    }

    #[test]
    fn dev_example_config_loads_for_local_smoke() {
        let cfg = NexusServerConfig::from_json_str(include_str!(
            "../../../docs/examples/dev-single-node.json"
        ))
        .unwrap();

        assert!(!cfg.production_mode);
        assert_eq!(cfg.auth_token.as_deref(), Some("dev-token"));
        assert_eq!(cfg.http.bind_addr, "127.0.0.1:18080");
        assert_eq!(cfg.bolt.bind_addr, "127.0.0.1:17687");
        assert_eq!(cfg.http.vector_index_mode, VectorIndexMode::Hnsw);

        cfg.validate_for_production().unwrap();
    }

    #[test]
    fn production_mode_validation_allows_disabled_bolt_without_bolt_tls() {
        let cfg = NexusServerConfig {
            production_mode: true,
            auth_token: Some("secret".into()),
            storage_path: Some("/var/lib/domyn-nexus".into()),
            http: HttpConfig {
                tls: Some(TlsConfig {
                    cert_path: "/cert.pem".into(),
                    key_path: "/key.pem".into(),
                    client_ca_path: None,
                    require_client_auth: false,
                }),
                audit_log_path: Some("/var/log/domyn-nexus/audit.jsonl".into()),
                backup_root: Some("/var/backups/domyn-nexus".into()),
                ..HttpConfig::default()
            },
            bolt: BoltConfig {
                enabled: false,
                tls: None,
                ..BoltConfig::default()
            },
            ..NexusServerConfig::default()
        };

        cfg.validate_for_production().unwrap();
    }
}
