# Domyn Nexus Config Reference

Config v1 is JSON. Generate templates with:

```bash
nexus-server init --profile dev
nexus-server init --profile production
```

Validate with:

```bash
nexus-server check-config --config /etc/domyn-nexus/config.json
```

Failed production validation reports stable issue codes, fields, messages, and
remediation text, for example:

```text
configuration is not production-ready:
- [HTTP_TLS_REQUIRED] http.tls: production_mode requires http.tls
  fix: set http.tls.cert_path and http.tls.key_path, or disable production_mode only for local development
```

## Top-Level Fields

| Field | Type | Required in production | Description |
|---|---:|---:|---|
| `production_mode` | bool | yes | Enables hard startup validation. |
| `storage_path` | string | yes | Durable data directory. Omit only for ephemeral dev/test. |
| `auth_token` | string | one auth source required | Legacy admin token. Prefer `auth_principals`. |
| `auth_principals` | array | one auth source required | Named principals with role and tenant scope. |
| `http` | object | yes | HTTP API config. |
| `bolt` | object | yes when Bolt enabled | Bolt protocol config. |

## Auth Principals

```json
{
  "name": "analyst",
  "token": "replace-me",
  "role": "read_only",
  "tenants": ["NVDA", "AAPL"]
}
```

Roles:

- `read_only`: read queries and vector reads.
- `read_write`: reads and writes for allowed tenants.
- `admin`: tenant enumeration, backup, compaction, and vector lifecycle.

Tenant scope accepts exact tenant IDs or `"*"`.

## HTTP Fields

| Field | Default | Production rule |
|---|---:|---|
| `bind_addr` | `127.0.0.1:8080` | Set an explicit service address. |
| `tls` | `null` | Required. |
| `query_timeout_secs` | 30 | Must be greater than 0. |
| `max_body_bytes` | 1048576 | Keep bounded. |
| `max_concurrent_queries` | 64 | Must be greater than 0. |
| `max_query_rate_per_sec` | 0 | `0` disables global rate limiting. |
| `max_query_rate_per_tenant_per_sec` | 0 | `0` disables tenant rate limiting. |
| `slow_query_ms` | 250 | Slow-query logging threshold. |
| `wal_segment_bytes` | 67108864 | WAL rotation target. |
| `wal_retention_segments` | 4-8 | Retained rotated WAL segments. |
| `snapshot_retention` | 2-4 | Retained snapshots. |
| `compaction_threshold` | 1000+ | Tombstone pressure before background compaction. |
| `query_memory_budget_bytes` | 67108864 | Must be greater than 0. |
| `process_memory_budget_bytes` | 0 | `0` disables process-level cap. |
| `default_query_limit` | 10000 | Must be greater than 0. |
| `vector_index_mode` | `Hnsw` | `Exact` or `Hnsw`. |
| `backup_root` | `null` | Required. |
| `audit_log_path` | `null` | Required. |

TLS object:

```json
{
  "cert_path": "/etc/domyn-nexus/tls/http.crt",
  "key_path": "/etc/domyn-nexus/tls/http.key",
  "client_ca_path": null,
  "require_client_auth": false
}
```

## Bolt Fields

| Field | Default | Production rule |
|---|---:|---|
| `enabled` | true | Optional. |
| `bind_addr` | `127.0.0.1:7687` | Set an explicit service address. |
| `max_connections` | 256 | Must be greater than 0 when enabled. |
| `query_timeout_secs` | 30 | Must be greater than 0 when enabled. |
| `default_query_limit` | 10000 | Must be greater than 0 when enabled. |
| `query_memory_budget_bytes` | 67108864 | Must be greater than 0 when enabled. |
| `tls` | `null` | Required when enabled in production. |

Bolt supports auto-commit query execution. Explicit Bolt write transactions are
rejected with a structured unsupported-feature error until the transaction
protocol is fully implemented.

## Environment Overrides

| Variable | Overrides |
|---|---|
| `DOMYN_NEXUS_CONFIG` | Config path fallback. |
| `DOMYN_NEXUS_PRODUCTION_MODE` | `production_mode`. |
| `DOMYN_NEXUS_HTTP_BIND_ADDR` | `http.bind_addr`. |
| `DOMYN_NEXUS_BOLT_BIND_ADDR` | `bolt.bind_addr`. |
| `DOMYN_NEXUS_AUTH_TOKEN` | `auth_token`. |
| `DOMYN_NEXUS_BACKUP_ROOT` | `http.backup_root`. |
| `DOMYN_NEXUS_AUDIT_LOG_PATH` | `http.audit_log_path`. |
| `DOMYN_NEXUS_TLS_CERT_PATH` | `http.tls.cert_path`. |
| `DOMYN_NEXUS_TLS_KEY_PATH` | `http.tls.key_path`. |
| `DOMYN_NEXUS_TLS_CLIENT_CA_PATH` | `http.tls.client_ca_path`. |
| `DOMYN_NEXUS_TLS_REQUIRE_CLIENT_AUTH` | `http.tls.require_client_auth`. |
| `DOMYN_NEXUS_BOLT_TLS_CERT_PATH` | `bolt.tls.cert_path`. |
| `DOMYN_NEXUS_BOLT_TLS_KEY_PATH` | `bolt.tls.key_path`. |
| `DOMYN_NEXUS_BOLT_TLS_CLIENT_CA_PATH` | `bolt.tls.client_ca_path`. |
| `DOMYN_NEXUS_BOLT_TLS_REQUIRE_CLIENT_AUTH` | `bolt.tls.require_client_auth`. |
