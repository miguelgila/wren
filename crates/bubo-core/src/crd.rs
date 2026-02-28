use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::*;

/// MPIJob is the primary user-facing CRD for submitting multi-node MPI workloads.
/// This is the Bubo equivalent of `sbatch`.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "hpc.cscs.ch",
    version = "v1alpha1",
    kind = "MPIJob",
    namespaced,
    status = "MPIJobStatus",
    printcolumn = r#"{"name":"State","type":"string","jsonPath":".status.state"}"#,
    printcolumn = r#"{"name":"Nodes","type":"integer","jsonPath":".spec.nodes"}"#,
    printcolumn = r#"{"name":"Queue","type":"string","jsonPath":".spec.queue"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MPIJobSpec {
    /// Queue to submit this job to
    #[serde(default = "default_queue")]
    pub queue: String,

    /// Job priority (higher = more important)
    #[serde(default = "default_priority")]
    pub priority: i32,

    /// Maximum wall time (e.g., "4h", "30m", "1d")
    #[serde(default)]
    pub walltime: Option<String>,

    /// Number of nodes required (gang scheduling unit)
    pub nodes: u32,

    /// MPI ranks per node
    #[serde(default = "default_tasks_per_node")]
    pub tasks_per_node: u32,

    /// Execution backend: "container" (default) or "reaper"
    #[serde(default)]
    pub backend: ExecutionBackendType,

    /// Container backend configuration
    #[serde(default)]
    pub container: Option<ContainerSpec>,

    /// Reaper backend configuration (bare-metal execution)
    #[serde(default)]
    pub reaper: Option<ReaperSpec>,

    /// MPI configuration
    #[serde(default)]
    pub mpi: Option<MPISpec>,

    /// Topology placement preferences
    #[serde(default)]
    pub topology: Option<TopologySpec>,

    /// Job dependencies (Slurm-style)
    #[serde(default)]
    pub dependencies: Vec<JobDependency>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSpec {
    pub image: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub resources: Option<ResourceRequirements>,
    /// Use host networking (required for Slingshot/RDMA)
    #[serde(default)]
    pub host_network: bool,
    /// Additional volume mounts
    #[serde(default)]
    pub volume_mounts: Vec<VolumeMount>,
    /// Environment variables
    #[serde(default)]
    pub env: Vec<EnvVar>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReaperSpec {
    /// Job script to execute on bare metal
    pub script: String,
    /// Environment variables
    #[serde(default)]
    pub environment: std::collections::HashMap<String, String>,
    /// Working directory
    #[serde(default)]
    pub working_dir: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MPISpec {
    /// MPI implementation: cray-mpich, openmpi, intel-mpi
    #[serde(default = "default_mpi_impl")]
    pub implementation: String,
    /// Use SSH-based MPI bootstrap (mount shared keys)
    #[serde(default = "default_true")]
    pub ssh_auth: bool,
    /// Network interface for MPI traffic (e.g., hsn0)
    #[serde(default)]
    pub fabric_interface: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TopologySpec {
    /// Prefer placing all pods on nodes sharing the same network switch
    #[serde(default)]
    pub prefer_same_switch: bool,
    /// Maximum allowed network hops between any two nodes in the placement
    #[serde(default)]
    pub max_hops: Option<u32>,
    /// Kubernetes label key for topology grouping
    #[serde(default)]
    pub topology_key: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobDependency {
    /// Dependency type: afterOk, afterAny, afterNotOk
    #[serde(rename = "type")]
    pub dep_type: DependencyType,
    /// Name of the job this depends on
    pub job: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
pub enum DependencyType {
    #[serde(rename = "afterOk")]
    AfterOk,
    #[serde(rename = "afterAny")]
    AfterAny,
    #[serde(rename = "afterNotOk")]
    AfterNotOk,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct MPIJobStatus {
    /// Current state of the job
    #[serde(default)]
    pub state: JobState,
    /// Human-readable message
    #[serde(default)]
    pub message: Option<String>,
    /// Nodes assigned to this job
    #[serde(default)]
    pub assigned_nodes: Vec<String>,
    /// Time the job started running
    #[serde(default)]
    pub start_time: Option<String>,
    /// Time the job completed
    #[serde(default)]
    pub completion_time: Option<String>,
    /// Number of ready workers
    #[serde(default)]
    pub ready_workers: u32,
    /// Total workers expected
    #[serde(default)]
    pub total_workers: u32,
}

/// BuboQueue defines a scheduling queue with policies.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "hpc.cscs.ch",
    version = "v1alpha1",
    kind = "BuboQueue",
    namespaced,
    printcolumn = r#"{"name":"MaxNodes","type":"integer","jsonPath":".spec.maxNodes"}"#,
    printcolumn = r#"{"name":"MaxWalltime","type":"string","jsonPath":".spec.maxWalltime"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct BuboQueueSpec {
    /// Maximum total nodes this queue can consume
    pub max_nodes: u32,
    /// Maximum walltime allowed for jobs in this queue
    #[serde(default)]
    pub max_walltime: Option<String>,
    /// Maximum concurrent jobs per user
    #[serde(default)]
    pub max_jobs_per_user: Option<u32>,
    /// Default priority for jobs without explicit priority
    #[serde(default = "default_priority")]
    pub default_priority: i32,
    /// Backfill configuration
    #[serde(default)]
    pub backfill: Option<BackfillConfig>,
    /// Fair-share configuration
    #[serde(default)]
    pub fair_share: Option<FairShareConfig>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackfillConfig {
    pub enabled: bool,
    /// How far ahead to project resource availability
    #[serde(default)]
    pub look_ahead: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FairShareConfig {
    pub enabled: bool,
    /// Usage history decay half-life (e.g., "7d")
    #[serde(default)]
    pub decay_half_life: Option<String>,
}

// --- Simple placeholder types to avoid pulling in full k8s_openapi for CRD schema ---

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRequirements {
    #[serde(default)]
    pub limits: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub requests: std::collections::HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VolumeMount {
    pub name: String,
    pub mount_path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

// --- Defaults ---

fn default_queue() -> String {
    "default".to_string()
}

fn default_priority() -> i32 {
    50
}

fn default_tasks_per_node() -> u32 {
    1
}

fn default_mpi_impl() -> String {
    "openmpi".to_string()
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mpijob_spec_defaults() {
        let json = r#"{"nodes": 4}"#;
        let spec: MPIJobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.queue, "default");
        assert_eq!(spec.priority, 50);
        assert_eq!(spec.tasks_per_node, 1);
        assert_eq!(spec.backend, ExecutionBackendType::Container);
        assert_eq!(spec.nodes, 4);
        assert!(spec.dependencies.is_empty());
    }

    #[test]
    fn test_mpijob_spec_full() {
        let yaml = r#"
            nodes: 8
            queue: gpu
            priority: 200
            walltime: "4h"
            tasksPerNode: 4
            backend: reaper
            dependencies:
              - type: afterOk
                job: previous-job
        "#;
        let spec: MPIJobSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.nodes, 8);
        assert_eq!(spec.queue, "gpu");
        assert_eq!(spec.priority, 200);
        assert_eq!(spec.walltime.as_deref(), Some("4h"));
        assert_eq!(spec.tasks_per_node, 4);
        assert_eq!(spec.backend, ExecutionBackendType::Reaper);
        assert_eq!(spec.dependencies.len(), 1);
        assert_eq!(spec.dependencies[0].dep_type, DependencyType::AfterOk);
        assert_eq!(spec.dependencies[0].job, "previous-job");
    }

    #[test]
    fn test_buboqueue_spec() {
        let json = r#"{"maxNodes": 128}"#;
        let spec: BuboQueueSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.max_nodes, 128);
        assert_eq!(spec.default_priority, 50);
        assert!(spec.backfill.is_none());
    }

    #[test]
    fn test_container_spec_serialization() {
        let spec = ContainerSpec {
            image: "pytorch:latest".to_string(),
            command: vec!["mpirun".to_string()],
            args: vec!["-np".to_string(), "4".to_string()],
            resources: None,
            host_network: true,
            volume_mounts: vec![],
            env: vec![EnvVar {
                name: "DEBUG".to_string(),
                value: "1".to_string(),
            }],
        };
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: ContainerSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.image, "pytorch:latest");
        assert!(parsed.host_network);
        assert_eq!(parsed.env.len(), 1);
    }

    #[test]
    fn test_job_status_defaults() {
        let status = MPIJobStatus::default();
        assert_eq!(status.state, JobState::Pending);
        assert!(status.message.is_none());
        assert!(status.assigned_nodes.is_empty());
        assert_eq!(status.ready_workers, 0);
    }
}
