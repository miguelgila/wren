use async_trait::async_trait;
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, ContainerPort, EnvVar, Pod, PodSpec, Service, ServicePort, ServiceSpec,
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

/// Returns true if the job needs the full MPI launcher pattern
/// (headless service + hostfile + worker pods + launcher pod with mpirun).
/// Jobs without an explicit MPI spec run the user's command directly on
/// worker pods — one per node — without a launcher.
fn needs_mpi_launcher(spec: &wren_core::WrenJobSpec) -> bool {
    spec.mpi.is_some()
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
        let svc_name = mpi::headless_service_name(job_name);

        let labels = mpi::job_labels(job_name, "worker", None);

        let svc = Service {
            metadata: ObjectMeta {
                name: Some(svc_name.clone()),
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
        };

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
        let cm_name = mpi::hostfile_configmap_name(job_name);
        let hostfile_content = mpi::generate_hostfile(placement, tasks_per_node);

        let mut data = BTreeMap::new();
        data.insert("hostfile".to_string(), hostfile_content);

        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(cm_name.clone()),
                namespace: Some(namespace.to_string()),
                labels: Some(mpi::job_labels(job_name, "config", None)),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };

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
    ) -> Result<String, WrenError> {
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
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

        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name.clone()),
                namespace: Some(namespace.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![container],
                hostname: Some(pod_name.clone()),
                subdomain: Some(svc_name),
                node_name: Some(node_name.to_string()),
                restart_policy: Some("Never".to_string()),
                host_network: if container_spec.host_network {
                    Some(true)
                } else {
                    None
                },
                ..Default::default()
            }),
            ..Default::default()
        };

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
    ) -> Result<String, WrenError> {
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
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

        let env_vars = vec![
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

        let container = Container {
            name: "launcher".to_string(),
            image: Some(container_spec.image.clone()),
            command: Some(command),
            env: Some(env_vars),
            ..Default::default()
        };

        // Launch on the first node in the placement
        let launcher_node = placement.nodes.first().cloned();

        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name.clone()),
                namespace: Some(namespace.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![container],
                node_name: launcher_node,
                restart_policy: Some("Never".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

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
                    .create_worker_pod(job_name, namespace, spec, node, rank as u32)
                    .await?;
                resource_ids.push(pod_name);
            }

            // 4. Create launcher pod
            let launcher = self
                .create_launcher_pod(job_name, namespace, spec, placement)
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
                    .create_worker_pod(job_name, namespace, spec, node, rank as u32)
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

        if pods.items.is_empty() {
            return Ok(BackendJobStatus::NotFound);
        }

        let workers: Vec<&Pod> = pods
            .items
            .iter()
            .filter(|p| {
                p.metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("wren.giar.dev/role"))
                    == Some(&"worker".to_string())
            })
            .collect();

        let launcher: Option<&Pod> = pods.items.iter().find(|p| {
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
                        "Succeeded" => return Ok(BackendJobStatus::Succeeded),
                        "Failed" => {
                            let msg = status
                                .message
                                .clone()
                                .unwrap_or_else(|| "launcher pod failed".to_string());
                            return Ok(BackendJobStatus::Failed { message: msg });
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
                Ok(BackendJobStatus::Running)
            } else {
                Ok(BackendJobStatus::Launching { ready, total })
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
                Ok(BackendJobStatus::Failed {
                    message: format!("{} of {} worker pods failed", failed, total),
                })
            } else if succeeded == total && total > 0 {
                Ok(BackendJobStatus::Succeeded)
            } else if running > 0 || succeeded > 0 {
                Ok(BackendJobStatus::Running)
            } else {
                Ok(BackendJobStatus::Launching {
                    ready: running + succeeded,
                    total,
                })
            }
        }
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
