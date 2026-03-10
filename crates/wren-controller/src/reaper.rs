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

}
