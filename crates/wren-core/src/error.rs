use thiserror::Error;

#[derive(Error, Debug)]
pub enum WrenError {
    #[error("no feasible placement for job '{job_name}': {reason}")]
    NoFeasiblePlacement { job_name: String, reason: String },

    #[error("queue '{queue_name}' not found")]
    QueueNotFound { queue_name: String },

    #[error("job '{job_name}' not found")]
    JobNotFound { job_name: String },

    #[error("invalid walltime format: '{input}'")]
    InvalidWalltime { input: String },

    #[error("job validation failed: {reason}")]
    ValidationError { reason: String },

    #[error("backend error: {message}")]
    BackendError { message: String },

    #[error("kubernetes API error: {0}")]
    KubeError(#[from] kube::Error),

    #[error("serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, WrenError>;
