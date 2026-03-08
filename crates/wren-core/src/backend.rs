use async_trait::async_trait;

use crate::crd::WrenJobSpec;
use crate::error::WrenError;
use crate::types::Placement;

/// Resolved user identity for job execution.
/// Populated by the controller from WrenUser CRD lookup.
#[derive(Clone, Debug, Default)]
pub struct UserIdentity {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub supplemental_groups: Vec<u32>,
    pub home_dir: Option<String>,
    pub default_project: Option<String>,
}

/// Result of launching a job via an execution backend.
#[derive(Clone, Debug)]
pub struct LaunchResult {
    /// Backend-specific identifiers for the running workload (e.g., Pod names).
    pub resource_ids: Vec<String>,
    /// Human-readable message about the launch.
    pub message: String,
}

/// Trait for execution backends (container-based or bare-metal).
///
/// The controller uses this trait to abstract over how MPI jobs are actually
/// executed. The scheduling layer decides *where*, the backend decides *how*.
#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    /// Launch the job on the given nodes.
    async fn launch(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &WrenJobSpec,
        placement: &Placement,
        user: Option<&UserIdentity>,
    ) -> Result<LaunchResult, WrenError>;

    /// Check the health/status of a running job's workloads.
    async fn status(&self, job_name: &str, namespace: &str) -> Result<BackendJobStatus, WrenError>;

    /// Terminate a running job's workloads.
    async fn terminate(&self, job_name: &str, namespace: &str) -> Result<(), WrenError>;

    /// Clean up any resources created for the job (services, configmaps, etc.).
    async fn cleanup(&self, job_name: &str, namespace: &str) -> Result<(), WrenError>;
}

/// Status of a job as reported by the execution backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendJobStatus {
    /// Workers are still starting up.
    Launching { ready: u32, total: u32 },
    /// All workers are running, MPI job is active.
    Running,
    /// The job completed successfully.
    Succeeded,
    /// The job failed.
    Failed { message: String },
    /// The job's resources were not found (already cleaned up or never created).
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_launch_result_clone() {
        let result = LaunchResult {
            resource_ids: vec!["pod-0".to_string(), "pod-1".to_string()],
            message: "launched 2 pods".to_string(),
        };
        let cloned = result.clone();
        assert_eq!(cloned.resource_ids, vec!["pod-0", "pod-1"]);
        assert_eq!(cloned.message, "launched 2 pods");
    }

    #[test]
    fn test_backend_job_status_launching() {
        let status = BackendJobStatus::Launching { ready: 2, total: 4 };
        assert_eq!(status, BackendJobStatus::Launching { ready: 2, total: 4 });
        assert_ne!(status, BackendJobStatus::Running);
    }

    #[test]
    fn test_backend_job_status_running() {
        assert_eq!(BackendJobStatus::Running, BackendJobStatus::Running);
        assert_ne!(BackendJobStatus::Running, BackendJobStatus::Succeeded);
    }

    #[test]
    fn test_backend_job_status_succeeded() {
        assert_eq!(BackendJobStatus::Succeeded, BackendJobStatus::Succeeded);
    }

    #[test]
    fn test_backend_job_status_failed() {
        let status = BackendJobStatus::Failed {
            message: "OOM killed".to_string(),
        };
        assert_eq!(
            status,
            BackendJobStatus::Failed {
                message: "OOM killed".to_string()
            }
        );
    }

    #[test]
    fn test_backend_job_status_not_found() {
        assert_eq!(BackendJobStatus::NotFound, BackendJobStatus::NotFound);
    }

    #[test]
    fn test_launch_result_debug() {
        let result = LaunchResult {
            resource_ids: vec!["pod-0".to_string()],
            message: "ok".to_string(),
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("pod-0"));
        assert!(debug.contains("ok"));
    }
}
