use async_trait::async_trait;

use crate::crd::BuboJobSpec;
use crate::error::BuboError;
use crate::types::Placement;

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
        spec: &BuboJobSpec,
        placement: &Placement,
    ) -> Result<LaunchResult, BuboError>;

    /// Check the health/status of a running job's workloads.
    async fn status(
        &self,
        job_name: &str,
        namespace: &str,
    ) -> Result<BackendJobStatus, BuboError>;

    /// Terminate a running job's workloads.
    async fn terminate(
        &self,
        job_name: &str,
        namespace: &str,
    ) -> Result<(), BuboError>;

    /// Clean up any resources created for the job (services, configmaps, etc.).
    async fn cleanup(
        &self,
        job_name: &str,
        namespace: &str,
    ) -> Result<(), BuboError>;
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
