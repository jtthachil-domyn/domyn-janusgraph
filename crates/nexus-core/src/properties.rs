//! Columnar property store.
//!
//! Properties are stored as typed flat arrays rather than per-vertex maps.
//! This eliminates per-vertex object overhead (~100+ bytes on JVM) and enables
//! SIMD-friendly sequential access for filter predicates.
//!
//! Design influence: Kuzu/LadybugDB columnar storage.

use crate::types::{PropertyKeyId, Value};
use std::collections::HashMap;

/// A single typed column storing values for one property across all vertices/edges.
#[derive(Debug, Clone)]
pub enum Column {
    Bool(Vec<Option<bool>>),
    Int64(Vec<Option<i64>>),
    Float64(Vec<Option<f64>>),
    String(Vec<Option<String>>),
    Bytes(Vec<Option<Vec<u8>>>),
}

impl Column {
    pub fn property_type(&self) -> PropertyType {
        match self {
            Column::Bool(_) => PropertyType::Bool,
            Column::Int64(_) => PropertyType::Int64,
            Column::Float64(_) => PropertyType::Float64,
            Column::String(_) => PropertyType::String,
            Column::Bytes(_) => PropertyType::Bytes,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Column::Bool(v) => v.len(),
            Column::Int64(v) => v.len(),
            Column::Float64(v) => v.len(),
            Column::String(v) => v.len(),
            Column::Bytes(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, idx: usize) -> Value {
        match self {
            Column::Bool(v) => v
                .get(idx)
                .and_then(|o| o.as_ref())
                .map(|&b| Value::Bool(b))
                .unwrap_or(Value::Null),
            Column::Int64(v) => v
                .get(idx)
                .and_then(|o| o.as_ref())
                .map(|&i| Value::Int64(i))
                .unwrap_or(Value::Null),
            Column::Float64(v) => v
                .get(idx)
                .and_then(|o| o.as_ref())
                .map(|&f| Value::Float64(f))
                .unwrap_or(Value::Null),
            Column::String(v) => v
                .get(idx)
                .and_then(|o| o.as_ref())
                .map(|s| Value::String(s.clone()))
                .unwrap_or(Value::Null),
            Column::Bytes(v) => v
                .get(idx)
                .and_then(|o| o.as_ref())
                .map(|b| Value::Bytes(b.clone()))
                .unwrap_or(Value::Null),
        }
    }

    pub fn set(&mut self, idx: usize, value: Value) {
        let _ = self.try_set(idx, value);
    }

    pub fn try_set(&mut self, idx: usize, value: Value) -> Result<(), PropertyError> {
        if idx >= self.len() {
            return Err(PropertyError::RowOutOfBounds {
                row: idx,
                rows: self.len(),
            });
        }

        if matches!(value, Value::Null) {
            match self {
                Column::Bool(v) => v[idx] = None,
                Column::Int64(v) => v[idx] = None,
                Column::Float64(v) => v[idx] = None,
                Column::String(v) => v[idx] = None,
                Column::Bytes(v) => v[idx] = None,
            }
            return Ok(());
        }

        match (self, value) {
            (Column::Bool(v), Value::Bool(b)) => {
                v[idx] = Some(b);
            }
            (Column::Int64(v), Value::Int64(i)) => {
                v[idx] = Some(i);
            }
            (Column::Float64(v), Value::Float64(f)) => {
                v[idx] = Some(f);
            }
            (Column::String(v), Value::String(s)) => {
                v[idx] = Some(s);
            }
            (Column::Bytes(v), Value::Bytes(b)) => {
                v[idx] = Some(b);
            }
            (col, value) => {
                return Err(PropertyError::TypeMismatch {
                    expected: col.property_type(),
                    actual: value.type_name(),
                });
            }
        }

        Ok(())
    }

    /// Create a new column of the same type with `n` null entries.
    pub fn new_with_size(template: &Column, n: usize) -> Column {
        match template {
            Column::Bool(_) => Column::Bool(vec![None; n]),
            Column::Int64(_) => Column::Int64(vec![None; n]),
            Column::Float64(_) => Column::Float64(vec![None; n]),
            Column::String(_) => Column::String(vec![None; n]),
            Column::Bytes(_) => Column::Bytes(vec![None; n]),
        }
    }
}

/// Describes a property's type for schema definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PropertyType {
    Bool,
    Int64,
    Float64,
    String,
    Bytes,
}

impl std::fmt::Display for PropertyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PropertyType::Bool => write!(f, "bool"),
            PropertyType::Int64 => write!(f, "int64"),
            PropertyType::Float64 => write!(f, "float64"),
            PropertyType::String => write!(f, "string"),
            PropertyType::Bytes => write!(f, "bytes"),
        }
    }
}

impl PropertyType {
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(_) => Some(PropertyType::Bool),
            Value::Int64(_) => Some(PropertyType::Int64),
            Value::Float64(_) => Some(PropertyType::Float64),
            Value::String(_) => Some(PropertyType::String),
            Value::Bytes(_) => Some(PropertyType::Bytes),
            Value::Null | Value::List(_) | Value::Map(_) => None,
        }
    }

    pub fn create_column(&self, size: usize) -> Column {
        match self {
            PropertyType::Bool => Column::Bool(vec![None; size]),
            PropertyType::Int64 => Column::Int64(vec![None; size]),
            PropertyType::Float64 => Column::Float64(vec![None; size]),
            PropertyType::String => Column::String(vec![None; size]),
            PropertyType::Bytes => Column::Bytes(vec![None; size]),
        }
    }
}

/// Property key definition: name + type + whether it's indexed.
#[derive(Debug, Clone)]
pub struct PropertyKeyDef {
    pub id: PropertyKeyId,
    pub name: String,
    pub property_type: PropertyType,
    pub indexed: bool,
    pub unique: bool,
}

/// Columnar property table for either vertices or edges.
/// Each property key is a column; rows correspond to vertex/edge IDs.
#[derive(Clone)]
pub struct PropertyStore {
    capacity: usize,
    count: usize,
    columns: HashMap<PropertyKeyId, Column>,
    key_defs: HashMap<PropertyKeyId, PropertyKeyDef>,
    name_to_id: HashMap<String, PropertyKeyId>,
    next_key_id: u16,
}

impl PropertyStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            count: 0,
            columns: HashMap::new(),
            key_defs: HashMap::new(),
            name_to_id: HashMap::new(),
            next_key_id: 0,
        }
    }

    /// Register a property key. Returns its ID.
    pub fn register_property(
        &mut self,
        name: &str,
        property_type: PropertyType,
        indexed: bool,
        unique: bool,
    ) -> PropertyKeyId {
        if let Some(&id) = self.name_to_id.get(name) {
            return id;
        }
        let id = PropertyKeyId(self.next_key_id);
        self.next_key_id += 1;

        let def = PropertyKeyDef {
            id,
            name: name.to_string(),
            property_type,
            indexed,
            unique,
        };

        self.columns
            .insert(id, property_type.create_column(self.capacity));
        self.key_defs.insert(id, def);
        self.name_to_id.insert(name.to_string(), id);
        id
    }

    /// Allocate a new row, returning its index. Used when adding a vertex/edge.
    pub fn allocate_row(&mut self) -> usize {
        let idx = self.count;
        self.count += 1;
        if self.count > self.capacity {
            self.grow();
        }
        idx
    }

    /// Set a property value for a given row.
    pub fn set(&mut self, row: usize, key: PropertyKeyId, value: Value) {
        let _ = self.try_set(row, key, value);
    }

    pub fn try_set(
        &mut self,
        row: usize,
        key: PropertyKeyId,
        value: Value,
    ) -> Result<(), PropertyError> {
        if row >= self.count {
            return Err(PropertyError::RowOutOfBounds {
                row,
                rows: self.count,
            });
        }

        let col = self
            .columns
            .get_mut(&key)
            .ok_or(PropertyError::UnknownPropertyId(key))?;
        col.try_set(row, value)
    }

    /// Set a property by name.
    pub fn set_by_name(&mut self, row: usize, name: &str, value: Value) {
        let _ = self.try_set_by_name(row, name, value);
    }

    pub fn try_set_by_name(
        &mut self,
        row: usize,
        name: &str,
        value: Value,
    ) -> Result<(), PropertyError> {
        let key = self
            .name_to_id
            .get(name)
            .copied()
            .ok_or_else(|| PropertyError::UnknownProperty(name.to_string()))?;
        self.try_set(row, key, value)
    }

    /// Get a property value for a given row.
    pub fn get(&self, row: usize, key: PropertyKeyId) -> Value {
        self.columns
            .get(&key)
            .map(|col| col.get(row))
            .unwrap_or(Value::Null)
    }

    /// Get a property by name.
    pub fn get_by_name(&self, row: usize, name: &str) -> Value {
        self.name_to_id
            .get(name)
            .and_then(|&key| self.columns.get(&key))
            .map(|col| col.get(row))
            .unwrap_or(Value::Null)
    }

    /// Get all properties for a row as (name, value) pairs.
    pub fn get_all(&self, row: usize) -> Vec<(String, Value)> {
        self.key_defs
            .values()
            .filter_map(|def| {
                let val = self.get(row, def.id);
                if val.is_null() {
                    None
                } else {
                    Some((def.name.clone(), val))
                }
            })
            .collect()
    }

    pub fn property_id(&self, name: &str) -> Option<PropertyKeyId> {
        self.name_to_id.get(name).copied()
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn key_defs(&self) -> impl Iterator<Item = &PropertyKeyDef> {
        self.key_defs.values()
    }

    fn grow(&mut self) {
        let new_cap = if self.capacity == 0 {
            8
        } else {
            self.capacity * 2
        };
        for col in self.columns.values_mut() {
            match col {
                Column::Bool(v) => v.resize(new_cap, None),
                Column::Int64(v) => v.resize(new_cap, None),
                Column::Float64(v) => v.resize(new_cap, None),
                Column::String(v) => v.resize(new_cap, None),
                Column::Bytes(v) => v.resize(new_cap, None),
            }
        }
        self.capacity = new_cap;
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum PropertyError {
    #[error("unknown property: {0}")]
    UnknownProperty(String),
    #[error("unknown property id: {0:?}")]
    UnknownPropertyId(PropertyKeyId),
    #[error("property row {row} out of bounds for {rows} rows")]
    RowOutOfBounds { row: usize, rows: usize },
    #[error("property type mismatch: expected {expected}, got {actual}")]
    TypeMismatch {
        expected: PropertyType,
        actual: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_store_basic() {
        let mut store = PropertyStore::new(10);
        let name_key = store.register_property("name", PropertyType::String, true, false);
        let age_key = store.register_property("age", PropertyType::Int64, false, false);

        let row0 = store.allocate_row();
        store.set(row0, name_key, Value::String("Alice".into()));
        store.set(row0, age_key, Value::Int64(30));

        let row1 = store.allocate_row();
        store.set(row1, name_key, Value::String("Bob".into()));

        assert_eq!(store.get(row0, name_key).as_str(), Some("Alice"));
        assert_eq!(store.get(row0, age_key).as_i64(), Some(30));
        assert_eq!(store.get(row1, name_key).as_str(), Some("Bob"));
        assert!(store.get(row1, age_key).is_null());
    }

    #[test]
    fn property_store_zero_capacity_grows() {
        let mut store = PropertyStore::new(0);
        store.register_property("name", PropertyType::String, true, false);
        let row = store.allocate_row();
        store.set_by_name(row, "name", Value::String("test".into()));
        assert_eq!(store.get_by_name(row, "name").as_str(), Some("test"));
    }

    #[test]
    fn property_store_by_name() {
        let mut store = PropertyStore::new(4);
        store.register_property("tenant_id", PropertyType::String, true, false);
        store.register_property("external_id", PropertyType::String, true, true);

        let row = store.allocate_row();
        store.set_by_name(row, "tenant_id", "AAPL".into());
        store.set_by_name(row, "external_id", "AAPL:Revenue:FIN_METRIC".into());

        assert_eq!(store.get_by_name(row, "tenant_id").as_str(), Some("AAPL"));
        assert_eq!(
            store.get_by_name(row, "external_id").as_str(),
            Some("AAPL:Revenue:FIN_METRIC")
        );

        let all = store.get_all(row);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn property_store_rejects_type_mismatch() {
        let mut store = PropertyStore::new(1);
        store.register_property("age", PropertyType::Int64, false, false);
        let row = store.allocate_row();

        let err = store
            .try_set_by_name(row, "age", Value::String("thirty".into()))
            .unwrap_err();

        assert!(matches!(
            err,
            PropertyError::TypeMismatch {
                expected: PropertyType::Int64,
                actual: "string",
            }
        ));
        assert!(store.get_by_name(row, "age").is_null());
    }
}
