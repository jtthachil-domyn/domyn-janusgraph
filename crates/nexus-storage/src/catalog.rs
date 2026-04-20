//! Graph catalog: redb-based persistent metadata store.
//!
//! Stores graph schema (labels, property keys), vertex/edge counts,
//! and snapshot metadata. This is the "system of record" for what
//! exists in a Nexus database.

use crate::error::StorageResult;
use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;

const META_TABLE: TableDefinition<&str, &str> = TableDefinition::new("meta");
const LABEL_TABLE: TableDefinition<&str, u16> = TableDefinition::new("labels");
const PROPERTY_TABLE: TableDefinition<&str, &str> = TableDefinition::new("properties");

/// Persistent catalog backed by redb.
pub struct Catalog {
    db: Database,
}

impl Catalog {
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        let db = Database::create(path.as_ref())?;

        {
            let txn = db.begin_write()?;
            {
                txn.open_table(META_TABLE)?;
                txn.open_table(LABEL_TABLE)?;
                txn.open_table(PROPERTY_TABLE)?;
            }
            txn.commit()?;
        }

        Ok(Self { db })
    }

    pub fn set_meta(&self, key: &str, value: &str) -> StorageResult<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(META_TABLE)?;
            table.insert(key, value)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> StorageResult<Option<String>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(META_TABLE)?;
        Ok(table.get(key)?.map(|v| v.value().to_string()))
    }

    pub fn register_label(&self, name: &str, id: u16) -> StorageResult<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(LABEL_TABLE)?;
            table.insert(name, id)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_label_id(&self, name: &str) -> StorageResult<Option<u16>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LABEL_TABLE)?;
        Ok(table.get(name)?.map(|v| v.value()))
    }

    pub fn list_labels(&self) -> StorageResult<Vec<(String, u16)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LABEL_TABLE)?;
        let mut labels = Vec::new();
        let iter = table.iter()?;
        for entry in iter {
            let entry = entry?;
            labels.push((entry.0.value().to_string(), entry.1.value()));
        }
        Ok(labels)
    }

    /// Store a property definition as JSON.
    pub fn register_property(&self, name: &str, definition_json: &str) -> StorageResult<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(PROPERTY_TABLE)?;
            table.insert(name, definition_json)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_property(&self, name: &str) -> StorageResult<Option<String>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PROPERTY_TABLE)?;
        Ok(table.get(name)?.map(|v| v.value().to_string()))
    }

    pub fn list_properties(&self) -> StorageResult<Vec<(String, String)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PROPERTY_TABLE)?;
        let mut props = Vec::new();
        let iter = table.iter()?;
        for entry in iter {
            let entry = entry?;
            props.push((entry.0.value().to_string(), entry.1.value().to_string()));
        }
        Ok(props)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn catalog_meta_roundtrip() {
        let dir = TempDir::new().unwrap();
        let cat = Catalog::open(dir.path().join("catalog.redb")).unwrap();

        cat.set_meta("schema_version", "1").unwrap();
        cat.set_meta("engine", "nexus").unwrap();

        assert_eq!(
            cat.get_meta("schema_version").unwrap(),
            Some("1".to_string())
        );
        assert_eq!(cat.get_meta("engine").unwrap(), Some("nexus".to_string()));
        assert_eq!(cat.get_meta("nonexistent").unwrap(), None);
    }

    #[test]
    fn catalog_labels() {
        let dir = TempDir::new().unwrap();
        let cat = Catalog::open(dir.path().join("catalog.redb")).unwrap();

        cat.register_label("Entity", 0).unwrap();
        cat.register_label("Document", 1).unwrap();
        cat.register_label("Discloses", 2).unwrap();

        assert_eq!(cat.get_label_id("Entity").unwrap(), Some(0));
        assert_eq!(cat.get_label_id("Discloses").unwrap(), Some(2));
        assert_eq!(cat.get_label_id("Missing").unwrap(), None);

        let labels = cat.list_labels().unwrap();
        assert_eq!(labels.len(), 3);
    }

    #[test]
    fn catalog_properties() {
        let dir = TempDir::new().unwrap();
        let cat = Catalog::open(dir.path().join("catalog.redb")).unwrap();

        cat.register_property("name", r#"{"type":"String","indexed":true,"unique":false}"#)
            .unwrap();
        cat.register_property(
            "external_id",
            r#"{"type":"String","indexed":true,"unique":true}"#,
        )
        .unwrap();

        let name_def = cat.get_property("name").unwrap().unwrap();
        assert!(name_def.contains("String"));
        assert!(name_def.contains("indexed"));

        let props = cat.list_properties().unwrap();
        assert_eq!(props.len(), 2);
    }
}
