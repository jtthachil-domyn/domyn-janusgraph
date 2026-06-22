//! Full-text search via tantivy (embedded Lucene-equivalent in Rust).
//!
//! Replaces the Elasticsearch dependency in the JanusGraph stack.
//! Indexes vertex properties like `name` and `description` for
//! text search queries.

use nexus_core::types::VertexId;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, INDEXED, STORED, Schema, TEXT, Value};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyError, Term, doc};

pub struct FullTextIndex {
    index: Index,
    reader: IndexReader,
    writer: parking_lot::Mutex<IndexWriter>,
    vertex_id_field: Field,
    text_field: Field,
}

impl FullTextIndex {
    pub fn create_in_memory() -> Result<Self, TantivyError> {
        let mut schema_builder = Schema::builder();
        let vertex_id_field = schema_builder.add_u64_field("vertex_id", INDEXED | STORED);
        let text_field = schema_builder.add_text_field("text", TEXT | STORED);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let writer = index.writer(50_000_000)?;

        Ok(Self {
            index,
            reader,
            writer: parking_lot::Mutex::new(writer),
            vertex_id_field,
            text_field,
        })
    }

    pub fn create_on_disk(path: impl AsRef<Path>) -> Result<Self, TantivyError> {
        let mut schema_builder = Schema::builder();
        let vertex_id_field = schema_builder.add_u64_field("vertex_id", INDEXED | STORED);
        let text_field = schema_builder.add_text_field("text", TEXT | STORED);
        let schema = schema_builder.build();

        std::fs::create_dir_all(path.as_ref()).ok();
        let index = Index::create_in_dir(path, schema)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let writer = index.writer(50_000_000)?;

        Ok(Self {
            index,
            reader,
            writer: parking_lot::Mutex::new(writer),
            vertex_id_field,
            text_field,
        })
    }

    /// Index a vertex's text property.
    pub fn add(&self, vertex_id: VertexId, text: &str) -> Result<(), TantivyError> {
        let writer = self.writer.lock();
        writer.add_document(doc!(
            self.vertex_id_field => vertex_id.0,
            self.text_field => text,
        ))?;
        Ok(())
    }

    /// Replace a vertex's indexed text.
    ///
    /// Tantivy applies deletes at commit time, so callers should call
    /// `commit()` after a batch of updates before expecting readers to observe
    /// the new state.
    pub fn update(&self, vertex_id: VertexId, text: &str) -> Result<(), TantivyError> {
        let writer = self.writer.lock();
        writer.delete_term(Term::from_field_u64(self.vertex_id_field, vertex_id.0));
        writer.add_document(doc!(
            self.vertex_id_field => vertex_id.0,
            self.text_field => text,
        ))?;
        Ok(())
    }

    /// Remove a vertex from the full-text index.
    pub fn remove(&self, vertex_id: VertexId) -> Result<(), TantivyError> {
        let writer = self.writer.lock();
        writer.delete_term(Term::from_field_u64(self.vertex_id_field, vertex_id.0));
        Ok(())
    }

    /// Commit pending additions and reload the reader.
    pub fn commit(&self) -> Result<(), TantivyError> {
        let mut writer = self.writer.lock();
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Search for vertices matching a text query. Returns up to `limit` results.
    pub fn search(&self, query_str: &str, limit: usize) -> Result<Vec<VertexId>, TantivyError> {
        let searcher = self.reader.searcher();
        let query_parser = QueryParser::for_index(&self.index, vec![self.text_field]);
        let query = query_parser.parse_query(query_str)?;
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        let mut results = Vec::with_capacity(top_docs.len());
        for (_score, doc_addr) in top_docs {
            let doc = searcher.doc::<tantivy::TantivyDocument>(doc_addr)?;
            if let Some(val) = doc.get_first(self.vertex_id_field) {
                if let Some(id) = val.as_u64() {
                    results.push(VertexId(id));
                }
            }
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fulltext_index_add_and_search() {
        let idx = FullTextIndex::create_in_memory().unwrap();

        idx.add(VertexId(0), "Apple Inc. technology company")
            .unwrap();
        idx.add(VertexId(1), "Revenue from product sales").unwrap();
        idx.add(VertexId(2), "Services segment including App Store")
            .unwrap();
        idx.add(VertexId(3), "NVIDIA corporation GPU manufacturer")
            .unwrap();
        idx.commit().unwrap();

        let results = idx.search("Apple", 10).unwrap();
        assert!(results.contains(&VertexId(0)));

        let results = idx.search("GPU", 10).unwrap();
        assert!(results.contains(&VertexId(3)));

        let results = idx.search("revenue", 10).unwrap();
        assert!(results.contains(&VertexId(1)));
    }

    #[test]
    fn fulltext_index_update_replaces_stale_text() {
        let idx = FullTextIndex::create_in_memory().unwrap();

        idx.add(VertexId(42), "old apple filing").unwrap();
        idx.commit().unwrap();
        assert!(idx.search("apple", 10).unwrap().contains(&VertexId(42)));

        idx.update(VertexId(42), "new nvidia filing").unwrap();
        idx.commit().unwrap();

        assert!(!idx.search("apple", 10).unwrap().contains(&VertexId(42)));
        assert!(idx.search("nvidia", 10).unwrap().contains(&VertexId(42)));
    }

    #[test]
    fn fulltext_index_remove_hides_deleted_vertex() {
        let idx = FullTextIndex::create_in_memory().unwrap();

        idx.add(VertexId(7), "delete me from search").unwrap();
        idx.commit().unwrap();
        assert!(idx.search("delete", 10).unwrap().contains(&VertexId(7)));

        idx.remove(VertexId(7)).unwrap();
        idx.commit().unwrap();

        assert!(!idx.search("delete", 10).unwrap().contains(&VertexId(7)));
    }
}
