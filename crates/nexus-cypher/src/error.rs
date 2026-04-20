use thiserror::Error;

#[derive(Error, Debug)]
pub enum CypherError {
    #[error("parse error at position {position}: {message}")]
    Parse { position: usize, message: String },

    #[error("unexpected token: expected {expected}, got {got}")]
    UnexpectedToken { expected: String, got: String },

    #[error("unknown function: {0}")]
    UnknownFunction(String),

    #[error("plan error: {0}")]
    Plan(String),

    #[error("execution error: {0}")]
    Execution(String),

    #[error("label not found: {0}")]
    LabelNotFound(String),

    #[error("property not found: {0}")]
    PropertyNotFound(String),
}

pub type CypherResult<T> = Result<T, CypherError>;
