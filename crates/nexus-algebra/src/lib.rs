//! GraphBLAS-inspired sparse matrix algebra for Domyn Nexus.
//!
//! This crate provides the linear algebra execution layer that sits between
//! the query engine and the CSR storage. Instead of recursive pointer-chasing
//! traversal, multi-hop operations execute as sparse matrix-vector multiplies.
//!
//! Key operations:
//! - `SpMV`: sparse matrix-vector multiply (1-hop traversal)
//! - `SpMM`: sparse matrix-matrix multiply (pattern composition)
//! - Semiring traits for graph algorithms (PageRank, BFS, CC)
//! - Masked operations for filtered traversal
//!
//! Design influence: FalkorDB's GraphBLAS-based execution model.

pub mod semiring;
pub mod sparse_vector;
pub mod spmv;
