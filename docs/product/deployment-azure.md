# Azure Deployment

Domyn Nexus has an Internal Beta single-node Azure skeleton under
`deploy/azure/single-node/`.

This deployment is for demos and internal beta validation. It is not HA,
distributed, or cross-shard.

## What It Creates

- Linux VM running the `nexus-server` container
- User-assigned managed identity, permissioned before VM boot
- Managed data disk mounted at `/var/lib/domyn-nexus`
- Public IP and network security group
- Key Vault containing API tokens, read by the VM through managed identity
- Storage account and Blob container for backup artifacts
- Log Analytics workspace, Azure Monitor Agent, and a Data Collection Rule for
  VM performance counters and syslog
- Cloud-init bootstrap for Docker, Key Vault-backed TLS/config generation,
  systemd, and backup sync
- Smoke script for health, readiness, metrics, Cypher write, and backup

## Deploy

```bash
az deployment group create \
  --resource-group <resource-group> \
  --template-file deploy/azure/single-node/main.bicep \
  --parameters @deploy/azure/single-node/parameters.example.json \
  --parameters adminSshPublicKey="$(cat ~/.ssh/id_rsa.pub)" \
  --parameters nexusImage="ghcr.io/<org>/<repo>:v0.1.0" \
  --parameters adminToken="<admin-token>" \
  --parameters writerToken="<writer-token>" \
  --parameters readerToken="<reader-token>" \
  --parameters allowedSourceCidr="<your-ip>/32"
```

For real TLS, also pass PEM-encoded certificate and key values:

```bash
az deployment group create \
  --resource-group <resource-group> \
  --template-file deploy/azure/single-node/main.bicep \
  --parameters @deploy/azure/single-node/parameters.example.json \
  --parameters tlsCertPem="$(cat /path/to/fullchain.pem)" \
  --parameters tlsKeyPem="$(cat /path/to/privkey.pem)"
```

## Smoke

```bash
deploy/azure/single-node/smoke.sh https://<public-ip>:8443 <admin-token>
```

## Runtime Secrets

The deployment still accepts API-token parameters so the template can create
the initial Key Vault secrets, but the secret values are not rendered into
cloud-init. The VM uses a user-assigned managed identity with `Key Vault
Secrets User`; the role assignment is created before VM boot.
`/usr/local/bin/domyn-nexus-fetch-secrets.sh` retrieves the tokens at boot and
writes `/etc/domyn-nexus/config.json` on the VM.

## TLS Notes

When `tlsCertPem` and `tlsKeyPem` are supplied, Bicep stores them as Key Vault
secrets and `/usr/local/bin/domyn-nexus-install-tls.sh` installs and validates
them before starting Nexus. If both are empty, the VM generates a 30-day
self-signed certificate for demo-only use. Supplying only one of the pair is a
boot-time error.

## Backup Notes

The Azure skeleton creates a storage account and `nexus-backups` Blob
container. The server backup operation writes to local disk first under
`/var/backups/domyn-nexus`; a systemd timer runs
`/usr/local/bin/domyn-nexus-backup-sync.sh` hourly to upload backup files to
Blob through the VM managed identity.

## Monitoring Notes

The template installs Azure Monitor Agent and associates a Data Collection Rule
that sends basic Linux performance counters and syslog to the Log Analytics
workspace. Nexus still exposes its own Prometheus `/metrics` endpoint; scraping
those application metrics into Azure Monitor or managed Grafana remains a
follow-up step.

## Security Notes

- Replace demo tokens before use.
- Use a narrow `allowedSourceCidr`.
- Pass `tlsCertPem` and `tlsKeyPem` before any shared environment.
- Do not represent this deployment as HA or production-distributed.

## Remaining Azure Work

- Validate Key Vault retrieval, TLS installation, and Blob sync in a clean
  Azure resource group.
- Validate Azure Monitor Agent data arrival and wire Nexus `/metrics` into
  Azure Monitor or managed Grafana.
- Add an automated clean-subscription smoke in CI.
