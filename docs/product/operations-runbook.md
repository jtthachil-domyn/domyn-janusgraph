# Domyn Nexus Operations Runbook

This runbook is for the single-node Internal Beta server.

## Startup Checklist

1. Generate or review config.
2. Run `nexus-server check-config --config /etc/domyn-nexus/config.json`.
3. Confirm TLS certificate and key files exist.
4. Confirm `storage_path`, `backup_root`, and log directories are writable by
   the server user.
5. Start the server.
6. Check `/health`, `/ready`, and `/metrics`.
7. For a self-starting local check, run `scripts/smoke-dev-single-node.sh`.
8. To smoke an already-running server, run `scripts/smoke-single-node.sh`.
9. For the packaged image path, run `scripts/smoke-docker-single-node.sh`.

## Health And Readiness

`GET /health` means the process is alive.

`GET /ready` means an engine or tenant is configured and the server can accept
queries.

These are intentionally separate. A process can be healthy but not ready.

## Metrics

`GET /metrics` returns Prometheus text format. Important families:

- `domyn_nexus_vertices`
- `domyn_nexus_edges`
- `domyn_nexus_tenants`
- `domyn_nexus_query_*`
- `domyn_nexus_storage_*`
- `domyn_nexus_compaction_*`
- `domyn_nexus_vector_*`
- `domyn_nexus_storage_documents`
- `domyn_nexus_storage_document_bytes`
- `domyn_nexus_memory_estimate_bytes`

Minimum alerts for beta:

- readiness down for 2 minutes
- compaction failures > 0
- backup failures > 0
- query timeout rate spike
- vector recall overlap below expected benchmark value
- memory estimate above configured process budget

## Backup

HTTP backup:

```bash
curl -k https://127.0.0.1:8443/admin/backup \
  -H "Authorization: Bearer $DOMYN_NEXUS_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path":"daily/2026-06-17"}'
```

CLI backup:

```bash
nexus-server backup --config /etc/domyn-nexus/config.json --output /var/backups/domyn-nexus/daily/2026-06-17
```

Backup writes a `backup-manifest.json` after copying data files and syncing the
directory. Treat backups without a manifest as incomplete.

## Restore

Restore into an empty data directory:

```bash
nexus-server restore \
  --backup /var/backups/domyn-nexus/daily/2026-06-17 \
  --data-dir /var/lib/domyn-nexus-restored
```

Then start a server pointed at the restored `storage_path` and run:

```bash
scripts/smoke-dev-single-node.sh
```

## Compaction

Manual compaction:

```bash
curl -k https://127.0.0.1:8443/admin/compact \
  -H "Authorization: Bearer $DOMYN_NEXUS_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{}'
```

Automatic compaction is threshold-triggered by tombstone/delta pressure. Watch:

- `domyn_nexus_compaction_deleted_vertices`
- `domyn_nexus_compaction_tombstoned_edges`
- `domyn_nexus_compaction_delta_edges`
- `domyn_nexus_compaction_failed_total`

## Query Safety

Production config must set:

- non-zero query timeout
- non-zero default row limit
- non-zero per-query memory budget
- non-zero concurrent query cap
- body limit
- optional global and tenant rate limits

Unsupported Cypher must fail at parse/bind time with structured errors. It must
not partially execute.

## Incident Playbooks

### Server Is Healthy But Not Ready

1. Check logs for config or storage open errors.
2. Run `nexus-server check-config --config ...`.
3. Confirm `storage_path` exists and is readable.
4. If restoring, confirm `backup-manifest.json` exists.

### Query Timeouts Spike

1. Check slow-query logs.
2. Check `/metrics` for active query count and memory estimate.
3. Lower `max_concurrent_queries` or tenant rate limit if the node is overloaded.
4. Add a stricter query `LIMIT` or move broad scans into a batch job.

### Compaction Fails

1. Stop new write traffic if tombstone pressure is growing rapidly.
2. Confirm data directory free space.
3. Run manual compaction once.
4. If compaction still fails, take a backup and restore into a fresh data dir.

### Restore Validation Fails

1. Confirm the backup manifest exists.
2. Confirm all manifest files exist under the backup directory.
3. Restore into an empty directory.
4. Run TCK/GraphRAG smoke against the restored server before promoting it.
