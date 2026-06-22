# Azure Single-Node Deployment

This is the first cloud-demo shell for Domyn Nexus. It provisions a single
Linux VM, user-assigned managed identity, managed data disk, Key Vault, storage
account for backups, Log Analytics workspace, Azure Monitor Agent, public IP,
network security group, and cloud-init bootstrap hooks.

It is intentionally single-node. It is not HA, not distributed, and not a
cross-shard deployment.

## Prerequisites

- Azure CLI logged in with `az login`
- A resource group
- A Domyn Nexus container image available to the VM, for example:
  `ghcr.io/<org>/<repo>:v0.1.0`

## Deploy

```bash
az deployment group create \
  --resource-group <resource-group> \
  --template-file deploy/azure/single-node/main.bicep \
  --parameters @deploy/azure/single-node/parameters.example.json \
  --parameters adminSshPublicKey="$(cat ~/.ssh/id_rsa.pub)" \
  --parameters nexusImage="ghcr.io/<org>/<repo>:v0.1.0"
```

For non-demo TLS, pass PEM values for both optional certificate parameters:

```bash
az deployment group create \
  --resource-group <resource-group> \
  --template-file deploy/azure/single-node/main.bicep \
  --parameters @deploy/azure/single-node/parameters.example.json \
  --parameters tlsCertPem="$(cat /path/to/fullchain.pem)" \
  --parameters tlsKeyPem="$(cat /path/to/privkey.pem)"
```

The template outputs:

- `publicIp`
- `httpsUrl`
- `boltEndpoint`
- `keyVaultName`
- `backupStorageAccountName`
- `backupContainerName`
- `logAnalyticsWorkspaceName`
- `dataCollectionRuleName`

## Smoke

```bash
deploy/azure/single-node/smoke.sh https://<public-ip>:8443 <admin-token>
```

## What The VM Bootstrap Does

Cloud-init:

1. Installs Docker.
2. Formats and mounts the managed data disk at `/var/lib/domyn-nexus`.
3. Installs TLS certs from Key Vault when `tlsCertPem` and `tlsKeyPem` are
   supplied, otherwise creates local TLS certificates for the demo.
4. Fetches API tokens from Key Vault through the VM managed identity.
5. Writes `/etc/domyn-nexus/config.json` from the local config template.
6. Starts the `nexus-server` container.
7. Starts an hourly backup-to-Blob sync timer.

The VM depends on the Key Vault and Blob role assignments before boot, so
cloud-init can read secrets and upload backups immediately instead of racing
Azure RBAC propagation.

## Backup Target

The template creates a storage account and `nexus-backups` Blob container. The
server backup path writes to local disk first, and
`domyn-nexus-backup-sync.timer` uploads files under `/var/backups/domyn-nexus`
to Blob through the VM managed identity.

## Monitoring Target

The template installs Azure Monitor Agent and associates a Data Collection Rule
that sends Linux performance counters and syslog to the Log Analytics
workspace. Nexus application metrics remain available at `/metrics`; scraping
that endpoint into Azure Monitor or managed Grafana is still follow-up work.

## Security Boundary

- Use real TLS certificates by passing `tlsCertPem` and `tlsKeyPem`.
- Restrict inbound source IPs before opening this outside a private test
  network.
- Treat this as a demo/internal-beta deployment only.
