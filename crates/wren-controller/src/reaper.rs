use async_trait::async_trait;
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, PostParams};
use kube::{Api, Client};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use tracing::{debug, info, warn};
use wren_core::backend::{BackendJobStatus, ExecutionBackend, LaunchResult, UserIdentity};
use wren_core::{Placement, WrenError, WrenJobSpec};

use crate::mpi;

// ---------------------------------------------------------------------------
// ReaperPod CRD status type (used to deserialize status from K8s API)
// ---------------------------------------------------------------------------

/// Status of a ReaperPod CRD, as set by the reaper-controller.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ReaperPodStatus {
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    pod_name: Option<String>,
    #[serde(default)]
    node_name: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    message: Option<String>,
}

// ---------------------------------------------------------------------------
// Backward-compat types (test-only, kept for serde roundtrip verification)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ReaperBackend — creates ReaperPod CRDs via the Kubernetes API
// ---------------------------------------------------------------------------

/// Bare-metal execution backend that creates ReaperPod CRDs on each allocated node.
///
/// The reaper-controller (in the reaper project) watches ReaperPod CRDs and
/// translates them into real Pods with `runtimeClassName: reaper-v2`. This gives
/// Kubernetes-native volumes, logs, exec, and lifecycle management while running
/// processes directly on bare metal.
///
/// For MPI jobs with hostfiles, a ConfigMap is created to hold the hostfile
/// content and mounted as a volume on each ReaperPod.
pub struct ReaperBackend {
    client: Client,
}

impl ReaperBackend {
    /// Create a new `ReaperBackend` with a Kubernetes client.
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Build the environment variables for a specific rank.
    fn build_rank_env(
        &self,
        job_name: &str,
        spec: &WrenJobSpec,
        placement: &Placement,
        rank: u32,
        user: Option<&UserIdentity>,
    ) -> Result<HashMap<String, String>, WrenError> {
        let reaper_spec = spec
            .reaper
            .as_ref()
            .ok_or_else(|| WrenError::ValidationError {
                reason: "reaper spec required for reaper backend".to_string(),
            })?;

        // Start with user-provided environment
        let mut environment = reaper_spec.environment.clone();

        // Hostfile
        let hostfile_content = mpi::generate_hostfile(placement, spec.tasks_per_node);
        let hostfile_path = format!("/tmp/wren-hostfile-{}", job_name);

        // MPI-implementation-specific env vars
        if spec.mpi.is_some() {
            let mpi_env =
                mpi::bare_metal_env_vars(job_name, spec, &placement.nodes, &hostfile_path);
            for (k, v) in mpi_env {
                environment.insert(k, v);
            }
            let launcher_args = mpi::bare_metal_mpirun_args(spec, &hostfile_path);
            environment.insert("WREN_MPI_LAUNCHER".to_string(), launcher_args.join(" "));
        }

        // Per-rank env vars
        environment.insert("WREN_MPI_RANK".to_string(), rank.to_string());
        environment.insert("WREN_LOCAL_RANK".to_string(), "0".to_string());

        // Distributed training env vars (PyTorch/NCCL compatible)
        let master_addr = placement.nodes.first().cloned().unwrap_or_default();
        environment.insert("MASTER_ADDR".to_string(), master_addr);
        environment.insert("MASTER_PORT".to_string(), "29500".to_string());
        environment.insert("RANK".to_string(), rank.to_string());
        environment.insert("WORLD_SIZE".to_string(), mpi::total_ranks(spec).to_string());
        environment.insert("LOCAL_RANK".to_string(), "0".to_string());

        // Hostfile content as env var
        environment.insert(
            "WREN_HOSTFILE_CONTENT".to_string(),
            hostfile_content.clone(),
        );
        environment.insert("WREN_HOSTFILE_PATH".to_string(), hostfile_path);

        environment
            .entry("WREN_JOB_NAME".to_string())
            .or_insert_with(|| job_name.to_string());
        environment
            .entry("WREN_NUM_NODES".to_string())
            .or_insert_with(|| spec.nodes.to_string());
        environment
            .entry("WREN_TOTAL_RANKS".to_string())
            .or_insert_with(|| mpi::total_ranks(spec).to_string());

        // User identity env vars
        if let Some(u) = user {
            environment.insert("USER".to_string(), u.username.clone());
            environment.insert("LOGNAME".to_string(), u.username.clone());
            if let Some(ref home) = u.home_dir {
                environment.insert("HOME".to_string(), home.clone());
            }
        }

        Ok(environment)
    }

    /// Build a ReaperPod JSON object for a specific rank.
    #[allow(clippy::too_many_arguments)]
    fn build_reaper_pod(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &WrenJobSpec,
        placement: &Placement,
        rank: u32,
        node: &str,
        user: Option<&UserIdentity>,
    ) -> Result<serde_json::Value, WrenError> {
        let reaper_spec = spec
            .reaper
            .as_ref()
            .ok_or_else(|| WrenError::ValidationError {
                reason: "reaper spec required for reaper backend".to_string(),
            })?;

        let environment = self.build_rank_env(job_name, spec, placement, rank, user)?;

        // Convert env HashMap to ReaperEnvVar list
        let env: Vec<serde_json::Value> = environment
            .iter()
            .map(|(k, v)| json!({"name": k, "value": v}))
            .collect();

        // Volumes: hostfile as ConfigMap if MPI is configured
        let mut volumes: Vec<serde_json::Value> = Vec::new();
        if spec.mpi.is_some() {
            let hostfile_cm_name = format!("{}-hostfile", job_name);
            volumes.push(json!({
                "name": "hostfile",
                "mountPath": format!("/tmp/wren-hostfile-{}", job_name),
                "readOnly": true,
                "configMap": hostfile_cm_name
            }));
        }

        let pod_name = format!("{}-rank-{}", job_name, rank);

        let mut pod_spec = json!({
            "command": ["/bin/sh", "-c"],
            "args": [reaper_spec.script],
            "env": env,
            "nodeName": node,
            "volumes": volumes,
            "restartPolicy": "Never"
        });

        // Working directory
        if let Some(ref wd) = reaper_spec.working_dir {
            pod_spec["workingDir"] = json!(wd);
        }

        // User identity
        if let Some(u) = user {
            pod_spec["runAsUser"] = json!(u.uid as i64);
            pod_spec["runAsGroup"] = json!(u.gid as i64);
            if !u.supplemental_groups.is_empty() {
                let groups: Vec<i64> = u.supplemental_groups.iter().map(|g| *g as i64).collect();
                pod_spec["supplementalGroups"] = json!(groups);
            }
        }

        let mut labels = BTreeMap::new();
        labels.insert(
            "wren.giar.dev/job".to_string(),
            job_name.to_string(),
        );
        labels.insert("wren.giar.dev/role".to_string(), "worker".to_string());
        labels.insert("wren.giar.dev/rank".to_string(), rank.to_string());
        labels.insert(
            "app.kubernetes.io/managed-by".to_string(),
            "wren".to_string(),
        );

        Ok(json!({
            "apiVersion": "reaper.io/v1alpha1",
            "kind": "ReaperPod",
            "metadata": {
                "name": pod_name,
                "namespace": namespace,
                "labels": labels
            },
            "spec": pod_spec
        }))
    }

    /// Create a ConfigMap with the MPI hostfile content.
    async fn create_hostfile_configmap(
        &self,
        job_name: &str,
        namespace: &str,
        placement: &Placement,
        tasks_per_node: u32,
    ) -> Result<(), WrenError> {
        let hostfile_content = mpi::generate_hostfile(placement, tasks_per_node);
        let cm_name = format!("{}-hostfile", job_name);

        let mut data = BTreeMap::new();
        data.insert("hostfile".to_string(), hostfile_content);

        let mut labels = BTreeMap::new();
        labels.insert("wren.giar.dev/job".to_string(), job_name.to_string());
        labels.insert(
            "app.kubernetes.io/managed-by".to_string(),
            "wren".to_string(),
        );

        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(cm_name.clone()),
                namespace: Some(namespace.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };

        let cm_api: Api<ConfigMap> = Api::namespaced(self.client.clone(), namespace);
        match cm_api.create(&PostParams::default(), &cm).await {
            Ok(_) => debug!(configmap = %cm_name, "created hostfile ConfigMap"),
            Err(kube::Error::Api(ref err)) if err.code == 409 => {
                warn!(configmap = %cm_name, "hostfile ConfigMap already exists, continuing");
            }
            Err(e) => return Err(WrenError::KubeError(e)),
        }
        Ok(())
    }

    /// Read the status of a ReaperPod by name.
    async fn get_reaper_pod_status(
        &self,
        pod_name: &str,
        namespace: &str,
    ) -> Result<Option<ReaperPodStatus>, WrenError> {
        let api_resource = kube::api::ApiResource {
            group: "reaper.io".to_string(),
            version: "v1alpha1".to_string(),
            api_version: "reaper.io/v1alpha1".to_string(),
            kind: "ReaperPod".to_string(),
            plural: "reaperpods".to_string(),
        };
        let api: Api<kube::api::DynamicObject> =
            Api::namespaced_with(self.client.clone(), namespace, &api_resource);

        match api.get(pod_name).await {
            Ok(obj) => {
                let status: Option<ReaperPodStatus> = obj
                    .data
                    .get("status")
                    .and_then(|s| serde_json::from_value(s.clone()).ok());
                Ok(status)
            }
            Err(kube::Error::Api(ref err)) if err.code == 404 => Ok(None),
            Err(e) => Err(WrenError::KubeError(e)),
        }
    }

}

#[async_trait]
impl ExecutionBackend for ReaperBackend {
    async fn launch(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &WrenJobSpec,
        placement: &Placement,
        user: Option<&UserIdentity>,
    ) -> Result<LaunchResult, WrenError> {
        info!(
            job = job_name,
            nodes = ?placement.nodes,
            "launching bare-metal job via ReaperPod CRDs"
        );

        // 1. Create hostfile ConfigMap if MPI is configured
        if spec.mpi.is_some() {
            self.create_hostfile_configmap(job_name, namespace, placement, spec.tasks_per_node)
                .await?;
        }

        // 2. Create a ReaperPod for each rank
        let api_resource = kube::api::ApiResource {
            group: "reaper.io".to_string(),
            version: "v1alpha1".to_string(),
            api_version: "reaper.io/v1alpha1".to_string(),
            kind: "ReaperPod".to_string(),
            plural: "reaperpods".to_string(),
        };
        let api: Api<kube::api::DynamicObject> =
            Api::namespaced_with(self.client.clone(), namespace, &api_resource);

        let mut resource_ids = Vec::new();
        for (rank, node) in placement.nodes.iter().enumerate() {
            let reaper_pod =
                self.build_reaper_pod(job_name, namespace, spec, placement, rank as u32, node, user)?;
            let pod_name = format!("{}-rank-{}", job_name, rank);

            let obj: kube::api::DynamicObject = serde_json::from_value(reaper_pod)
                .map_err(|e| WrenError::BackendError {
                    message: format!("failed to build ReaperPod object: {e}"),
                })?;

            match api.create(&PostParams::default(), &obj).await {
                Ok(_) => {
                    debug!(reaperpod = %pod_name, node = %node, rank, "created ReaperPod");
                }
                Err(kube::Error::Api(ref err)) if err.code == 409 => {
                    warn!(reaperpod = %pod_name, "ReaperPod already exists, continuing");
                }
                Err(e) => return Err(WrenError::KubeError(e)),
            }

            resource_ids.push(pod_name);
        }

        Ok(LaunchResult {
            resource_ids,
            message: format!(
                "created {} ReaperPod CRDs across {} nodes",
                placement.nodes.len(),
                placement.nodes.len()
            ),
        })
    }

    async fn status(
        &self,
        job_name: &str,
        namespace: &str,
    ) -> Result<BackendJobStatus, WrenError> {
        // Check rank 0 as the representative status (same as old HTTP approach).
        // TODO: aggregate across all ranks for multi-node jobs.
        let pod_name = format!("{}-rank-0", job_name);
        let status = self.get_reaper_pod_status(&pod_name, namespace).await?;

        match status {
            None => Ok(BackendJobStatus::NotFound),
            Some(s) => Ok(map_phase_to_status(&s)),
        }
    }

    async fn terminate(&self, job_name: &str, namespace: &str) -> Result<(), WrenError> {
        let api_resource = kube::api::ApiResource {
            group: "reaper.io".to_string(),
            version: "v1alpha1".to_string(),
            api_version: "reaper.io/v1alpha1".to_string(),
            kind: "ReaperPod".to_string(),
            plural: "reaperpods".to_string(),
        };
        let api: Api<kube::api::DynamicObject> =
            Api::namespaced_with(self.client.clone(), namespace, &api_resource);

        // Delete all ReaperPods with the job label
        let lp = kube::api::ListParams::default()
            .labels(&format!("wren.giar.dev/job={}", job_name));

        match api.delete_collection(&DeleteParams::default(), &lp).await {
            Ok(_) => {
                info!(job = job_name, "terminated ReaperPods");
            }
            Err(e) => {
                warn!(job = job_name, error = %e, "failed to delete ReaperPods");
            }
        }

        Ok(())
    }

    async fn cleanup(&self, job_name: &str, namespace: &str) -> Result<(), WrenError> {
        // 1. Terminate ReaperPods
        self.terminate(job_name, namespace).await?;

        // 2. Delete hostfile ConfigMap
        let cm_api: Api<ConfigMap> = Api::namespaced(self.client.clone(), namespace);
        let cm_name = format!("{}-hostfile", job_name);
        match cm_api.delete(&cm_name, &DeleteParams::default()).await {
            Ok(_) => debug!(configmap = %cm_name, "deleted hostfile ConfigMap"),
            Err(kube::Error::Api(ref err)) if err.code == 404 => {
                debug!(configmap = %cm_name, "hostfile ConfigMap not found, already cleaned up");
            }
            Err(e) => {
                warn!(configmap = %cm_name, error = %e, "failed to delete hostfile ConfigMap");
            }
        }

        debug!(job = job_name, "cleaned up Reaper job resources");
        Ok(())
    }
}

/// Map a ReaperPod phase to the generic BackendJobStatus.
fn map_phase_to_status(status: &ReaperPodStatus) -> BackendJobStatus {
    match status.phase.as_deref() {
        Some("Succeeded") => BackendJobStatus::Succeeded,
        Some("Failed") => BackendJobStatus::Failed {
            message: status.message.clone().unwrap_or_else(|| {
                format!("job exited with code {}", status.exit_code.unwrap_or(-1))
            }),
        },
        Some("Running") => BackendJobStatus::Running,
        Some("Pending") | None => BackendJobStatus::Launching { ready: 0, total: 1 },
        Some(other) => {
            warn!(phase = %other, "unknown ReaperPod phase");
            BackendJobStatus::Launching { ready: 0, total: 1 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wren_core::{ExecutionBackendType, WrenJobSpec};

    // -------------------------------------------------------------------------
    // Backward-compat HTTP API types (test-only, kept for serde verification)
    // -------------------------------------------------------------------------

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct ReaperJobRequest {
        script: String,
        environment: HashMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        uid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        gid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        home_dir: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        supplemental_groups: Option<Vec<u32>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hostfile: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hostfile_path: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct ReaperJobResponse {
        job_id: String,
        status: ReaperJobState,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct ReaperJobStatusCompat {
        job_id: String,
        status: ReaperJobState,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "lowercase")]
    enum ReaperJobState {
        Pending,
        Running,
        Succeeded,
        Failed,
        Unknown,
    }

    fn map_reaper_state(status: &ReaperJobStatusCompat) -> BackendJobStatus {
        match status.status {
            ReaperJobState::Pending => BackendJobStatus::Launching { ready: 0, total: 1 },
            ReaperJobState::Running => BackendJobStatus::Running,
            ReaperJobState::Succeeded => BackendJobStatus::Succeeded,
            ReaperJobState::Failed => BackendJobStatus::Failed {
                message: status.message.clone().unwrap_or_else(|| {
                    format!("job exited with code {}", status.exit_code.unwrap_or(-1))
                }),
            },
            ReaperJobState::Unknown => BackendJobStatus::NotFound,
        }
    }

    /// Build a backward-compatible ReaperJobRequest from the same env builder.
    fn build_job_request(
        backend: &ReaperBackend,
        job_name: &str,
        spec: &WrenJobSpec,
        placement: &Placement,
        rank: u32,
        user: Option<&UserIdentity>,
    ) -> Result<ReaperJobRequest, WrenError> {
        let reaper_spec = spec
            .reaper
            .as_ref()
            .ok_or_else(|| WrenError::ValidationError {
                reason: "reaper spec required for reaper backend".to_string(),
            })?;

        let environment = backend.build_rank_env(job_name, spec, placement, rank, user)?;
        let hostfile_content = mpi::generate_hostfile(placement, spec.tasks_per_node);
        let hostfile_path = format!("/tmp/wren-hostfile-{}", job_name);

        Ok(ReaperJobRequest {
            script: reaper_spec.script.clone(),
            environment,
            working_dir: reaper_spec.working_dir.clone(),
            uid: user.map(|u| u.uid),
            gid: user.map(|u| u.gid),
            username: user.map(|u| u.username.clone()),
            home_dir: user.and_then(|u| u.home_dir.clone()),
            supplemental_groups: user
                .map(|u| u.supplemental_groups.clone())
                .filter(|g| !g.is_empty()),
            hostfile: if spec.mpi.is_some() {
                Some(hostfile_content)
            } else {
                None
            },
            hostfile_path: if spec.mpi.is_some() {
                Some(hostfile_path)
            } else {
                None
            },
        })
    }

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    fn make_placement(nodes: &[&str]) -> Placement {
        Placement {
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            score: 1.0,
        }
    }

    fn make_reaper_spec(nodes: u32, tasks_per_node: u32) -> WrenJobSpec {
        use wren_core::crd::ReaperSpec;
        let mut env = HashMap::new();
        env.insert("SCRATCH".to_string(), "/scratch/project".to_string());

        WrenJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes,
            tasks_per_node,
            backend: ExecutionBackendType::Reaper,
            container: None,
            reaper: Some(ReaperSpec {
                script: "#!/bin/bash\nmpirun ./app".to_string(),
                environment: env,
                working_dir: Some("/scratch/project".to_string()),
            }),
            mpi: None,
            topology: None,
            dependencies: vec![],
            project: None,
        }
    }

    // Dummy client for unit tests. The tested methods (build_job_request,
    // build_rank_env) are synchronous and never make API calls.
    // kube::Client::new uses tower::Buffer which needs a Tokio runtime.
    fn make_backend() -> ReaperBackend {
        use std::sync::OnceLock;
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        let rt = RT.get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        });
        let _guard = rt.enter();
        let client = Client::new(
            tower::service_fn(|_: http::Request<kube::client::Body>| async move {
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                    http::Response::new(http_body_util::Empty::<bytes::Bytes>::new()),
                )
            }),
            "default",
        );
        ReaperBackend::new(client)
    }

    // -------------------------------------------------------------------------
    // Request / response payload serialization (backward compat)
    // -------------------------------------------------------------------------

    #[test]
    fn test_reaper_job_request_serialization_roundtrip() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        env.insert("RANKS".to_string(), "8".to_string());

        let req = ReaperJobRequest {
            script: "#!/bin/bash\necho hello".to_string(),
            environment: env.clone(),
            working_dir: Some("/tmp/work".to_string()),
            uid: Some(1000),
            gid: Some(1000),
            username: Some("test".to_string()),
            home_dir: Some("/home/test".to_string()),
            supplemental_groups: Some(vec![1000, 2000]),
            hostfile: Some("node-0 slots=4".to_string()),
            hostfile_path: Some("/tmp/wren-hostfile-test".to_string()),
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: ReaperJobRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.script, req.script);
        assert_eq!(parsed.environment, env);
        assert_eq!(parsed.working_dir, Some("/tmp/work".to_string()));
        assert_eq!(parsed.uid, Some(1000));
        assert_eq!(parsed.gid, Some(1000));
        assert_eq!(parsed.username, Some("test".to_string()));
        assert_eq!(parsed.home_dir, Some("/home/test".to_string()));
        assert_eq!(parsed.supplemental_groups, Some(vec![1000, 2000]));
    }

    #[test]
    fn test_reaper_job_request_omits_none_working_dir() {
        let req = ReaperJobRequest {
            script: "echo ok".to_string(),
            environment: HashMap::new(),
            working_dir: None,
            uid: None,
            gid: None,
            username: None,
            home_dir: None,
            supplemental_groups: None,
            hostfile: None,
            hostfile_path: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(
            !json.contains("working_dir"),
            "None fields should be omitted"
        );
        assert!(!json.contains("uid"), "None uid should be omitted");
        assert!(!json.contains("gid"), "None gid should be omitted");
        assert!(
            !json.contains("username"),
            "None username should be omitted"
        );
        assert!(
            !json.contains("home_dir"),
            "None home_dir should be omitted"
        );
        assert!(
            !json.contains("supplemental_groups"),
            "None supplemental_groups should be omitted"
        );
    }

    #[test]
    fn test_reaper_job_response_roundtrip() {
        let resp = ReaperJobResponse {
            job_id: "abc-123".to_string(),
            status: ReaperJobState::Running,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: ReaperJobResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.job_id, "abc-123");
        assert_eq!(parsed.status, ReaperJobState::Running);
    }

    #[test]
    fn test_reaper_job_state_deserialization() {
        assert_eq!(
            serde_json::from_str::<ReaperJobState>(r#""pending""#).unwrap(),
            ReaperJobState::Pending
        );
        assert_eq!(
            serde_json::from_str::<ReaperJobState>(r#""running""#).unwrap(),
            ReaperJobState::Running
        );
        assert_eq!(
            serde_json::from_str::<ReaperJobState>(r#""succeeded""#).unwrap(),
            ReaperJobState::Succeeded
        );
        assert_eq!(
            serde_json::from_str::<ReaperJobState>(r#""failed""#).unwrap(),
            ReaperJobState::Failed
        );
        assert_eq!(
            serde_json::from_str::<ReaperJobState>(r#""unknown""#).unwrap(),
            ReaperJobState::Unknown
        );
    }

    #[test]
    fn test_reaper_job_status_with_exit_code() {
        let json = r#"{
            "job_id": "xyz",
            "status": "failed",
            "exit_code": 1,
            "message": "OOM"
        }"#;
        let status: ReaperJobStatusCompat = serde_json::from_str(json).expect("deserialize");
        assert_eq!(status.job_id, "xyz");
        assert_eq!(status.status, ReaperJobState::Failed);
        assert_eq!(status.exit_code, Some(1));
        assert_eq!(status.message.as_deref(), Some("OOM"));
    }

    // -------------------------------------------------------------------------
    // Status mapping (backward compat)
    // -------------------------------------------------------------------------

    #[test]
    fn test_map_pending_to_launching() {
        let status = ReaperJobStatusCompat {
            job_id: "j1".to_string(),
            status: ReaperJobState::Pending,
            exit_code: None,
            message: None,
        };
        assert_eq!(
            map_reaper_state(&status),
            BackendJobStatus::Launching { ready: 0, total: 1 }
        );
    }

    #[test]
    fn test_map_running() {
        let status = ReaperJobStatusCompat {
            job_id: "j1".to_string(),
            status: ReaperJobState::Running,
            exit_code: None,
            message: None,
        };
        assert_eq!(map_reaper_state(&status), BackendJobStatus::Running);
    }

    #[test]
    fn test_map_succeeded() {
        let status = ReaperJobStatusCompat {
            job_id: "j1".to_string(),
            status: ReaperJobState::Succeeded,
            exit_code: Some(0),
            message: None,
        };
        assert_eq!(map_reaper_state(&status), BackendJobStatus::Succeeded);
    }

    #[test]
    fn test_map_failed_with_message() {
        let status = ReaperJobStatusCompat {
            job_id: "j1".to_string(),
            status: ReaperJobState::Failed,
            exit_code: Some(2),
            message: Some("segfault".to_string()),
        };
        assert_eq!(
            map_reaper_state(&status),
            BackendJobStatus::Failed {
                message: "segfault".to_string()
            }
        );
    }

    #[test]
    fn test_map_failed_without_message_uses_exit_code() {
        let status = ReaperJobStatusCompat {
            job_id: "j1".to_string(),
            status: ReaperJobState::Failed,
            exit_code: Some(137),
            message: None,
        };
        match map_reaper_state(&status) {
            BackendJobStatus::Failed { message } => {
                assert!(
                    message.contains("137"),
                    "exit code should appear in message: {message}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn test_map_failed_without_message_or_exit_code() {
        let status = ReaperJobStatusCompat {
            job_id: "j1".to_string(),
            status: ReaperJobState::Failed,
            exit_code: None,
            message: None,
        };
        match map_reaper_state(&status) {
            BackendJobStatus::Failed { message } => {
                assert!(message.contains("-1"), "should fall back to -1: {message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn test_map_unknown_to_not_found() {
        let status = ReaperJobStatusCompat {
            job_id: "j1".to_string(),
            status: ReaperJobState::Unknown,
            exit_code: None,
            message: None,
        };
        assert_eq!(map_reaper_state(&status), BackendJobStatus::NotFound);
    }

    // -------------------------------------------------------------------------
    // ReaperPod phase mapping
    // -------------------------------------------------------------------------

    #[test]
    fn test_map_phase_succeeded() {
        let s = ReaperPodStatus {
            phase: Some("Succeeded".to_string()),
            exit_code: Some(0),
            ..Default::default()
        };
        assert_eq!(map_phase_to_status(&s), BackendJobStatus::Succeeded);
    }

    #[test]
    fn test_map_phase_failed() {
        let s = ReaperPodStatus {
            phase: Some("Failed".to_string()),
            exit_code: Some(1),
            message: Some("OOM".to_string()),
            ..Default::default()
        };
        assert_eq!(
            map_phase_to_status(&s),
            BackendJobStatus::Failed {
                message: "OOM".to_string()
            }
        );
    }

    #[test]
    fn test_map_phase_running() {
        let s = ReaperPodStatus {
            phase: Some("Running".to_string()),
            ..Default::default()
        };
        assert_eq!(map_phase_to_status(&s), BackendJobStatus::Running);
    }

    #[test]
    fn test_map_phase_pending() {
        let s = ReaperPodStatus {
            phase: Some("Pending".to_string()),
            ..Default::default()
        };
        assert_eq!(
            map_phase_to_status(&s),
            BackendJobStatus::Launching { ready: 0, total: 1 }
        );
    }

    #[test]
    fn test_map_phase_none() {
        let s = ReaperPodStatus::default();
        assert_eq!(
            map_phase_to_status(&s),
            BackendJobStatus::Launching { ready: 0, total: 1 }
        );
    }

    // -------------------------------------------------------------------------
    // Job request building (backward compat, reuses build_rank_env)
    // -------------------------------------------------------------------------

    #[test]
    fn test_build_job_request_injects_mpi_env() {
        let backend = make_backend();
        let spec = make_reaper_spec(4, 2);
        let placement = make_placement(&["n0", "n1", "n2", "n3"]);

        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, None)
            .unwrap();

        assert_eq!(req.environment["WREN_MPI_RANK"], "0");
        assert_eq!(req.environment["WREN_TOTAL_RANKS"], "8");
        assert_eq!(req.environment["WREN_NUM_NODES"], "4");
        assert!(req.environment.contains_key("WREN_HOSTFILE_CONTENT"));
        assert_eq!(req.environment["SCRATCH"], "/scratch/project");
    }

    #[test]
    fn test_build_job_request_rank_varies() {
        let backend = make_backend();
        let spec = make_reaper_spec(2, 4);
        let placement = make_placement(&["n0", "n1"]);

        let req0 = build_job_request(&backend, "test-job", &spec, &placement, 0, None)
            .unwrap();
        let req1 = build_job_request(&backend, "test-job", &spec, &placement, 1, None)
            .unwrap();

        assert_eq!(req0.environment["WREN_MPI_RANK"], "0");
        assert_eq!(req1.environment["WREN_MPI_RANK"], "1");
        assert_eq!(
            req0.environment["WREN_HOSTFILE_CONTENT"],
            req1.environment["WREN_HOSTFILE_CONTENT"]
        );
    }

    #[test]
    fn test_build_job_request_working_dir_forwarded() {
        let backend = make_backend();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);

        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, None)
            .unwrap();
        assert_eq!(req.working_dir, Some("/scratch/project".to_string()));
    }

    #[test]
    fn test_build_job_request_error_without_reaper_spec() {
        let backend = make_backend();
        let spec = WrenJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes: 1,
            tasks_per_node: 1,
            backend: ExecutionBackendType::Reaper,
            container: None,
            reaper: None,
            mpi: None,
            topology: None,
            dependencies: vec![],
            project: None,
        };
        let placement = make_placement(&["n0"]);
        let result = build_job_request(&backend, "test-job", &spec, &placement, 0, None);
        assert!(result.is_err());
        match result.unwrap_err() {
            WrenError::ValidationError { reason } => {
                assert!(reason.contains("reaper spec"), "unexpected: {reason}");
            }
            e => panic!("expected ValidationError, got {e:?}"),
        }
    }

    #[test]
    fn test_build_job_request_hostfile_format() {
        let backend = make_backend();
        let spec = make_reaper_spec(3, 4);
        let placement = make_placement(&["node-0", "node-1", "node-2"]);

        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, None)
            .unwrap();
        let hostfile = &req.environment["WREN_HOSTFILE_CONTENT"];
        assert_eq!(hostfile, "node-0 slots=4\nnode-1 slots=4\nnode-2 slots=4");
    }

    #[test]
    fn test_build_job_request_with_mpi_spec() {
        use wren_core::MPISpec;

        let backend = make_backend();
        let mut spec = make_reaper_spec(2, 1);
        spec.mpi = Some(MPISpec {
            implementation: "cray-mpich".to_string(),
            ssh_auth: false,
            fabric_interface: Some("hsn0".to_string()),
        });
        let placement = make_placement(&["node-0", "node-1"]);

        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, None)
            .unwrap();
        assert_eq!(req.environment["MPICH_OFI_IFNAME"], "hsn0");
        assert_eq!(req.environment["MPICH_OFI_STARTUP_CONNECT"], "1");
    }

    #[test]
    fn test_build_job_request_mpi_impl_without_fabric() {
        use wren_core::MPISpec;
        let backend = make_backend();
        let mut spec = make_reaper_spec(2, 4);
        spec.mpi = Some(MPISpec {
            implementation: "openmpi".to_string(),
            ssh_auth: false,
            fabric_interface: None,
        });
        let placement = make_placement(&["n0", "n1"]);
        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, None)
            .unwrap();
        assert!(!req.environment.contains_key("UCX_NET_DEVICES"));
        assert!(req.environment.contains_key("WREN_MPI_LAUNCHER"));
    }

    #[test]
    fn test_build_job_request_script_preserved() {
        let backend = make_backend();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);
        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, None)
            .unwrap();
        assert_eq!(req.script, "#!/bin/bash\nmpirun ./app");
    }

    #[test]
    fn test_build_job_request_single_node_single_rank() {
        let backend = make_backend();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["solo-node"]);
        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, None)
            .unwrap();
        assert_eq!(req.environment["WREN_TOTAL_RANKS"], "1");
        assert_eq!(req.environment["WREN_NUM_NODES"], "1");
        assert_eq!(req.environment["WREN_MPI_RANK"], "0");
        assert_eq!(
            req.environment["WREN_HOSTFILE_CONTENT"],
            "solo-node slots=1"
        );
    }

    #[test]
    fn test_build_job_request_large_cluster() {
        let backend = make_backend();
        let spec = make_reaper_spec(8, 4);
        let placement = Placement {
            nodes: (0..8).map(|i| format!("node-{i}")).collect(),
            score: 1.0,
        };
        let req = build_job_request(&backend, "test-job", &spec, &placement, 7, None)
            .unwrap();
        assert_eq!(req.environment["WREN_TOTAL_RANKS"], "32");
        assert_eq!(req.environment["WREN_MPI_RANK"], "7");
        assert_eq!(req.environment["WREN_NUM_NODES"], "8");
    }

    #[test]
    fn test_map_reaper_state_failed_exit_code_zero_with_failed_status() {
        let status = ReaperJobStatusCompat {
            job_id: "j1".to_string(),
            status: ReaperJobState::Failed,
            exit_code: Some(0),
            message: Some("killed by signal".to_string()),
        };
        let mapped = map_reaper_state(&status);
        assert_eq!(
            mapped,
            BackendJobStatus::Failed {
                message: "killed by signal".to_string()
            }
        );
    }

    #[test]
    fn test_reaper_job_status_minimal_json() {
        let json = r#"{"job_id": "abc", "status": "pending"}"#;
        let status: ReaperJobStatusCompat = serde_json::from_str(json).unwrap();
        assert_eq!(status.job_id, "abc");
        assert_eq!(status.status, ReaperJobState::Pending);
        assert!(status.exit_code.is_none());
        assert!(status.message.is_none());
    }

    #[test]
    fn test_reaper_job_status_succeeded_with_exit_code_zero() {
        let json = r#"{"job_id": "xyz", "status": "succeeded", "exit_code": 0}"#;
        let status: ReaperJobStatusCompat = serde_json::from_str(json).unwrap();
        assert_eq!(status.status, ReaperJobState::Succeeded);
        assert_eq!(status.exit_code, Some(0));
    }

    // -------------------------------------------------------------------------
    // User identity
    // -------------------------------------------------------------------------

    fn make_user_identity() -> UserIdentity {
        UserIdentity {
            username: "testuser".to_string(),
            uid: 1001,
            gid: 1001,
            supplemental_groups: vec![1001, 5000],
            home_dir: Some("/home/testuser".to_string()),
            default_project: Some("my-project".to_string()),
        }
    }

    #[test]
    fn test_build_job_request_with_user_identity() {
        let backend = make_backend();
        let spec = make_reaper_spec(2, 1);
        let placement = make_placement(&["n0", "n1"]);
        let user = make_user_identity();

        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, Some(&user))
            .unwrap();

        assert_eq!(req.uid, Some(1001));
        assert_eq!(req.gid, Some(1001));
        assert_eq!(req.username, Some("testuser".to_string()));
        assert_eq!(req.home_dir, Some("/home/testuser".to_string()));
        assert_eq!(req.supplemental_groups, Some(vec![1001, 5000]));

        assert_eq!(req.environment["USER"], "testuser");
        assert_eq!(req.environment["LOGNAME"], "testuser");
        assert_eq!(req.environment["HOME"], "/home/testuser");
    }

    #[test]
    fn test_build_job_request_without_user_leaves_fields_none() {
        let backend = make_backend();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);

        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, None)
            .unwrap();

        assert!(req.uid.is_none());
        assert!(req.gid.is_none());
        assert!(req.username.is_none());
        assert!(req.home_dir.is_none());
        assert!(req.supplemental_groups.is_none());
        assert!(!req.environment.contains_key("USER"));
        assert!(!req.environment.contains_key("LOGNAME"));
        assert!(!req.environment.contains_key("HOME"));
    }

    #[test]
    fn test_build_job_request_user_without_home_dir() {
        let backend = make_backend();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);
        let user = UserIdentity {
            username: "nohome".to_string(),
            uid: 2000,
            gid: 2000,
            supplemental_groups: vec![],
            home_dir: None,
            default_project: None,
        };

        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, Some(&user))
            .unwrap();

        assert_eq!(req.uid, Some(2000));
        assert_eq!(req.gid, Some(2000));
        assert_eq!(req.home_dir, None);
        assert!(req.supplemental_groups.is_none());
        assert_eq!(req.environment["USER"], "nohome");
        assert!(!req.environment.contains_key("HOME"));
    }

    #[test]
    fn test_build_job_request_user_identity_serde_roundtrip() {
        let backend = make_backend();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);
        let user = make_user_identity();

        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, Some(&user))
            .unwrap();
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: ReaperJobRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.uid, req.uid);
        assert_eq!(parsed.gid, req.gid);
        assert_eq!(parsed.username, req.username);
        assert_eq!(parsed.home_dir, req.home_dir);
        assert_eq!(parsed.supplemental_groups, req.supplemental_groups);
    }

    #[test]
    fn test_build_job_request_none_user_serde_omits_fields() {
        let backend = make_backend();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);

        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, None)
            .unwrap();
        let json = serde_json::to_string(&req).expect("serialize");

        assert!(!json.contains("uid"), "None uid should be omitted: {json}");
        assert!(!json.contains("gid"), "None gid should be omitted: {json}");
        assert!(
            !json.contains("username"),
            "None username should be omitted: {json}"
        );
        assert!(
            !json.contains("home_dir"),
            "None home_dir should be omitted: {json}"
        );
        assert!(
            !json.contains("supplemental_groups"),
            "None groups should be omitted: {json}"
        );
    }

    #[test]
    fn test_build_job_request_user_env_vars_dont_override_user_provided() {
        let backend = make_backend();
        let mut spec = make_reaper_spec(1, 1);
        spec.reaper
            .as_mut()
            .unwrap()
            .environment
            .insert("USER".to_string(), "custom-user".to_string());
        let placement = make_placement(&["n0"]);
        let user = make_user_identity();

        let req = build_job_request(&backend, "test-job", &spec, &placement, 0, Some(&user))
            .unwrap();

        assert_eq!(req.environment["USER"], "testuser");
    }

    // -------------------------------------------------------------------------
    // Distributed training env vars
    // -------------------------------------------------------------------------

    #[test]
    fn test_build_job_request_distributed_training_env_vars() {
        let backend = make_backend();
        let spec = make_reaper_spec(4, 1);
        let placement = make_placement(&["node-0", "node-1", "node-2", "node-3"]);

        let req = build_job_request(&backend, "train-job", &spec, &placement, 2, None)
            .unwrap();

        assert_eq!(req.environment["MASTER_ADDR"], "node-0");
        assert_eq!(req.environment["MASTER_PORT"], "29500");
        assert_eq!(req.environment["RANK"], "2");
        assert_eq!(req.environment["WORLD_SIZE"], "4");
        assert_eq!(req.environment["LOCAL_RANK"], "0");
    }

    #[test]
    fn test_build_job_request_with_mpi_spec_uses_bare_metal_env() {
        use wren_core::MPISpec;
        let backend = make_backend();
        let mut spec = make_reaper_spec(2, 1);
        spec.mpi = Some(MPISpec {
            implementation: "cray-mpich".to_string(),
            ssh_auth: false,
            fabric_interface: Some("hsn0".to_string()),
        });
        let placement = make_placement(&["node-0", "node-1"]);

        let req = build_job_request(&backend, "mpi-job", &spec, &placement, 0, None)
            .unwrap();

        assert_eq!(req.environment["MPICH_OFI_STARTUP_CONNECT"], "1");
        assert_eq!(req.environment["MPICH_OFI_NUM_NICS"], "1");
        assert_eq!(req.environment["MPICH_OFI_IFNAME"], "hsn0");
        assert!(req.environment["WREN_MPI_LAUNCHER"].contains("srun"));
    }

    #[test]
    fn test_build_job_request_hostfile_fields() {
        use wren_core::MPISpec;
        let backend = make_backend();
        let mut spec = make_reaper_spec(2, 1);
        spec.mpi = Some(MPISpec {
            implementation: "openmpi".to_string(),
            ssh_auth: false,
            fabric_interface: None,
        });
        let placement = make_placement(&["n0", "n1"]);

        let req = build_job_request(&backend, "hf-job", &spec, &placement, 0, None)
            .unwrap();

        assert!(req.hostfile.is_some());
        assert_eq!(req.hostfile.as_deref(), Some("n0 slots=1\nn1 slots=1"));
        assert_eq!(
            req.hostfile_path.as_deref(),
            Some("/tmp/wren-hostfile-hf-job")
        );
    }

    #[test]
    fn test_build_job_request_no_mpi_spec_no_hostfile_fields() {
        let backend = make_backend();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);

        let req = build_job_request(&backend, "no-mpi", &spec, &placement, 0, None)
            .unwrap();

        assert!(req.hostfile.is_none());
        assert!(req.hostfile_path.is_none());
        assert_eq!(req.environment["MASTER_ADDR"], "n0");
        assert_eq!(req.environment["RANK"], "0");
        assert_eq!(req.environment["WORLD_SIZE"], "1");
    }

    #[test]
    fn test_build_job_request_master_addr_is_first_node() {
        let backend = make_backend();
        let spec = make_reaper_spec(3, 1);
        let placement = make_placement(&["alpha", "beta", "gamma"]);

        let req0 = build_job_request(&backend, "j", &spec, &placement, 0, None)
            .unwrap();
        let req2 = build_job_request(&backend, "j", &spec, &placement, 2, None)
            .unwrap();

        assert_eq!(req0.environment["MASTER_ADDR"], "alpha");
        assert_eq!(req2.environment["MASTER_ADDR"], "alpha");
    }
}
