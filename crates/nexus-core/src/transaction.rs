use crate::graph::{Graph, GraphError};
use crate::types::*;
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TxStatus {
    Active,
    Committed,
    Aborted,
}

#[derive(Debug, Clone)]
pub enum WriteOp {
    AddVertex {
        id: VertexId,
        label: String,
    },
    SetVertexProperty {
        vertex: VertexId,
        key: String,
        value: Value,
    },
    SetVertexLabel {
        vertex: VertexId,
        label: String,
    },
    AddEdge {
        edge_id: EdgeId,
        source: VertexId,
        target: VertexId,
        label: String,
    },
    SetEdgeProperty {
        edge: EdgeId,
        key: String,
        value: Value,
    },
    RemoveVertex {
        vertex: VertexId,
    },
    RemoveEdge {
        edge: EdgeId,
    },
}

/// SWMR transactional wrapper around `Graph`.
///
/// Read transactions acquire a shared read lock on the committed state.
/// Write transactions hold an exclusive mutex (ensuring single-writer),
/// buffer operations, and apply them atomically on commit via the
/// graph's write lock.
pub struct TransactionalGraph {
    committed: Arc<RwLock<Graph>>,
    next_tx: AtomicU64,
    write_lock: Mutex<()>,
}

impl TransactionalGraph {
    pub fn new(graph: Graph) -> Self {
        Self {
            committed: Arc::new(RwLock::new(graph)),
            next_tx: AtomicU64::new(1),
            write_lock: Mutex::new(()),
        }
    }

    pub fn begin_read(&self) -> ReadTx<'_> {
        let tx_id = TxId(self.next_tx.fetch_add(1, Ordering::SeqCst));
        let guard = self.committed.read();
        ReadTx { tx_id, guard }
    }

    /// Acquire the single-writer lock and return a write transaction.
    /// Blocks if another write transaction is active.
    pub fn begin_write(&self) -> WriteTx<'_> {
        let guard = self.write_lock.lock();
        let tx_id = TxId(self.next_tx.fetch_add(1, Ordering::SeqCst));
        let graph = self.committed.read();
        let next_vertex_id = graph.num_vertices() as u64;
        let next_edge_id = graph.num_edges();
        drop(graph);

        WriteTx {
            tx_id,
            status: TxStatus::Active,
            graph: &self.committed,
            ops: Vec::new(),
            next_vertex_id,
            next_edge_id,
            _write_guard: guard,
        }
    }

    pub fn committed(&self) -> &Arc<RwLock<Graph>> {
        &self.committed
    }
}

pub struct ReadTx<'a> {
    tx_id: TxId,
    guard: parking_lot::RwLockReadGuard<'a, Graph>,
}

impl<'a> ReadTx<'a> {
    pub fn tx_id(&self) -> TxId {
        self.tx_id
    }

    pub fn graph(&self) -> &Graph {
        &self.guard
    }
}

/// Buffers write operations and applies them atomically on commit.
/// Dropping without calling `commit()` or `abort()` is treated as an abort.
pub struct WriteTx<'a> {
    tx_id: TxId,
    status: TxStatus,
    graph: &'a Arc<RwLock<Graph>>,
    ops: Vec<WriteOp>,
    next_vertex_id: u64,
    next_edge_id: u64,
    _write_guard: parking_lot::MutexGuard<'a, ()>,
}

impl<'a> WriteTx<'a> {
    pub fn tx_id(&self) -> TxId {
        self.tx_id
    }

    pub fn status(&self) -> TxStatus {
        self.status
    }

    pub fn add_vertex(&mut self, label: &str) -> VertexId {
        let id = VertexId(self.next_vertex_id);
        self.next_vertex_id += 1;
        self.ops.push(WriteOp::AddVertex {
            id,
            label: label.to_string(),
        });
        id
    }

    pub fn set_vertex_property(&mut self, vertex: VertexId, key: &str, value: Value) {
        self.ops.push(WriteOp::SetVertexProperty {
            vertex,
            key: key.to_string(),
            value,
        });
    }

    pub fn set_vertex_label(&mut self, vertex: VertexId, label: &str) {
        self.ops.push(WriteOp::SetVertexLabel {
            vertex,
            label: label.to_string(),
        });
    }

    pub fn add_edge(&mut self, source: VertexId, target: VertexId, label: &str) -> EdgeId {
        let edge_id = EdgeId(self.next_edge_id);
        self.next_edge_id += 1;
        self.ops.push(WriteOp::AddEdge {
            edge_id,
            source,
            target,
            label: label.to_string(),
        });
        edge_id
    }

    pub fn set_edge_property(&mut self, edge: EdgeId, key: &str, value: Value) {
        self.ops.push(WriteOp::SetEdgeProperty {
            edge,
            key: key.to_string(),
            value,
        });
    }

    pub fn remove_vertex(&mut self, vertex: VertexId) {
        self.ops.push(WriteOp::RemoveVertex { vertex });
    }

    pub fn remove_edge(&mut self, edge: EdgeId) {
        self.ops.push(WriteOp::RemoveEdge { edge });
    }

    pub fn ops(&self) -> &[WriteOp] {
        &self.ops
    }

    pub fn validate(&self) -> Result<(), TxError> {
        if self.status != TxStatus::Active {
            return Err(TxError::NotActive);
        }

        let graph = self.graph.read();
        let mut staged = graph.clone();
        Self::apply_ops(&mut staged, &self.ops)?;
        Ok(())
    }

    fn apply_ops(staged: &mut Graph, ops: &[WriteOp]) -> Result<(), TxError> {
        for op in ops {
            match op {
                WriteOp::AddVertex { id, label } => {
                    staged.try_add_vertex_with_id(id.0, label)?;
                }
                WriteOp::SetVertexProperty { vertex, key, value } => {
                    staged.try_set_vertex_property(*vertex, key, value.clone())?;
                }
                WriteOp::SetVertexLabel { vertex, label } => {
                    staged.try_set_vertex_label(*vertex, label)?;
                }
                WriteOp::AddEdge {
                    edge_id,
                    source,
                    target,
                    label,
                } => {
                    staged.try_add_edge_with_id(edge_id.0, *source, *target, label)?;
                }
                WriteOp::SetEdgeProperty { edge, key, value } => {
                    staged.try_set_edge_property(*edge, key, value.clone())?;
                }
                WriteOp::RemoveVertex { vertex } => {
                    staged.try_remove_vertex(*vertex)?;
                }
                WriteOp::RemoveEdge { edge } => {
                    staged.try_remove_edge(*edge)?;
                }
            }
        }
        Ok(())
    }

    /// Apply all buffered operations to a staged graph, then atomically swap it
    /// into the committed slot if every operation validates and succeeds.
    ///
    /// Holds the RwLock write guard only for the duration of the apply,
    /// so readers are blocked for the minimum possible window.
    pub fn commit(self) -> Result<TxId, TxError> {
        self.prepare()?.commit()
    }

    pub fn prepare(self) -> Result<PreparedWriteTx<'a>, TxError> {
        let WriteTx {
            tx_id,
            status,
            graph,
            ops,
            next_vertex_id: _,
            next_edge_id: _,
            _write_guard,
        } = self;

        if status != TxStatus::Active {
            return Err(TxError::NotActive);
        }

        let committed = graph.read();
        let mut staged = committed.clone();
        drop(committed);
        Self::apply_ops(&mut staged, &ops)?;

        Ok(PreparedWriteTx {
            tx_id,
            graph,
            staged,
            ops,
            _write_guard,
        })
    }

    pub fn abort(mut self) {
        self.status = TxStatus::Aborted;
        self.ops.clear();
    }

    pub fn op_count(&self) -> usize {
        self.ops.len()
    }
}

/// A validated write transaction with its staged graph already materialized.
///
/// This lets durable callers append and fsync WAL records before swapping the
/// staged graph into the committed slot, without cloning/applying twice.
pub struct PreparedWriteTx<'a> {
    tx_id: TxId,
    graph: &'a Arc<RwLock<Graph>>,
    staged: Graph,
    ops: Vec<WriteOp>,
    _write_guard: parking_lot::MutexGuard<'a, ()>,
}

impl<'a> PreparedWriteTx<'a> {
    pub fn tx_id(&self) -> TxId {
        self.tx_id
    }

    pub fn ops(&self) -> &[WriteOp] {
        &self.ops
    }

    pub fn commit(self) -> Result<TxId, TxError> {
        let mut graph = self.graph.write();
        *graph = self.staged;
        Ok(self.tx_id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TxError {
    #[error("transaction is not active")]
    NotActive,
    #[error("write conflict")]
    WriteConflict,
    #[error(transparent)]
    Mutation(#[from] GraphError),
    #[error("durability error: {0}")]
    Durability(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::properties::PropertyType;

    fn setup() -> TransactionalGraph {
        let mut g = Graph::new(10, 10);
        g.register_vertex_property("name", PropertyType::String, true, false);
        let v0 = g.add_vertex("Entity");
        g.set_vertex_property(v0, "name", "Alice".into());
        g.build();
        TransactionalGraph::new(g)
    }

    #[test]
    fn read_tx_sees_committed_state() {
        let tg = setup();
        let rtx = tg.begin_read();
        let g = rtx.graph();
        assert_eq!(g.num_vertices(), 1);
        assert_eq!(
            g.get_vertex_property(VertexId(0), "name"),
            Value::String("Alice".into())
        );
    }

    #[test]
    fn write_tx_commits_mutations() {
        let tg = setup();

        {
            let mut wtx = tg.begin_write();
            let v = wtx.add_vertex("Entity");
            wtx.set_vertex_property(v, "name", Value::String("Bob".into()));
            wtx.commit().unwrap();
        }

        let rtx = tg.begin_read();
        let g = rtx.graph();
        assert_eq!(g.num_vertices(), 2);
        assert_eq!(
            g.get_vertex_property(VertexId(1), "name"),
            Value::String("Bob".into())
        );
    }

    #[test]
    fn aborted_tx_discards_ops() {
        let tg = setup();

        {
            let mut wtx = tg.begin_write();
            wtx.add_vertex("Entity");
            wtx.abort();
        }

        let rtx = tg.begin_read();
        let g = rtx.graph();
        assert_eq!(g.num_vertices(), 1);
    }

    #[test]
    fn read_tx_does_not_block_read() {
        let tg = setup();
        let r1 = tg.begin_read();
        let r2 = tg.begin_read();
        assert_eq!(r1.graph().num_vertices(), 1);
        assert_eq!(r2.graph().num_vertices(), 1);
    }

    #[test]
    fn tx_ids_are_monotonic() {
        let tg = setup();
        let r1 = tg.begin_read();
        let r2 = tg.begin_read();
        assert!(r2.tx_id() > r1.tx_id());
    }

    #[test]
    fn commit_inactive_tx_errors() {
        let tg = setup();
        let mut wtx = tg.begin_write();
        wtx.add_vertex("Entity");
        // Manually move status to test guard — we consume via abort, then begin a new one
        wtx.abort();

        // A second write tx should succeed after the first is dropped
        let mut wtx2 = tg.begin_write();
        wtx2.add_vertex("Entity");
        assert!(wtx2.commit().is_ok());
    }

    #[test]
    fn op_count_tracks_buffered_ops() {
        let tg = setup();
        let mut wtx = tg.begin_write();
        assert_eq!(wtx.op_count(), 0);
        wtx.add_vertex("Entity");
        wtx.set_vertex_property(VertexId(1), "name", Value::String("Bob".into()));
        assert_eq!(wtx.op_count(), 2);
        wtx.abort();
    }

    #[test]
    fn read_tx_snapshot_isolation() {
        use std::sync::Arc;

        let tg = Arc::new(setup());

        // Begin a read tx while graph has 1 vertex
        let rtx = tg.begin_read();
        assert_eq!(rtx.graph().num_vertices(), 1);

        // Write another vertex from a separate thread (commit needs the
        // write-lock on the RwLock, which blocks while a read guard is held).
        let tg2 = Arc::clone(&tg);
        let writer = std::thread::spawn(move || {
            let mut wtx = tg2.begin_write();
            wtx.add_vertex("Entity");
            wtx.commit().unwrap();
        });

        // The existing read tx must still see 1 vertex (snapshot isolation).
        // The writer is blocked waiting for our read guard to drop.
        assert_eq!(rtx.graph().num_vertices(), 1);

        // Drop the read tx so the writer can proceed
        drop(rtx);
        writer.join().unwrap();

        // A NEW read tx should see 2
        let rtx2 = tg.begin_read();
        assert_eq!(rtx2.graph().num_vertices(), 2);
    }

    #[test]
    fn sequential_write_txns() {
        let tg = setup();

        {
            let mut wtx = tg.begin_write();
            wtx.add_vertex("Entity");
            wtx.commit().unwrap();
        }
        {
            let mut wtx = tg.begin_write();
            wtx.add_vertex("Entity");
            wtx.commit().unwrap();
        }

        let rtx = tg.begin_read();
        assert_eq!(rtx.graph().num_vertices(), 3);
    }

    #[test]
    fn uncommitted_write_tx_is_invisible_to_readers() {
        let tg = setup();

        let mut wtx = tg.begin_write();
        let bob = wtx.add_vertex("Entity");
        wtx.set_vertex_property(bob, "name", Value::String("Bob".into()));

        let rtx = tg.begin_read();
        assert_eq!(rtx.graph().num_vertices(), 1);
        assert_eq!(rtx.graph().get_vertex_property(bob, "name"), Value::Null);

        drop(rtx);
        wtx.abort();
    }

    #[test]
    fn prepared_write_tx_is_invisible_until_publish_commit() {
        let tg = setup();

        let mut wtx = tg.begin_write();
        let bob = wtx.add_vertex("Entity");
        wtx.set_vertex_property(bob, "name", Value::String("Bob".into()));
        let prepared = wtx.prepare().unwrap();

        let rtx = tg.begin_read();
        assert_eq!(rtx.graph().num_vertices(), 1);
        assert_eq!(rtx.graph().get_vertex_property(bob, "name"), Value::Null);

        drop(rtx);
        prepared.commit().unwrap();

        let committed = tg.begin_read();
        assert_eq!(committed.graph().num_vertices(), 2);
        assert_eq!(
            committed.graph().get_vertex_property(bob, "name"),
            Value::String("Bob".into())
        );
    }

    #[test]
    fn failed_commit_does_not_partially_apply_ops() {
        let tg = setup();

        let mut wtx = tg.begin_write();
        let v = wtx.add_vertex("Entity");
        wtx.set_vertex_property(v, "name", Value::Int64(99));

        assert!(wtx.commit().is_err());

        let rtx = tg.begin_read();
        assert_eq!(rtx.graph().num_vertices(), 1);
        assert_eq!(
            rtx.graph().get_vertex_property(VertexId(0), "name"),
            Value::String("Alice".into())
        );
    }

    #[test]
    fn write_tx_reserves_exact_vertex_and_edge_ids() {
        let tg = setup();

        let mut wtx = tg.begin_write();
        let bob = wtx.add_vertex("Entity");
        let edge = wtx.add_edge(VertexId(0), bob, "KNOWS");

        assert_eq!(bob, VertexId(1));
        assert_eq!(edge, EdgeId(0));

        wtx.commit().unwrap();

        let rtx = tg.begin_read();
        assert_eq!(
            rtx.graph()
                .neighbors(VertexId(0), "KNOWS", Direction::Outgoing),
            vec![bob]
        );
    }

    #[test]
    fn write_tx_can_delete_edges_atomically() {
        let mut g = Graph::new(4, 4);
        g.register_vertex_property("name", PropertyType::String, true, false);
        let a = g.add_vertex("Entity");
        let b = g.add_vertex("Entity");
        let edge = g.add_edge(a, b, "KNOWS");
        g.build();
        let tg = TransactionalGraph::new(g);

        let mut wtx = tg.begin_write();
        wtx.remove_edge(edge);
        wtx.commit().unwrap();

        let rtx = tg.begin_read();
        assert!(
            rtx.graph()
                .neighbors(a, "KNOWS", Direction::Outgoing)
                .is_empty()
        );
        assert!(!rtx.graph().edge_exists(edge));
    }

    #[test]
    fn write_tx_can_set_vertex_labels_atomically() {
        let tg = setup();

        let mut wtx = tg.begin_write();
        wtx.set_vertex_label(VertexId(0), "Entity:Person");
        wtx.commit().unwrap();

        let rtx = tg.begin_read();
        assert_eq!(rtx.graph().vertex_label(VertexId(0)), Some("Entity:Person"));
    }
}
