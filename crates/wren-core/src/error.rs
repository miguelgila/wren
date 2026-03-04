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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_feasible_placement_display() {
        let err = WrenError::NoFeasiblePlacement {
            job_name: "sim-1".to_string(),
            reason: "insufficient GPUs".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "no feasible placement for job 'sim-1': insufficient GPUs"
        );
    }

    #[test]
    fn test_queue_not_found_display() {
        let err = WrenError::QueueNotFound {
            queue_name: "gpu".to_string(),
        };
        assert_eq!(err.to_string(), "queue 'gpu' not found");
    }

    #[test]
    fn test_job_not_found_display() {
        let err = WrenError::JobNotFound {
            job_name: "my-job".to_string(),
        };
        assert_eq!(err.to_string(), "job 'my-job' not found");
    }

    #[test]
    fn test_invalid_walltime_display() {
        let err = WrenError::InvalidWalltime {
            input: "abc".to_string(),
        };
        assert_eq!(err.to_string(), "invalid walltime format: 'abc'");
    }

    #[test]
    fn test_validation_error_display() {
        let err = WrenError::ValidationError {
            reason: "nodes must be > 0".to_string(),
        };
        assert_eq!(err.to_string(), "job validation failed: nodes must be > 0");
    }

    #[test]
    fn test_backend_error_display() {
        let err = WrenError::BackendError {
            message: "connection refused".to_string(),
        };
        assert_eq!(err.to_string(), "backend error: connection refused");
    }

    #[test]
    fn test_internal_error_display() {
        let err = WrenError::Internal("unexpected state".to_string());
        assert_eq!(err.to_string(), "internal error: unexpected state");
    }

    #[test]
    fn test_serialization_error_from() {
        let json_err = serde_json::from_str::<String>("not json").unwrap_err();
        let err = WrenError::from(json_err);
        assert!(err.to_string().starts_with("serialization error:"));
    }
}
