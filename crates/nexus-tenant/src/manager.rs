//! Tenant manager: create, open, list, and drop tenant namespaces.

use crate::namespace::TenantNamespace;
use nexus_core::types::TenantId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const ALL_TENANTS: &str = "__ALL__";

pub struct TenantManager {
    base_dir: PathBuf,
    tenants: HashMap<String, TenantNamespace>,
}

impl TenantManager {
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            tenants: HashMap::new(),
        }
    }

    /// Create a new tenant namespace. Returns error if already exists.
    pub fn create_tenant(&mut self, tenant_id: &str) -> Result<&TenantNamespace, TenantError> {
        if self.tenants.contains_key(tenant_id) {
            return Err(TenantError::AlreadyExists(tenant_id.to_string()));
        }

        let ns = TenantNamespace::new(TenantId::new(tenant_id), &self.base_dir);
        ns.create_dirs()
            .map_err(|e| TenantError::Io(e.to_string()))?;

        self.tenants.insert(tenant_id.to_string(), ns);
        Ok(&self.tenants[tenant_id])
    }

    /// Open an existing tenant namespace.
    pub fn open_tenant(&mut self, tenant_id: &str) -> Result<&TenantNamespace, TenantError> {
        if !self.tenants.contains_key(tenant_id) {
            let ns = TenantNamespace::new(TenantId::new(tenant_id), &self.base_dir);
            if !ns.exists() {
                return Err(TenantError::NotFound(tenant_id.to_string()));
            }
            self.tenants.insert(tenant_id.to_string(), ns);
        }
        Ok(&self.tenants[tenant_id])
    }

    /// Get a tenant namespace if loaded.
    pub fn get_tenant(&self, tenant_id: &str) -> Option<&TenantNamespace> {
        self.tenants.get(tenant_id)
    }

    /// List all loaded tenants.
    pub fn list_tenants(&self) -> Vec<&str> {
        self.tenants.keys().map(|s| s.as_str()).collect()
    }

    /// Scan the base directory for existing tenant directories.
    pub fn discover_tenants(&mut self) -> Result<Vec<String>, TenantError> {
        let mut found = Vec::new();

        if !self.base_dir.exists() {
            return Ok(found);
        }

        let entries =
            std::fs::read_dir(&self.base_dir).map_err(|e| TenantError::Io(e.to_string()))?;

        for entry in entries {
            let entry = entry.map_err(|e| TenantError::Io(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("tenant_") && entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            {
                let tenant_id = name.strip_prefix("tenant_").unwrap().to_string();
                if !self.tenants.contains_key(&tenant_id) {
                    let ns = TenantNamespace::new(TenantId::new(&tenant_id), &self.base_dir);
                    self.tenants.insert(tenant_id.clone(), ns);
                }
                found.push(tenant_id);
            }
        }

        Ok(found)
    }

    /// Drop a tenant: removes it from the manager (does NOT delete files).
    pub fn drop_tenant(&mut self, tenant_id: &str) -> bool {
        self.tenants.remove(tenant_id).is_some()
    }

    /// Drop a tenant and delete its data directory.
    pub fn delete_tenant(&mut self, tenant_id: &str) -> Result<bool, TenantError> {
        if let Some(ns) = self.tenants.remove(tenant_id) {
            if ns.data_dir.exists() {
                std::fs::remove_dir_all(&ns.data_dir)
                    .map_err(|e| TenantError::Io(e.to_string()))?;
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TenantError {
    #[error("tenant already exists: {0}")]
    AlreadyExists(String),
    #[error("tenant not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_and_list_tenants() {
        let dir = TempDir::new().unwrap();
        let mut mgr = TenantManager::new(dir.path());

        mgr.create_tenant("AAPL").unwrap();
        mgr.create_tenant("NVDA").unwrap();
        mgr.create_tenant("AMD").unwrap();

        let tenants = mgr.list_tenants();
        assert_eq!(tenants.len(), 3);

        assert!(mgr.get_tenant("AAPL").is_some());
        assert!(mgr.get_tenant("MISSING").is_none());
    }

    #[test]
    fn duplicate_tenant_rejected() {
        let dir = TempDir::new().unwrap();
        let mut mgr = TenantManager::new(dir.path());

        mgr.create_tenant("AAPL").unwrap();
        assert!(mgr.create_tenant("AAPL").is_err());
    }

    #[test]
    fn discover_existing_tenants() {
        let dir = TempDir::new().unwrap();

        // Create dirs manually
        std::fs::create_dir_all(dir.path().join("tenant_AAPL")).unwrap();
        std::fs::create_dir_all(dir.path().join("tenant_NVDA")).unwrap();
        std::fs::create_dir_all(dir.path().join("not_a_tenant")).unwrap();

        let mut mgr = TenantManager::new(dir.path());
        let discovered = mgr.discover_tenants().unwrap();

        assert_eq!(discovered.len(), 2);
        assert!(discovered.contains(&"AAPL".to_string()));
        assert!(discovered.contains(&"NVDA".to_string()));
    }

    #[test]
    fn delete_tenant_removes_data() {
        let dir = TempDir::new().unwrap();
        let mut mgr = TenantManager::new(dir.path());

        mgr.create_tenant("TEMP").unwrap();
        assert!(dir.path().join("tenant_TEMP").exists());

        mgr.delete_tenant("TEMP").unwrap();
        assert!(!dir.path().join("tenant_TEMP").exists());
    }
}
