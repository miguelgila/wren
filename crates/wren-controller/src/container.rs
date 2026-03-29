use async_trait::async_trait;
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, ContainerPort, EnvVar, Pod, PodSecurityContext, PodSpec, Service,
    ServicePort, ServiceSpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::{Api, Client};
use std::collections::BTreeMap;
use tracing::{debug, info, warn};
use wren_core::backend::{BackendJobStatus, ExecutionBackend, LaunchResult};
use wren_core::{Placement, WrenError, WrenJobSpec};

use crate::mpi;
use wren_core::UserIdentity;

/// Returns true if the job needs the full MPI launcher pattern
/// (headless service + hostfile + worker pods + launcher pod with mpirun).
/// Jobs without an explicit MPI spec run the user's command directly on
/// worker pods — one per node — without a launcher.
fn needs_mpi_launcher(spec: &wren_core::WrenJobSpec) -> bool {
    spec.mpi.is_some()
}

/// Build a headless Service object for worker pod DNS discovery.
pub(crate) fn build_headless_service(job_name: &str, namespace: &str) -> Service {
    let svc_name = mpi::headless_service_name(job_name);
    let labels = mpi::job_labels(job_name, "worker", None);

    Service {
        metadata: ObjectMeta {
            name: Some(svc_name),
            namespace: Some(namespace.to_string()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".to_string()),
            selector: Some(labels),
            ports: Some(vec![ServicePort {
                port: 22,
                name: Some("ssh".to_string()),
                target_port: Some(IntOrString::Int(22)),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build a hostfile ConfigMap object.
pub(crate) fn build_hostfile_configmap(
    job_name: &str,
    namespace: &str,
    placement: &Placement,
    tasks_per_node: u32,
) -> ConfigMap {
    let cm_name = mpi::hostfile_configmap_name(job_name);
    let hostfile_content = mpi::generate_hostfile(placement, tasks_per_node);

    let mut data = BTreeMap::new();
    data.insert("hostfile".to_string(), hostfile_content);

    ConfigMap {
        metadata: ObjectMeta {
            name: Some(cm_name),
            namespace: Some(namespace.to_string()),
            labels: Some(mpi::job_labels(job_name, "config", None)),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    }
}

/// Build a worker Pod object for a specific node and rank.
pub(crate) fn build_worker_pod(
    job_name: &str,
    namespace: &str,
    spec: &WrenJobSpec,
    node_name: &str,
    rank: u32,
    user: Option<&UserIdentity>,
) -> Result<Pod, WrenError> {
    let pod_name = mpi::worker_pod_name(job_name, rank);
    let labels = mpi::job_labels(job_name, "worker", Some(rank));

    let container_spec = spec
        .container
        .as_ref()
        .ok_or_else(|| WrenError::ValidationError {
            reason: "container spec required for container backend".to_string(),
        })?;

    let mut env_vars: Vec<EnvVar> = container_spec
        .env
        .iter()
        .map(|e| EnvVar {
            name: e.name.clone(),
            value: Some(e.value.clone()),
            ..Default::default()
        })
        .collect();

    env_vars.push(EnvVar {
        name: "WREN_RANK".to_string(),
        value: Some(rank.to_string()),
        ..Default::default()
    });
    env_vars.push(EnvVar {
        name: "WREN_JOB_NAME".to_string(),
        value: Some(job_name.to_string()),
        ..Default::default()
    });

    // Inject user identity env vars
    if let Some(u) = user {
        env_vars.push(EnvVar {
            name: "USER".to_string(),
            value: Some(u.username.clone()),
            ..Default::default()
        });
        env_vars.push(EnvVar {
            name: "LOGNAME".to_string(),
            value: Some(u.username.clone()),
            ..Default::default()
        });
        if let Some(home) = &u.home_dir {
            env_vars.push(EnvVar {
                name: "HOME".to_string(),
                value: Some(home.clone()),
                ..Default::default()
            });
        }
    }

    let container = Container {
        name: "worker".to_string(),
        image: Some(container_spec.image.clone()),
        command: if container_spec.command.is_empty() {
            None
        } else {
            Some(container_spec.command.clone())
        },
        args: if container_spec.args.is_empty() {
            None
        } else {
            Some(container_spec.args.clone())
        },
        env: Some(env_vars),
        ports: Some(vec![ContainerPort {
            container_port: 22,
            name: Some("ssh".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let svc_name = mpi::headless_service_name(job_name);

    let security_context = user.map(|u| PodSecurityContext {
        run_as_user: Some(u.uid as i64),
        run_as_group: Some(u.gid as i64),
        supplemental_groups: if u.supplemental_groups.is_empty() {
            None
        } else {
            Some(u.supplemental_groups.iter().map(|g| *g as i64).collect())
        },
        ..Default::default()
    });

    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(pod_name.clone()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            hostname: Some(pod_name),
            subdomain: Some(svc_name),
            node_name: Some(node_name.to_string()),
            restart_policy: Some("Never".to_string()),
            host_network: if container_spec.host_network {
                Some(true)
            } else {
                None
            },
            security_context,
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Build the launcher Pod object that runs the MPI command.
pub(crate) fn build_launcher_pod(
    job_name: &str,
    namespace: &str,
    spec: &WrenJobSpec,
    placement: &Placement,
    user: Option<&UserIdentity>,
) -> Result<Pod, WrenError> {
    let pod_name = mpi::launcher_pod_name(job_name);
    let labels = mpi::job_labels(job_name, "launcher", None);

    let container_spec = spec
        .container
        .as_ref()
        .ok_or_else(|| WrenError::ValidationError {
            reason: "container spec required for container backend".to_string(),
        })?;

    let hostfile_path = "/etc/wren/hostfile";
    let mut command = mpi::mpirun_args(spec, hostfile_path);

    // Append user command/args after mpirun flags
    if !container_spec.command.is_empty() {
        command.extend(container_spec.command.clone());
    }
    if !container_spec.args.is_empty() {
        command.extend(container_spec.args.clone());
    }

    let mut env_vars = vec![
        EnvVar {
            name: "WREN_JOB_NAME".to_string(),
            value: Some(job_name.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "WREN_NUM_NODES".to_string(),
            value: Some(spec.nodes.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "WREN_TOTAL_RANKS".to_string(),
            value: Some(mpi::total_ranks(spec).to_string()),
            ..Default::default()
        },
    ];

    // Inject user identity env vars
    if let Some(u) = user {
        env_vars.push(EnvVar {
            name: "USER".to_string(),
            value: Some(u.username.clone()),
            ..Default::default()
        });
        env_vars.push(EnvVar {
            name: "LOGNAME".to_string(),
            value: Some(u.username.clone()),
            ..Default::default()
        });
        if let Some(home) = &u.home_dir {
            env_vars.push(EnvVar {
                name: "HOME".to_string(),
                value: Some(home.clone()),
                ..Default::default()
            });
        }
    }

    let container = Container {
        name: "launcher".to_string(),
        image: Some(container_spec.image.clone()),
        command: Some(command),
        env: Some(env_vars),
        ..Default::default()
    };

    // Launch on the first node in the placement
    let launcher_node = placement.nodes.first().cloned();

    let security_context = user.map(|u| PodSecurityContext {
        run_as_user: Some(u.uid as i64),
        run_as_group: Some(u.gid as i64),
        supplemental_groups: if u.supplemental_groups.is_empty() {
            None
        } else {
            Some(u.supplemental_groups.iter().map(|g| *g as i64).collect())
        },
        ..Default::default()
    });

    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(pod_name),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            node_name: launcher_node,
            restart_policy: Some("Never".to_string()),
            security_context,
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Interprets the status of a job from its pods.
/// Separated from the async `status()` method for testability.
pub(crate) fn interpret_pod_status(pods: &[Pod]) -> BackendJobStatus {
    if pods.is_empty() {
        return BackendJobStatus::NotFound;
    }

    let workers: Vec<&Pod> = pods
        .iter()
        .filter(|p| {
            p.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("wren.giar.dev/role"))
                == Some(&"worker".to_string())
        })
        .collect();

    let launcher: Option<&Pod> = pods.iter().find(|p| {
        p.metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("wren.giar.dev/role"))
            == Some(&"launcher".to_string())
    });

    if let Some(launcher_pod) = launcher {
        // MPI launcher pattern: launcher pod drives completion
        if let Some(status) = &launcher_pod.status {
            if let Some(phase) = &status.phase {
                match phase.as_str() {
                    "Succeeded" => return BackendJobStatus::Succeeded,
                    "Failed" => {
                        let msg = status
                            .message
                            .clone()
                            .unwrap_or_else(|| "launcher pod failed".to_string());
                        return BackendJobStatus::Failed { message: msg };
                    }
                    _ => {}
                }
            }
        }

        // Count ready workers
        let ready = workers
            .iter()
            .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
            .count() as u32;

        let total = workers.len() as u32;

        if ready == total && total > 0 {
            BackendJobStatus::Running
        } else {
            BackendJobStatus::Launching { ready, total }
        }
    } else {
        // Simple mode (no launcher): worker pods drive completion
        let total = workers.len() as u32;
        let succeeded = workers
            .iter()
            .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Succeeded"))
            .count() as u32;
        let failed = workers
            .iter()
            .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Failed"))
            .count() as u32;
        let running = workers
            .iter()
            .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
            .count() as u32;

        if failed > 0 {
            BackendJobStatus::Failed {
                message: format!("{} of {} worker pods failed", failed, total),
            }
        } else if succeeded == total && total > 0 {
            BackendJobStatus::Succeeded
        } else if running > 0 || succeeded > 0 {
            BackendJobStatus::Running
        } else {
            BackendJobStatus::Launching {
                ready: running + succeeded,
                total,
            }
        }
    }
}

/// Container-based execution backend.
/// Creates Pods + Services for MPI worker/launcher pattern.
/// For simple single-node jobs without MPI, runs the user command directly.
pub struct ContainerBackend {
    client: Client,
}

impl ContainerBackend {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Create the headless service for worker pod DNS discovery.
    async fn create_headless_service(
        &self,
        job_name: &str,
        namespace: &str,
    ) -> Result<(), WrenError> {
        let svc_api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        let svc = build_headless_service(job_name, namespace);
        let svc_name = svc.metadata.name.clone().unwrap_or_default();
        match svc_api.create(&PostParams::default(), &svc).await {
            Ok(_) => debug!(service = %svc_name, "created headless service"),
            Err(kube::Error::Api(ref err)) if err.code == 409 => {
                warn!(service = %svc_name, "headless service already exists, continuing");
            }
            Err(e) => return Err(WrenError::KubeError(e)),
        }
        Ok(())
    }

    /// Create the hostfile ConfigMap.
    async fn create_hostfile_configmap(
        &self,
        job_name: &str,
        namespace: &str,
        placement: &Placement,
        tasks_per_node: u32,
    ) -> Result<(), WrenError> {
        let cm_api: Api<ConfigMap> = Api::namespaced(self.client.clone(), namespace);
        let cm = build_hostfile_configmap(job_name, namespace, placement, tasks_per_node);
        let cm_name = cm.metadata.name.clone().unwrap_or_default();
        match cm_api.create(&PostParams::default(), &cm).await {
            Ok(_) => debug!(configmap = %cm_name, "created hostfile configmap"),
            Err(kube::Error::Api(ref err)) if err.code == 409 => {
                warn!(configmap = %cm_name, "hostfile configmap already exists, continuing");
            }
            Err(e) => return Err(WrenError::KubeError(e)),
        }
        Ok(())
    }

    /// Create a worker pod on a specific node.
    async fn create_worker_pod(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &WrenJobSpec,
        node_name: &str,
        rank: u32,
        user: Option<&UserIdentity>,
    ) -> Result<String, WrenError> {
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod = build_worker_pod(job_name, namespace, spec, node_name, rank, user)?;
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        match pod_api.create(&PostParams::default(), &pod).await {
            Ok(_) => debug!(pod = %pod_name, node = %node_name, rank, "created worker pod"),
            Err(kube::Error::Api(ref err)) if err.code == 409 => {
                // Pod already exists — tolerate this to handle re-creation races
                warn!(pod = %pod_name, "worker pod already exists, continuing");
            }
            Err(e) => return Err(WrenError::KubeError(e)),
        }
        Ok(pod_name)
    }

    /// Create the launcher pod that runs the MPI command.
    async fn create_launcher_pod(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &WrenJobSpec,
        placement: &Placement,
        user: Option<&UserIdentity>,
    ) -> Result<String, WrenError> {
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod = build_launcher_pod(job_name, namespace, spec, placement, user)?;
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        match pod_api.create(&PostParams::default(), &pod).await {
            Ok(_) => debug!(pod = %pod_name, "created launcher pod"),
            Err(kube::Error::Api(ref err)) if err.code == 409 => {
                warn!(pod = %pod_name, "launcher pod already exists, continuing");
            }
            Err(e) => return Err(WrenError::KubeError(e)),
        }
        Ok(pod_name)
    }
}

#[async_trait]
impl ExecutionBackend for ContainerBackend {
    async fn launch(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &WrenJobSpec,
        placement: &Placement,
        user: Option<&UserIdentity>,
    ) -> Result<LaunchResult, WrenError> {
        let use_launcher = needs_mpi_launcher(spec);

        if use_launcher {
            info!(job = job_name, nodes = ?placement.nodes, "launching MPI job with launcher pattern");

            // 1. Create headless service for DNS discovery
            self.create_headless_service(job_name, namespace).await?;

            // 2. Create hostfile ConfigMap
            self.create_hostfile_configmap(job_name, namespace, placement, spec.tasks_per_node)
                .await?;

            // 3. Create worker pods — one per node
            let mut resource_ids = Vec::new();
            for (rank, node) in placement.nodes.iter().enumerate() {
                let pod_name = self
                    .create_worker_pod(job_name, namespace, spec, node, rank as u32, user)
                    .await?;
                resource_ids.push(pod_name);
            }

            // 4. Create launcher pod
            let launcher = self
                .create_launcher_pod(job_name, namespace, spec, placement, user)
                .await?;
            resource_ids.push(launcher);

            Ok(LaunchResult {
                resource_ids,
                message: format!(
                    "launched {} worker pods + launcher on {} nodes",
                    placement.nodes.len(),
                    placement.nodes.len()
                ),
            })
        } else {
            info!(job = job_name, nodes = ?placement.nodes, "launching job without MPI launcher");

            // No MPI: create a worker pod per node that runs the user's command directly
            let mut resource_ids = Vec::new();
            for (rank, node) in placement.nodes.iter().enumerate() {
                let pod_name = self
                    .create_worker_pod(job_name, namespace, spec, node, rank as u32, user)
                    .await?;
                resource_ids.push(pod_name);
            }

            Ok(LaunchResult {
                resource_ids,
                message: format!(
                    "launched {} worker pods (no MPI launcher) on {} nodes",
                    placement.nodes.len(),
                    placement.nodes.len()
                ),
            })
        }
    }

    async fn status(&self, job_name: &str, namespace: &str) -> Result<BackendJobStatus, WrenError> {
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let lp = ListParams::default().labels(&format!("wren.giar.dev/job-name={}", job_name));
        let pods = pod_api.list(&lp).await.map_err(WrenError::KubeError)?;
        Ok(interpret_pod_status(&pods.items))
    }

    async fn terminate(&self, job_name: &str, namespace: &str) -> Result<(), WrenError> {
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let lp = ListParams::default().labels(&format!("wren.giar.dev/job-name={}", job_name));
        let pods = pod_api.list(&lp).await.map_err(WrenError::KubeError)?;

        for pod in &pods.items {
            if let Some(name) = &pod.metadata.name {
                match pod_api.delete(name, &DeleteParams::default()).await {
                    Ok(_) => debug!(pod = %name, "deleted pod"),
                    Err(e) => warn!(pod = %name, error = %e, "failed to delete pod"),
                }
            }
        }
        Ok(())
    }

    async fn cleanup(&self, job_name: &str, namespace: &str) -> Result<(), WrenError> {
        // Note: pods are intentionally NOT deleted here so that logs
        // remain available after job completion. Pods are cleaned up
        // by the reconciler after a TTL (see handle_terminal).

        // Delete headless service
        let svc_api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        let svc_name = mpi::headless_service_name(job_name);
        match svc_api.delete(&svc_name, &DeleteParams::default()).await {
            Ok(_) => debug!(service = %svc_name, "deleted service"),
            Err(e) => warn!(service = %svc_name, error = %e, "failed to delete service"),
        }

        // Delete hostfile configmap
        let cm_api: Api<ConfigMap> = Api::namespaced(self.client.clone(), namespace);
        let cm_name = mpi::hostfile_configmap_name(job_name);
        match cm_api.delete(&cm_name, &DeleteParams::default()).await {
            Ok(_) => debug!(configmap = %cm_name, "deleted configmap"),
            Err(e) => warn!(configmap = %cm_name, error = %e, "failed to delete configmap"),
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::PodStatus;
    use wren_core::{ContainerSpec, EnvVar as WrenEnvVar, ExecutionBackendType, MPISpec};

    fn make_spec_no_mpi() -> wren_core::WrenJobSpec {
        wren_core::WrenJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes: 2,
            tasks_per_node: 1,
            backend: ExecutionBackendType::Container,
            container: Some(ContainerSpec {
                image: "busybox".to_string(),
                command: vec!["sleep".to_string(), "infinity".to_string()],
                args: vec![],
                resources: None,
                host_network: false,
                volume_mounts: vec![],
                env: vec![],
            }),
            reaper: None,
            mpi: None,
            topology: None,
            dependencies: vec![],
            project: None,
        }
    }

    fn make_spec_with_mpi() -> wren_core::WrenJobSpec {
        wren_core::WrenJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes: 4,
            tasks_per_node: 8,
            backend: ExecutionBackendType::Container,
            container: Some(ContainerSpec {
                image: "openmpi:latest".to_string(),
                command: vec![],
                args: vec![],
                resources: None,
                host_network: true,
                volume_mounts: vec![],
                env: vec![],
            }),
            reaper: None,
            mpi: Some(MPISpec {
                implementation: "openmpi".to_string(),
                ssh_auth: true,
                fabric_interface: None,
            }),
            topology: None,
            dependencies: vec![],
            project: None,
        }
    }

    fn make_placement(nodes: &[&str]) -> Placement {
        Placement {
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            score: 1.0,
        }
    }

    fn make_pod(role: &str, rank: Option<u32>, phase: &str) -> Pod {
        let mut labels = BTreeMap::new();
        labels.insert("wren.giar.dev/role".to_string(), role.to_string());
        labels.insert("wren.giar.dev/job-name".to_string(), "test-job".to_string());
        if let Some(r) = rank {
            labels.insert("wren.giar.dev/rank".to_string(), r.to_string());
        }
        Pod {
            metadata: ObjectMeta {
                name: Some(format!("test-job-{}-{}", role, rank.unwrap_or(0))),
                labels: Some(labels),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some(phase.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    // --- needs_mpi_launcher tests ---

    #[test]
    fn test_needs_mpi_launcher_without_mpi_spec_returns_false() {
        let spec = make_spec_no_mpi();
        assert!(!needs_mpi_launcher(&spec));
    }

    #[test]
    fn test_needs_mpi_launcher_with_mpi_spec_returns_true() {
        let spec = make_spec_with_mpi();
        assert!(needs_mpi_launcher(&spec));
    }

    #[test]
    fn test_needs_mpi_launcher_mpi_none_is_false() {
        let mut spec = make_spec_with_mpi();
        spec.mpi = None;
        assert!(!needs_mpi_launcher(&spec));
    }

    #[test]
    fn test_needs_mpi_launcher_cray_mpich_is_true() {
        let mut spec = make_spec_with_mpi();
        spec.mpi = Some(MPISpec {
            implementation: "cray-mpich".to_string(),
            ssh_auth: false,
            fabric_interface: Some("hsn0".to_string()),
        });
        assert!(needs_mpi_launcher(&spec));
    }

    // --- build_headless_service tests ---

    #[test]
    fn test_build_headless_service_name_and_namespace() {
        let svc = build_headless_service("my-job", "my-ns");
        assert_eq!(svc.metadata.name.as_deref(), Some("my-job-workers"));
        assert_eq!(svc.metadata.namespace.as_deref(), Some("my-ns"));
    }

    #[test]
    fn test_build_headless_service_cluster_ip_none() {
        let svc = build_headless_service("my-job", "default");
        let spec = svc.spec.unwrap();
        assert_eq!(spec.cluster_ip.as_deref(), Some("None"));
    }

    #[test]
    fn test_build_headless_service_labels() {
        let svc = build_headless_service("my-job", "default");
        let labels = svc.metadata.labels.unwrap();
        assert_eq!(labels["wren.giar.dev/job-name"], "my-job");
        assert_eq!(labels["wren.giar.dev/role"], "worker");
        assert_eq!(labels["app.kubernetes.io/managed-by"], "wren");
    }

    #[test]
    fn test_build_headless_service_ports() {
        let svc = build_headless_service("my-job", "default");
        let ports = svc.spec.unwrap().ports.unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, 22);
        assert_eq!(ports[0].name.as_deref(), Some("ssh"));
    }

    // --- build_hostfile_configmap tests ---

    #[test]
    fn test_build_hostfile_configmap_single_node() {
        let placement = make_placement(&["node-0"]);
        let cm = build_hostfile_configmap("my-job", "default", &placement, 1);
        let data = cm.data.unwrap();
        assert_eq!(data["hostfile"], "node-0 slots=1");
    }

    #[test]
    fn test_build_hostfile_configmap_multi_node() {
        let placement = make_placement(&["node-0", "node-1", "node-2", "node-3"]);
        let cm = build_hostfile_configmap("sim", "hpc", &placement, 8);
        let data = cm.data.unwrap();
        assert_eq!(
            data["hostfile"],
            "node-0 slots=8\nnode-1 slots=8\nnode-2 slots=8\nnode-3 slots=8"
        );
    }

    #[test]
    fn test_build_hostfile_configmap_labels() {
        let placement = make_placement(&["node-0"]);
        let cm = build_hostfile_configmap("my-job", "default", &placement, 1);
        let labels = cm.metadata.labels.unwrap();
        assert_eq!(labels["wren.giar.dev/job-name"], "my-job");
        assert_eq!(labels["wren.giar.dev/role"], "config");
        assert_eq!(labels["app.kubernetes.io/managed-by"], "wren");
    }

    #[test]
    fn test_build_hostfile_configmap_name() {
        let placement = make_placement(&["node-0"]);
        let cm = build_hostfile_configmap("my-job", "default", &placement, 1);
        assert_eq!(cm.metadata.name.as_deref(), Some("my-job-hostfile"));
    }

    // --- build_worker_pod tests ---

    #[test]
    fn test_build_worker_pod_name_and_labels() {
        let spec = make_spec_no_mpi();
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, None).unwrap();
        assert_eq!(pod.metadata.name.as_deref(), Some("my-job-worker-0"));
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(labels["wren.giar.dev/job-name"], "my-job");
        assert_eq!(labels["wren.giar.dev/role"], "worker");
        assert_eq!(labels["wren.giar.dev/rank"], "0");
    }

    #[test]
    fn test_build_worker_pod_node_affinity() {
        let spec = make_spec_no_mpi();
        let pod = build_worker_pod("my-job", "default", &spec, "node-42", 0, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        assert_eq!(pod_spec.node_name.as_deref(), Some("node-42"));
    }

    #[test]
    fn test_build_worker_pod_env_vars() {
        let spec = make_spec_no_mpi();
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 3, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        let rank_var = env.iter().find(|e| e.name == "WREN_RANK").unwrap();
        assert_eq!(rank_var.value.as_deref(), Some("3"));
        let job_var = env.iter().find(|e| e.name == "WREN_JOB_NAME").unwrap();
        assert_eq!(job_var.value.as_deref(), Some("my-job"));
    }

    #[test]
    fn test_build_worker_pod_user_env_vars() {
        let mut spec = make_spec_no_mpi();
        spec.container = Some(ContainerSpec {
            image: "busybox".to_string(),
            command: vec![],
            args: vec![],
            resources: None,
            host_network: false,
            volume_mounts: vec![],
            env: vec![WrenEnvVar {
                name: "MY_VAR".to_string(),
                value: "hello".to_string(),
            }],
        });
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        let user_var = env.iter().find(|e| e.name == "MY_VAR").unwrap();
        assert_eq!(user_var.value.as_deref(), Some("hello"));
    }

    #[test]
    fn test_build_worker_pod_host_network_true() {
        let mut spec = make_spec_no_mpi();
        spec.container.as_mut().unwrap().host_network = true;
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        assert_eq!(pod_spec.host_network, Some(true));
    }

    #[test]
    fn test_build_worker_pod_host_network_false() {
        let spec = make_spec_no_mpi(); // host_network = false
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        assert_eq!(pod_spec.host_network, None);
    }

    #[test]
    fn test_build_worker_pod_no_container_spec() {
        let mut spec = make_spec_no_mpi();
        spec.container = None;
        let result = build_worker_pod("my-job", "default", &spec, "node-0", 0, None);
        assert!(matches!(result, Err(WrenError::ValidationError { .. })));
    }

    #[test]
    fn test_build_worker_pod_empty_command() {
        let mut spec = make_spec_no_mpi();
        spec.container.as_mut().unwrap().command = vec![];
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        assert_eq!(pod_spec.containers[0].command, None);
    }

    #[test]
    fn test_build_worker_pod_with_command() {
        let spec = make_spec_no_mpi(); // command = ["sleep", "infinity"]
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        assert_eq!(
            pod_spec.containers[0].command,
            Some(vec!["sleep".to_string(), "infinity".to_string()])
        );
    }

    #[test]
    fn test_build_worker_pod_restart_policy() {
        let spec = make_spec_no_mpi();
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        assert_eq!(pod_spec.restart_policy.as_deref(), Some("Never"));
    }

    #[test]
    fn test_build_worker_pod_subdomain() {
        let spec = make_spec_no_mpi();
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        assert_eq!(pod_spec.subdomain.as_deref(), Some("my-job-workers"));
    }

    // --- build_launcher_pod tests ---

    #[test]
    fn test_build_launcher_pod_name_and_labels() {
        let spec = make_spec_with_mpi();
        let placement = make_placement(&["node-0", "node-1"]);
        let pod = build_launcher_pod("my-job", "default", &spec, &placement, None).unwrap();
        assert_eq!(pod.metadata.name.as_deref(), Some("my-job-launcher"));
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(labels["wren.giar.dev/role"], "launcher");
        assert_eq!(labels["wren.giar.dev/job-name"], "my-job");
        assert!(!labels.contains_key("wren.giar.dev/rank"));
    }

    #[test]
    fn test_build_launcher_pod_mpirun_command() {
        let spec = make_spec_with_mpi();
        let placement = make_placement(&["node-0", "node-1"]);
        let pod = build_launcher_pod("my-job", "default", &spec, &placement, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        let cmd = pod_spec.containers[0].command.as_ref().unwrap();
        assert_eq!(cmd[0], "mpirun");
        assert!(cmd.contains(&"-np".to_string()));
        assert!(cmd.contains(&"--hostfile".to_string()));
    }

    #[test]
    fn test_build_launcher_pod_env_vars() {
        let spec = make_spec_with_mpi();
        let placement = make_placement(&["node-0", "node-1", "node-2", "node-3"]);
        let pod = build_launcher_pod("my-job", "default", &spec, &placement, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        let job_name_var = env.iter().find(|e| e.name == "WREN_JOB_NAME").unwrap();
        assert_eq!(job_name_var.value.as_deref(), Some("my-job"));
        let num_nodes_var = env.iter().find(|e| e.name == "WREN_NUM_NODES").unwrap();
        assert_eq!(num_nodes_var.value.as_deref(), Some("4"));
        let total_ranks_var = env.iter().find(|e| e.name == "WREN_TOTAL_RANKS").unwrap();
        assert_eq!(total_ranks_var.value.as_deref(), Some("32")); // 4 nodes * 8 tasks_per_node
    }

    #[test]
    fn test_build_launcher_pod_node_placement() {
        let spec = make_spec_with_mpi();
        let placement = make_placement(&["first-node", "second-node"]);
        let pod = build_launcher_pod("my-job", "default", &spec, &placement, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        assert_eq!(pod_spec.node_name.as_deref(), Some("first-node"));
    }

    #[test]
    fn test_build_launcher_pod_no_container_spec() {
        let mut spec = make_spec_with_mpi();
        spec.container = None;
        let placement = make_placement(&["node-0"]);
        let result = build_launcher_pod("my-job", "default", &spec, &placement, None);
        assert!(matches!(result, Err(WrenError::ValidationError { .. })));
    }

    #[test]
    fn test_build_launcher_pod_user_command_appended() {
        let mut spec = make_spec_with_mpi();
        spec.container.as_mut().unwrap().command = vec!["./my_app".to_string()];
        spec.container.as_mut().unwrap().args = vec!["--input".to_string(), "data.h5".to_string()];
        let placement = make_placement(&["node-0"]);
        let pod = build_launcher_pod("my-job", "default", &spec, &placement, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        let cmd = pod_spec.containers[0].command.as_ref().unwrap();
        // mpirun args come first, then user command/args
        assert_eq!(cmd[0], "mpirun");
        assert!(cmd.contains(&"./my_app".to_string()));
        assert!(cmd.contains(&"--input".to_string()));
        assert!(cmd.contains(&"data.h5".to_string()));
        // user command comes after mpirun flags
        let mpirun_idx = cmd.iter().position(|s| s == "mpirun").unwrap();
        let app_idx = cmd.iter().position(|s| s == "./my_app").unwrap();
        assert!(app_idx > mpirun_idx);
    }

    // --- interpret_pod_status tests ---

    #[test]
    fn test_interpret_pod_status_empty_pods() {
        assert_eq!(interpret_pod_status(&[]), BackendJobStatus::NotFound);
    }

    #[test]
    fn test_interpret_pod_status_launcher_succeeded() {
        let pods = vec![
            make_pod("worker", Some(0), "Running"),
            make_pod("launcher", None, "Succeeded"),
        ];
        assert_eq!(interpret_pod_status(&pods), BackendJobStatus::Succeeded);
    }

    #[test]
    fn test_interpret_pod_status_launcher_failed() {
        let pods = vec![
            make_pod("worker", Some(0), "Running"),
            make_pod("launcher", None, "Failed"),
        ];
        let status = interpret_pod_status(&pods);
        assert!(matches!(status, BackendJobStatus::Failed { .. }));
        if let BackendJobStatus::Failed { message } = status {
            assert!(!message.is_empty());
        }
    }

    #[test]
    fn test_interpret_pod_status_launcher_running_all_workers_ready() {
        let pods = vec![
            make_pod("worker", Some(0), "Running"),
            make_pod("worker", Some(1), "Running"),
            make_pod("launcher", None, "Running"),
        ];
        assert_eq!(interpret_pod_status(&pods), BackendJobStatus::Running);
    }

    #[test]
    fn test_interpret_pod_status_launcher_running_some_workers_pending() {
        let pods = vec![
            make_pod("worker", Some(0), "Running"),
            make_pod("worker", Some(1), "Pending"),
            make_pod("launcher", None, "Running"),
        ];
        let status = interpret_pod_status(&pods);
        assert_eq!(status, BackendJobStatus::Launching { ready: 1, total: 2 });
    }

    #[test]
    fn test_interpret_pod_status_no_launcher_all_succeeded() {
        let pods = vec![
            make_pod("worker", Some(0), "Succeeded"),
            make_pod("worker", Some(1), "Succeeded"),
        ];
        assert_eq!(interpret_pod_status(&pods), BackendJobStatus::Succeeded);
    }

    #[test]
    fn test_interpret_pod_status_no_launcher_some_failed() {
        let pods = vec![
            make_pod("worker", Some(0), "Succeeded"),
            make_pod("worker", Some(1), "Failed"),
        ];
        let status = interpret_pod_status(&pods);
        assert!(matches!(status, BackendJobStatus::Failed { .. }));
    }

    #[test]
    fn test_interpret_pod_status_no_launcher_some_running() {
        let pods = vec![
            make_pod("worker", Some(0), "Running"),
            make_pod("worker", Some(1), "Pending"),
        ];
        assert_eq!(interpret_pod_status(&pods), BackendJobStatus::Running);
    }

    #[test]
    fn test_interpret_pod_status_no_launcher_all_pending() {
        let pods = vec![
            make_pod("worker", Some(0), "Pending"),
            make_pod("worker", Some(1), "Pending"),
        ];
        assert_eq!(
            interpret_pod_status(&pods),
            BackendJobStatus::Launching { ready: 0, total: 2 }
        );
    }

    // --- user identity tests ---

    fn make_user_identity() -> UserIdentity {
        UserIdentity {
            username: "miguel".to_string(),
            uid: 1001,
            gid: 1001,
            supplemental_groups: vec![1001, 5000],
            home_dir: Some("/home/miguel".to_string()),
            default_project: Some("climate-sim".to_string()),
        }
    }

    #[test]
    fn test_build_worker_pod_with_user_identity_sets_security_context() {
        let spec = make_spec_no_mpi();
        let user = make_user_identity();
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, Some(&user)).unwrap();
        let pod_spec = pod.spec.unwrap();
        let sc = pod_spec.security_context.unwrap();
        assert_eq!(sc.run_as_user, Some(1001));
        assert_eq!(sc.run_as_group, Some(1001));
        assert_eq!(sc.supplemental_groups, Some(vec![1001, 5000]));
    }

    #[test]
    fn test_build_worker_pod_with_user_identity_sets_env_vars() {
        let spec = make_spec_no_mpi();
        let user = make_user_identity();
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, Some(&user)).unwrap();
        let pod_spec = pod.spec.unwrap();
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        let user_var = env.iter().find(|e| e.name == "USER").unwrap();
        assert_eq!(user_var.value.as_deref(), Some("miguel"));
        let logname_var = env.iter().find(|e| e.name == "LOGNAME").unwrap();
        assert_eq!(logname_var.value.as_deref(), Some("miguel"));
        let home_var = env.iter().find(|e| e.name == "HOME").unwrap();
        assert_eq!(home_var.value.as_deref(), Some("/home/miguel"));
    }

    #[test]
    fn test_build_worker_pod_without_user_no_security_context() {
        let spec = make_spec_no_mpi();
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        assert!(pod_spec.security_context.is_none());
    }

    #[test]
    fn test_build_worker_pod_without_user_no_user_env_vars() {
        let spec = make_spec_no_mpi();
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, None).unwrap();
        let pod_spec = pod.spec.unwrap();
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        assert!(env.iter().all(|e| e.name != "USER"));
        assert!(env.iter().all(|e| e.name != "LOGNAME"));
        assert!(env.iter().all(|e| e.name != "HOME"));
    }

    #[test]
    fn test_build_launcher_pod_with_user_identity() {
        let spec = make_spec_with_mpi();
        let placement = make_placement(&["node-0", "node-1"]);
        let user = make_user_identity();
        let pod = build_launcher_pod("my-job", "default", &spec, &placement, Some(&user)).unwrap();
        let pod_spec = pod.spec.unwrap();
        let sc = pod_spec.security_context.unwrap();
        assert_eq!(sc.run_as_user, Some(1001));
        assert_eq!(sc.run_as_group, Some(1001));
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        assert!(env
            .iter()
            .any(|e| e.name == "USER" && e.value.as_deref() == Some("miguel")));
        assert!(env
            .iter()
            .any(|e| e.name == "HOME" && e.value.as_deref() == Some("/home/miguel")));
    }

    #[test]
    fn test_build_worker_pod_user_no_home_dir() {
        let spec = make_spec_no_mpi();
        let user = UserIdentity {
            username: "test".to_string(),
            uid: 2000,
            gid: 2000,
            supplemental_groups: vec![],
            home_dir: None,
            default_project: None,
        };
        let pod = build_worker_pod("my-job", "default", &spec, "node-0", 0, Some(&user)).unwrap();
        let pod_spec = pod.spec.unwrap();
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        assert!(env.iter().any(|e| e.name == "USER"));
        assert!(env.iter().all(|e| e.name != "HOME"));
        let sc = pod_spec.security_context.unwrap();
        assert!(sc.supplemental_groups.is_none());
    }

    // =========================================================================
    // Async tests for ContainerBackend ExecutionBackend trait impl
    // =========================================================================

    use bytes::Bytes;
    use http_body_util::Full;
    use std::sync::{Arc, Mutex};
    use tower::service_fn;

    /// Build a ContainerBackend with a mock client that tracks HTTP calls.
    /// The `router` closure maps `(method, path) -> (status, body)`.
    #[allow(clippy::type_complexity)]
    fn make_tracking_container(
        router: impl Fn(&str, &str) -> (u16, String) + Send + Sync + 'static,
    ) -> (ContainerBackend, Arc<Mutex<Vec<(String, String)>>>) {
        let calls = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let calls_clone = calls.clone();
        let router = Arc::new(router);
        let svc = service_fn(move |req: http::Request<_>| {
            let method = req.method().to_string();
            let uri = req.uri().path().to_string();
            calls_clone
                .lock()
                .unwrap()
                .push((method.clone(), uri.clone()));
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
        (ContainerBackend::new(client), calls)
    }

    /// Default router for container backend tests: returns valid JSON for each resource type.
    fn container_router(method: &str, path: &str) -> (u16, String) {
        if method == "GET" && path.contains("pods") {
            // Pod list with 2 running worker pods (must have role label for interpret_pod_status)
            return (
                200,
                serde_json::json!({
                    "apiVersion": "v1", "kind": "PodList", "metadata": {},
                    "items": [
                        {
                            "metadata": {"name": "job-worker-0", "namespace": "default",
                                "labels": {"wren.giar.dev/role": "worker"}},
                            "spec": {"containers": []},
                            "status": {"phase": "Running"}
                        },
                        {
                            "metadata": {"name": "job-worker-1", "namespace": "default",
                                "labels": {"wren.giar.dev/role": "worker"}},
                            "spec": {"containers": []},
                            "status": {"phase": "Running"}
                        }
                    ]
                })
                .to_string(),
            );
        }
        // POST (create) — return a minimal object with metadata.name
        if method == "POST" {
            return (
                201,
                serde_json::json!({
                    "metadata": {"name": "created-resource", "namespace": "default"},
                    "spec": {}
                })
                .to_string(),
            );
        }
        // DELETE — return a status OK
        if method == "DELETE" {
            return (
                200,
                serde_json::json!({
                    "apiVersion": "v1", "kind": "Status", "status": "Success"
                })
                .to_string(),
            );
        }
        (200, r#"{"metadata":{}}"#.to_string())
    }

    fn make_n_placement(n: usize) -> Placement {
        Placement {
            nodes: (0..n).map(|i| format!("node-{}", i)).collect(),
            score: 1.0,
        }
    }

    // --- launch() tests ---

    #[tokio::test]
    async fn test_launch_mpi_job_creates_all_resources() {
        let (backend, calls) = make_tracking_container(container_router);
        let spec = make_spec_with_mpi();
        let placement = make_n_placement(2);
        let result = backend
            .launch("mpi-job", "default", &spec, &placement, None)
            .await;
        assert!(result.is_ok());
        let launch = result.unwrap();
        assert!(!launch.resource_ids.is_empty());
        // Should create: 1 service + 1 configmap + 2 workers + 1 launcher = 5 POSTs
        let calls = calls.lock().unwrap();
        let post_count = calls.iter().filter(|(m, _)| m == "POST").count();
        assert_eq!(
            post_count, 5,
            "MPI job should create svc + cm + 2 workers + launcher"
        );
    }

    #[tokio::test]
    async fn test_launch_simple_job_no_launcher() {
        let (backend, calls) = make_tracking_container(container_router);
        let spec = make_spec_no_mpi();
        let placement = make_n_placement(2);
        let result = backend
            .launch("simple-job", "default", &spec, &placement, None)
            .await;
        assert!(result.is_ok());
        let launch = result.unwrap();
        assert_eq!(
            launch.resource_ids.len(),
            2,
            "simple job: 1 pod per node, no launcher"
        );
        let calls = calls.lock().unwrap();
        let post_count = calls.iter().filter(|(m, _)| m == "POST").count();
        assert_eq!(post_count, 2, "simple job should only create worker pods");
    }

    #[tokio::test]
    async fn test_launch_with_user_identity() {
        let (backend, _calls) = make_tracking_container(container_router);
        let spec = make_spec_no_mpi();
        let placement = make_n_placement(1);
        let user = UserIdentity {
            username: "alice".to_string(),
            uid: 1001,
            gid: 1001,
            supplemental_groups: vec![2000],
            home_dir: Some("/home/alice".to_string()),
            default_project: None,
        };
        let result = backend
            .launch("user-job", "default", &spec, &placement, Some(&user))
            .await;
        assert!(result.is_ok(), "launch with user identity should succeed");
    }

    #[tokio::test]
    async fn test_launch_tolerates_409_conflict() {
        // All POST requests return 409 (AlreadyExists) — should be tolerated
        let (backend, _) = make_tracking_container(|method, _path| {
            if method == "POST" {
                (
                    409,
                    serde_json::json!({
                        "kind": "Status", "apiVersion": "v1", "status": "Failure",
                        "reason": "AlreadyExists", "code": 409,
                        "message": "already exists"
                    })
                    .to_string(),
                )
            } else {
                container_router(method, _path)
            }
        });
        let spec = make_spec_no_mpi();
        let placement = make_n_placement(2);
        let result = backend
            .launch("conflict-job", "default", &spec, &placement, None)
            .await;
        assert!(result.is_ok(), "409 on create should be tolerated");
    }

    #[tokio::test]
    async fn test_launch_returns_resource_ids() {
        let (backend, _) = make_tracking_container(container_router);
        let spec = make_spec_with_mpi();
        let placement = make_n_placement(3);
        let launch = backend
            .launch("id-job", "default", &spec, &placement, None)
            .await
            .unwrap();
        // 3 workers + 1 launcher = 4 resource IDs
        assert_eq!(launch.resource_ids.len(), 4);
    }

    // --- status() tests ---

    #[tokio::test]
    async fn test_status_all_pods_running() {
        let (backend, _) = make_tracking_container(container_router);
        let status = backend.status("test-job", "default").await.unwrap();
        assert_eq!(status, BackendJobStatus::Running);
    }

    #[tokio::test]
    async fn test_status_no_pods_found() {
        let (backend, _) = make_tracking_container(|method, path| {
            if method == "GET" && path.contains("pods") {
                (
                    200,
                    serde_json::json!({
                        "apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": []
                    })
                    .to_string(),
                )
            } else {
                container_router(method, path)
            }
        });
        let status = backend.status("ghost-job", "default").await.unwrap();
        assert_eq!(status, BackendJobStatus::NotFound);
    }

    #[tokio::test]
    async fn test_status_some_pods_pending() {
        let (backend, _) = make_tracking_container(|method, path| {
            if method == "GET" && path.contains("pods") {
                (200, serde_json::json!({
                    "apiVersion": "v1", "kind": "PodList", "metadata": {},
                    "items": [
                        {"metadata": {"name": "w-0", "labels": {"wren.giar.dev/role": "worker"}}, "spec": {"containers": []}, "status": {"phase": "Running"}},
                        {"metadata": {"name": "w-1", "labels": {"wren.giar.dev/role": "worker"}}, "spec": {"containers": []}, "status": {"phase": "Pending"}}
                    ]
                }).to_string())
            } else {
                container_router(method, path)
            }
        });
        let status = backend.status("mixed-job", "default").await.unwrap();
        // interpret_pod_status (no-launcher mode): any running worker → Running
        assert_eq!(status, BackendJobStatus::Running);
    }

    // --- terminate() tests ---

    #[tokio::test]
    async fn test_terminate_deletes_pods() {
        let (backend, calls) = make_tracking_container(container_router);
        let result = backend.terminate("term-job", "default").await;
        assert!(result.is_ok());
        let calls = calls.lock().unwrap();
        // 1 GET (list pods) + 2 DELETE (one per pod from the default list)
        assert!(calls.iter().any(|(m, _)| m == "GET"), "should list pods");
        let delete_count = calls.iter().filter(|(m, _)| m == "DELETE").count();
        assert_eq!(delete_count, 2, "should delete each pod individually");
    }

    // --- cleanup() tests ---

    #[tokio::test]
    async fn test_cleanup_deletes_service_and_configmap() {
        let (backend, calls) = make_tracking_container(container_router);
        let result = backend.cleanup("clean-job", "default").await;
        assert!(result.is_ok());
        let calls = calls.lock().unwrap();
        let delete_count = calls.iter().filter(|(m, _)| m == "DELETE").count();
        assert_eq!(
            delete_count, 2,
            "should delete service + configmap (not pods)"
        );
        // Verify no pods were deleted (no GET for pod listing)
        assert!(
            !calls.iter().any(|(m, p)| m == "GET" && p.contains("pods")),
            "cleanup should not touch pods"
        );
    }
}
