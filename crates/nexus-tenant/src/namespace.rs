//! Tenant namespace: physically isolated storage per tenant.
//!
//! Each tenant gets its own subdirectory containing:
//! - CSR + column store data
//! - WAL file
//! - Catalog (redb)
//!
//! Cross-tenant queries (the "__ALL__" view) iterate over all namespaces.

use nexus_core::types::TenantId;
use std::path::{Path, PathBuf};

/// Metadata for a single tenant namespace.
#[derive(Debug, Clone)]
pub struct TenantNamespace {
    pub tenant_id: TenantId,
    pub data_dir: PathBuf,
    pub schema_version: u32,
    pub vertex_count: u64,
    pub edge_count: u64,
}

impl TenantNamespace {
    pub fn new(tenant_id: TenantId, base_dir: &Path) -> Self {
        let data_dir = base_dir.join(format!("tenant_{}", tenant_id.as_str()));
        Self {
            tenant_id,
            data_dir,
            schema_version: 1,
            vertex_count: 0,
            edge_count: 0,
        }
    }

    pub fn wal_path(&self) -> PathBuf {
        self.data_dir.join("graph.wal")
    }

    pub fn catalog_path(&self) -> PathBuf {
        self.data_dir.join("catalog.redb")
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.data_dir.join("snapshot.json")
    }

    pub fn exists(&self) -> bool {
        self.data_dir.exists()
    }

    pub fn create_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_paths() {
        let ns = TenantNamespace::new(TenantId::new("AAPL"), Path::new("/data/nexus"));
        assert_eq!(ns.data_dir, PathBuf::from("/data/nexus/tenant_AAPL"));
        assert_eq!(
            ns.wal_path(),
            PathBuf::from("/data/nexus/tenant_AAPL/graph.wal")
        );
        assert_eq!(
            ns.catalog_path(),
            PathBuf::from("/data/nexus/tenant_AAPL/catalog.redb")
        );
    }
}
