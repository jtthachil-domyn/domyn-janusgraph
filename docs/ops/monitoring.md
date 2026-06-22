# Domyn Nexus Monitoring Pack

This folder contains the starter production monitoring artifacts for a
single-node Domyn Nexus deployment:

- `prometheus-alerts.yml`: Prometheus alert rules for vector recall, query
  latency/failures, admission control, compaction pressure, and storage growth.
- `grafana-dashboard.json`: Grafana dashboard JSON for query health, vector
  recall by index, compaction pressure, WAL/snapshot footprint, and operational
  error counters.

These artifacts assume Prometheus scrapes the Nexus HTTP `/metrics` endpoint.
They are intentionally environment-neutral: Kubernetes, Azure Monitor managed
Prometheus, bare-VM Prometheus, and Grafana Cloud can all import the same rule
and dashboard definitions with small datasource wiring changes.

## Required Scrape Target

The HTTP server exposes Prometheus text format at:

```text
GET /metrics
```

If HTTP auth is configured globally, scrape jobs should use the same bearer
token or API key accepted by the server.

## Key Signals

| Area | Metrics | Why it matters |
| --- | --- | --- |
| Query latency | `domyn_nexus_query_elapsed_ms_bucket` | Estimates p50/p95 latency from server-side buckets. |
| Query health | `domyn_nexus_queries_total`, `domyn_nexus_queries_succeeded_total`, `domyn_nexus_queries_failed_total`, `domyn_nexus_queries_timed_out_total` | Tracks basic request quality and timeout pressure. |
| Admission control | `domyn_nexus_queries_rejected_total`, `domyn_nexus_queries_rate_limited_total`, `domyn_nexus_queries_memory_rejected_total`, `domyn_nexus_queries_limited_total` | Shows memory, concurrency, rate, and result-limit protection firing before OOM. |
| Configured guardrails | `domyn_nexus_config_query_memory_budget_bytes`, `domyn_nexus_config_process_memory_budget_bytes`, `domyn_nexus_config_default_query_limit`, `domyn_nexus_config_max_concurrent_queries` | Shows the runtime safety limits that shape query admission, bounded result behavior, and process-memory alerting. |
| Process memory | `domyn_nexus_process_resident_memory_bytes`, `domyn_nexus_graph_memory_estimate_bytes`, `domyn_nexus_index_memory_estimate_bytes`, `domyn_nexus_vector_index_memory_estimate_bytes`, `domyn_nexus_total_memory_estimate_bytes` | Tracks process RSS as observed by the host OS plus graph/index owned memory as internal trend estimates. The bundled RSS alert and query admission rejection are disabled until `process_memory_budget_bytes` is configured above zero. |
| Vector recall | `domyn_nexus_vector_search_exact_overlap_total{index=...}`, `domyn_nexus_vector_search_exact_candidates_total{index=...}` | Measures ANN result overlap against the exact oracle by vector index. |
| Compaction and backup | `domyn_nexus_compaction_deleted_vertices`, `domyn_nexus_compaction_tombstoned_edges`, `domyn_nexus_compaction_delta_edges`, `domyn_nexus_compaction_failed_total`, `domyn_nexus_backup_started_total`, `domyn_nexus_backup_completed_total`, `domyn_nexus_backup_failed_total` | Detects tombstone/delta buildup, failed compaction runs, and failed admin hot backups. |
| Audit logging | `domyn_nexus_audit_events_total`, `domyn_nexus_audit_failed_total` | Confirms security-sensitive write/admin events are being appended and catches audit sink failures. |
| Storage | `domyn_nexus_storage_wal_recoverable_bytes`, `domyn_nexus_storage_snapshot_bytes`, `domyn_nexus_storage_snapshot_age_seconds`, `domyn_nexus_storage_vector_snapshot_bytes` | Tracks crash-recovery footprint, snapshot freshness, and snapshot/vector persistence size. |

## Vector Recall Formula

Recall overlap is reported as:

```promql
sum by (index) (increase(domyn_nexus_vector_search_exact_overlap_total{index!=""}[15m]))
/
clamp_min(
  sum by (index) (increase(domyn_nexus_vector_search_exact_candidates_total{index!=""}[15m])),
  1
)
```

This is not a theoretical full-recall proof. It is a production sentinel: every
served vector search also runs the exact oracle for the same `k`, then counts
how many returned IDs overlap. Low overlap means the ANN layer, tombstone
filtering, update lifecycle, or compaction behavior needs investigation.

The included alerts require at least 100 exact candidates in the evaluation
window before firing, which avoids noisy alerts on tiny traffic samples.

## Import Steps

1. Configure Prometheus to scrape `GET /metrics` from each Nexus process.
2. Load `prometheus-alerts.yml` into the Prometheus rule path or managed
   Prometheus rule group.
3. Import `grafana-dashboard.json` into Grafana.
4. Select the Prometheus datasource in the dashboard variable.
5. Tune thresholds for production SLOs after one week of baseline traffic.

## Current Gaps

- Nexus exports process RSS plus graph, schema-index, vector-index, and total
  internal memory estimates. These do not include exact allocator
  fragmentation, query scratch memory, or Tantivy/full-text internals. Use
  node/container telemetry for host-level memory headroom.
- Alerts are single-node oriented. Distributed leader/follower metrics should
  be added when replication lands.
- The alert thresholds are conservative starter values, not contractual SLOs.
