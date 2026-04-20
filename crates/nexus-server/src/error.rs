use thiserror::Error;

#[derive(Error, Debug)]
pub enum ServerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Cypher error: {0}")]
    Cypher(#[from] nexus_cypher::error::CypherError),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("tenant not found: {0}")]
    TenantNotFound(String),

    #[error("graph not built")]
    GraphNotBuilt,
}

pub type ServerResult<T> = Result<T, ServerError>;
