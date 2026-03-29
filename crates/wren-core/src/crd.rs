use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::*;

/// WrenJob is the primary user-facing CRD for submitting multi-node MPI workloads.
/// This is the Wren equivalent of `sbatch`.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "wren.giar.dev",
    version = "v1alpha1",
    kind = "WrenJob",
    namespaced,
    status = "WrenJobStatus",
    printcolumn = r#"{"name":"JobID","type":"integer","jsonPath":".status.jobId","priority":0}"#,
    printcolumn = r#"{"name":"State","type":"string","jsonPath":".status.state"}"#,
    printcolumn = r#"{"name":"Nodes","type":"integer","jsonPath":".spec.nodes"}"#,
    printcolumn = r#"{"name":"Queue","type":"string","jsonPath":".spec.queue"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct WrenJobSpec {
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

    /// Project for fair-share grouping (optional, user-settable)
    #[serde(default)]
    pub project: Option<String>,
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

#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReaperSpec {
    /// Command to execute on bare metal (e.g., ["python3", "/opt/train/train.py"]).
    /// If not set, `script` is used with `/bin/sh -c`.
    #[serde(default)]
    pub command: Vec<String>,
    /// Arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Job script to execute on bare metal (wrapped in `/bin/sh -c`).
    /// Ignored if `command` is set.
    #[serde(default)]
    pub script: Option<String>,
    /// Environment variables
    #[serde(default)]
    pub environment: std::collections::HashMap<String, String>,
    /// Working directory
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Volumes to mount (ConfigMap, Secret, hostPath, emptyDir)
    #[serde(default)]
    pub volumes: Vec<ReaperVolumeSpec>,
    /// DNS resolution mode: "host" (default) or "kubernetes"
    #[serde(default)]
    pub dns_mode: Option<String>,
    /// Named overlay group for shared overlay filesystem
    #[serde(default)]
    pub overlay_name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReaperVolumeSpec {
    /// Volume name (used internally)
    pub name: String,
    /// Path inside the container/overlay where this volume is mounted
    pub mount_path: String,
    /// Mount as read-only
    #[serde(default)]
    pub read_only: bool,
    /// ConfigMap name to mount
    #[serde(default)]
    pub config_map: Option<String>,
    /// Secret name to mount
    #[serde(default)]
    pub secret: Option<String>,
    /// Host path to mount
    #[serde(default)]
    pub host_path: Option<String>,
    /// Use an emptyDir volume
    #[serde(default)]
    pub empty_dir: bool,
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
pub struct WrenJobStatus {
    /// Sequential numeric job ID (Slurm-style), assigned by the controller
    #[serde(default)]
    pub job_id: Option<u64>,
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

/// WrenQueue defines a scheduling queue with policies.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "wren.giar.dev",
    version = "v1alpha1",
    kind = "WrenQueue",
    namespaced,
    printcolumn = r#"{"name":"MaxNodes","type":"integer","jsonPath":".spec.maxNodes"}"#,
    printcolumn = r#"{"name":"MaxWalltime","type":"string","jsonPath":".spec.maxWalltime"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct WrenQueueSpec {
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

/// WrenUser maps Kubernetes usernames to Unix UID/GID for execution identity.
/// Cluster-scoped — one per user across all namespaces.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "wren.giar.dev",
    version = "v1alpha1",
    kind = "WrenUser",
    printcolumn = r#"{"name":"UID","type":"integer","jsonPath":".spec.uid"}"#,
    printcolumn = r#"{"name":"GID","type":"integer","jsonPath":".spec.gid"}"#,
    printcolumn = r#"{"name":"HomeDir","type":"string","jsonPath":".spec.homeDir"}"#,
    printcolumn = r#"{"name":"Project","type":"string","jsonPath":".spec.defaultProject"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct WrenUserSpec {
    /// Unix UID for process execution
    pub uid: u32,
    /// Primary Unix GID
    pub gid: u32,
    /// Additional GIDs (e.g., project groups)
    #[serde(default)]
    pub supplemental_groups: Vec<u32>,
    /// HOME env var and optional volume mount path
    #[serde(default)]
    pub home_dir: Option<String>,
    /// Default project for fair-share grouping
    #[serde(default)]
    pub default_project: Option<String>,
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
    fn test_wrenjob_spec_defaults() {
        let json = r#"{"nodes": 4}"#;
        let spec: WrenJobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.queue, "default");
        assert_eq!(spec.priority, 50);
        assert_eq!(spec.tasks_per_node, 1);
        assert_eq!(spec.backend, ExecutionBackendType::Container);
        assert_eq!(spec.nodes, 4);
        assert!(spec.dependencies.is_empty());
    }

    #[test]
    fn test_wrenjob_spec_full() {
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
        let spec: WrenJobSpec = serde_yaml::from_str(yaml).unwrap();
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
    fn test_wrenqueue_spec() {
        let json = r#"{"maxNodes": 128}"#;
        let spec: WrenQueueSpec = serde_json::from_str(json).unwrap();
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
        let status = WrenJobStatus::default();
        assert!(status.job_id.is_none());
        assert_eq!(status.state, JobState::Pending);
        assert!(status.message.is_none());
        assert!(status.assigned_nodes.is_empty());
        assert_eq!(status.ready_workers, 0);
    }

    #[test]
    fn test_mpi_spec_defaults() {
        let yaml = r#"
            implementation: openmpi
        "#;
        let spec: MPISpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.implementation, "openmpi");
        assert!(spec.ssh_auth); // default_true
        assert!(spec.fabric_interface.is_none());
    }

    #[test]
    fn test_mpi_spec_full() {
        let yaml = r#"
            implementation: cray-mpich
            sshAuth: false
            fabricInterface: hsn0
        "#;
        let spec: MPISpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.implementation, "cray-mpich");
        assert!(!spec.ssh_auth);
        assert_eq!(spec.fabric_interface.as_deref(), Some("hsn0"));
    }

    #[test]
    fn test_topology_spec_serde() {
        let yaml = r#"
            preferSameSwitch: true
            maxHops: 2
            topologyKey: "topology.kubernetes.io/zone"
        "#;
        let spec: TopologySpec = serde_yaml::from_str(yaml).unwrap();
        assert!(spec.prefer_same_switch);
        assert_eq!(spec.max_hops, Some(2));
        assert_eq!(
            spec.topology_key.as_deref(),
            Some("topology.kubernetes.io/zone")
        );
    }

    #[test]
    fn test_topology_spec_defaults() {
        let yaml = "{}";
        let spec: TopologySpec = serde_yaml::from_str(yaml).unwrap();
        assert!(!spec.prefer_same_switch);
        assert!(spec.max_hops.is_none());
        assert!(spec.topology_key.is_none());
    }

    #[test]
    fn test_reaper_spec_script() {
        let yaml = r#"
            script: |
              #!/bin/bash
              srun ./app
            environment:
              SCRATCH: /scratch
            workingDir: /home/user
        "#;
        let spec: ReaperSpec = serde_yaml::from_str(yaml).unwrap();
        assert!(spec.script.as_ref().unwrap().contains("srun"));
        assert_eq!(spec.environment["SCRATCH"], "/scratch");
        assert_eq!(spec.working_dir.as_deref(), Some("/home/user"));
        assert!(spec.command.is_empty());
        assert!(spec.volumes.is_empty());
    }

    #[test]
    fn test_reaper_spec_command() {
        let yaml = r#"
            command: ["python3", "/opt/train/train.py"]
            args: ["--epochs", "5"]
            environment:
              PYTHONUNBUFFERED: "1"
        "#;
        let spec: ReaperSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.command, vec!["python3", "/opt/train/train.py"]);
        assert_eq!(spec.args, vec!["--epochs", "5"]);
        assert!(spec.script.is_none());
    }

    #[test]
    fn test_reaper_spec_volumes() {
        let yaml = r#"
            command: ["python3", "/opt/train/train.py"]
            volumes:
              - name: training-script
                mountPath: /opt/train
                readOnly: true
                configMap: pytorch-ddp-train
              - name: secrets
                mountPath: /etc/secrets
                readOnly: true
                secret: my-secret
              - name: scratch
                mountPath: /scratch
                hostPath: /mnt/scratch
              - name: tmp
                mountPath: /tmp/work
                emptyDir: true
        "#;
        let spec: ReaperSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.volumes.len(), 4);
        assert_eq!(spec.volumes[0].name, "training-script");
        assert_eq!(spec.volumes[0].mount_path, "/opt/train");
        assert!(spec.volumes[0].read_only);
        assert_eq!(
            spec.volumes[0].config_map.as_deref(),
            Some("pytorch-ddp-train")
        );
        assert_eq!(spec.volumes[1].secret.as_deref(), Some("my-secret"));
        assert_eq!(spec.volumes[2].host_path.as_deref(), Some("/mnt/scratch"));
        assert!(spec.volumes[3].empty_dir);
    }

    #[test]
    fn test_reaper_spec_dns_overlay() {
        let yaml = r#"
            command: ["./app"]
            dnsMode: kubernetes
            overlayName: shared-team
        "#;
        let spec: ReaperSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.dns_mode.as_deref(), Some("kubernetes"));
        assert_eq!(spec.overlay_name.as_deref(), Some("shared-team"));
    }

    #[test]
    fn test_dependency_type_serde() {
        let json = r#""afterOk""#;
        let dt: DependencyType = serde_json::from_str(json).unwrap();
        assert_eq!(dt, DependencyType::AfterOk);

        let json = r#""afterAny""#;
        let dt: DependencyType = serde_json::from_str(json).unwrap();
        assert_eq!(dt, DependencyType::AfterAny);

        let json = r#""afterNotOk""#;
        let dt: DependencyType = serde_json::from_str(json).unwrap();
        assert_eq!(dt, DependencyType::AfterNotOk);
    }

    #[test]
    fn test_job_dependency_serde() {
        let yaml = r#"
            type: afterOk
            job: previous-sim
        "#;
        let dep: JobDependency = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(dep.dep_type, DependencyType::AfterOk);
        assert_eq!(dep.job, "previous-sim");
    }

    #[test]
    fn test_resource_requirements_serde() {
        let yaml = r#"
            limits:
              nvidia.com/gpu: "4"
              memory: "64Gi"
            requests:
              cpu: "4000m"
        "#;
        let rr: ResourceRequirements = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(rr.limits["nvidia.com/gpu"], "4");
        assert_eq!(rr.limits["memory"], "64Gi");
        assert_eq!(rr.requests["cpu"], "4000m");
    }

    #[test]
    fn test_volume_mount_serde() {
        let yaml = r#"
            name: data
            mountPath: /mnt/data
            readOnly: true
        "#;
        let vm: VolumeMount = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(vm.name, "data");
        assert_eq!(vm.mount_path, "/mnt/data");
        assert!(vm.read_only);
    }

    #[test]
    fn test_env_var_serde() {
        let json = r#"{"name": "FOO", "value": "bar"}"#;
        let ev: EnvVar = serde_json::from_str(json).unwrap();
        assert_eq!(ev.name, "FOO");
        assert_eq!(ev.value, "bar");
    }

    #[test]
    fn test_backfill_config_serde() {
        let yaml = r#"
            enabled: true
            lookAhead: "2h"
        "#;
        let bc: BackfillConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(bc.enabled);
        assert_eq!(bc.look_ahead.as_deref(), Some("2h"));
    }

    #[test]
    fn test_fair_share_config_serde() {
        let yaml = r#"
            enabled: true
            decayHalfLife: "7d"
        "#;
        let fs: FairShareConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(fs.enabled);
        assert_eq!(fs.decay_half_life.as_deref(), Some("7d"));
    }

    #[test]
    fn test_wrenqueue_spec_full() {
        let yaml = r#"
            maxNodes: 64
            maxWalltime: "24h"
            maxJobsPerUser: 10
            defaultPriority: 100
            backfill:
              enabled: true
              lookAhead: "4h"
            fairShare:
              enabled: true
              decayHalfLife: "14d"
        "#;
        let spec: WrenQueueSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.max_nodes, 64);
        assert_eq!(spec.max_walltime.as_deref(), Some("24h"));
        assert_eq!(spec.max_jobs_per_user, Some(10));
        assert_eq!(spec.default_priority, 100);
        assert!(spec.backfill.unwrap().enabled);
        assert!(spec.fair_share.unwrap().enabled);
    }

    #[test]
    fn test_wrenjob_status_serde_roundtrip() {
        let status = WrenJobStatus {
            job_id: Some(42),
            state: JobState::Running,
            message: Some("all workers ready".to_string()),
            assigned_nodes: vec!["node-0".to_string(), "node-1".to_string()],
            start_time: Some("2024-01-01T00:00:00Z".to_string()),
            completion_time: None,
            ready_workers: 2,
            total_workers: 2,
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: WrenJobStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.job_id, Some(42));
        assert_eq!(parsed.state, JobState::Running);
        assert_eq!(parsed.ready_workers, 2);
        assert_eq!(parsed.assigned_nodes.len(), 2);
    }

    #[test]
    fn test_wrenjob_spec_with_container_and_mpi() {
        let yaml = r#"
            nodes: 4
            tasksPerNode: 8
            queue: gpu
            priority: 200
            walltime: "8h"
            container:
              image: "nvcr.io/nvidia/pytorch:24.01"
              command: ["python", "train.py"]
              hostNetwork: true
              env:
                - name: NCCL_DEBUG
                  value: INFO
            mpi:
              implementation: openmpi
              sshAuth: true
              fabricInterface: hsn0
            topology:
              preferSameSwitch: true
              maxHops: 2
        "#;
        let spec: WrenJobSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.nodes, 4);
        assert_eq!(spec.tasks_per_node, 8);
        let container = spec.container.unwrap();
        assert_eq!(container.image, "nvcr.io/nvidia/pytorch:24.01");
        assert!(container.host_network);
        assert_eq!(container.env.len(), 1);
        let mpi = spec.mpi.unwrap();
        assert_eq!(mpi.implementation, "openmpi");
        assert_eq!(mpi.fabric_interface.as_deref(), Some("hsn0"));
        let topo = spec.topology.unwrap();
        assert!(topo.prefer_same_switch);
        assert_eq!(topo.max_hops, Some(2));
    }

    // --- WrenUser tests ---

    #[test]
    fn test_wrenuser_spec_serde() {
        let yaml = r#"
            uid: 1001
            gid: 1001
            supplementalGroups: [1001, 5000]
            homeDir: /home/miguel
            defaultProject: climate-sim
        "#;
        let spec: WrenUserSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.uid, 1001);
        assert_eq!(spec.gid, 1001);
        assert_eq!(spec.supplemental_groups, vec![1001, 5000]);
        assert_eq!(spec.home_dir.as_deref(), Some("/home/miguel"));
        assert_eq!(spec.default_project.as_deref(), Some("climate-sim"));
    }

    #[test]
    fn test_wrenuser_spec_minimal() {
        let yaml = r#"
            uid: 2000
            gid: 2000
        "#;
        let spec: WrenUserSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.uid, 2000);
        assert_eq!(spec.gid, 2000);
        assert!(spec.supplemental_groups.is_empty());
        assert!(spec.home_dir.is_none());
        assert!(spec.default_project.is_none());
    }

    #[test]
    fn test_wrenuser_spec_roundtrip() {
        let spec = WrenUserSpec {
            uid: 1001,
            gid: 1001,
            supplemental_groups: vec![1001, 5000],
            home_dir: Some("/home/miguel".to_string()),
            default_project: Some("climate-sim".to_string()),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: WrenUserSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.uid, 1001);
        assert_eq!(parsed.supplemental_groups, vec![1001, 5000]);
    }

    // --- project field tests ---

    #[test]
    fn test_wrenjob_spec_with_project() {
        let yaml = r#"
            nodes: 2
            project: climate-sim
        "#;
        let spec: WrenJobSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.project.as_deref(), Some("climate-sim"));
    }

    #[test]
    fn test_wrenjob_spec_without_project() {
        let json = r#"{"nodes": 4}"#;
        let spec: WrenJobSpec = serde_json::from_str(json).unwrap();
        assert!(spec.project.is_none());
    }
}
