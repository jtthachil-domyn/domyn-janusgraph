use serde::{Deserialize, Serialize};
use std::fmt;

/// Internal vertex identifier. Stable within a storage generation.
/// Maps directly to a row index in the CSR and columnar stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
pub struct VertexId(pub u64);

/// Internal edge identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
pub struct EdgeId(pub u64);

/// Label identifier -- indexes into a label dictionary.
/// Each unique vertex label (e.g., "Entity", "Document") or edge label
/// (e.g., "Discloses", "CONTAINS") maps to a `LabelId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
pub struct LabelId(pub u16);

/// Property key identifier -- indexes into a property key dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
pub struct PropertyKeyId(pub u16);

/// Tenant namespace identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TenantId(pub String);

impl TenantId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Dynamically typed property value. Kept small for columnar storage;
/// large blobs (embeddings) use the `Bytes` variant and are stored separately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    String(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(Vec<(String, Value)>),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int64(_) => "int64",
            Value::Float64(_) => "float64",
            Value::String(_) => "string",
            Value::Bytes(_) => "bytes",
            Value::List(_) => "list",
            Value::Map(_) => "map",
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float64(v) => Some(*v),
            Value::Int64(v) => Some(*v as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(v) => write!(f, "{v}"),
            Value::Int64(v) => write!(f, "{v}"),
            Value::Float64(v) => write!(f, "{v}"),
            Value::String(v) => write!(f, "\"{v}\""),
            Value::Bytes(v) => write!(f, "<{} bytes>", v.len()),
            Value::List(v) => write!(f, "[{} items]", v.len()),
            Value::Map(v) => write!(f, "{{{} entries}}", v.len()),
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int64(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float64(v)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

/// Direction for edge traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

/// A vertex with its label and properties resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vertex {
    pub id: VertexId,
    pub label: String,
    pub properties: Vec<(String, Value)>,
}

/// An edge with source, target, label, and properties resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub source: VertexId,
    pub target: VertexId,
    pub label: String,
    pub properties: Vec<(String, Value)>,
}

/// Configuration for bounded traversal -- engine-enforced, not application-level.
#[derive(Debug, Clone)]
pub struct TraversalBudget {
    pub max_depth: u32,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub predicate_filter: Option<Vec<LabelId>>,
}

impl Default for TraversalBudget {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_nodes: 100,
            max_edges: 500,
            predicate_filter: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_id_ordering() {
        assert!(VertexId(0) < VertexId(1));
        assert_eq!(VertexId(42), VertexId(42));
    }

    #[test]
    fn value_conversions() {
        let v: Value = "hello".into();
        assert_eq!(v.as_str(), Some("hello"));

        let v: Value = 42i64.into();
        assert_eq!(v.as_i64(), Some(42));
        assert_eq!(v.as_f64(), Some(42.0));

        let v: Value = 3.14f64.into();
        assert_eq!(v.as_f64(), Some(3.14));

        assert!(Value::Null.is_null());
    }
}
