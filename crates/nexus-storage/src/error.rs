use thiserror::Error;

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),

    #[error("redb database error: {0}")]
    RedbDatabase(#[from] redb::DatabaseError),

    #[error("redb transaction error: {0}")]
    RedbTransaction(#[from] redb::TransactionError),

    #[error("redb table error: {0}")]
    RedbTable(#[from] redb::TableError),

    #[error("redb commit error: {0}")]
    RedbCommit(#[from] redb::CommitError),

    #[error("redb storage error: {0}")]
    RedbStorage(#[from] redb::StorageError),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("WAL corrupted: {0}")]
    WalCorrupted(String),

    #[error("snapshot not found: {0}")]
    SnapshotNotFound(u64),
}

pub type StorageResult<T> = Result<T, StorageError>;
