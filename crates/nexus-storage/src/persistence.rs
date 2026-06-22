//! Graph persistence: save/load graph state to/from disk.
//!
//! Combines WAL (for crash recovery) with redb catalog (for schema)
//! and JSON snapshots (for full graph state).

use crate::catalog::Catalog;
use crate::error::{StorageError, StorageResult};
use crate::wal::{WalEntry, WalOp, WalOptions, WalReader, WalWriter};
use nexus_core::graph::Graph;
use nexus_core::properties::PropertyType;
use nexus_core::types::{EdgeId, Value, VertexId};
use nexus_index::vector::VectorIndex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

#[derive(Debug, Serialize, Deserialize)]
pub struct BackupManifest {
    pub engine: String,
    pub version: String,
    pub created_unix_nanos: u128,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentRecord {
    pub collection: String,
    pub key: String,
    pub document: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentSearchHit {
    pub record: DocumentRecord,
    pub score: f64,
    pub snippet: Option<String>,
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentIndexKind {
    #[default]
    Scalar,
    FullText,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DocumentIndexDef {
    pub collection: String,
    pub path: String,
    #[serde(default)]
    pub kind: DocumentIndexKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DocumentIndexQuery {
    Exact(serde_json::Value),
    Prefix(String),
    FullText(String),
    FullTextAdvanced(DocumentFullTextQuery),
    Range {
        gte: Option<serde_json::Value>,
        lte: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentFullTextQuery {
    pub text: String,
    pub phrase: bool,
    pub fuzzy_distance: Option<u8>,
    pub stem: bool,
    pub ranking: DocumentFullTextRanking,
    pub include_snippets: bool,
    pub include_explanations: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentFullTextRanking {
    MatchCount,
    TfIdf,
    #[default]
    Bm25,
}

impl DocumentFullTextQuery {
    pub fn token(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            phrase: false,
            fuzzy_distance: None,
            stem: false,
            ranking: DocumentFullTextRanking::Bm25,
            include_snippets: false,
            include_explanations: false,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DocumentIndexCatalog {
    indexes: Vec<DocumentIndexDef>,
}

#[derive(Debug, Default)]
struct DocumentIndexes {
    defs: Vec<DocumentIndexDef>,
    entries: BTreeMap<(String, String, DocumentIndexKind, String), BTreeSet<String>>,
}

#[derive(Debug, Default)]
struct FullTextScore {
    points: f64,
    matched_terms: BTreeSet<String>,
    token_pairs: Vec<(String, String, f64)>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreMetrics {
    pub wal_sequence: u64,
    pub wal_active_bytes: u64,
    pub wal_live_segment_count: usize,
    pub wal_live_segment_bytes: u64,
    pub wal_archived_segment_count: usize,
    pub wal_archived_segment_bytes: u64,
    pub snapshot_bytes: u64,
    pub snapshot_modified_unix_seconds: u64,
    pub snapshot_archive_count: usize,
    pub snapshot_archive_bytes: u64,
    pub vector_snapshot_count: usize,
    pub vector_snapshot_bytes: u64,
    pub document_count: usize,
    pub document_bytes: u64,
    pub document_index_count: usize,
    pub document_index_entries: usize,
    pub catalog_bytes: u64,
}

impl StoreMetrics {
    pub fn wal_recoverable_bytes(&self) -> u64 {
        self.wal_active_bytes + self.wal_live_segment_bytes
    }
}

/// Database directory layout:
/// ```text
/// data_dir/
///   catalog.redb    -- schema, metadata (redb)
///   graph.wal       -- write-ahead log
///   graph.wal.seg.* -- rotated live WAL segments
///   wal-archive/    -- retained compacted WAL segments, ignored by recovery
///   snapshot.json   -- latest full graph snapshot
///   vectors/        -- named vector index snapshots
/// ```
pub struct NexusStore {
    data_dir: PathBuf,
    catalog: Catalog,
    wal: WalWriter,
    snapshot_retention: usize,
    document_indexes: DocumentIndexes,
}

#[derive(Debug, Clone, Default)]
pub struct StoreOptions {
    pub wal: WalOptions,
    pub snapshot_retention: usize,
}

impl NexusStore {
    /// Open or create a Nexus database at the given directory.
    pub fn open(data_dir: impl AsRef<Path>) -> StorageResult<Self> {
        Self::open_with_options(data_dir, StoreOptions::default())
    }

    /// Open or create a Nexus database with explicit WAL lifecycle options.
    pub fn open_with_wal_options(
        data_dir: impl AsRef<Path>,
        wal_options: WalOptions,
    ) -> StorageResult<Self> {
        Self::open_with_options(
            data_dir,
            StoreOptions {
                wal: wal_options,
                ..StoreOptions::default()
            },
        )
    }

    /// Open or create a Nexus database with explicit persistence lifecycle options.
    pub fn open_with_options(
        data_dir: impl AsRef<Path>,
        options: StoreOptions,
    ) -> StorageResult<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(&data_dir)?;
        cleanup_incomplete_snapshot_tmp(&data_dir)?;

        let catalog = Catalog::open(data_dir.join("catalog.redb"))?;
        let wal = WalWriter::open_with_options(data_dir.join("graph.wal"), options.wal)?;

        catalog.set_meta("engine", "domyn-nexus")?;
        catalog.set_meta("version", "0.1.0")?;

        let document_indexes = DocumentIndexes {
            defs: load_document_index_defs(&data_dir)?,
            entries: BTreeMap::new(),
        };
        let mut store = Self {
            data_dir,
            catalog,
            wal,
            snapshot_retention: options.snapshot_retention,
            document_indexes,
        };
        store.recover_cross_model_state()?;
        store.rebuild_document_indexes()?;
        Ok(store)
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn wal_mut(&mut self) -> &mut WalWriter {
        &mut self.wal
    }

    pub fn metrics(&self) -> StorageResult<StoreMetrics> {
        let wal_path = self.data_dir.join("graph.wal");
        let wal_segments = paths_with_prefix(&self.data_dir, "graph.wal.seg.")?;
        let wal_archive_dir = self.data_dir.join("wal-archive");
        let wal_archived_segments = paths_with_prefix(&wal_archive_dir, "graph.wal.seg.")?;
        let snapshot_archive_dir = self.snapshot_archive_dir();
        let snapshot_archives = snapshot_archive_paths(&snapshot_archive_dir)?;
        let vector_snapshots = vector_snapshot_paths(&self.vectors_dir())?;
        let document_paths = document_backup_paths(&self.documents_dir())?;

        Ok(StoreMetrics {
            wal_sequence: self.wal.sequence(),
            wal_active_bytes: file_len(&wal_path)?,
            wal_live_segment_count: wal_segments.len(),
            wal_live_segment_bytes: paths_len(&wal_segments)?,
            wal_archived_segment_count: wal_archived_segments.len(),
            wal_archived_segment_bytes: paths_len(&wal_archived_segments)?,
            snapshot_bytes: file_len(self.data_dir.join("snapshot.json"))?,
            snapshot_modified_unix_seconds: file_modified_unix_seconds(
                self.data_dir.join("snapshot.json"),
            )?,
            snapshot_archive_count: snapshot_archives.len(),
            snapshot_archive_bytes: paths_len(&snapshot_archives)?,
            vector_snapshot_count: vector_snapshots.len(),
            vector_snapshot_bytes: paths_len(&vector_snapshots)?,
            document_count: document_paths.len(),
            document_bytes: paths_len(
                &document_paths
                    .iter()
                    .map(|(path, _)| path.clone())
                    .collect::<Vec<_>>(),
            )?,
            document_index_count: self.document_indexes.defs.len(),
            document_index_entries: self
                .document_indexes
                .entries
                .values()
                .map(BTreeSet::len)
                .sum(),
            catalog_bytes: file_len(self.data_dir.join("catalog.redb"))?,
        })
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

    /// Write a WAL entry for a vertex label replacement.
    pub fn log_set_vertex_label(&mut self, vertex_id: u64, label: &str) -> StorageResult<u64> {
        self.wal.append(&WalEntry::SetVertexLabel {
            vertex_id,
            label: label.to_string(),
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

    /// Append an atomic cross-model commit record.
    pub fn log_commit(&mut self, tx_id: u64, ops: Vec<WalOp>) -> StorageResult<u64> {
        self.wal.append(&WalEntry::Commit { tx_id, ops })
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
        self.archive_current_snapshot()?;
        fs::rename(&tmp_path, self.data_dir.join("snapshot.json"))?;
        sync_dir(&self.data_dir)?;
        self.prune_snapshot_archives()?;

        self.wal.checkpoint()?;
        self.wal.compact()?;
        Ok(())
    }

    /// Persist a named vector index alongside graph storage.
    ///
    /// The vector index handles active-only atomic snapshots internally; this
    /// method gives the store ownership of where those files live and ensures
    /// the vector directory is synced after the rename.
    pub fn save_vector_index(&self, name: &str, index: &VectorIndex) -> StorageResult<()> {
        let path = self.vector_snapshot_path(name)?;
        index
            .save_to_path(&path)
            .map_err(|err| StorageError::Serialization(err.to_string()))?;
        sync_dir(&self.vectors_dir())?;
        Ok(())
    }

    /// Load a named vector index snapshot if one exists.
    pub fn load_vector_index(&self, name: &str) -> StorageResult<Option<VectorIndex>> {
        let path = self.vector_snapshot_path(name)?;
        if !path.exists() {
            return Ok(None);
        }
        VectorIndex::load_from_path(path)
            .map(Some)
            .map_err(|err| StorageError::Serialization(err.to_string()))
    }

    /// List persisted vector index snapshots by index name.
    pub fn list_vector_indexes(&self) -> StorageResult<Vec<String>> {
        vector_snapshot_names(&self.vectors_dir())
    }

    /// Persist a JSON document in a named collection.
    pub fn upsert_document(
        &mut self,
        collection: &str,
        key: &str,
        document: &serde_json::Value,
    ) -> StorageResult<()> {
        validate_document_name("collection", collection)?;
        validate_document_name("document key", key)?;
        self.log_commit(
            self.wal.sequence() + 1,
            vec![WalOp::UpsertDocument {
                collection: collection.to_string(),
                key: key.to_string(),
                document: document.clone(),
            }],
        )?;
        self.wal.sync()?;
        let old = self.load_document(collection, key)?;
        self.write_document_file(collection, key, document)?;
        self.remove_document_from_indexes(collection, key, old.as_ref());
        self.add_document_to_indexes(collection, key, document);
        Ok(())
    }

    fn write_document_file(
        &self,
        collection: &str,
        key: &str,
        document: &serde_json::Value,
    ) -> StorageResult<()> {
        let path = self.document_path(collection, key)?;
        let Some(parent) = path.parent() else {
            return Err(StorageError::Serialization(format!(
                "invalid document path for collection {collection:?} and key {key:?}"
            )));
        };
        let parent = parent.to_path_buf();
        fs::create_dir_all(&parent)?;
        let tmp_path = parent.join(format!("{key}.json.tmp"));
        let json = serde_json::to_vec_pretty(document)
            .map_err(|err| StorageError::Serialization(err.to_string()))?;
        let mut file = File::create(&tmp_path)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(tmp_path, path)?;
        sync_dir(&parent)?;
        Ok(())
    }

    /// Load a JSON document by collection and key.
    pub fn load_document(
        &self,
        collection: &str,
        key: &str,
    ) -> StorageResult<Option<serde_json::Value>> {
        let path = self.document_path(collection, key)?;
        if !path.exists() {
            return Ok(None);
        }
        serde_json::from_slice(&fs::read(path)?)
            .map(Some)
            .map_err(|err| StorageError::Serialization(err.to_string()))
    }

    /// Delete a JSON document by collection and key.
    pub fn delete_document(&mut self, collection: &str, key: &str) -> StorageResult<bool> {
        validate_document_name("collection", collection)?;
        validate_document_name("document key", key)?;
        let existed = self.document_path(collection, key)?.exists();
        self.log_commit(
            self.wal.sequence() + 1,
            vec![WalOp::DeleteDocument {
                collection: collection.to_string(),
                key: key.to_string(),
            }],
        )?;
        self.wal.sync()?;
        let old = self.load_document(collection, key)?;
        self.delete_document_file(collection, key)?;
        self.remove_document_from_indexes(collection, key, old.as_ref());
        Ok(existed)
    }

    fn delete_document_file(&self, collection: &str, key: &str) -> StorageResult<bool> {
        let path = self.document_path(collection, key)?;
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_file(&path)?;
        if let Some(parent) = path.parent() {
            sync_dir(parent)?;
        }
        Ok(true)
    }

    /// List document collection names.
    pub fn list_document_collections(&self) -> StorageResult<Vec<String>> {
        document_collection_names(&self.documents_dir())
    }

    /// List documents in one collection, sorted by key.
    pub fn list_documents(
        &self,
        collection: &str,
        limit: Option<usize>,
    ) -> StorageResult<Vec<DocumentRecord>> {
        validate_document_name("collection", collection)?;
        let mut records = Vec::new();
        for (path, key) in document_paths_for_collection(&self.documents_dir(), collection)? {
            let document: serde_json::Value = serde_json::from_slice(&fs::read(path)?)
                .map_err(|err| StorageError::Serialization(err.to_string()))?;
            records.push(DocumentRecord {
                collection: collection.to_string(),
                key,
                document,
            });
            if limit.is_some_and(|limit| records.len() >= limit) {
                break;
            }
        }
        Ok(records)
    }

    /// Create and rebuild a secondary index on a JSON document path.
    pub fn create_document_index(
        &mut self,
        collection: &str,
        path: &str,
    ) -> StorageResult<DocumentIndexDef> {
        self.create_document_index_with_kind(collection, path, DocumentIndexKind::Scalar)
    }

    /// Create and rebuild a secondary index on a JSON document path.
    pub fn create_document_index_with_kind(
        &mut self,
        collection: &str,
        path: &str,
        kind: DocumentIndexKind,
    ) -> StorageResult<DocumentIndexDef> {
        validate_document_name("collection", collection)?;
        validate_document_index_path(path)?;
        let def = DocumentIndexDef {
            collection: collection.to_string(),
            path: path.to_string(),
            kind,
        };
        if !self.document_indexes.defs.contains(&def) {
            self.document_indexes.defs.push(def.clone());
            self.document_indexes.defs.sort();
            self.save_document_index_defs()?;
        }
        self.rebuild_document_indexes()?;
        Ok(def)
    }

    /// List document secondary indexes, optionally scoped to one collection.
    pub fn list_document_indexes(
        &self,
        collection: Option<&str>,
    ) -> StorageResult<Vec<DocumentIndexDef>> {
        if let Some(collection) = collection {
            validate_document_name("collection", collection)?;
        }
        Ok(self
            .document_indexes
            .defs
            .iter()
            .filter(|def| collection.is_none_or(|collection| def.collection == collection))
            .cloned()
            .collect())
    }

    /// Query a bounded document index by exact scalar value.
    pub fn query_documents_by_index(
        &self,
        collection: &str,
        path: &str,
        value: &serde_json::Value,
        limit: Option<usize>,
    ) -> StorageResult<Vec<DocumentRecord>> {
        self.query_documents_by_index_with(
            collection,
            path,
            DocumentIndexQuery::Exact(value.clone()),
            limit,
        )
    }

    /// Query a bounded document index using exact, prefix, or range semantics.
    ///
    /// Prefix and range queries scan distinct scalar index values, not every
    /// document file. This keeps Document Collections v0 lightweight while
    /// giving product users indexed lookup modes beyond equality.
    pub fn query_documents_by_index_with(
        &self,
        collection: &str,
        path: &str,
        query: DocumentIndexQuery,
        limit: Option<usize>,
    ) -> StorageResult<Vec<DocumentRecord>> {
        Ok(self
            .query_document_hits_by_index_with(collection, path, query, limit)?
            .into_iter()
            .map(|hit| hit.record)
            .collect())
    }

    pub fn query_document_hits_by_index_with(
        &self,
        collection: &str,
        path: &str,
        query: DocumentIndexQuery,
        limit: Option<usize>,
    ) -> StorageResult<Vec<DocumentSearchHit>> {
        validate_document_name("collection", collection)?;
        validate_document_index_path(path)?;
        let kind = match &query {
            DocumentIndexQuery::Exact(_)
            | DocumentIndexQuery::Prefix(_)
            | DocumentIndexQuery::Range { .. } => DocumentIndexKind::Scalar,
            DocumentIndexQuery::FullText(_) | DocumentIndexQuery::FullTextAdvanced(_) => {
                DocumentIndexKind::FullText
            }
        };
        let def = DocumentIndexDef {
            collection: collection.to_string(),
            path: path.to_string(),
            kind,
        };
        if !self.document_indexes.defs.contains(&def) {
            return Err(StorageError::Serialization(format!(
                "document {kind:?} index does not exist for collection {collection:?} path {path:?}"
            )));
        }

        let mut records = Vec::new();
        let limit = limit.unwrap_or(100);
        match query {
            DocumentIndexQuery::Exact(value) => {
                let Some(index_key) = document_index_value_key(&value) else {
                    return Ok(Vec::new());
                };
                let keys = self
                    .document_indexes
                    .entries
                    .get(&(
                        collection.to_string(),
                        path.to_string(),
                        DocumentIndexKind::Scalar,
                        index_key,
                    ))
                    .cloned()
                    .unwrap_or_default();
                self.push_document_records(collection, keys, limit, &mut records)?;
            }
            DocumentIndexQuery::Prefix(prefix) => {
                for ((entry_collection, entry_path, entry_kind, index_key), keys) in
                    &self.document_indexes.entries
                {
                    if entry_collection != collection
                        || entry_path != path
                        || *entry_kind != DocumentIndexKind::Scalar
                    {
                        continue;
                    }
                    let Some(serde_json::Value::String(value)) =
                        document_index_key_value(index_key)
                    else {
                        continue;
                    };
                    if value.starts_with(&prefix) {
                        self.push_document_records(collection, keys.clone(), limit, &mut records)?;
                        if records.len() >= limit {
                            break;
                        }
                    }
                }
            }
            DocumentIndexQuery::FullText(text) => {
                let query = DocumentFullTextQuery::token(text);
                return self.query_document_full_text_hits(collection, path, &query, limit);
            }
            DocumentIndexQuery::FullTextAdvanced(query) => {
                return self.query_document_full_text_hits(collection, path, &query, limit);
            }
            DocumentIndexQuery::Range { gte, lte } => {
                if gte.is_none() && lte.is_none() {
                    return Err(StorageError::Serialization(
                        "document range query requires gte and/or lte".into(),
                    ));
                }
                for ((entry_collection, entry_path, entry_kind, index_key), keys) in
                    &self.document_indexes.entries
                {
                    if entry_collection != collection
                        || entry_path != path
                        || *entry_kind != DocumentIndexKind::Scalar
                    {
                        continue;
                    }
                    let Some(value) = document_index_key_value(index_key) else {
                        continue;
                    };
                    if document_scalar_in_range(&value, gte.as_ref(), lte.as_ref()) {
                        self.push_document_records(collection, keys.clone(), limit, &mut records)?;
                        if records.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }
        Ok(records
            .into_iter()
            .map(|record| DocumentSearchHit {
                record,
                score: 1.0,
                snippet: None,
                explanation: None,
            })
            .collect())
    }

    fn query_document_full_text_hits(
        &self,
        collection: &str,
        path: &str,
        query: &DocumentFullTextQuery,
        limit: usize,
    ) -> StorageResult<Vec<DocumentSearchHit>> {
        let query_tokens = document_fulltext_query_tokens_with_options(&query.text, query.stem);
        if query_tokens.is_empty() {
            return Ok(Vec::new());
        }
        let corpus_doc_count = self.document_full_text_doc_count(collection, path).max(1);
        let avg_doc_len = self.document_full_text_average_len(collection, path, query.stem)?;
        let mut scores = BTreeMap::<String, FullTextScore>::new();
        let fuzzy_distance = query.fuzzy_distance.unwrap_or(0).min(3);
        for ((entry_collection, entry_path, entry_kind, index_token), keys) in
            &self.document_indexes.entries
        {
            if entry_collection != collection
                || entry_path != path
                || *entry_kind != DocumentIndexKind::FullText
            {
                continue;
            }
            let indexed_cmp = if query.stem {
                document_stem(index_token)
            } else {
                index_token.clone()
            };
            for query_token in &query_tokens {
                let matched = indexed_cmp == *query_token;
                let fuzzy_matched = !matched
                    && fuzzy_distance > 0
                    && levenshtein_at_most(&indexed_cmp, query_token, fuzzy_distance as usize);
                if !matched && !fuzzy_matched {
                    continue;
                }
                let points = if fuzzy_matched { 0.5 } else { 1.0 };
                for key in keys {
                    let score = scores.entry(key.clone()).or_default();
                    score.points += points;
                    score
                        .matched_terms
                        .insert(format!("{query_token}->{index_token}"));
                    score
                        .token_pairs
                        .push((query_token.clone(), index_token.clone(), points));
                }
            }
        }

        let mut hits = Vec::new();
        for (key, score) in scores {
            let Some(document) = self.load_document(collection, &key)? else {
                continue;
            };
            let Some(value) = document_path_value(&document, path) else {
                continue;
            };
            if query.phrase && !document_fulltext_phrase_matches(value, &query.text) {
                continue;
            }
            let ranked_score = self.document_full_text_rank_score(
                collection,
                path,
                value,
                query,
                &score,
                corpus_doc_count,
                avg_doc_len,
            );
            let snippet = query
                .include_snippets
                .then(|| document_fulltext_snippet(value, &query.text, &query_tokens))
                .flatten();
            let explanation = query.include_explanations.then(|| {
                let mut terms: Vec<_> = score.matched_terms.iter().cloned().collect();
                terms.sort();
                format!(
                    "mode={}, ranking={:?}, matched_terms={}, score={:.3}",
                    if query.phrase {
                        "phrase+token"
                    } else {
                        "token"
                    },
                    query.ranking,
                    terms.join("|"),
                    ranked_score
                )
            });
            hits.push(DocumentSearchHit {
                record: DocumentRecord {
                    collection: collection.to_string(),
                    key,
                    document,
                },
                score: ranked_score,
                snippet,
                explanation,
            });
        }
        hits.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.record.key.cmp(&right.record.key))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    fn document_full_text_rank_score(
        &self,
        collection: &str,
        path: &str,
        value: &serde_json::Value,
        query: &DocumentFullTextQuery,
        score: &FullTextScore,
        corpus_doc_count: usize,
        avg_doc_len: f64,
    ) -> f64 {
        match query.ranking {
            DocumentFullTextRanking::MatchCount => score.points,
            DocumentFullTextRanking::TfIdf => {
                let doc_terms = document_fulltext_term_counts(value, query.stem);
                let mut total = 0.0;
                for (_, index_token, fuzzy_weight) in &score.token_pairs {
                    let term = if query.stem {
                        document_stem(index_token)
                    } else {
                        index_token.clone()
                    };
                    let tf = *doc_terms.get(&term).unwrap_or(&0) as f64;
                    if tf == 0.0 {
                        continue;
                    }
                    let df = self.document_full_text_doc_frequency(collection, path, index_token);
                    let idf =
                        (((corpus_doc_count as f64 + 1.0) / (df as f64 + 1.0)).ln() + 1.0).max(0.0);
                    total += fuzzy_weight * tf * idf;
                }
                total
            }
            DocumentFullTextRanking::Bm25 => {
                let doc_terms = document_fulltext_term_counts(value, query.stem);
                let doc_len = doc_terms.values().copied().sum::<usize>().max(1) as f64;
                let avg_doc_len = avg_doc_len.max(1.0);
                let k1 = 1.2;
                let b = 0.75;
                let mut total = 0.0;
                for (_, index_token, fuzzy_weight) in &score.token_pairs {
                    let term = if query.stem {
                        document_stem(index_token)
                    } else {
                        index_token.clone()
                    };
                    let tf = *doc_terms.get(&term).unwrap_or(&0) as f64;
                    if tf == 0.0 {
                        continue;
                    }
                    let df = self.document_full_text_doc_frequency(collection, path, index_token);
                    let idf = (1.0
                        + (corpus_doc_count as f64 - df as f64 + 0.5) / (df as f64 + 0.5))
                        .ln()
                        .max(0.0);
                    let denom = tf + k1 * (1.0 - b + b * doc_len / avg_doc_len);
                    total += fuzzy_weight * idf * (tf * (k1 + 1.0)) / denom;
                }
                total
            }
        }
    }

    fn document_full_text_doc_count(&self, collection: &str, path: &str) -> usize {
        let mut keys = BTreeSet::new();
        for ((entry_collection, entry_path, entry_kind, _), doc_keys) in
            &self.document_indexes.entries
        {
            if entry_collection == collection
                && entry_path == path
                && *entry_kind == DocumentIndexKind::FullText
            {
                keys.extend(doc_keys.iter().cloned());
            }
        }
        keys.len()
    }

    fn document_full_text_doc_frequency(&self, collection: &str, path: &str, token: &str) -> usize {
        self.document_indexes
            .entries
            .get(&(
                collection.to_string(),
                path.to_string(),
                DocumentIndexKind::FullText,
                token.to_string(),
            ))
            .map(BTreeSet::len)
            .unwrap_or(0)
    }

    fn document_full_text_average_len(
        &self,
        collection: &str,
        path: &str,
        stem: bool,
    ) -> StorageResult<f64> {
        let mut keys = BTreeSet::new();
        for ((entry_collection, entry_path, entry_kind, _), doc_keys) in
            &self.document_indexes.entries
        {
            if entry_collection == collection
                && entry_path == path
                && *entry_kind == DocumentIndexKind::FullText
            {
                keys.extend(doc_keys.iter().cloned());
            }
        }
        let mut total_len = 0usize;
        let mut total_docs = 0usize;
        for key in keys {
            let Some(document) = self.load_document(collection, &key)? else {
                continue;
            };
            let Some(value) = document_path_value(&document, path) else {
                continue;
            };
            let len = document_fulltext_term_counts(value, stem)
                .values()
                .copied()
                .sum::<usize>();
            total_len += len.max(1);
            total_docs += 1;
        }
        if total_docs == 0 {
            Ok(1.0)
        } else {
            Ok(total_len as f64 / total_docs as f64)
        }
    }

    fn push_document_records(
        &self,
        collection: &str,
        keys: impl IntoIterator<Item = String>,
        limit: usize,
        records: &mut Vec<DocumentRecord>,
    ) -> StorageResult<()> {
        for key in keys {
            if records.len() >= limit {
                break;
            }
            if let Some(document) = self.load_document(collection, &key)? {
                records.push(DocumentRecord {
                    collection: collection.to_string(),
                    key,
                    document,
                });
            }
        }
        Ok(())
    }

    /// Apply document operations from an already-fsynced commit record.
    ///
    /// This path intentionally does not append WAL. It is used both by
    /// cross-model commits after the commit record is durable and by recovery
    /// replay on startup.
    pub fn apply_committed_document_ops(&mut self, ops: &[WalOp]) -> StorageResult<()> {
        for op in ops {
            match op {
                WalOp::UpsertDocument {
                    collection,
                    key,
                    document,
                } => {
                    validate_document_name("collection", collection)?;
                    validate_document_name("document key", key)?;
                    let old = self.load_document(collection, key)?;
                    self.write_document_file(collection, key, document)?;
                    self.remove_document_from_indexes(collection, key, old.as_ref());
                    self.add_document_to_indexes(collection, key, document);
                }
                WalOp::DeleteDocument { collection, key } => {
                    validate_document_name("collection", collection)?;
                    validate_document_name("document key", key)?;
                    let old = self.load_document(collection, key)?;
                    self.delete_document_file(collection, key)?;
                    self.remove_document_from_indexes(collection, key, old.as_ref());
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Apply vector index operations from an already-fsynced commit record.
    ///
    /// The named vector index must already exist as a persisted snapshot. This
    /// keeps index creation as an explicit setup step and makes recovery
    /// deterministic after a cross-model commit.
    pub fn apply_committed_vector_ops(&mut self, ops: &[WalOp]) -> StorageResult<()> {
        for op in ops {
            match op {
                WalOp::UpsertVector {
                    index,
                    vertex_id,
                    embedding,
                } => {
                    validate_vector_index_name(index)?;
                    let mut vector_index = self.load_vector_index(index)?.ok_or_else(|| {
                        StorageError::Serialization(format!(
                            "vector index not found during WAL replay: {index}"
                        ))
                    })?;
                    if vector_index.dimension() != embedding.len() {
                        return Err(StorageError::Serialization(format!(
                            "embedding dimension mismatch for vector index {index:?}: expected {}, got {}",
                            vector_index.dimension(),
                            embedding.len()
                        )));
                    }
                    vector_index.update(VertexId(*vertex_id), embedding.clone());
                    self.save_vector_index(index, &vector_index)?;
                }
                WalOp::RemoveVector { index, vertex_id } => {
                    validate_vector_index_name(index)?;
                    let mut vector_index = self.load_vector_index(index)?.ok_or_else(|| {
                        StorageError::Serialization(format!(
                            "vector index not found during WAL replay: {index}"
                        ))
                    })?;
                    if vector_index.remove(VertexId(*vertex_id)) {
                        self.save_vector_index(index, &vector_index)?;
                    }
                }
                _ => {}
            }
        }
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

    /// Create a hot backup by syncing WAL state and copying the durable files
    /// needed for recovery into `backup_dir`.
    pub fn backup_to(&mut self, backup_dir: impl AsRef<Path>) -> StorageResult<BackupManifest> {
        self.wal.sync()?;
        let backup_dir = backup_dir.as_ref();
        fs::create_dir_all(backup_dir)?;

        let mut files = Vec::new();
        for (source, relative) in backup_data_paths(&self.data_dir)? {
            let destination = backup_dir.join(&relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &destination)?;
            files.push(relative);
        }
        files.sort();

        let manifest = BackupManifest {
            engine: "domyn-nexus".into(),
            version: "0.1.0".into(),
            created_unix_nanos: unix_nanos()?,
            files,
        };

        let json = serde_json::to_string_pretty(&manifest)
            .map_err(|err| StorageError::Serialization(err.to_string()))?;
        let tmp_path = backup_dir.join("backup-manifest.json.tmp");
        let mut file = File::create(&tmp_path)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        fs::rename(tmp_path, backup_dir.join("backup-manifest.json"))?;
        sync_dir(backup_dir)?;
        Ok(manifest)
    }

    /// Restore a backup produced by `backup_to` into `data_dir`.
    pub fn restore_backup(
        backup_dir: impl AsRef<Path>,
        data_dir: impl AsRef<Path>,
    ) -> StorageResult<()> {
        let backup_dir = backup_dir.as_ref();
        let data_dir = data_dir.as_ref();
        let manifest_path = backup_dir.join("backup-manifest.json");
        if !manifest_path.exists() {
            return Err(StorageError::Serialization(format!(
                "backup manifest missing: {}",
                manifest_path.display()
            )));
        }
        let manifest: BackupManifest = serde_json::from_slice(&fs::read(&manifest_path)?)
            .map_err(|err| StorageError::Serialization(err.to_string()))?;
        if manifest.engine != "domyn-nexus" {
            return Err(StorageError::Serialization(format!(
                "unsupported backup engine: {}",
                manifest.engine
            )));
        }

        fs::create_dir_all(data_dir)?;

        for relative in manifest.files {
            if !is_backup_data_file(&relative) {
                return Err(StorageError::Serialization(format!(
                    "backup manifest contains unsupported file: {relative}"
                )));
            }
            let source = backup_dir.join(&relative);
            if !source.is_file() {
                return Err(StorageError::Serialization(format!(
                    "backup manifest file missing: {}",
                    source.display()
                )));
            }
            let destination = data_dir.join(&relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source, destination)?;
        }
        sync_dir(data_dir)?;
        sync_dir(&data_dir.join("vectors"))?;
        sync_dir(&data_dir.join("documents"))?;
        Ok(())
    }

    fn archive_current_snapshot(&self) -> StorageResult<()> {
        if self.snapshot_retention == 0 {
            return Ok(());
        }

        let current = self.data_dir.join("snapshot.json");
        if !current.exists() {
            return Ok(());
        }

        let archive_dir = self.snapshot_archive_dir();
        fs::create_dir_all(&archive_dir)?;
        let archived = unique_snapshot_archive_path(&archive_dir)?;
        if fs::hard_link(&current, &archived).is_err() {
            fs::copy(&current, &archived)?;
        }
        sync_dir(&archive_dir)?;
        Ok(())
    }

    fn prune_snapshot_archives(&self) -> StorageResult<()> {
        if self.snapshot_retention == 0 {
            return Ok(());
        }

        let archive_dir = self.snapshot_archive_dir();
        if !archive_dir.exists() {
            return Ok(());
        }

        let mut snapshots = snapshot_archive_paths(&archive_dir)?;
        let prune_count = snapshots.len().saturating_sub(self.snapshot_retention);
        for snapshot in snapshots.drain(..prune_count) {
            fs::remove_file(snapshot)?;
        }
        sync_dir(&archive_dir)?;
        Ok(())
    }

    fn snapshot_archive_dir(&self) -> PathBuf {
        self.data_dir.join("snapshot-archive")
    }

    fn vectors_dir(&self) -> PathBuf {
        self.data_dir.join("vectors")
    }

    fn documents_dir(&self) -> PathBuf {
        self.data_dir.join("documents")
    }

    fn vector_snapshot_path(&self, name: &str) -> StorageResult<PathBuf> {
        validate_vector_index_name(name)?;
        Ok(self.vectors_dir().join(format!("{name}.json")))
    }

    fn document_path(&self, collection: &str, key: &str) -> StorageResult<PathBuf> {
        validate_document_name("collection", collection)?;
        validate_document_name("document key", key)?;
        Ok(self
            .documents_dir()
            .join(collection)
            .join(format!("{key}.json")))
    }

    fn document_index_defs_path(&self) -> PathBuf {
        self.data_dir.join("document-indexes.json")
    }

    fn save_document_index_defs(&self) -> StorageResult<()> {
        let catalog = DocumentIndexCatalog {
            indexes: self.document_indexes.defs.clone(),
        };
        let json = serde_json::to_vec_pretty(&catalog)
            .map_err(|err| StorageError::Serialization(err.to_string()))?;
        let tmp_path = self.data_dir.join("document-indexes.json.tmp");
        let mut file = File::create(&tmp_path)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(tmp_path, self.document_index_defs_path())?;
        sync_dir(&self.data_dir)?;
        Ok(())
    }

    fn rebuild_document_indexes(&mut self) -> StorageResult<()> {
        self.document_indexes.entries.clear();
        let defs = self.document_indexes.defs.clone();
        for def in defs {
            for record in self.list_documents(&def.collection, None)? {
                self.add_document_to_index(&def, &record.key, &record.document);
            }
        }
        Ok(())
    }

    fn add_document_to_indexes(
        &mut self,
        collection: &str,
        key: &str,
        document: &serde_json::Value,
    ) {
        let defs = self.document_indexes.defs.clone();
        for def in defs.iter().filter(|def| def.collection == collection) {
            self.add_document_to_index(def, key, document);
        }
    }

    fn add_document_to_index(
        &mut self,
        def: &DocumentIndexDef,
        key: &str,
        document: &serde_json::Value,
    ) {
        let Some(value) = document_path_value(document, &def.path) else {
            return;
        };
        match def.kind {
            DocumentIndexKind::Scalar => {
                let Some(index_key) = document_index_value_key(value) else {
                    return;
                };
                self.document_indexes
                    .entries
                    .entry((
                        def.collection.clone(),
                        def.path.clone(),
                        DocumentIndexKind::Scalar,
                        index_key,
                    ))
                    .or_default()
                    .insert(key.to_string());
            }
            DocumentIndexKind::FullText => {
                for token in document_fulltext_tokens(value) {
                    self.document_indexes
                        .entries
                        .entry((
                            def.collection.clone(),
                            def.path.clone(),
                            DocumentIndexKind::FullText,
                            token,
                        ))
                        .or_default()
                        .insert(key.to_string());
                }
            }
        }
    }

    fn remove_document_from_indexes(
        &mut self,
        collection: &str,
        key: &str,
        document: Option<&serde_json::Value>,
    ) {
        let defs = self.document_indexes.defs.clone();
        for def in defs.iter().filter(|def| def.collection == collection) {
            if let Some(document) = document {
                if let Some(value) = document_path_value(document, &def.path) {
                    let index_keys: Vec<_> = match def.kind {
                        DocumentIndexKind::Scalar => {
                            document_index_value_key(value).into_iter().collect()
                        }
                        DocumentIndexKind::FullText => {
                            document_fulltext_tokens(value).into_iter().collect()
                        }
                    };
                    if !index_keys.is_empty() {
                        for index_key in index_keys {
                            let entry_key = (
                                def.collection.clone(),
                                def.path.clone(),
                                def.kind,
                                index_key,
                            );
                            if let Some(keys) = self.document_indexes.entries.get_mut(&entry_key) {
                                keys.remove(key);
                                if keys.is_empty() {
                                    self.document_indexes.entries.remove(&entry_key);
                                }
                            }
                        }
                        continue;
                    }
                }
            }
            for ((entry_collection, _, _, _), keys) in self.document_indexes.entries.iter_mut() {
                if entry_collection == collection {
                    keys.remove(key);
                }
            }
            self.document_indexes
                .entries
                .retain(|_, keys| !keys.is_empty());
        }
    }

    /// Replay WAL entries since the last checkpoint. Returns entries to apply.
    pub fn recover(&self) -> StorageResult<Vec<WalEntry>> {
        let reader = WalReader::new(self.data_dir.join("graph.wal"));
        reader.read_since_last_checkpoint()
    }

    /// Replay non-graph WAL entries since the last graph checkpoint.
    ///
    /// Document files and vector snapshots are durable model snapshots. The
    /// WAL covers the crash window after a cross-model mutation is logged but
    /// before those files are atomically renamed into place.
    fn recover_cross_model_state(&mut self) -> StorageResult<()> {
        for entry in self.recover()? {
            let ops = entry.ops();
            self.apply_committed_document_ops(&ops)?;
            self.apply_committed_vector_ops(&ops)?;
        }
        Ok(())
    }

    fn apply_graph_wal_op(graph: &mut Graph, op: WalOp) {
        match op {
            WalOp::AddVertex { id, label } => {
                graph.add_vertex_with_id(id, &label);
            }
            WalOp::SetVertexProperty {
                vertex_id,
                key,
                value,
            } => {
                graph.set_vertex_property(VertexId(vertex_id), &key, value);
            }
            WalOp::SetVertexLabel { vertex_id, label } => {
                let _ = graph.try_set_vertex_label(VertexId(vertex_id), &label);
            }
            WalOp::AddEdge {
                edge_id,
                source,
                target,
                label,
            } => {
                graph.add_edge_with_id(edge_id, VertexId(source), VertexId(target), &label);
            }
            WalOp::SetEdgeProperty {
                edge_id,
                key,
                value,
            } => {
                graph.set_edge_property(EdgeId(edge_id), &key, value);
            }
            WalOp::RemoveVertex { vertex_id } => {
                graph.remove_vertex(VertexId(vertex_id));
            }
            WalOp::RemoveEdge { edge_id } => {
                graph.remove_edge(EdgeId(edge_id));
            }
            WalOp::UpsertDocument { .. }
            | WalOp::DeleteDocument { .. }
            | WalOp::UpsertVector { .. }
            | WalOp::RemoveVector { .. } => {}
        }
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
            for op in entry.ops() {
                Self::apply_graph_wal_op(&mut graph, op);
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
        "any" | "value" | "json" => PropertyType::Any,
        _ => PropertyType::String,
    }
}

fn unique_snapshot_archive_path(archive_dir: &Path) -> StorageResult<PathBuf> {
    let base = unix_nanos()?;
    for suffix in 0..1000_u32 {
        let path = archive_dir.join(format!("snapshot-{base}-{suffix:03}.json"));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(StorageError::Serialization(
        "could not allocate unique snapshot archive path".into(),
    ))
}

fn snapshot_archive_paths(archive_dir: &Path) -> StorageResult<Vec<PathBuf>> {
    let mut snapshots = Vec::new();
    if !archive_dir.exists() {
        return Ok(snapshots);
    }
    for entry in fs::read_dir(archive_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with("snapshot-") && name.ends_with(".json") {
            snapshots.push(entry.path());
        }
    }
    snapshots.sort();
    Ok(snapshots)
}

fn vector_snapshot_paths(vectors_dir: &Path) -> StorageResult<Vec<PathBuf>> {
    let mut snapshots = Vec::new();
    if !vectors_dir.exists() {
        return Ok(snapshots);
    }
    for entry in fs::read_dir(vectors_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.ends_with(".json") {
            snapshots.push(entry.path());
        }
    }
    snapshots.sort();
    Ok(snapshots)
}

fn vector_snapshot_names(vectors_dir: &Path) -> StorageResult<Vec<String>> {
    let mut names = Vec::new();
    for path in vector_snapshot_paths(vectors_dir)? {
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        names.push(stem.to_string());
    }
    names.sort();
    Ok(names)
}

fn paths_with_prefix(dir: &Path, prefix: &str) -> StorageResult<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if !dir.exists() {
        return Ok(paths);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(prefix) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn paths_len(paths: &[PathBuf]) -> StorageResult<u64> {
    paths
        .iter()
        .try_fold(0_u64, |total, path| Ok(total + fs::metadata(path)?.len()))
}

fn file_len(path: impl AsRef<Path>) -> StorageResult<u64> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(0);
    }
    Ok(fs::metadata(path)?.len())
}

fn file_modified_unix_seconds(path: impl AsRef<Path>) -> StorageResult<u64> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(0);
    }
    let modified = fs::metadata(path)?.modified()?;
    let seconds = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|err| StorageError::Serialization(format!("file mtime before epoch: {err}")))?
        .as_secs();
    Ok(seconds)
}

fn sync_dir(path: &Path) -> StorageResult<()> {
    if path.exists() {
        File::open(path)?.sync_all()?;
    }
    Ok(())
}

fn unix_nanos() -> StorageResult<u128> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| StorageError::Serialization(format!("system clock before epoch: {err}")))?;
    Ok(now.as_nanos())
}

fn backup_data_paths(data_dir: &Path) -> StorageResult<Vec<(PathBuf, String)>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(data_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_backup_data_file(name) {
            paths.push((entry.path(), name.to_string()));
        }
    }
    for vector_path in vector_snapshot_paths(&data_dir.join("vectors"))? {
        let Some(name) = vector_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let relative = format!("vectors/{name}");
        if is_backup_data_file(&relative) {
            paths.push((vector_path, relative));
        }
    }
    for (document_path, relative) in document_backup_paths(&data_dir.join("documents"))? {
        if is_backup_data_file(&relative) {
            paths.push((document_path, relative));
        }
    }
    paths.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(paths)
}

fn is_backup_data_file(name: &str) -> bool {
    matches!(
        name,
        "catalog.redb" | "graph.wal" | "snapshot.json" | "document-indexes.json"
    ) || name.starts_with("graph.wal.seg.")
        || (name.starts_with("vectors/") && name.ends_with(".json") && !name.contains(".."))
        || (name.starts_with("documents/") && name.ends_with(".json") && !name.contains(".."))
}

fn cleanup_incomplete_snapshot_tmp(data_dir: &Path) -> StorageResult<()> {
    let mut removed = false;
    for name in ["snapshot.json.tmp", "document-indexes.json.tmp"] {
        let tmp_path = data_dir.join(name);
        if tmp_path.exists() {
            fs::remove_file(tmp_path)?;
            removed = true;
        }
    }
    if removed {
        sync_dir(data_dir)?;
    }
    Ok(())
}

fn validate_vector_index_name(name: &str) -> StorageResult<()> {
    if !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Ok(());
    }
    Err(StorageError::Serialization(format!(
        "invalid vector index name: {name:?}"
    )))
}

fn validate_document_name(kind: &str, name: &str) -> StorageResult<()> {
    if !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Ok(());
    }
    Err(StorageError::Serialization(format!(
        "invalid {kind}: {name:?}"
    )))
}

fn validate_document_index_path(path: &str) -> StorageResult<()> {
    if !path.is_empty()
        && path
            .split('.')
            .all(|part| validate_document_name("document index path segment", part).is_ok())
    {
        return Ok(());
    }
    Err(StorageError::Serialization(format!(
        "invalid document index path: {path:?}"
    )))
}

fn load_document_index_defs(data_dir: &Path) -> StorageResult<Vec<DocumentIndexDef>> {
    let path = data_dir.join("document-indexes.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let catalog: DocumentIndexCatalog = serde_json::from_slice(&fs::read(path)?)
        .map_err(|err| StorageError::Serialization(err.to_string()))?;
    let mut defs = Vec::new();
    for def in catalog.indexes {
        validate_document_name("collection", &def.collection)?;
        validate_document_index_path(&def.path)?;
        if !defs.contains(&def) {
            defs.push(def);
        }
    }
    defs.sort();
    Ok(defs)
}

fn document_path_value<'a>(
    document: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = document;
    for segment in path.split('.') {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

fn document_index_value_key(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            None
        }
        serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => serde_json::to_string(value).ok(),
    }
}

fn document_index_key_value(key: &str) -> Option<serde_json::Value> {
    serde_json::from_str(key).ok()
}

fn document_fulltext_query_tokens_with_options(query: &str, stem: bool) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    push_document_text_tokens_with_options(query, stem, &mut tokens);
    tokens
}

fn document_fulltext_tokens(value: &serde_json::Value) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    collect_document_text_tokens(value, &mut tokens);
    tokens
}

fn document_fulltext_term_counts(value: &serde_json::Value, stem: bool) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    push_document_term_counts(&document_text_blob(value), stem, &mut counts);
    counts
}

fn push_document_term_counts(text: &str, stem: bool, counts: &mut BTreeMap<String, usize>) {
    for token in text.split(|ch: char| !ch.is_alphanumeric()) {
        if token.chars().count() < 2 {
            continue;
        }
        let token = token.to_lowercase();
        let token = if stem { document_stem(&token) } else { token };
        *counts.entry(token).or_default() += 1;
    }
}

fn collect_document_text_tokens(value: &serde_json::Value, tokens: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::String(text) => push_document_text_tokens(text, tokens),
        serde_json::Value::Array(values) => {
            for value in values {
                collect_document_text_tokens(value, tokens);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_document_text_tokens(value, tokens);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn push_document_text_tokens(text: &str, tokens: &mut BTreeSet<String>) {
    push_document_text_tokens_with_options(text, false, tokens);
}

fn push_document_text_tokens_with_options(text: &str, stem: bool, tokens: &mut BTreeSet<String>) {
    for token in text.split(|ch: char| !ch.is_alphanumeric()) {
        if token.chars().count() < 2 {
            continue;
        }
        let token = token.to_lowercase();
        tokens.insert(if stem { document_stem(&token) } else { token });
    }
}

fn document_text_blob(value: &serde_json::Value) -> String {
    let mut text = String::new();
    collect_document_text_blob(value, &mut text);
    text
}

fn collect_document_text_blob(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(text) => {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(text);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_document_text_blob(value, out);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_document_text_blob(value, out);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn document_fulltext_phrase_matches(value: &serde_json::Value, phrase: &str) -> bool {
    let phrase = phrase.trim().to_lowercase();
    !phrase.is_empty() && document_text_blob(value).to_lowercase().contains(&phrase)
}

fn document_fulltext_snippet(
    value: &serde_json::Value,
    query: &str,
    query_tokens: &BTreeSet<String>,
) -> Option<String> {
    let text = document_text_blob(value);
    if text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    let needle = query.trim().to_lowercase();
    let pos = if !needle.is_empty() {
        lower.find(&needle)
    } else {
        None
    }
    .or_else(|| {
        query_tokens
            .iter()
            .filter(|token| token.len() >= 2)
            .find_map(|token| lower.find(token))
    })
    .unwrap_or(0);
    let start = text[..pos]
        .char_indices()
        .rev()
        .nth(40)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    let end_limit = pos.saturating_add(120).min(text.len());
    let end = text
        .char_indices()
        .map(|(idx, _)| idx)
        .find(|idx| *idx >= end_limit)
        .unwrap_or(text.len());
    let snippet = text[start..end].trim();
    (!snippet.is_empty()).then(|| snippet.to_string())
}

fn document_stem(token: &str) -> String {
    for suffix in [
        "ization", "ational", "fulness", "iveness", "ingly", "edly", "ing", "ed", "ies", "s",
    ] {
        if token.len() > suffix.len() + 2 && token.ends_with(suffix) {
            let stem = &token[..token.len() - suffix.len()];
            return if suffix == "ies" {
                format!("{stem}y")
            } else {
                stem.to_string()
            };
        }
    }
    token.to_string()
}

fn levenshtein_at_most(left: &str, right: &str, max_distance: usize) -> bool {
    if left == right {
        return true;
    }
    let left_len = left.chars().count();
    let right_len = right.chars().count();
    if left_len.abs_diff(right_len) > max_distance {
        return false;
    }
    let mut previous: Vec<usize> = (0..=right_len).collect();
    let mut current = vec![0; right_len + 1];
    for (i, left_ch) in left.chars().enumerate() {
        current[0] = i + 1;
        let mut row_min = current[0];
        for (j, right_ch) in right.chars().enumerate() {
            let substitution = previous[j] + usize::from(left_ch != right_ch);
            let insertion = current[j] + 1;
            let deletion = previous[j + 1] + 1;
            current[j + 1] = substitution.min(insertion).min(deletion);
            row_min = row_min.min(current[j + 1]);
        }
        if row_min > max_distance {
            return false;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right_len] <= max_distance
}

fn document_scalar_in_range(
    value: &serde_json::Value,
    gte: Option<&serde_json::Value>,
    lte: Option<&serde_json::Value>,
) -> bool {
    if matches!(
        value,
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_)
    ) {
        return false;
    }
    if let Some(lower) = gte
        && !document_scalar_cmp(value, lower).is_some_and(|ord| {
            matches!(ord, std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        })
    {
        return false;
    }
    if let Some(upper) = lte
        && !document_scalar_cmp(value, upper)
            .is_some_and(|ord| matches!(ord, std::cmp::Ordering::Less | std::cmp::Ordering::Equal))
    {
        return false;
    }
    true
}

fn document_scalar_cmp(
    left: &serde_json::Value,
    right: &serde_json::Value,
) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (serde_json::Value::String(left), serde_json::Value::String(right)) => {
            Some(left.cmp(right))
        }
        (serde_json::Value::Number(left), serde_json::Value::Number(right)) => {
            let left = left.as_f64()?;
            let right = right.as_f64()?;
            left.partial_cmp(&right)
        }
        (serde_json::Value::Bool(left), serde_json::Value::Bool(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

fn document_collection_names(documents_dir: &Path) -> StorageResult<Vec<String>> {
    let mut names = Vec::new();
    if !documents_dir.exists() {
        return Ok(names);
    }
    for entry in fs::read_dir(documents_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if validate_document_name("collection", name).is_ok() {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

fn document_paths_for_collection(
    documents_dir: &Path,
    collection: &str,
) -> StorageResult<Vec<(PathBuf, String)>> {
    let collection_dir = documents_dir.join(collection);
    let mut paths = Vec::new();
    if !collection_dir.exists() {
        return Ok(paths);
    }
    for entry in fs::read_dir(collection_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(key) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if validate_document_name("document key", &key).is_ok() {
            paths.push((path, key));
        }
    }
    paths.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(paths)
}

fn document_backup_paths(documents_dir: &Path) -> StorageResult<Vec<(PathBuf, String)>> {
    let mut paths = Vec::new();
    for collection in document_collection_names(documents_dir)? {
        for (path, key) in document_paths_for_collection(documents_dir, &collection)? {
            paths.push((path, format!("documents/{collection}/{key}.json")));
        }
    }
    paths.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(paths)
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
    fn store_metrics_report_wal_snapshot_and_catalog_sizes() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let mut store =
            NexusStore::open_with_wal_options(&db_path, WalOptions::with_rotation(1, 1)).unwrap();

        store.log_add_vertex(0, "Entity").unwrap();
        store.wal_mut().sync().unwrap();

        let metrics = store.metrics().unwrap();
        assert!(metrics.wal_sequence >= 1);
        assert!(metrics.wal_recoverable_bytes() > 0);
        assert!(metrics.wal_live_segment_count >= 1);
        assert!(metrics.catalog_bytes > 0);

        store.save_snapshot(&build_test_graph()).unwrap();
        let metrics = store.metrics().unwrap();
        assert!(metrics.snapshot_bytes > 0);
        assert!(metrics.snapshot_modified_unix_seconds > 0);
        assert_eq!(metrics.wal_recoverable_bytes(), 0);
        assert_eq!(metrics.wal_archived_segment_count, 1);
        assert!(metrics.wal_archived_segment_bytes > 0);

        store
            .upsert_document(
                "filings",
                "nvda-2024",
                &serde_json::json!({ "ticker": "NVDA" }),
            )
            .unwrap();
        let metrics = store.metrics().unwrap();
        assert_eq!(metrics.document_count, 1);
        assert!(metrics.document_bytes > 0);
        store.create_document_index("filings", "ticker").unwrap();
        let metrics = store.metrics().unwrap();
        assert_eq!(metrics.document_index_count, 1);
        assert_eq!(metrics.document_index_entries, 1);
    }

    #[test]
    fn store_saves_and_loads_vector_index_snapshots() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let store = NexusStore::open(&db_path).unwrap();
        let mut vectors = VectorIndex::new(2);
        vectors.add(VertexId(0), vec![1.0, 0.0]);
        vectors.add(VertexId(1), vec![0.0, 1.0]);
        vectors.remove(VertexId(0));

        store.save_vector_index("entities", &vectors).unwrap();

        let metrics = store.metrics().unwrap();
        assert_eq!(metrics.vector_snapshot_count, 1);
        assert!(metrics.vector_snapshot_bytes > 0);

        let loaded = store.load_vector_index("entities").unwrap().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.tombstone_count(), 0);
        assert_eq!(loaded.search(&[0.0, 1.0], 1)[0].0, VertexId(1));
        assert!(
            !loaded
                .search_exact(&[1.0, 0.0], 2)
                .iter()
                .any(|(id, _)| *id == VertexId(0))
        );
        assert!(store.load_vector_index("missing").unwrap().is_none());
        assert!(store.load_vector_index("../escape").is_err());
    }

    #[test]
    fn store_lists_vector_index_snapshots() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let store = NexusStore::open(&db_path).unwrap();
        let mut entities = VectorIndex::new(2);
        entities.add(VertexId(0), vec![1.0, 0.0]);
        let mut chunks = VectorIndex::new(3);
        chunks.add(VertexId(1), vec![0.0, 1.0, 0.0]);

        store.save_vector_index("entities", &entities).unwrap();
        store.save_vector_index("chunks", &chunks).unwrap();
        fs::write(db_path.join("vectors").join("README.txt"), b"ignore me").unwrap();

        assert_eq!(
            store.list_vector_indexes().unwrap(),
            vec!["chunks".to_string(), "entities".to_string()]
        );
    }

    #[test]
    fn store_persists_document_collections() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let mut store = NexusStore::open(&db_path).unwrap();

        store
            .upsert_document(
                "filings",
                "nvda-2024",
                &serde_json::json!({
                    "ticker": "NVDA",
                    "year": 2024,
                    "tags": ["10-K", "risk"]
                }),
            )
            .unwrap();
        store
            .upsert_document(
                "chunks",
                "chunk-1",
                &serde_json::json!({ "text": "Data center revenue increased." }),
            )
            .unwrap();

        assert_eq!(
            store.list_document_collections().unwrap(),
            vec!["chunks".to_string(), "filings".to_string()]
        );
        assert_eq!(
            store
                .load_document("filings", "nvda-2024")
                .unwrap()
                .unwrap()["ticker"],
            "NVDA"
        );
        let docs = store.list_documents("filings", Some(10)).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].collection, "filings");
        assert_eq!(docs[0].key, "nvda-2024");
        assert!(store.delete_document("filings", "nvda-2024").unwrap());
        assert!(!store.delete_document("filings", "nvda-2024").unwrap());
        assert!(
            store
                .load_document("filings", "nvda-2024")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .upsert_document("../escape", "key", &serde_json::json!({}))
                .is_err()
        );
        assert!(
            store
                .upsert_document("safe", "../escape", &serde_json::json!({}))
                .is_err()
        );
    }

    #[test]
    fn document_wal_replays_logged_upsert_when_file_is_missing() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store
                .wal_mut()
                .append(&WalEntry::UpsertDocument {
                    collection: "filings".into(),
                    key: "nvda-2024".into(),
                    document: serde_json::json!({ "ticker": "NVDA", "year": 2024 }),
                })
                .unwrap();
            store.wal_mut().sync().unwrap();
            assert!(
                store
                    .load_document("filings", "nvda-2024")
                    .unwrap()
                    .is_none()
            );
        }

        let recovered = NexusStore::open(&db_path).unwrap();
        assert_eq!(
            recovered
                .load_document("filings", "nvda-2024")
                .unwrap()
                .unwrap()["ticker"],
            "NVDA"
        );
    }

    #[test]
    fn document_wal_replays_logged_delete_when_file_still_exists() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store
                .upsert_document(
                    "filings",
                    "nvda-2024",
                    &serde_json::json!({ "ticker": "NVDA" }),
                )
                .unwrap();
            assert!(
                store
                    .load_document("filings", "nvda-2024")
                    .unwrap()
                    .is_some()
            );
            store
                .wal_mut()
                .append(&WalEntry::DeleteDocument {
                    collection: "filings".into(),
                    key: "nvda-2024".into(),
                })
                .unwrap();
            store.wal_mut().sync().unwrap();
        }

        let recovered = NexusStore::open(&db_path).unwrap();
        assert!(
            recovered
                .load_document("filings", "nvda-2024")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cross_model_commit_replays_graph_and_document_ops() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store
                .log_commit(
                    42,
                    vec![
                        WalOp::AddVertex {
                            id: 0,
                            label: "Document".into(),
                        },
                        WalOp::SetVertexProperty {
                            vertex_id: 0,
                            key: "name".into(),
                            value: Value::String("NVDA 10-K".into()),
                        },
                        WalOp::UpsertDocument {
                            collection: "filings".into(),
                            key: "nvda-2024".into(),
                            document: serde_json::json!({ "ticker": "NVDA" }),
                        },
                    ],
                )
                .unwrap();
            store.wal_mut().sync().unwrap();
        }

        let recovered = NexusStore::open(&db_path).unwrap();
        assert_eq!(
            recovered
                .load_document("filings", "nvda-2024")
                .unwrap()
                .unwrap()["ticker"],
            "NVDA"
        );
        let graph = recovered.load_graph(4, 4).unwrap();
        assert_eq!(graph.vertex_label(VertexId(0)), Some("Document"));
        assert_eq!(
            graph.get_vertex_property(VertexId(0), "name"),
            Value::String("NVDA 10-K".into())
        );
    }

    #[test]
    fn cross_model_commit_replays_vector_ops() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            let vectors = VectorIndex::new(2);
            store.save_vector_index("entities", &vectors).unwrap();
            store
                .log_commit(
                    7,
                    vec![WalOp::UpsertVector {
                        index: "entities".into(),
                        vertex_id: 42,
                        embedding: vec![1.0, 0.0],
                    }],
                )
                .unwrap();
            store.wal_mut().sync().unwrap();
        }

        let recovered = NexusStore::open(&db_path).unwrap();
        let vectors = recovered.load_vector_index("entities").unwrap().unwrap();
        assert_eq!(vectors.search_exact(&[1.0, 0.0], 1)[0].0, VertexId(42));
    }

    #[test]
    fn document_secondary_index_tracks_upsert_update_delete_and_reopen() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.create_document_index("filings", "ticker").unwrap();
            store
                .create_document_index("filings", "metadata.form")
                .unwrap();
            store
                .upsert_document(
                    "filings",
                    "nvda-2024",
                    &serde_json::json!({
                        "ticker": "NVDA",
                        "metadata": { "form": "10-K" }
                    }),
                )
                .unwrap();
            store
                .upsert_document(
                    "filings",
                    "aapl-2024",
                    &serde_json::json!({
                        "ticker": "AAPL",
                        "metadata": { "form": "10-K" }
                    }),
                )
                .unwrap();

            assert_eq!(
                store
                    .query_documents_by_index(
                        "filings",
                        "ticker",
                        &serde_json::json!("NVDA"),
                        Some(10),
                    )
                    .unwrap()
                    .iter()
                    .map(|record| record.key.as_str())
                    .collect::<Vec<_>>(),
                vec!["nvda-2024"]
            );
            assert_eq!(
                store
                    .query_documents_by_index(
                        "filings",
                        "metadata.form",
                        &serde_json::json!("10-K"),
                        Some(10),
                    )
                    .unwrap()
                    .len(),
                2
            );

            store
                .upsert_document(
                    "filings",
                    "nvda-2024",
                    &serde_json::json!({
                        "ticker": "NVDA-UPDATED",
                        "metadata": { "form": "10-Q" }
                    }),
                )
                .unwrap();
            assert!(
                store
                    .query_documents_by_index(
                        "filings",
                        "ticker",
                        &serde_json::json!("NVDA"),
                        Some(10),
                    )
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                store
                    .query_documents_by_index(
                        "filings",
                        "ticker",
                        &serde_json::json!("NVDA-UPDATED"),
                        Some(10),
                    )
                    .unwrap()[0]
                    .key,
                "nvda-2024"
            );

            assert!(store.delete_document("filings", "aapl-2024").unwrap());
            assert!(
                store
                    .query_documents_by_index(
                        "filings",
                        "ticker",
                        &serde_json::json!("AAPL"),
                        Some(10),
                    )
                    .unwrap()
                    .is_empty()
            );
        }

        let reopened = NexusStore::open(&db_path).unwrap();
        assert_eq!(
            reopened.list_document_indexes(Some("filings")).unwrap(),
            vec![
                DocumentIndexDef {
                    collection: "filings".into(),
                    path: "metadata.form".into(),
                    kind: DocumentIndexKind::Scalar,
                },
                DocumentIndexDef {
                    collection: "filings".into(),
                    path: "ticker".into(),
                    kind: DocumentIndexKind::Scalar,
                },
            ]
        );
        assert_eq!(
            reopened
                .query_documents_by_index(
                    "filings",
                    "ticker",
                    &serde_json::json!("NVDA-UPDATED"),
                    Some(10),
                )
                .unwrap()[0]
                .key,
            "nvda-2024"
        );
    }

    #[test]
    fn document_secondary_index_supports_prefix_and_range_queries() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let mut store = NexusStore::open(&db_path).unwrap();
        store.create_document_index("filings", "ticker").unwrap();
        store.create_document_index("filings", "year").unwrap();

        for (key, ticker, year) in [
            ("nvda-2024", "NVDA", 2024),
            ("nflx-2022", "NFLX", 2022),
            ("aapl-2023", "AAPL", 2023),
        ] {
            store
                .upsert_document(
                    "filings",
                    key,
                    &serde_json::json!({
                        "ticker": ticker,
                        "year": year,
                    }),
                )
                .unwrap();
        }

        let prefix_keys: Vec<_> = store
            .query_documents_by_index_with(
                "filings",
                "ticker",
                DocumentIndexQuery::Prefix("N".into()),
                Some(10),
            )
            .unwrap()
            .into_iter()
            .map(|record| record.key)
            .collect();
        assert_eq!(prefix_keys, vec!["nflx-2022", "nvda-2024"]);

        let range_keys: Vec<_> = store
            .query_documents_by_index_with(
                "filings",
                "year",
                DocumentIndexQuery::Range {
                    gte: Some(serde_json::json!(2023)),
                    lte: Some(serde_json::json!(2024)),
                },
                Some(10),
            )
            .unwrap()
            .into_iter()
            .map(|record| record.key)
            .collect();
        assert_eq!(range_keys, vec!["aapl-2023", "nvda-2024"]);
    }

    #[test]
    fn document_fulltext_index_tracks_upsert_delete_and_reopen() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let mut store = NexusStore::open(&db_path).unwrap();
        store
            .create_document_index_with_kind("filings", "body", DocumentIndexKind::FullText)
            .unwrap();

        store
            .upsert_document(
                "filings",
                "nvda-2024",
                &serde_json::json!({
                    "body": "Revenue growth accelerated while supply risk remained visible."
                }),
            )
            .unwrap();
        store
            .upsert_document(
                "filings",
                "aapl-2024",
                &serde_json::json!({
                    "body": "Services margin expanded with stable device demand."
                }),
            )
            .unwrap();

        let keys: Vec<_> = store
            .query_documents_by_index_with(
                "filings",
                "body",
                DocumentIndexQuery::FullText("revenue risk".into()),
                Some(10),
            )
            .unwrap()
            .into_iter()
            .map(|record| record.key)
            .collect();
        assert_eq!(keys, vec!["nvda-2024"]);

        store
            .upsert_document(
                "filings",
                "nvda-2024",
                &serde_json::json!({
                    "body": "Cash flow improved with no supply-chain concern."
                }),
            )
            .unwrap();
        assert!(
            store
                .query_documents_by_index_with(
                    "filings",
                    "body",
                    DocumentIndexQuery::FullText("revenue".into()),
                    Some(10),
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .query_documents_by_index_with(
                    "filings",
                    "body",
                    DocumentIndexQuery::FullText("cash flow".into()),
                    Some(10),
                )
                .unwrap()[0]
                .key,
            "nvda-2024"
        );

        store.delete_document("filings", "aapl-2024").unwrap();
        assert!(
            store
                .query_documents_by_index_with(
                    "filings",
                    "body",
                    DocumentIndexQuery::FullText("services margin".into()),
                    Some(10),
                )
                .unwrap()
                .is_empty()
        );

        drop(store);
        let reopened = NexusStore::open(&db_path).unwrap();
        assert_eq!(
            reopened.list_document_indexes(Some("filings")).unwrap(),
            vec![DocumentIndexDef {
                collection: "filings".into(),
                path: "body".into(),
                kind: DocumentIndexKind::FullText,
            }]
        );
        assert_eq!(
            reopened
                .query_documents_by_index_with(
                    "filings",
                    "body",
                    DocumentIndexQuery::FullText("cash".into()),
                    Some(10),
                )
                .unwrap()[0]
                .key,
            "nvda-2024"
        );
    }

    #[test]
    fn document_fulltext_advanced_hits_report_scores_snippets_and_explanations() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let mut store = NexusStore::open(&db_path).unwrap();
        store
            .create_document_index_with_kind("filings", "body", DocumentIndexKind::FullText)
            .unwrap();

        store
            .upsert_document(
                "filings",
                "nvda-2024",
                &serde_json::json!({
                    "body": "Revenue growth accelerated while supply risk remained visible."
                }),
            )
            .unwrap();
        store
            .upsert_document(
                "filings",
                "aapl-2024",
                &serde_json::json!({
                    "body": "Services margins expanded while device demand was stabilizing."
                }),
            )
            .unwrap();

        let phrase_hits = store
            .query_document_hits_by_index_with(
                "filings",
                "body",
                DocumentIndexQuery::FullTextAdvanced(DocumentFullTextQuery {
                    text: "supply risk".into(),
                    phrase: true,
                    fuzzy_distance: None,
                    stem: false,
                    ranking: DocumentFullTextRanking::MatchCount,
                    include_snippets: true,
                    include_explanations: true,
                }),
                Some(10),
            )
            .unwrap();
        assert_eq!(phrase_hits.len(), 1);
        assert_eq!(phrase_hits[0].record.key, "nvda-2024");
        assert!(phrase_hits[0].score >= 2.0);
        assert!(
            phrase_hits[0]
                .snippet
                .as_deref()
                .unwrap()
                .contains("supply risk")
        );
        assert!(
            phrase_hits[0]
                .explanation
                .as_deref()
                .unwrap()
                .contains("matched_terms")
        );

        let fuzzy_hits = store
            .query_document_hits_by_index_with(
                "filings",
                "body",
                DocumentIndexQuery::FullTextAdvanced(DocumentFullTextQuery {
                    text: "reveneu".into(),
                    phrase: false,
                    fuzzy_distance: Some(2),
                    stem: false,
                    ranking: DocumentFullTextRanking::MatchCount,
                    include_snippets: false,
                    include_explanations: false,
                }),
                Some(10),
            )
            .unwrap();
        assert_eq!(fuzzy_hits[0].record.key, "nvda-2024");

        let stemmed_hits = store
            .query_document_hits_by_index_with(
                "filings",
                "body",
                DocumentIndexQuery::FullTextAdvanced(DocumentFullTextQuery {
                    text: "margin".into(),
                    phrase: false,
                    fuzzy_distance: None,
                    stem: true,
                    ranking: DocumentFullTextRanking::MatchCount,
                    include_snippets: false,
                    include_explanations: false,
                }),
                Some(10),
            )
            .unwrap();
        assert_eq!(stemmed_hits[0].record.key, "aapl-2024");
    }

    #[test]
    fn document_fulltext_supports_bm25_and_tfidf_ranking() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let mut store = NexusStore::open(&db_path).unwrap();
        store
            .create_document_index_with_kind("filings", "body", DocumentIndexKind::FullText)
            .unwrap();

        store
            .upsert_document(
                "filings",
                "dense",
                &serde_json::json!({
                    "body": "revenue revenue revenue revenue margin"
                }),
            )
            .unwrap();
        store
            .upsert_document(
                "filings",
                "sparse",
                &serde_json::json!({
                    "body": "revenue margin"
                }),
            )
            .unwrap();
        store
            .upsert_document(
                "filings",
                "other",
                &serde_json::json!({
                    "body": "supply chain inventory"
                }),
            )
            .unwrap();

        let bm25 = store
            .query_document_hits_by_index_with(
                "filings",
                "body",
                DocumentIndexQuery::FullTextAdvanced(DocumentFullTextQuery {
                    text: "revenue".into(),
                    phrase: false,
                    fuzzy_distance: None,
                    stem: false,
                    ranking: DocumentFullTextRanking::Bm25,
                    include_snippets: false,
                    include_explanations: true,
                }),
                Some(10),
            )
            .unwrap();
        assert_eq!(bm25[0].record.key, "dense");
        assert!(bm25[0].score > bm25[1].score);
        assert!(
            bm25[0]
                .explanation
                .as_deref()
                .unwrap()
                .contains("ranking=Bm25")
        );

        let tfidf = store
            .query_document_hits_by_index_with(
                "filings",
                "body",
                DocumentIndexQuery::FullTextAdvanced(DocumentFullTextQuery {
                    text: "revenue".into(),
                    phrase: false,
                    fuzzy_distance: None,
                    stem: false,
                    ranking: DocumentFullTextRanking::TfIdf,
                    include_snippets: false,
                    include_explanations: false,
                }),
                Some(10),
            )
            .unwrap();
        assert_eq!(tfidf[0].record.key, "dense");
        assert!(tfidf[0].score > tfidf[1].score);
    }

    #[test]
    fn hot_backup_restore_carries_document_index_definitions() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let backup_path = dir.path().join("backup");
        let restore_path = dir.path().join("restore");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.create_document_index("filings", "ticker").unwrap();
            store
                .upsert_document(
                    "filings",
                    "nvda-2024",
                    &serde_json::json!({ "ticker": "NVDA" }),
                )
                .unwrap();
            let manifest = store.backup_to(&backup_path).unwrap();
            assert!(
                manifest
                    .files
                    .iter()
                    .any(|file| file == "document-indexes.json")
            );
        }

        NexusStore::restore_backup(&backup_path, &restore_path).unwrap();
        let restored = NexusStore::open(&restore_path).unwrap();
        assert_eq!(
            restored
                .query_documents_by_index("filings", "ticker", &serde_json::json!("NVDA"), Some(10))
                .unwrap()[0]
                .key,
            "nvda-2024"
        );
    }

    #[test]
    fn hot_backup_restore_carries_document_collections() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let backup_path = dir.path().join("backup");
        let restore_path = dir.path().join("restore");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.save_snapshot(&build_test_graph()).unwrap();
            store
                .upsert_document(
                    "filings",
                    "nvda-2024",
                    &serde_json::json!({ "ticker": "NVDA", "year": 2024 }),
                )
                .unwrap();
            let manifest = store.backup_to(&backup_path).unwrap();
            assert!(
                manifest
                    .files
                    .iter()
                    .any(|file| { file == "documents/filings/nvda-2024.json" })
            );
        }

        NexusStore::restore_backup(&backup_path, &restore_path).unwrap();
        let restored = NexusStore::open(&restore_path).unwrap();
        assert_eq!(
            restored
                .load_document("filings", "nvda-2024")
                .unwrap()
                .unwrap()["ticker"],
            "NVDA"
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
    fn open_removes_stale_snapshot_tmp_without_replacing_committed_snapshot() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            let mut graph = Graph::new(2, 2);
            graph.register_vertex_property("name", PropertyType::String, true, false);
            let vertex = graph.add_vertex("Entity");
            graph.set_vertex_property(vertex, "name", Value::String("committed".into()));
            graph.build();
            store.save_snapshot(&graph).unwrap();
        }

        fs::write(
            db_path.join("snapshot.json.tmp"),
            r#"{"vertices":[{"id":0,"label":"Entity","properties":[["name","\"stale\""]]}],"edges":[],"schema":{"vertex_properties":[],"edge_properties":[]}}"#,
        )
        .unwrap();
        assert!(db_path.join("snapshot.json.tmp").exists());

        let store = NexusStore::open(&db_path).unwrap();
        assert!(
            !db_path.join("snapshot.json.tmp").exists(),
            "stale temp snapshot must be removed on open"
        );
        let graph = store.load_graph(2, 2).unwrap();
        assert_eq!(
            graph.get_vertex_property(VertexId(0), "name"),
            Value::String("committed".into())
        );
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

    #[test]
    fn store_recovery_reads_rotated_wal_segments() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        {
            let mut store =
                NexusStore::open_with_wal_options(&db_path, WalOptions::with_rotation(1, 0))
                    .unwrap();
            store.log_add_vertex(0, "Entity").unwrap();
            store
                .log_set_vertex_property(0, "name", &Value::String("A".into()))
                .unwrap();
            store.log_add_vertex(1, "Entity").unwrap();
            store.log_add_edge(0, 0, 1, "KNOWS").unwrap();
            store.wal_mut().sync().unwrap();
        }
        {
            let store = NexusStore::open(&db_path).unwrap();
            let graph = store.load_graph(10, 10).unwrap();
            assert_eq!(
                graph.get_vertex_property(VertexId(0), "name"),
                Value::String("A".into())
            );
            assert_eq!(
                graph.neighbors(VertexId(0), "KNOWS", Direction::Outgoing),
                vec![VertexId(1)]
            );
        }
    }

    #[test]
    fn save_snapshot_archives_retained_rotated_wal_segments() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        {
            let mut store =
                NexusStore::open_with_wal_options(&db_path, WalOptions::with_rotation(1, 1))
                    .unwrap();
            for id in 0..4 {
                store.log_add_vertex(id, "Entity").unwrap();
            }
            store.wal_mut().sync().unwrap();
            store.save_snapshot(&build_test_graph()).unwrap();
        }
        {
            let store = NexusStore::open(&db_path).unwrap();
            assert!(store.recover().unwrap().is_empty());
            let archived_count = fs::read_dir(db_path.join("wal-archive"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|ty| ty.is_file()))
                .count();
            assert_eq!(archived_count, 1);

            let graph = store.load_graph(10, 10).unwrap();
            assert_eq!(
                graph.get_vertex_property(VertexId(0), "name"),
                Value::String("Apple Inc.".into())
            );
            assert_eq!(graph.edge_records().len(), 2);
        }
    }

    #[test]
    fn save_snapshot_retains_previous_snapshots() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");

        {
            let mut store = NexusStore::open_with_options(
                &db_path,
                StoreOptions {
                    snapshot_retention: 2,
                    ..StoreOptions::default()
                },
            )
            .unwrap();

            for idx in 0..4 {
                let mut graph = Graph::new(4, 4);
                graph.register_vertex_property("name", PropertyType::String, true, false);
                let vertex = graph.add_vertex("Entity");
                graph.set_vertex_property(vertex, "name", Value::String(format!("v{idx}")));
                graph.build();
                store.save_snapshot(&graph).unwrap();
            }
        }

        let archive_dir = db_path.join("snapshot-archive");
        assert_eq!(snapshot_archive_paths(&archive_dir).unwrap().len(), 2);

        let store = NexusStore::open(&db_path).unwrap();
        let snapshot = store.load_snapshot().unwrap().unwrap();
        assert_eq!(snapshot.vertices.len(), 1);
        let latest_name: Value = serde_json::from_str(&snapshot.vertices[0].properties[0].1)
            .expect("snapshot property should be typed JSON");
        assert_eq!(latest_name, Value::String("v3".into()));
    }

    #[test]
    fn hot_backup_restore_recovers_snapshot_plus_rotated_wal() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let backup_path = dir.path().join("backup");
        let restore_path = dir.path().join("restoredb");

        {
            let mut store =
                NexusStore::open_with_wal_options(&db_path, WalOptions::with_rotation(1, 0))
                    .unwrap();
            store.save_snapshot(&build_test_graph()).unwrap();
            store.log_add_vertex(3, "Entity").unwrap();
            store
                .log_set_vertex_property(3, "name", &Value::String("Post Snapshot".into()))
                .unwrap();

            let manifest = store.backup_to(&backup_path).unwrap();
            assert!(manifest.files.iter().any(|file| file == "snapshot.json"));
            assert!(
                manifest
                    .files
                    .iter()
                    .any(|file| file.starts_with("graph.wal.seg."))
            );
        }

        NexusStore::restore_backup(&backup_path, &restore_path).unwrap();
        let restored = NexusStore::open(&restore_path).unwrap();
        let graph = restored.load_graph(10, 10).unwrap();

        assert_eq!(
            graph.get_vertex_property(VertexId(0), "name"),
            Value::String("Apple Inc.".into())
        );
        assert_eq!(
            graph.get_vertex_property(VertexId(3), "name"),
            Value::String("Post Snapshot".into())
        );
        assert_eq!(graph.edge_records().len(), 2);
    }

    #[test]
    fn restore_rejects_backup_without_manifest() {
        let dir = TempDir::new().unwrap();
        let backup_path = dir.path().join("incomplete-backup");
        let restore_path = dir.path().join("restoredb");
        fs::create_dir_all(&backup_path).unwrap();
        fs::write(backup_path.join("snapshot.json"), "{}").unwrap();

        let err = NexusStore::restore_backup(&backup_path, &restore_path).unwrap_err();
        assert!(err.to_string().contains("backup manifest missing"));
    }

    #[test]
    fn hot_backup_restore_uses_invocation_cutoff() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let backup_path = dir.path().join("backup");
        let restore_path = dir.path().join("restoredb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.save_snapshot(&build_test_graph()).unwrap();
            store.log_add_vertex(3, "Entity").unwrap();
            store
                .log_set_vertex_property(3, "name", &Value::String("included".into()))
                .unwrap();

            let manifest = store.backup_to(&backup_path).unwrap();
            assert!(manifest.files.iter().any(|file| file == "graph.wal"));

            store.log_add_vertex(4, "Entity").unwrap();
            store
                .log_set_vertex_property(4, "name", &Value::String("after-backup".into()))
                .unwrap();
            store.wal_mut().sync().unwrap();
        }

        NexusStore::restore_backup(&backup_path, &restore_path).unwrap();
        let restored = NexusStore::open(&restore_path).unwrap();
        let graph = restored.load_graph(10, 10).unwrap();

        assert_eq!(
            graph.get_vertex_property(VertexId(3), "name"),
            Value::String("included".into())
        );
        assert_eq!(graph.vertex_label(VertexId(4)), None);
    }

    #[test]
    fn hot_backup_restore_carries_vector_snapshots() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let backup_path = dir.path().join("backup");
        let restore_path = dir.path().join("restoredb");

        {
            let mut store = NexusStore::open(&db_path).unwrap();
            store.save_snapshot(&build_test_graph()).unwrap();
            let mut vectors = VectorIndex::new(2);
            vectors.add(VertexId(0), vec![1.0, 0.0]);
            vectors.add(VertexId(1), vec![0.0, 1.0]);
            vectors.update(VertexId(1), vec![0.0, 0.99]);
            store.save_vector_index("entities", &vectors).unwrap();

            let manifest = store.backup_to(&backup_path).unwrap();
            assert!(manifest.files.iter().any(|file| file == "snapshot.json"));
            assert!(
                manifest
                    .files
                    .iter()
                    .any(|file| file == "vectors/entities.json")
            );
        }

        NexusStore::restore_backup(&backup_path, &restore_path).unwrap();
        let restored = NexusStore::open(&restore_path).unwrap();
        let graph = restored.load_graph(10, 10).unwrap();
        let vectors = restored.load_vector_index("entities").unwrap().unwrap();

        assert_eq!(
            graph.get_vertex_property(VertexId(0), "name"),
            Value::String("Apple Inc.".into())
        );
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors.tombstone_count(), 0);
        assert_eq!(vectors.search(&[0.0, 1.0], 1)[0].0, VertexId(1));
    }
}
