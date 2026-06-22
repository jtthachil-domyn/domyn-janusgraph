# Changelog

All notable Domyn Nexus changes will be recorded here.

## Unreleased

### Added

- Server product CLI:
  - `nexus-server check-config --config <path>`
  - `nexus-server init --profile dev|production [--output <path>]`
  - `nexus-server backup --config <path> --output <dir>`
  - `nexus-server restore --backup <dir> --data-dir <dir>`
- `cargo xtask verify-internal-beta` canonical gate, with a quick iteration
  mode.
- Docker image defaults to `/etc/domyn-nexus/config.json` and healthchecks
  `/health`.
- Product documentation under `docs/product/`.
- GitHub Actions for CI and tagged release artifacts.

### Changed

- Production readiness documentation now links the product packaging plan and
  server docs.

### Notes

- Current target remains Internal Beta single-node server. Distributed
  replication/sharding and enterprise GA claims remain out of scope for this
  phase.
