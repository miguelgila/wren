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

        // Validate that either command or script is provided
        if reaper_spec.command.is_empty() && reaper_spec.script.is_none() {
            return Err(WrenError::ValidationError {
                reason: "reaper spec requires either 'command' or 'script'".to_string(),
            });
        }

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

        // Volumes: start with user-defined volumes from ReaperSpec
        let mut volumes: Vec<serde_json::Value> = reaper_spec
            .volumes
            .iter()
            .map(|v| {
                let mut vol = json!({
                    "name": v.name,
                    "mountPath": v.mount_path,
                    "readOnly": v.read_only
                });
                if let Some(ref cm) = v.config_map {
                    vol["configMap"] = json!(cm);
                }
                if let Some(ref secret) = v.secret {
                    vol["secret"] = json!(secret);
                }
                if let Some(ref hp) = v.host_path {
                    vol["hostPath"] = json!(hp);
                }
                if v.empty_dir {
                    vol["emptyDir"] = json!(true);
                }
                vol
            })
            .collect();

        // Add hostfile as ConfigMap volume if MPI is configured
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

        // Build command: prefer explicit command list, fall back to script via sh -c
        let (cmd, args) = if !reaper_spec.command.is_empty() {
            (json!(reaper_spec.command), json!(reaper_spec.args))
        } else if let Some(ref script) = reaper_spec.script {
            (json!(["/bin/sh", "-c"]), json!([script]))
        } else {
            // Already validated in build_rank_env, but be safe
            return Err(WrenError::ValidationError {
                reason: "reaper spec requires either 'command' or 'script'".to_string(),
            });
        };

        let mut pod_spec = json!({
            "command": cmd,
            "args": args,
            "env": env,
            "nodeName": node,
            "volumes": volumes,
            "restartPolicy": "Never"
        });

        // Working directory
        if let Some(ref wd) = reaper_spec.working_dir {
            pod_spec["workingDir"] = json!(wd);
        }

        // DNS mode
        if let Some(ref dns) = reaper_spec.dns_mode {
            pod_spec["dnsMode"] = json!(dns);
        }

        // Overlay name
        if let Some(ref overlay) = reaper_spec.overlay_name {
            pod_spec["overlayName"] = json!(overlay);
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
        labels.insert("wren.giar.dev/job".to_string(), job_name.to_string());
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
            let reaper_pod = self.build_reaper_pod(
                job_name,
                namespace,
                spec,
                placement,
                rank as u32,
                node,
                user,
            )?;
            let pod_name = format!("{}-rank-{}", job_name, rank);

            let obj: kube::api::DynamicObject =
                serde_json::from_value(reaper_pod).map_err(|e| WrenError::BackendError {
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

    async fn status(&self, job_name: &str, namespace: &str) -> Result<BackendJobStatus, WrenError> {
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
        let lp =
            kube::api::ListParams::default().labels(&format!("wren.giar.dev/job={}", job_name));

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
    use wren_core::crd::{MPISpec, ReaperSpec, ReaperVolumeSpec};

    // -------------------------------------------------------------------------
    // Mock client helpers
    // -------------------------------------------------------------------------

    /// Build a `ReaperBackend` with a fake `Client` that never makes real API
    /// calls.  `build_reaper_pod` and `build_rank_env` are pure functions that
    /// never touch `self.client`, so any valid `Client` instance suffices.
    fn make_backend() -> ReaperBackend {
        use bytes::Bytes;
        use http_body_util::Full;
        use tower::service_fn;

        let svc = service_fn(|_req: http::Request<_>| async {
            let resp = http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Full::from(Bytes::from("{}")))
                .unwrap();
            Ok::<_, std::convert::Infallible>(resp)
        });
        let client = Client::new(svc, "default");
        ReaperBackend::new(client)
    }

    /// Minimal `WrenJobSpec` with a command-based `ReaperSpec`.
    fn make_spec(nodes: u32) -> WrenJobSpec {
        WrenJobSpec {
            nodes,
            tasks_per_node: 1,
            backend: wren_core::types::ExecutionBackendType::Reaper,
            reaper: Some(ReaperSpec {
                command: vec!["python3".to_string(), "/opt/train.py".to_string()],
                args: vec!["--epochs".to_string(), "5".to_string()],
                ..Default::default()
            }),
            ..serde_json::from_str(&format!(r#"{{"nodes": {}}}"#, nodes)).unwrap()
        }
    }

    /// Placement with `count` sequentially-named nodes.
    fn make_placement(count: usize) -> Placement {
        Placement {
            nodes: (0..count).map(|i| format!("node-{}", i)).collect(),
            score: 1.0,
        }
    }

    /// A `UserIdentity` representing a typical HPC user.
    fn make_user() -> UserIdentity {
        UserIdentity {
            username: "alice".to_string(),
            uid: 1001,
            gid: 1001,
            supplemental_groups: vec![2000, 3000],
            home_dir: Some("/home/alice".to_string()),
            default_project: None,
        }
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
    fn test_map_phase_failed_uses_exit_code_when_no_message() {
        let s = ReaperPodStatus {
            phase: Some("Failed".to_string()),
            exit_code: Some(2),
            message: None,
            ..Default::default()
        };
        assert_eq!(
            map_phase_to_status(&s),
            BackendJobStatus::Failed {
                message: "job exited with code 2".to_string()
            }
        );
    }

    #[test]
    fn test_map_phase_failed_uses_minus_one_when_no_exit_code_and_no_message() {
        let s = ReaperPodStatus {
            phase: Some("Failed".to_string()),
            exit_code: None,
            message: None,
            ..Default::default()
        };
        assert_eq!(
            map_phase_to_status(&s),
            BackendJobStatus::Failed {
                message: "job exited with code -1".to_string()
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

    #[test]
    fn test_map_phase_unknown_string_treated_as_launching() {
        let s = ReaperPodStatus {
            phase: Some("Initializing".to_string()),
            ..Default::default()
        };
        assert_eq!(
            map_phase_to_status(&s),
            BackendJobStatus::Launching { ready: 0, total: 1 }
        );
    }

    // -------------------------------------------------------------------------
    // build_reaper_pod — metadata and top-level structure
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_reaper_pod_returns_correct_api_version_and_kind() {
        let backend = make_backend();
        let spec = make_spec(2);
        let placement = make_placement(2);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert_eq!(pod["apiVersion"], "reaper.io/v1alpha1");
        assert_eq!(pod["kind"], "ReaperPod");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_metadata_name_encodes_job_name_and_rank() {
        let backend = make_backend();
        let spec = make_spec(4);
        let placement = make_placement(4);
        let pod = backend
            .build_reaper_pod("training-job", "hpc", &spec, &placement, 3, "node-3", None)
            .unwrap();

        assert_eq!(pod["metadata"]["name"], "training-job-rank-3");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_metadata_namespace_matches_argument() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "team-ns", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert_eq!(pod["metadata"]["namespace"], "team-ns");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_labels_contain_job_name_and_rank() {
        let backend = make_backend();
        let spec = make_spec(3);
        let placement = make_placement(3);
        let pod = backend
            .build_reaper_pod("sim-job", "default", &spec, &placement, 1, "node-1", None)
            .unwrap();

        let labels = &pod["metadata"]["labels"];
        assert_eq!(labels["wren.giar.dev/job"], "sim-job");
        assert_eq!(labels["wren.giar.dev/rank"], "1");
        assert_eq!(labels["wren.giar.dev/role"], "worker");
        assert_eq!(labels["app.kubernetes.io/managed-by"], "wren");
    }

    // -------------------------------------------------------------------------
    // build_reaper_pod — spec.command and spec.args
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_reaper_pod_command_taken_from_reaper_spec() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        let cmd = pod["spec"]["command"].as_array().unwrap();
        assert_eq!(cmd[0], "python3");
        assert_eq!(cmd[1], "/opt/train.py");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_args_taken_from_reaper_spec() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        let args = pod["spec"]["args"].as_array().unwrap();
        assert_eq!(args[0], "--epochs");
        assert_eq!(args[1], "5");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_script_wraps_in_sh_c_when_no_command() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec![],
            script: Some("#!/bin/bash\necho hello".to_string()),
            ..Default::default()
        });
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        let cmd = pod["spec"]["command"].as_array().unwrap();
        assert_eq!(cmd[0], "/bin/sh");
        assert_eq!(cmd[1], "-c");

        let args = pod["spec"]["args"].as_array().unwrap();
        assert!(args[0].as_str().unwrap().contains("echo hello"));
    }

    #[tokio::test]
    async fn test_build_reaper_pod_errors_when_neither_command_nor_script_provided() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec![],
            script: None,
            ..Default::default()
        });
        let placement = make_placement(1);
        let result =
            backend.build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None);

        assert!(result.is_err());
    }

    // -------------------------------------------------------------------------
    // build_reaper_pod — spec.nodeName
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_reaper_pod_node_name_matches_placement_node_argument() {
        let backend = make_backend();
        let spec = make_spec(2);
        let placement = make_placement(2);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 1, "node-1", None)
            .unwrap();

        assert_eq!(pod["spec"]["nodeName"], "node-1");
    }

    // -------------------------------------------------------------------------
    // build_reaper_pod — user identity fields
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_reaper_pod_sets_run_as_user_and_group_when_user_provided() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let user = make_user();
        let pod = backend
            .build_reaper_pod(
                "myjob",
                "default",
                &spec,
                &placement,
                0,
                "node-0",
                Some(&user),
            )
            .unwrap();

        assert_eq!(pod["spec"]["runAsUser"], 1001);
        assert_eq!(pod["spec"]["runAsGroup"], 1001);
    }

    #[tokio::test]
    async fn test_build_reaper_pod_sets_supplemental_groups_when_user_provided() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let user = make_user();
        let pod = backend
            .build_reaper_pod(
                "myjob",
                "default",
                &spec,
                &placement,
                0,
                "node-0",
                Some(&user),
            )
            .unwrap();

        let groups = pod["spec"]["supplementalGroups"].as_array().unwrap();
        assert!(groups.contains(&serde_json::json!(2000)));
        assert!(groups.contains(&serde_json::json!(3000)));
    }

    #[tokio::test]
    async fn test_build_reaper_pod_omits_identity_fields_when_no_user() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert!(pod["spec"].get("runAsUser").is_none());
        assert!(pod["spec"].get("runAsGroup").is_none());
        assert!(pod["spec"].get("supplementalGroups").is_none());
    }

    #[tokio::test]
    async fn test_build_reaper_pod_omits_supplemental_groups_when_user_has_none() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let user = UserIdentity {
            supplemental_groups: vec![],
            ..make_user()
        };
        let pod = backend
            .build_reaper_pod(
                "myjob",
                "default",
                &spec,
                &placement,
                0,
                "node-0",
                Some(&user),
            )
            .unwrap();

        assert!(pod["spec"].get("supplementalGroups").is_none());
    }

    // -------------------------------------------------------------------------
    // build_reaper_pod — environment variables
    // -------------------------------------------------------------------------

    fn env_value(pod: &serde_json::Value, key: &str) -> Option<String> {
        pod["spec"]["env"]
            .as_array()?
            .iter()
            .find(|e| e["name"].as_str() == Some(key))
            .and_then(|e| e["value"].as_str())
            .map(|s| s.to_string())
    }

    #[tokio::test]
    async fn test_build_reaper_pod_env_contains_rank_and_world_size() {
        let backend = make_backend();
        let mut spec = make_spec(4);
        spec.nodes = 4;
        spec.tasks_per_node = 1;
        let placement = make_placement(4);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 2, "node-2", None)
            .unwrap();

        assert_eq!(env_value(&pod, "RANK").as_deref(), Some("2"));
        assert_eq!(env_value(&pod, "WORLD_SIZE").as_deref(), Some("4"));
        assert_eq!(env_value(&pod, "LOCAL_RANK").as_deref(), Some("0"));
    }

    #[tokio::test]
    async fn test_build_reaper_pod_env_master_addr_is_first_placement_node() {
        let backend = make_backend();
        let spec = make_spec(3);
        let placement = make_placement(3);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        // MASTER_ADDR is always the first node in the placement
        assert_eq!(env_value(&pod, "MASTER_ADDR").as_deref(), Some("node-0"));
        assert_eq!(env_value(&pod, "MASTER_PORT").as_deref(), Some("29500"));
    }

    #[tokio::test]
    async fn test_build_reaper_pod_env_contains_user_home_logname_when_user_provided() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let user = make_user();
        let pod = backend
            .build_reaper_pod(
                "myjob",
                "default",
                &spec,
                &placement,
                0,
                "node-0",
                Some(&user),
            )
            .unwrap();

        assert_eq!(env_value(&pod, "USER").as_deref(), Some("alice"));
        assert_eq!(env_value(&pod, "LOGNAME").as_deref(), Some("alice"));
        assert_eq!(env_value(&pod, "HOME").as_deref(), Some("/home/alice"));
    }

    #[tokio::test]
    async fn test_build_reaper_pod_env_omits_home_when_user_has_no_home_dir() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let user = UserIdentity {
            home_dir: None,
            ..make_user()
        };
        let pod = backend
            .build_reaper_pod(
                "myjob",
                "default",
                &spec,
                &placement,
                0,
                "node-0",
                Some(&user),
            )
            .unwrap();

        assert_eq!(env_value(&pod, "USER").as_deref(), Some("alice"));
        // HOME should not appear when not set
        assert!(env_value(&pod, "HOME").is_none());
    }

    #[tokio::test]
    async fn test_build_reaper_pod_env_omits_user_vars_when_no_user() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert!(env_value(&pod, "USER").is_none());
        assert!(env_value(&pod, "LOGNAME").is_none());
        assert!(env_value(&pod, "HOME").is_none());
    }

    #[tokio::test]
    async fn test_build_reaper_pod_env_contains_wren_job_name() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("my-sim", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert_eq!(env_value(&pod, "WREN_JOB_NAME").as_deref(), Some("my-sim"));
    }

    #[tokio::test]
    async fn test_build_reaper_pod_env_contains_wren_mpi_rank() {
        let backend = make_backend();
        let spec = make_spec(2);
        let placement = make_placement(2);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 1, "node-1", None)
            .unwrap();

        assert_eq!(env_value(&pod, "WREN_MPI_RANK").as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn test_build_reaper_pod_env_contains_wren_hostfile_content() {
        let backend = make_backend();
        let spec = make_spec(2);
        let placement = make_placement(2);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        let content = env_value(&pod, "WREN_HOSTFILE_CONTENT").unwrap();
        assert!(content.contains("node-0"));
        assert!(content.contains("node-1"));
    }

    // -------------------------------------------------------------------------
    // build_reaper_pod — MPI hostfile volume
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_reaper_pod_adds_hostfile_volume_when_mpi_configured() {
        let backend = make_backend();
        let mut spec = make_spec(2);
        spec.mpi = Some(MPISpec {
            implementation: "openmpi".to_string(),
            ssh_auth: false,
            fabric_interface: None,
        });
        let placement = make_placement(2);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        let volumes = pod["spec"]["volumes"].as_array().unwrap();
        let hostfile_vol = volumes.iter().find(|v| v["name"] == "hostfile");
        assert!(hostfile_vol.is_some(), "hostfile volume should be present");
        let hv = hostfile_vol.unwrap();
        assert_eq!(hv["configMap"], "myjob-hostfile");
        assert_eq!(hv["readOnly"], true);
    }

    #[tokio::test]
    async fn test_build_reaper_pod_omits_hostfile_volume_when_no_mpi() {
        let backend = make_backend();
        let spec = make_spec(2);
        let placement = make_placement(2);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        let volumes = pod["spec"]["volumes"].as_array().unwrap();
        let hostfile_vol = volumes.iter().find(|v| v["name"] == "hostfile");
        assert!(hostfile_vol.is_none(), "hostfile volume should be absent");
    }

    // -------------------------------------------------------------------------
    // build_reaper_pod — user-defined volumes
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_reaper_pod_includes_configmap_volume_from_reaper_spec() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec!["./app".to_string()],
            volumes: vec![ReaperVolumeSpec {
                name: "training-data".to_string(),
                mount_path: "/data".to_string(),
                read_only: true,
                config_map: Some("my-cm".to_string()),
                secret: None,
                host_path: None,
                empty_dir: false,
            }],
            ..Default::default()
        });
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        let volumes = pod["spec"]["volumes"].as_array().unwrap();
        let data_vol = volumes.iter().find(|v| v["name"] == "training-data");
        assert!(data_vol.is_some());
        let dv = data_vol.unwrap();
        assert_eq!(dv["mountPath"], "/data");
        assert_eq!(dv["readOnly"], true);
        assert_eq!(dv["configMap"], "my-cm");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_includes_secret_volume_from_reaper_spec() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec!["./app".to_string()],
            volumes: vec![ReaperVolumeSpec {
                name: "creds".to_string(),
                mount_path: "/etc/creds".to_string(),
                read_only: true,
                config_map: None,
                secret: Some("my-secret".to_string()),
                host_path: None,
                empty_dir: false,
            }],
            ..Default::default()
        });
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        let volumes = pod["spec"]["volumes"].as_array().unwrap();
        let sec_vol = volumes.iter().find(|v| v["name"] == "creds");
        assert!(sec_vol.is_some());
        assert_eq!(sec_vol.unwrap()["secret"], "my-secret");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_includes_host_path_volume_from_reaper_spec() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec!["./app".to_string()],
            volumes: vec![ReaperVolumeSpec {
                name: "scratch".to_string(),
                mount_path: "/scratch".to_string(),
                read_only: false,
                config_map: None,
                secret: None,
                host_path: Some("/mnt/nvme".to_string()),
                empty_dir: false,
            }],
            ..Default::default()
        });
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        let volumes = pod["spec"]["volumes"].as_array().unwrap();
        let hp_vol = volumes.iter().find(|v| v["name"] == "scratch");
        assert!(hp_vol.is_some());
        assert_eq!(hp_vol.unwrap()["hostPath"], "/mnt/nvme");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_includes_empty_dir_volume_from_reaper_spec() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec!["./app".to_string()],
            volumes: vec![ReaperVolumeSpec {
                name: "tmpwork".to_string(),
                mount_path: "/tmp/work".to_string(),
                read_only: false,
                config_map: None,
                secret: None,
                host_path: None,
                empty_dir: true,
            }],
            ..Default::default()
        });
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        let volumes = pod["spec"]["volumes"].as_array().unwrap();
        let ed_vol = volumes.iter().find(|v| v["name"] == "tmpwork");
        assert!(ed_vol.is_some());
        assert_eq!(ed_vol.unwrap()["emptyDir"], true);
    }

    // -------------------------------------------------------------------------
    // build_reaper_pod — optional spec fields: dnsMode, overlayName, workingDir
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_reaper_pod_sets_dns_mode_when_provided() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec!["./app".to_string()],
            dns_mode: Some("kubernetes".to_string()),
            ..Default::default()
        });
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert_eq!(pod["spec"]["dnsMode"], "kubernetes");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_omits_dns_mode_when_not_provided() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert!(pod["spec"].get("dnsMode").is_none());
    }

    #[tokio::test]
    async fn test_build_reaper_pod_sets_overlay_name_when_provided() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec!["./app".to_string()],
            overlay_name: Some("shared-team".to_string()),
            ..Default::default()
        });
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert_eq!(pod["spec"]["overlayName"], "shared-team");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_omits_overlay_name_when_not_provided() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert!(pod["spec"].get("overlayName").is_none());
    }

    #[tokio::test]
    async fn test_build_reaper_pod_sets_working_dir_when_provided() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec!["./app".to_string()],
            working_dir: Some("/workspace".to_string()),
            ..Default::default()
        });
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert_eq!(pod["spec"]["workingDir"], "/workspace");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_omits_working_dir_when_not_provided() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert!(pod["spec"].get("workingDir").is_none());
    }

    // -------------------------------------------------------------------------
    // build_reaper_pod — spec invariants
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_reaper_pod_restart_policy_is_never() {
        let backend = make_backend();
        let spec = make_spec(1);
        let placement = make_placement(1);
        let pod = backend
            .build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None)
            .unwrap();

        assert_eq!(pod["spec"]["restartPolicy"], "Never");
    }

    #[tokio::test]
    async fn test_build_reaper_pod_errors_when_reaper_spec_missing() {
        let backend = make_backend();
        // WrenJobSpec with no reaper section
        let spec: WrenJobSpec = serde_json::from_str(r#"{"nodes": 1}"#).unwrap();
        let placement = make_placement(1);
        let result =
            backend.build_reaper_pod("myjob", "default", &spec, &placement, 0, "node-0", None);

        assert!(result.is_err());
    }

    // -------------------------------------------------------------------------
    // build_rank_env — validation
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_rank_env_errors_when_neither_command_nor_script() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec![],
            script: None,
            ..Default::default()
        });
        let placement = make_placement(1);
        let result = backend.build_rank_env("myjob", &spec, &placement, 0, None);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_build_rank_env_errors_when_reaper_spec_missing() {
        let backend = make_backend();
        let spec: WrenJobSpec = serde_json::from_str(r#"{"nodes": 1}"#).unwrap();
        let placement = make_placement(1);
        let result = backend.build_rank_env("myjob", &spec, &placement, 0, None);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_build_rank_env_succeeds_with_script_and_no_command() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        spec.reaper = Some(ReaperSpec {
            command: vec![],
            script: Some("echo hi".to_string()),
            ..Default::default()
        });
        let placement = make_placement(1);
        let result = backend.build_rank_env("myjob", &spec, &placement, 0, None);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_build_rank_env_user_env_vars_override_defaults() {
        let backend = make_backend();
        let mut spec = make_spec(1);
        let mut user_env = std::collections::HashMap::new();
        user_env.insert("CUSTOM_VAR".to_string(), "custom_value".to_string());
        spec.reaper = Some(ReaperSpec {
            command: vec!["./app".to_string()],
            environment: user_env,
            ..Default::default()
        });
        let placement = make_placement(1);
        let env = backend
            .build_rank_env("myjob", &spec, &placement, 0, None)
            .unwrap();

        assert_eq!(env["CUSTOM_VAR"], "custom_value");
    }

    // =========================================================================
    // Async tests for ReaperBackend ExecutionBackend trait impl
    // =========================================================================

    use std::sync::{Arc, Mutex};

    /// Build a ReaperBackend with a tracking mock client.
    /// The `router` maps `(method, path) -> (status, body)`.
    #[allow(clippy::type_complexity)]
    fn make_tracking_reaper(
        router: impl Fn(&str, &str) -> (u16, String) + Send + Sync + 'static,
    ) -> (ReaperBackend, Arc<Mutex<Vec<(String, String)>>>) {
        use bytes::Bytes;
        use http_body_util::Full;
        use tower::service_fn;

        let calls = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let calls_clone = calls.clone();
        let router = Arc::new(router);
        let svc = service_fn(move |req: http::Request<_>| {
            let method = req.method().to_string();
            let uri = req.uri().path().to_string();
            calls_clone.lock().unwrap().push((method.clone(), uri.clone()));
            let router = Arc::clone(&router);
            async move {
                let (status, body) = router(&method, &uri);
                let resp = http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Full::from(Bytes::from(body)))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        let client = Client::new(svc, "default");
        (ReaperBackend::new(client), calls)
    }

    /// Default router for reaper backend tests.
    fn reaper_router(method: &str, path: &str) -> (u16, String) {
        // GET on reaperpods (single object) — return a running ReaperPod
        if method == "GET" && path.contains("reaperpods") && !path.ends_with("reaperpods") {
            return (200, serde_json::json!({
                "apiVersion": "reaper.io/v1alpha1",
                "kind": "ReaperPod",
                "metadata": {"name": "test-rank-0", "namespace": "default"},
                "status": {"phase": "Running"}
            }).to_string());
        }
        // POST (create) — return appropriate object based on path
        if method == "POST" {
            if path.contains("configmaps") {
                return (201, serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"name": "created-configmap", "namespace": "default"},
                    "data": {}
                }).to_string());
            }
            return (201, serde_json::json!({
                "apiVersion": "reaper.io/v1alpha1",
                "kind": "ReaperPod",
                "metadata": {"name": "created-reaperpod", "namespace": "default"}
            }).to_string());
        }
        // DELETE — return success
        if method == "DELETE" {
            return (200, serde_json::json!({
                "apiVersion": "v1", "kind": "Status", "status": "Success"
            }).to_string());
        }
        (200, r#"{"metadata":{}}"#.to_string())
    }

    // --- launch() tests ---

    #[tokio::test]
    async fn test_launch_creates_reaper_pods_for_each_rank() {
        let (backend, calls) = make_tracking_reaper(reaper_router);
        let spec = make_spec(3);
        let placement = make_placement(3);
        let user = make_user();
        let result = backend.launch("rjob", "default", &spec, &placement, Some(&user)).await;
        assert!(result.is_ok());
        let launch = result.unwrap();
        assert_eq!(launch.resource_ids.len(), 3);
        let calls = calls.lock().unwrap();
        let post_count = calls.iter().filter(|(m, _)| m == "POST").count();
        // 3 ReaperPods (no hostfile because no MPI spec)
        assert_eq!(post_count, 3, "should create 3 ReaperPods");
    }

    #[tokio::test]
    async fn test_launch_creates_hostfile_for_mpi_job() {
        let (backend, calls) = make_tracking_reaper(reaper_router);
        let mut spec = make_spec(2);
        spec.mpi = Some(MPISpec {
            implementation: "openmpi".to_string(),
            ssh_auth: true,
            fabric_interface: None,
        });
        let placement = make_placement(2);
        let result = backend.launch("mpi-rjob", "default", &spec, &placement, None).await;
        assert!(result.is_ok(), "launch failed: {:?}", result.err());
        let calls = calls.lock().unwrap();
        let post_count = calls.iter().filter(|(m, _)| m == "POST").count();
        // 1 hostfile ConfigMap + 2 ReaperPods = 3 POSTs
        assert_eq!(post_count, 3, "MPI job should create hostfile + 2 ReaperPods");
        assert!(
            calls.iter().any(|(m, p)| m == "POST" && p.contains("configmaps")),
            "should create hostfile ConfigMap"
        );
    }

    #[tokio::test]
    async fn test_launch_returns_resource_ids() {
        let (backend, _) = make_tracking_reaper(reaper_router);
        let spec = make_spec(4);
        let placement = make_placement(4);
        let launch = backend.launch("id-rjob", "default", &spec, &placement, None).await.unwrap();
        assert_eq!(launch.resource_ids.len(), 4);
        assert_eq!(launch.resource_ids[0], "id-rjob-rank-0");
        assert_eq!(launch.resource_ids[3], "id-rjob-rank-3");
    }

    #[tokio::test]
    async fn test_launch_tolerates_409_on_reaperpod() {
        let (backend, _) = make_tracking_reaper(|method, path| {
            if method == "POST" && path.contains("reaperpods") {
                (409, serde_json::json!({
                    "kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "reason": "AlreadyExists", "code": 409,
                    "message": "reaperpods already exists"
                }).to_string())
            } else {
                reaper_router(method, path)
            }
        });
        let spec = make_spec(2);
        let placement = make_placement(2);
        let result = backend.launch("conflict-rjob", "default", &spec, &placement, None).await;
        assert!(result.is_ok(), "409 on ReaperPod create should be tolerated");
    }

    // --- status() tests ---

    #[tokio::test]
    async fn test_status_running() {
        let (backend, _) = make_tracking_reaper(reaper_router);
        let status = backend.status("test-job", "default").await.unwrap();
        assert_eq!(status, BackendJobStatus::Running);
    }

    #[tokio::test]
    async fn test_status_succeeded() {
        let (backend, _) = make_tracking_reaper(|method, path| {
            if method == "GET" && path.contains("reaperpods") && !path.ends_with("reaperpods") {
                (200, serde_json::json!({
                    "apiVersion": "reaper.io/v1alpha1", "kind": "ReaperPod",
                    "metadata": {"name": "test-rank-0"},
                    "status": {"phase": "Succeeded", "exitCode": 0}
                }).to_string())
            } else {
                reaper_router(method, path)
            }
        });
        let status = backend.status("done-job", "default").await.unwrap();
        assert_eq!(status, BackendJobStatus::Succeeded);
    }

    #[tokio::test]
    async fn test_status_failed() {
        let (backend, _) = make_tracking_reaper(|method, path| {
            if method == "GET" && path.contains("reaperpods") && !path.ends_with("reaperpods") {
                (200, serde_json::json!({
                    "apiVersion": "reaper.io/v1alpha1", "kind": "ReaperPod",
                    "metadata": {"name": "test-rank-0"},
                    "status": {"phase": "Failed", "exitCode": 1, "message": "OOM"}
                }).to_string())
            } else {
                reaper_router(method, path)
            }
        });
        let status = backend.status("fail-job", "default").await.unwrap();
        assert_eq!(status, BackendJobStatus::Failed { message: "OOM".to_string() });
    }

    #[tokio::test]
    async fn test_status_not_found() {
        let (backend, _) = make_tracking_reaper(|method, path| {
            if method == "GET" && path.contains("reaperpods") && !path.ends_with("reaperpods") {
                (404, serde_json::json!({
                    "kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "reason": "NotFound", "code": 404
                }).to_string())
            } else {
                reaper_router(method, path)
            }
        });
        let status = backend.status("ghost-job", "default").await.unwrap();
        assert_eq!(status, BackendJobStatus::NotFound);
    }

    // --- terminate() tests ---

    #[tokio::test]
    async fn test_terminate_deletes_reaperpods() {
        let (backend, calls) = make_tracking_reaper(reaper_router);
        let result = backend.terminate("term-rjob", "default").await;
        assert!(result.is_ok());
        let calls = calls.lock().unwrap();
        assert!(
            calls.iter().any(|(m, _)| m == "DELETE"),
            "terminate should issue DELETE for ReaperPods"
        );
    }

    // --- cleanup() tests ---

    #[tokio::test]
    async fn test_cleanup_deletes_pods_and_configmap() {
        let (backend, calls) = make_tracking_reaper(reaper_router);
        let result = backend.cleanup("clean-rjob", "default").await;
        assert!(result.is_ok());
        let calls = calls.lock().unwrap();
        let delete_count = calls.iter().filter(|(m, _)| m == "DELETE").count();
        // cleanup calls terminate (1 DELETE for collection) + delete configmap (1 DELETE)
        assert!(delete_count >= 2, "cleanup should delete ReaperPods + ConfigMap, got {}", delete_count);
        assert!(
            calls.iter().any(|(m, p)| m == "DELETE" && p.contains("configmaps")),
            "cleanup should delete hostfile ConfigMap"
        );
    }
}
