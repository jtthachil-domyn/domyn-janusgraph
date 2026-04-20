//! Graph persistence: save/load graph state to/from disk.
//!
//! Combines WAL (for crash recovery) with redb catalog (for schema)
//! and JSON snapshots (for full graph state).

use crate::catalog::Catalog;
use crate::error::{StorageError, StorageResult};
use crate::wal::{WalEntry, WalReader, WalWriter};
use nexus_core::graph::Graph;
use nexus_core::properties::PropertyType;
use nexus_core::types::{EdgeId, Value, VertexId};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Serializable snapshot of a complete graph.
#[derive(Debug, Serialize, Deserialize)]
pub struct GraphSnapshot {
    pub vertices: Vec<VertexSnapshot>,
    pub edges: Vec<EdgeSnapshot>,
    pub schema: SchemaSnapshot,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VertexSnapshot {
    pub id: u64,
    pub label: String,
    pub properties: Vec<(String, String)>, // (key, json-encoded value)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EdgeSnapshot {
    pub id: u64,
    pub source: u64,
    pub target: u64,
    pub label: String,
    pub properties: Vec<(String, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    pub vertex_properties: Vec<PropertyDef>,
    pub edge_properties: Vec<PropertyDef>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PropertyDef {
    pub name: String,
    pub property_type: String,
    pub indexed: bool,
    pub unique: bool,
}

/// Database directory layout:
/// ```text
/// data_dir/
///   catalog.redb    -- schema, metadata (redb)
///   graph.wal       -- write-ahead log
///   snapshot.json   -- latest full graph snapshot
/// ```
pub struct NexusStore {
    data_dir: PathBuf,
    catalog: Catalog,
    wal: WalWriter,
}

impl NexusStore {
    /// Open or create a Nexus database at the given directory.
    pub fn open(data_dir: impl AsRef<Path>) -> StorageResult<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(&data_dir)?;

        let catalog = Catalog::open(data_dir.join("catalog.redb"))?;
        let wal = WalWriter::open(data_dir.join("graph.wal"))?;

        catalog.set_meta("engine", "domyn-nexus")?;
        catalog.set_meta("version", "0.1.0")?;

        Ok(Self {
            data_dir,
            catalog,
            wal,
        })
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn wal_mut(&mut self) -> &mut WalWriter {
        &mut self.wal
    }

    /// Write a WAL entry for a vertex addition.
    pub fn log_add_vertex(&mut self, id: u64, label: &str) -> StorageResult<u64> {
        self.wal.append(&WalEntry::AddVertex {
            id,
            label: label.to_string(),
        })
    }

    /// Write a WAL entry for a vertex property set.
    pub fn log_set_vertex_property(
        &mut self,
        vertex_id: u64,
        key: &str,
        value: &Value,
    ) -> StorageResult<u64> {
        self.wal.append(&WalEntry::SetVertexProperty {
            vertex_id,
            key: key.to_string(),
            value: value.clone(),
        })
    }

    /// Write a WAL entry for an edge addition.
    pub fn log_add_edge(
        &mut self,
        edge_id: u64,
        source: u64,
        target: u64,
        label: &str,
    ) -> StorageResult<u64> {
        self.wal.append(&WalEntry::AddEdge {
            edge_id,
            source,
            target,
            label: label.to_string(),
        })
    }

    /// Write a WAL entry for an edge property set.
    pub fn log_set_edge_property(
        &mut self,
        edge_id: u64,
        key: &str,
        value: &Value,
    ) -> StorageResult<u64> {
        self.wal.append(&WalEntry::SetEdgeProperty {
            edge_id,
            key: key.to_string(),
            value: value.clone(),
        })
    }

    /// Write a WAL entry for a vertex deletion.
    pub fn log_remove_vertex(&mut self, vertex_id: u64) -> StorageResult<u64> {
        self.wal.append(&WalEntry::RemoveVertex { vertex_id })
    }

    /// Write a WAL entry for an edge deletion.
    pub fn log_remove_edge(&mut self, edge_id: u64) -> StorageResult<u64> {
        self.wal.append(&WalEntry::RemoveEdge { edge_id })
    }

    /// Save a full graph snapshot to disk.
    pub fn save_snapshot(&mut self, graph: &Graph) -> StorageResult<()> {
        let mut vertices = Vec::new();
        for i in 0..graph.num_vertices() as u64 {
            let vid = VertexId(i);
            if let Some(label) = graph.vertex_label(vid) {
                let props: Vec<(String, String)> = graph
                    .get_vertex_properties(vid)
                    .into_iter()
                    .map(|(k, v)| {
                        let json = serde_json::to_string(&v).unwrap_or_default();
                        (k, json)
                    })
                    .collect();
                vertices.push(VertexSnapshot {
                    id: i,
                    label: label.to_string(),
                    properties: props,
                });
            }
        }

        let edges = graph
            .edge_records()
            .into_iter()
            .map(|edge| {
                let props: Vec<(String, String)> = edge
                    .properties
                    .into_iter()
                    .map(|(k, val)| {
                        let json = serde_json::to_string(&val).unwrap_or_default();
                        (k, json)
                    })
                    .collect();
                EdgeSnapshot {
                    id: edge.id.0,
                    source: edge.source.0,
                    target: edge.target.0,
                    label: edge.label,
                    properties: props,
                }
            })
            .collect();

        let snapshot = GraphSnapshot {
            vertices,
            edges,
            schema: SchemaSnapshot {
                vertex_properties: graph
                    .vertex_property_defs()
                    .map(|def| PropertyDef {
                        name: def.name.clone(),
                        property_type: def.property_type.to_string(),
                        indexed: def.indexed,
                        unique: def.unique,
                    })
                    .collect(),
                edge_properties: graph
                    .edge_property_defs()
                    .map(|def| PropertyDef {
                        name: def.name.clone(),
                        property_type: def.property_type.to_string(),
                        indexed: def.indexed,
                        unique: def.unique,
                    })
                    .collect(),
            },
        };

        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        let tmp_path = self.data_dir.join("snapshot.json.tmp");
        let mut file = File::create(&tmp_path)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp_path, self.data_dir.join("snapshot.json"))?;

        self.wal.checkpoint()?;
        self.wal.compact()?;
        Ok(())
    }

    /// Load the latest snapshot from disk (if it exists).
    pub fn load_snapshot(&self) -> StorageResult<Option<GraphSnapshot>> {
        let path = self.data_dir.join("snapshot.json");
        if !path.exists() {
            return Ok(None);
        }
        let json = fs::read_to_string(path)?;
        let snapshot: GraphSnapshot =
            serde_json::from_str(&json).map_err(|e| StorageError::Serialization(e.to_string()))?;
        Ok(Some(snapshot))
    }

    /// Replay WAL entries since the last checkpoint. Returns entries to apply.
    pub fn recover(&self) -> StorageResult<Vec<WalEntry>> {
        let reader = WalReader::new(self.data_dir.join("graph.wal"));
        reader.read_since_last_checkpoint()
    }

    /// Rebuild a Graph from snapshot + WAL replay.
    pub fn load_graph(&self, vertex_capacity: usize, edge_capacity: usize) -> StorageResult<Graph> {
        let mut graph = Graph::new(vertex_capacity, edge_capacity);

        graph.register_vertex_property("name", PropertyType::String, true, false);
        graph.register_vertex_property("entity_type", PropertyType::String, true, false);
        graph.register_vertex_property("external_id", PropertyType::String, true, true);
        graph.register_vertex_property("tenant_id", PropertyType::String, true, false);
        graph.register_vertex_property("count", PropertyType::Int64, false, false);
        graph.register_vertex_property("score", PropertyType::Float64, false, false);
        graph.register_edge_property("weight", PropertyType::Float64, false, false);
        graph.register_edge_property("label_text", PropertyType::String, false, false);

        if let Some(snapshot) = self.load_snapshot()? {
            for def in &snapshot.schema.vertex_properties {
                graph.register_vertex_property(
                    &def.name,
                    parse_property_type(&def.property_type),
                    def.indexed,
                    def.unique,
                );
            }
            for def in &snapshot.schema.edge_properties {
                graph.register_edge_property(
                    &def.name,
                    parse_property_type(&def.property_type),
                    def.indexed,
                    def.unique,
                );
            }
            for vs in &snapshot.vertices {
                let vid = graph.add_vertex_with_id(vs.id, &vs.label);
                for (k, v) in &vs.properties {
                    let value: Value =
                        serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.clone()));
                    graph.set_vertex_property(vid, k, value);
                }
            }
            for es in &snapshot.edges {
                let eid = graph.add_edge_with_id(
                    es.id,
                    VertexId(es.source),
                    VertexId(es.target),
                    &es.label,
                );
                for (k, v) in &es.properties {
                    let value: Value =
                        serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.clone()));
                    graph.set_edge_property(eid, k, value);
                }
            }
        }

        let wal_entries = self.recover()?;
        for entry in wal_entries {
            match entry {
                WalEntry::AddVertex { id, label } => {
                    graph.add_vertex_with_id(id, &label);
                }
                WalEntry::SetVertexProperty {
                    vertex_id,
                    key,
                    value,
                } => {
                    graph.set_vertex_property(VertexId(vertex_id), &key, value);
                }
                WalEntry::AddEdge {
                    edge_id,
                    source,
                    target,
                    label,
                } => {
                    graph.add_edge_with_id(edge_id, VertexId(source), VertexId(target), &label);
                }
                WalEntry::SetEdgeProperty {
                    edge_id,
                    key,
                    value,
                } => {
                    graph.set_edge_property(EdgeId(edge_id), &key, value);
                }
                WalEntry::RemoveVertex { vertex_id } => {
                    graph.remove_vertex(VertexId(vertex_id));
                }
                WalEntry::RemoveEdge { edge_id } => {
                    graph.remove_edge(EdgeId(edge_id));
                }
                WalEntry::Checkpoint { .. } => {}
            }
        }

        graph.build();
        Ok(graph)
    }
}

fn parse_property_type(raw: &str) -> PropertyType {
    match raw.to_ascii_lowercase().as_str() {
        "bool" | "boolean" => PropertyType::Bool,
        "int64" | "int" | "integer" => PropertyType::Int64,
        "float64" | "float" | "double" => PropertyType::Float64,
        "bytes" | "blob" => PropertyType::Bytes,
        _ => PropertyType::String,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::types::Direction;
    use tempfile::TempDir;

    fn build_test_graph() -> Graph {
        let mut g = Graph::new(10, 10);
        g.register_vertex_property("name", PropertyType::String, true, false);
        g.register_vertex_property("count", PropertyType::Int64, false, false);
        g.register_vertex_property("score", PropertyType::Float64, false, false);
        g.register_edge_property("weight", PropertyType::Float64, false, false);
        g.register_edge_property("label_text", PropertyType::String, false, false);

        let apple = g.add_vertex("Entity");
        g.set_vertex_property(apple, "name", Value::String("Apple Inc.".into()));
        g.set_vertex_property(apple, "count", Value::Int64(42));
        g.set_vertex_property(apple, "score", Value::Float64(0.95));

        let revenue = g.add_vertex("Entity");
        g.set_vertex_property(revenue, "name", Value::String("Revenue".into()));

        let usa = g.add_vertex("Geography");
        g.set_vertex_property(usa, "name", Value::String("United States".into()));

        let e1 = g.add_edge(apple, revenue, "Discloses");
        g.set_edge_property(e1, "weight", Value::Float64(0.95));
        g.set_edge_property(e1, "label_text", Value::String("primary".into()));

        let _e2 = g.add_edge(apple, usa, "Operates_In");

        g.build();
        g
    }

    #[test]
    fn store_open_and_metadata() {
        let dir = TempDir::new().unwrap();
        let store = NexusStore::open(dir.path().join("testdb")).unwrap();

        assert_eq!(
            store.catalog().get_meta("engine").unwrap(),
            Some("domyn-nexus".to_string())
        );
        assert_eq!(
            store.catalog().get_meta("version").unwrap(),
            Some("0.1.0".to_string())
        );
    }

    #[test]
    fn store_wal_and_recovery() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.log_add_vertex(0, "Entity").unwrap();
            store
                .log_set_vertex_property(0, "name", &Value::String("Apple Inc.".into()))
                .unwrap();
            store.log_add_vertex(1, "Entity").unwrap();
            store.log_add_edge(0, 0, 1, "Discloses").unwrap();
            store.wal_mut().sync().unwrap();
        }

        {
            let store = NexusStore::open(&db_path).unwrap();
            let entries = store.recover().unwrap();
            assert_eq!(entries.len(), 4);
        }
    }

    #[test]
    fn store_snapshot_and_load() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            let graph = build_test_graph();
            store.save_snapshot(&graph).unwrap();
        }

        {
            let store = NexusStore::open(&db_path).unwrap();
            let snapshot = store.load_snapshot().unwrap().unwrap();
            assert_eq!(snapshot.vertices.len(), 3);
            assert_eq!(snapshot.vertices[0].label, "Entity");
        }
    }

    #[test]
    fn save_snapshot_compacts_wal() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.log_add_vertex(0, "Entity").unwrap();
            store.wal_mut().sync().unwrap();

            let graph = build_test_graph();
            store.save_snapshot(&graph).unwrap();
        }

        {
            let store = NexusStore::open(&db_path).unwrap();
            assert!(store.recover().unwrap().is_empty());
            let graph = store.load_graph(10, 10).unwrap();
            assert_eq!(graph.num_vertices(), 3);
            assert_eq!(
                graph.get_vertex_property(VertexId(0), "name").as_str(),
                Some("Apple Inc.")
            );
        }
    }

    #[test]
    fn snapshot_preserves_edges() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            let graph = build_test_graph();
            store.save_snapshot(&graph).unwrap();
        }

        {
            let store = NexusStore::open(&db_path).unwrap();
            let snapshot = store.load_snapshot().unwrap().unwrap();

            assert_eq!(snapshot.edges.len(), 2, "both edges must survive snapshot");

            let discloses: Vec<_> = snapshot
                .edges
                .iter()
                .filter(|e| e.label == "Discloses")
                .collect();
            assert_eq!(discloses.len(), 1);
            assert_eq!(discloses[0].source, 0);
            assert_eq!(discloses[0].target, 1);

            let operates_in: Vec<_> = snapshot
                .edges
                .iter()
                .filter(|e| e.label == "Operates_In")
                .collect();
            assert_eq!(operates_in.len(), 1);
            assert_eq!(operates_in[0].source, 0);
            assert_eq!(operates_in[0].target, 2);
        }
    }

    #[test]
    fn snapshot_preserves_property_types() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            let graph = build_test_graph();
            store.save_snapshot(&graph).unwrap();
        }

        {
            let store = NexusStore::open(&db_path).unwrap();
            let graph = store.load_graph(10, 10).unwrap();

            let name_val = graph.get_vertex_property(VertexId(0), "name");
            assert_eq!(
                name_val.as_str(),
                Some("Apple Inc."),
                "String property must round-trip"
            );

            let count_val = graph.get_vertex_property(VertexId(0), "count");
            assert_eq!(
                count_val.as_i64(),
                Some(42),
                "Int64 must not degrade to String"
            );

            let score_val = graph.get_vertex_property(VertexId(0), "score");
            assert_eq!(
                score_val.as_f64(),
                Some(0.95),
                "Float64 must not degrade to String"
            );
        }
    }

    #[test]
    fn loaded_graph_is_built_and_queryable() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            let graph = build_test_graph();
            store.save_snapshot(&graph).unwrap();
        }

        {
            let store = NexusStore::open(&db_path).unwrap();
            let graph = store.load_graph(10, 10).unwrap();

            assert!(graph.is_built(), "load_graph must call build()");

            let neighbors = graph.neighbors(VertexId(0), "Discloses", Direction::Outgoing);
            assert_eq!(neighbors.len(), 1);
            assert_eq!(neighbors[0], VertexId(1));

            let geo = graph.neighbors(VertexId(0), "Operates_In", Direction::Outgoing);
            assert_eq!(geo.len(), 1);
            assert_eq!(geo[0], VertexId(2));
        }
    }

    #[test]
    fn full_save_load_query_roundtrip() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            let graph = build_test_graph();
            store.save_snapshot(&graph).unwrap();
        }

        let store = NexusStore::open(&db_path).unwrap();
        let loaded = store.load_graph(10, 10).unwrap();

        assert_eq!(loaded.num_vertices(), 3);
        assert!(loaded.is_built());

        assert_eq!(loaded.vertex_label(VertexId(0)), Some("Entity"));
        assert_eq!(loaded.vertex_label(VertexId(2)), Some("Geography"));

        assert_eq!(
            loaded.get_vertex_property(VertexId(0), "name").as_str(),
            Some("Apple Inc.")
        );
        assert_eq!(
            loaded.get_vertex_property(VertexId(0), "count").as_i64(),
            Some(42)
        );
        assert_eq!(
            loaded.get_vertex_property(VertexId(0), "score").as_f64(),
            Some(0.95)
        );
        assert_eq!(
            loaded.get_vertex_property(VertexId(1), "name").as_str(),
            Some("Revenue")
        );
        assert_eq!(
            loaded.get_vertex_property(VertexId(2), "name").as_str(),
            Some("United States")
        );

        let discloses_out = loaded.neighbors(VertexId(0), "Discloses", Direction::Outgoing);
        assert_eq!(discloses_out, vec![VertexId(1)]);

        let discloses_in = loaded.neighbors(VertexId(1), "Discloses", Direction::Incoming);
        assert_eq!(discloses_in, vec![VertexId(0)]);

        let operates_in = loaded.neighbors(VertexId(0), "Operates_In", Direction::Outgoing);
        assert_eq!(operates_in, vec![VertexId(2)]);

        let fwd = loaded.forward_matrix("Discloses").unwrap();
        assert_eq!(fwd.degree(0), 1);
        assert_eq!(fwd.neighbors_of(0), &[1]);

        let edge_props = loaded.get_edge_properties(EdgeId(0));
        let weight = edge_props.iter().find(|(k, _)| k == "weight");
        assert!(weight.is_some(), "edge properties must survive roundtrip");
        assert_eq!(weight.unwrap().1.as_f64(), Some(0.95));
    }

    #[test]
    fn wal_recovery_after_simulated_crash() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        // Phase 1: Write data via WAL only (no snapshot — simulating crash)
        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.log_add_vertex(0, "Entity").unwrap();
            store
                .log_set_vertex_property(0, "name", &Value::String("Apple Inc.".into()))
                .unwrap();
            store.log_add_vertex(1, "Metric").unwrap();
            store
                .log_set_vertex_property(1, "name", &Value::String("Revenue".into()))
                .unwrap();
            store.log_add_edge(0, 0, 1, "DISCLOSES").unwrap();
            store.wal_mut().sync().unwrap();
        }

        // Phase 2: Re-open and recover from WAL alone
        {
            let store = NexusStore::open(&db_path).unwrap();
            let graph = store.load_graph(10, 10).unwrap();

            assert_eq!(graph.num_vertices(), 2);
            assert!(graph.is_built());

            assert_eq!(
                graph.get_vertex_property(VertexId(0), "name").as_str(),
                Some("Apple Inc."),
            );
            assert_eq!(
                graph.get_vertex_property(VertexId(1), "name").as_str(),
                Some("Revenue"),
            );

            let neighbors = graph.neighbors(VertexId(0), "DISCLOSES", Direction::Outgoing);
            assert_eq!(neighbors.len(), 1);
            assert_eq!(neighbors[0], VertexId(1));
        }
    }

    #[test]
    fn snapshot_plus_wal_recovery() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        // Phase 1: Create graph, snapshot it
        {
            let mut store = NexusStore::open(&db_path).unwrap();
            let mut graph = Graph::new(10, 10);
            graph.register_vertex_property("name", PropertyType::String, true, false);

            let v0 = graph.add_vertex("Entity");
            graph.set_vertex_property(v0, "name", Value::String("Alice".into()));
            let v1 = graph.add_vertex("Entity");
            graph.set_vertex_property(v1, "name", Value::String("Bob".into()));
            graph.add_edge(v0, v1, "KNOWS");
            graph.build();

            store.save_snapshot(&graph).unwrap();
        }

        // Phase 2: Add more via WAL after snapshot (no new snapshot — simulating crash)
        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.log_add_vertex(2, "Entity").unwrap();
            store
                .log_set_vertex_property(2, "name", &Value::String("Charlie".into()))
                .unwrap();
            store.log_add_edge(1, 1, 2, "KNOWS").unwrap();
            store.wal_mut().sync().unwrap();
        }

        // Phase 3: Recover — snapshot data + WAL replay
        {
            let store = NexusStore::open(&db_path).unwrap();
            let graph = store.load_graph(10, 10).unwrap();

            assert_eq!(graph.num_vertices(), 3);
            assert!(graph.is_built());

            assert_eq!(
                graph.get_vertex_property(VertexId(0), "name").as_str(),
                Some("Alice"),
            );
            assert_eq!(
                graph.get_vertex_property(VertexId(1), "name").as_str(),
                Some("Bob"),
            );
            assert_eq!(
                graph.get_vertex_property(VertexId(2), "name").as_str(),
                Some("Charlie"),
            );

            let neighbors_bob = graph.neighbors(VertexId(1), "KNOWS", Direction::Outgoing);
            assert!(
                neighbors_bob.contains(&VertexId(2)),
                "WAL-replayed edge must be queryable",
            );
        }
    }

    #[test]
    fn wal_survives_corrupted_entry() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");

        {
            let mut writer = WalWriter::open(&wal_path).unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 0,
                    label: "A".into(),
                })
                .unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 1,
                    label: "B".into(),
                })
                .unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 2,
                    label: "C".into(),
                })
                .unwrap();
            writer.sync().unwrap();
        }

        // Corrupt the middle entry: replace the second line with a valid-looking
        // CRC\tJSON format but with a wrong checksum so it triggers CRC mismatch
        // (which the WAL reader skips rather than erroring).
        {
            let content = std::fs::read_to_string(&wal_path).unwrap();
            let lines: Vec<&str> = content.lines().collect();
            assert_eq!(lines.len(), 3);

            let corrupted = format!("{}\n99999\t{{\"garbage\":true}}\n{}\n", lines[0], lines[2],);
            std::fs::write(&wal_path, corrupted).unwrap();
        }

        let reader = WalReader::new(&wal_path);
        let entries = reader.read_all().unwrap();
        assert_eq!(entries.len(), 2, "corrupted entry must be skipped");

        match &entries[0] {
            WalEntry::AddVertex { id, label } => {
                assert_eq!(*id, 0);
                assert_eq!(label, "A");
            }
            other => panic!("expected AddVertex(A), got {other:?}"),
        }
        match &entries[1] {
            WalEntry::AddVertex { id, label } => {
                assert_eq!(*id, 2);
                assert_eq!(label, "C");
            }
            other => panic!("expected AddVertex(C), got {other:?}"),
        }
    }

    #[test]
    fn large_graph_snapshot_roundtrip() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            let mut graph = Graph::new(1000, 2000);
            graph.register_vertex_property("name", PropertyType::String, true, false);
            graph.register_vertex_property("score", PropertyType::Float64, false, false);
            graph.register_edge_property("weight", PropertyType::Float64, false, false);

            for i in 0..500u64 {
                let v = graph.add_vertex("Node");
                graph.set_vertex_property(v, "name", Value::String(format!("node_{i}")));
                graph.set_vertex_property(v, "score", Value::Float64(i as f64 * 1.1));
            }
            for i in 0..999u64 {
                let eid = graph.add_edge(VertexId(i % 500), VertexId((i + 1) % 500), "LINK");
                graph.set_edge_property(eid, "weight", Value::Float64(i as f64 * 0.5));
            }
            graph.build();

            store.save_snapshot(&graph).unwrap();
        }

        let store2 = NexusStore::open(&db_path).unwrap();
        let loaded = store2.load_graph(1000, 2000).unwrap();

        assert_eq!(loaded.num_vertices(), 500);
        assert_eq!(loaded.num_edges(), 999);
        assert!(loaded.is_built());

        assert_eq!(
            loaded.get_vertex_property(VertexId(42), "name").as_str(),
            Some("node_42"),
        );
        let expected_score = 42.0 * 1.1;
        assert_eq!(
            loaded.get_vertex_property(VertexId(42), "score").as_f64(),
            Some(expected_score),
        );

        let neighbors = loaded.neighbors(VertexId(0), "LINK", Direction::Outgoing);
        assert!(
            !neighbors.is_empty(),
            "vertex 0 must have outgoing LINK edges"
        );
    }

    #[test]
    fn snapshot_preserves_post_build_delta_edges() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            let mut graph = Graph::new(4, 4);
            graph.register_vertex_property("name", PropertyType::String, true, false);

            let a = graph.add_vertex("Entity");
            let b = graph.add_vertex("Entity");
            let c = graph.add_vertex("Entity");
            graph.add_edge(a, b, "KNOWS");
            graph.build();
            graph.add_edge(b, c, "KNOWS");

            store.save_snapshot(&graph).unwrap();
        }

        let store = NexusStore::open(&db_path).unwrap();
        let loaded = store.load_graph(4, 4).unwrap();
        let neighbors = loaded.neighbors(VertexId(1), "KNOWS", Direction::Outgoing);

        assert_eq!(neighbors, vec![VertexId(2)]);
        assert_eq!(loaded.num_edges(), 2);
    }

    #[test]
    fn wal_only_recovery_preserves_property_types() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.log_add_vertex(0, "Entity").unwrap();
            store
                .log_set_vertex_property(0, "name", &Value::String("Apple".into()))
                .unwrap();
            store
                .log_set_vertex_property(0, "count", &Value::Int64(42))
                .unwrap();
            store
                .log_set_vertex_property(0, "score", &Value::Float64(0.95))
                .unwrap();
            store.wal_mut().sync().unwrap();
        }
        {
            let store = NexusStore::open(&db_path).unwrap();
            let graph = store.load_graph(10, 10).unwrap();
            assert_eq!(
                graph.get_vertex_property(VertexId(0), "count").as_i64(),
                Some(42)
            );
            assert_eq!(
                graph.get_vertex_property(VertexId(0), "score").as_f64(),
                Some(0.95)
            );
        }
    }

    #[test]
    fn wal_replay_preserves_vertex_and_edge_ids() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.log_add_vertex(0, "A").unwrap();
            store.log_add_vertex(1, "B").unwrap();
            store.log_add_edge(42, 0, 1, "REL").unwrap();
            store
                .log_set_edge_property(42, "weight", &Value::Float64(1.5))
                .unwrap();
            store.wal_mut().sync().unwrap();
        }
        {
            let store = NexusStore::open(&db_path).unwrap();
            let graph = store.load_graph(10, 10).unwrap();
            let props = graph.get_edge_properties(EdgeId(42));
            let w = props.iter().find(|(k, _)| k == "weight");
            assert!(w.is_some());
            assert_eq!(w.unwrap().1.as_f64(), Some(1.5));
        }
    }

    #[test]
    fn wal_replay_applies_delete_tombstones() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.log_add_vertex(0, "Entity").unwrap();
            store.log_add_vertex(1, "Entity").unwrap();
            store.log_add_edge(0, 0, 1, "KNOWS").unwrap();
            store.log_remove_edge(0).unwrap();
            store.wal_mut().sync().unwrap();
        }
        {
            let store = NexusStore::open(&db_path).unwrap();
            let graph = store.load_graph(10, 10).unwrap();
            assert!(
                graph
                    .neighbors(VertexId(0), "KNOWS", Direction::Outgoing)
                    .is_empty()
            );
            assert!(graph.edge_records().is_empty());
        }
    }
}
