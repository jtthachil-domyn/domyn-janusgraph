//! Hash-based composite indexes for O(1) exact lookups.
//!
//! These replace JanusGraph's Cassandra-backed composite indexes and the
//! Redis sidecar for external_id -> internal_id mapping.
//!
//! Each composite index maps a string key to a set of vertex/edge IDs.
//! Unique indexes enforce single-value semantics (like external_id).

use ahash::AHashMap;
use nexus_core::types::VertexId;
use std::collections::HashSet;
use std::mem::size_of;

/// A non-unique composite index: one key maps to many vertex IDs.
/// Used for tenant_id, entity_type, name, etc.
pub struct CompositeIndex {
    name: String,
    map: AHashMap<String, HashSet<u64>>,
}

impl CompositeIndex {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            map: AHashMap::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn insert(&mut self, key: &str, vertex_id: VertexId) {
        self.map
            .entry(key.to_string())
            .or_default()
            .insert(vertex_id.0);
    }

    pub fn get(&self, key: &str) -> Vec<VertexId> {
        self.map
            .get(key)
            .map(|set| set.iter().map(|&id| VertexId(id)).collect())
            .unwrap_or_default()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.map.get(key).is_some_and(|set| !set.is_empty())
    }

    pub fn remove(&mut self, key: &str, vertex_id: VertexId) {
        if let Some(set) = self.map.get_mut(key) {
            set.remove(&vertex_id.0);
            if set.is_empty() {
                self.map.remove(key);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.map.values().map(|s| s.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn num_keys(&self) -> usize {
        self.map.len()
    }

    pub fn estimated_heap_bytes(&self) -> usize {
        self.name.capacity()
            + self.map.capacity() * size_of::<(String, HashSet<u64>)>()
            + self
                .map
                .iter()
                .map(|(key, set)| key.capacity() + set.capacity() * size_of::<u64>())
                .sum::<usize>()
    }
}

/// A unique composite index: one key maps to exactly one vertex ID.
/// Used for external_id (the integration key from DOMYNGRAPH.md).
///
/// This is the first-class replacement for the Redis `external_id -> internal_id`
/// mapping described in GRAPHRAG_ARCHITECTURE.md.
pub struct UniqueIndex {
    name: String,
    map: AHashMap<String, u64>,
    reverse: AHashMap<u64, String>,
}

impl UniqueIndex {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            map: AHashMap::new(),
            reverse: AHashMap::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Insert a unique mapping. Returns Err if the key already exists
    /// with a different vertex.
    pub fn insert(&mut self, key: &str, vertex_id: VertexId) -> Result<(), UniqueIndexError> {
        if let Some(&existing) = self.map.get(key) {
            if existing != vertex_id.0 {
                return Err(UniqueIndexError::DuplicateKey {
                    index: self.name.clone(),
                    key: key.to_string(),
                    existing: VertexId(existing),
                    attempted: vertex_id,
                });
            }
            return Ok(());
        }
        self.map.insert(key.to_string(), vertex_id.0);
        self.reverse.insert(vertex_id.0, key.to_string());
        Ok(())
    }

    /// O(1) lookup: key -> vertex ID.
    pub fn get(&self, key: &str) -> Option<VertexId> {
        self.map.get(key).map(|&id| VertexId(id))
    }

    /// Reverse lookup: vertex ID -> key.
    pub fn reverse_get(&self, vertex_id: VertexId) -> Option<&str> {
        self.reverse.get(&vertex_id.0).map(|s| s.as_str())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    pub fn remove(&mut self, key: &str) -> Option<VertexId> {
        if let Some(id) = self.map.remove(key) {
            self.reverse.remove(&id);
            Some(VertexId(id))
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn estimated_heap_bytes(&self) -> usize {
        self.name.capacity()
            + self.map.capacity() * size_of::<(String, u64)>()
            + self.map.keys().map(String::capacity).sum::<usize>()
            + self.reverse.capacity() * size_of::<(u64, String)>()
            + self.reverse.values().map(String::capacity).sum::<usize>()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UniqueIndexError {
    #[error(
        "duplicate key in index '{index}': key='{key}' already maps to vertex {existing:?}, cannot map to {attempted:?}"
    )]
    DuplicateKey {
        index: String,
        key: String,
        existing: VertexId,
        attempted: VertexId,
    },
}

/// Collection of all indexes for a graph.
pub struct IndexSet {
    composite: Vec<CompositeIndex>,
    unique: Vec<UniqueIndex>,
}

impl IndexSet {
    pub fn new() -> Self {
        Self {
            composite: Vec::new(),
            unique: Vec::new(),
        }
    }

    pub fn add_composite(&mut self, name: &str) -> usize {
        let idx = self.composite.len();
        self.composite.push(CompositeIndex::new(name));
        idx
    }

    pub fn add_unique(&mut self, name: &str) -> usize {
        let idx = self.unique.len();
        self.unique.push(UniqueIndex::new(name));
        idx
    }

    pub fn composite(&self, idx: usize) -> Option<&CompositeIndex> {
        self.composite.get(idx)
    }

    pub fn composite_mut(&mut self, idx: usize) -> Option<&mut CompositeIndex> {
        self.composite.get_mut(idx)
    }

    pub fn unique(&self, idx: usize) -> Option<&UniqueIndex> {
        self.unique.get(idx)
    }

    pub fn unique_mut(&mut self, idx: usize) -> Option<&mut UniqueIndex> {
        self.unique.get_mut(idx)
    }

    pub fn find_composite(&self, name: &str) -> Option<(usize, &CompositeIndex)> {
        self.composite
            .iter()
            .enumerate()
            .find(|(_, idx)| idx.name() == name)
    }

    pub fn find_unique(&self, name: &str) -> Option<(usize, &UniqueIndex)> {
        self.unique
            .iter()
            .enumerate()
            .find(|(_, idx)| idx.name() == name)
    }

    pub fn estimated_heap_bytes(&self) -> usize {
        self.composite.capacity() * size_of::<CompositeIndex>()
            + self
                .composite
                .iter()
                .map(CompositeIndex::estimated_heap_bytes)
                .sum::<usize>()
            + self.unique.capacity() * size_of::<UniqueIndex>()
            + self
                .unique
                .iter()
                .map(UniqueIndex::estimated_heap_bytes)
                .sum::<usize>()
    }
}

impl Default for IndexSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_index_multi_value() {
        let mut idx = CompositeIndex::new("by_tenant");
        idx.insert("AAPL", VertexId(0));
        idx.insert("AAPL", VertexId(1));
        idx.insert("AAPL", VertexId(2));
        idx.insert("NVDA", VertexId(3));

        let aapl = idx.get("AAPL");
        assert_eq!(aapl.len(), 3);
        assert_eq!(idx.get("NVDA").len(), 1);
        assert_eq!(idx.get("MISSING").len(), 0);
        assert_eq!(idx.num_keys(), 2);
    }

    #[test]
    fn unique_index_external_id() {
        let mut idx = UniqueIndex::new("by_external_id");
        idx.insert("AAPL:Revenue:FIN_METRIC", VertexId(42)).unwrap();
        idx.insert("AAPL:Apple Inc.:ORG", VertexId(100)).unwrap();

        assert_eq!(idx.get("AAPL:Revenue:FIN_METRIC"), Some(VertexId(42)));
        assert_eq!(
            idx.reverse_get(VertexId(42)),
            Some("AAPL:Revenue:FIN_METRIC")
        );
        assert_eq!(idx.get("MISSING"), None);
    }

    #[test]
    fn unique_index_rejects_duplicate() {
        let mut idx = UniqueIndex::new("by_external_id");
        idx.insert("key1", VertexId(0)).unwrap();

        let result = idx.insert("key1", VertexId(99));
        assert!(result.is_err());

        // Re-inserting same key+value is idempotent
        idx.insert("key1", VertexId(0)).unwrap();
    }

    #[test]
    fn index_set_management() {
        let mut set = IndexSet::new();
        let tenant_idx = set.add_composite("by_tenant");
        let ext_id_idx = set.add_unique("by_external_id");

        set.composite_mut(tenant_idx)
            .unwrap()
            .insert("AAPL", VertexId(0));
        set.unique_mut(ext_id_idx)
            .unwrap()
            .insert("AAPL:Apple:ORG", VertexId(0))
            .unwrap();

        assert!(set.find_composite("by_tenant").is_some());
        assert!(set.find_unique("by_external_id").is_some());
        assert!(set.find_composite("nonexistent").is_none());
    }

    #[test]
    fn index_set_estimated_heap_bytes_tracks_owned_entries() {
        let mut set = IndexSet::new();
        let comp = set.add_composite("tenant_id");
        let unique = set.add_unique("external_id");
        let empty = set.estimated_heap_bytes();

        set.composite_mut(comp).unwrap().insert("NVDA", VertexId(1));
        set.composite_mut(comp).unwrap().insert("NVDA", VertexId(2));
        set.unique_mut(unique)
            .unwrap()
            .insert("NVDA:entity:001", VertexId(1))
            .unwrap();

        assert!(set.estimated_heap_bytes() > empty);
    }
}
