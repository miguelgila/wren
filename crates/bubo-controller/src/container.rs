use async_trait::async_trait;
use bubo_core::backend::{BackendJobStatus, ExecutionBackend, LaunchResult};
use bubo_core::{BuboError, MPIJobSpec, Placement};
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, ContainerPort, EnvVar, Pod, PodSpec, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::{Api, Client};
use std::collections::BTreeMap;
use tracing::{debug, info, warn};

use crate::mpi;

/// Container-based execution backend.
/// Creates Pods + Services for MPI worker/launcher pattern.
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
    ) -> Result<(), BuboError> {
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

        svc_api
            .create(&PostParams::default(), &svc)
            .await
            .map_err(BuboError::KubeError)?;
        debug!(service = %svc_name, "created headless service");
        Ok(())
    }

    /// Create the hostfile ConfigMap.
    async fn create_hostfile_configmap(
        &self,
        job_name: &str,
        namespace: &str,
        placement: &Placement,
        tasks_per_node: u32,
    ) -> Result<(), BuboError> {
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

        cm_api
            .create(&PostParams::default(), &cm)
            .await
            .map_err(BuboError::KubeError)?;
        debug!(configmap = %cm_name, "created hostfile configmap");
        Ok(())
    }

    /// Create a worker pod on a specific node.
    async fn create_worker_pod(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &MPIJobSpec,
        node_name: &str,
        rank: u32,
    ) -> Result<String, BuboError> {
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod_name = mpi::worker_pod_name(job_name, rank);
        let labels = mpi::job_labels(job_name, "worker", Some(rank));

        let container_spec = spec
            .container
            .as_ref()
            .ok_or_else(|| BuboError::ValidationError {
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
            name: "BUBO_RANK".to_string(),
            value: Some(rank.to_string()),
            ..Default::default()
        });
        env_vars.push(EnvVar {
            name: "BUBO_JOB_NAME".to_string(),
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

        pod_api
            .create(&PostParams::default(), &pod)
            .await
            .map_err(BuboError::KubeError)?;
        debug!(pod = %pod_name, node = %node_name, rank, "created worker pod");
        Ok(pod_name)
    }

    /// Create the launcher pod that runs the MPI command.
    async fn create_launcher_pod(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &MPIJobSpec,
        placement: &Placement,
    ) -> Result<String, BuboError> {
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod_name = mpi::launcher_pod_name(job_name);
        let labels = mpi::job_labels(job_name, "launcher", None);

        let container_spec = spec
            .container
            .as_ref()
            .ok_or_else(|| BuboError::ValidationError {
                reason: "container spec required for container backend".to_string(),
            })?;

        let hostfile_path = "/etc/bubo/hostfile";
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
                name: "BUBO_JOB_NAME".to_string(),
                value: Some(job_name.to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "BUBO_NUM_NODES".to_string(),
                value: Some(spec.nodes.to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "BUBO_TOTAL_RANKS".to_string(),
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

        pod_api
            .create(&PostParams::default(), &pod)
            .await
            .map_err(BuboError::KubeError)?;
        debug!(pod = %pod_name, "created launcher pod");
        Ok(pod_name)
    }
}

#[async_trait]
impl ExecutionBackend for ContainerBackend {
    async fn launch(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &MPIJobSpec,
        placement: &Placement,
    ) -> Result<LaunchResult, BuboError> {
        info!(job = job_name, nodes = ?placement.nodes, "launching container-based MPI job");

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
    }

    async fn status(
        &self,
        job_name: &str,
        namespace: &str,
    ) -> Result<BackendJobStatus, BuboError> {
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let lp = ListParams::default().labels(&format!("bubo.io/job-name={}", job_name));
        let pods = pod_api.list(&lp).await.map_err(BuboError::KubeError)?;

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
                    .and_then(|l| l.get("bubo.io/role"))
                    == Some(&"worker".to_string())
            })
            .collect();

        let launcher: Option<&Pod> = pods.items.iter().find(|p| {
            p.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("bubo.io/role"))
                == Some(&"launcher".to_string())
        });

        // Check launcher status
        if let Some(launcher_pod) = launcher {
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
        }

        // Count ready workers
        let ready = workers
            .iter()
            .filter(|p| {
                p.status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    == Some("Running")
            })
            .count() as u32;

        let total = workers.len() as u32;

        if ready == total && total > 0 {
            Ok(BackendJobStatus::Running)
        } else {
            Ok(BackendJobStatus::Launching { ready, total })
        }
    }

    async fn terminate(
        &self,
        job_name: &str,
        namespace: &str,
    ) -> Result<(), BuboError> {
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let lp = ListParams::default().labels(&format!("bubo.io/job-name={}", job_name));
        let pods = pod_api.list(&lp).await.map_err(BuboError::KubeError)?;

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

    async fn cleanup(
        &self,
        job_name: &str,
        namespace: &str,
    ) -> Result<(), BuboError> {
        // Delete pods
        self.terminate(job_name, namespace).await?;

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
