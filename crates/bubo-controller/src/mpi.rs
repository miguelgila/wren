use bubo_core::{MPIJobSpec, Placement};

/// Generates an MPI hostfile from a placement.
///
/// Format (compatible with OpenMPI and MPICH):
/// ```text
/// node-0 slots=4
/// node-1 slots=4
/// ```
pub fn generate_hostfile(placement: &Placement, tasks_per_node: u32) -> String {
    placement
        .nodes
        .iter()
        .map(|node| format!("{} slots={}", node, tasks_per_node))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns the total number of MPI ranks for this job.
pub fn total_ranks(spec: &MPIJobSpec) -> u32 {
    spec.nodes * spec.tasks_per_node
}

/// Generates the mpirun command prefix based on the MPI spec.
pub fn mpirun_args(spec: &MPIJobSpec, hostfile_path: &str) -> Vec<String> {
    let mpi = spec.mpi.as_ref();
    let impl_name = mpi
        .map(|m| m.implementation.as_str())
        .unwrap_or("openmpi");

    let np = total_ranks(spec);

    let mut args = vec![
        "mpirun".to_string(),
        "-np".to_string(),
        np.to_string(),
        "--hostfile".to_string(),
        hostfile_path.to_string(),
    ];

    // Add implementation-specific flags
    match impl_name {
        "openmpi" => {
            args.push("--allow-run-as-root".to_string());
            if let Some(mpi_spec) = mpi {
                if let Some(ref iface) = mpi_spec.fabric_interface {
                    args.push("--mca".to_string());
                    args.push("btl_tcp_if_include".to_string());
                    args.push(iface.clone());
                }
            }
        }
        "cray-mpich" | "intel-mpi" => {
            // These typically use srun or process managers directly
        }
        _ => {}
    }

    args
}

/// Standard labels applied to all pods created for an MPIJob.
pub fn job_labels(job_name: &str, role: &str, rank: Option<u32>) -> std::collections::BTreeMap<String, String> {
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("app.kubernetes.io/managed-by".to_string(), "bubo".to_string());
    labels.insert("bubo.io/job-name".to_string(), job_name.to_string());
    labels.insert("bubo.io/role".to_string(), role.to_string());
    if let Some(r) = rank {
        labels.insert("bubo.io/rank".to_string(), r.to_string());
    }
    labels
}

/// Name of the headless service created for pod DNS discovery.
pub fn headless_service_name(job_name: &str) -> String {
    format!("{}-workers", job_name)
}

/// Name of the ConfigMap holding the hostfile.
pub fn hostfile_configmap_name(job_name: &str) -> String {
    format!("{}-hostfile", job_name)
}

/// Name of the Secret holding SSH keys for MPI bootstrap.
pub fn ssh_secret_name(job_name: &str) -> String {
    format!("{}-ssh", job_name)
}

/// Pod name for a worker at a given rank.
pub fn worker_pod_name(job_name: &str, rank: u32) -> String {
    format!("{}-worker-{}", job_name, rank)
}

/// Pod name for the launcher.
pub fn launcher_pod_name(job_name: &str) -> String {
    format!("{}-launcher", job_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubo_core::{MPISpec, TopologySpec};

    fn make_placement(nodes: &[&str]) -> Placement {
        Placement {
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            score: 1.0,
        }
    }

    fn make_spec(nodes: u32, tasks_per_node: u32) -> MPIJobSpec {
        MPIJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes,
            tasks_per_node,
            backend: bubo_core::ExecutionBackendType::Container,
            container: None,
            reaper: None,
            mpi: Some(MPISpec {
                implementation: "openmpi".to_string(),
                ssh_auth: true,
                fabric_interface: None,
            }),
            topology: None,
            dependencies: vec![],
        }
    }

    #[test]
    fn test_generate_hostfile() {
        let placement = make_placement(&["node-0", "node-1", "node-2"]);
        let hostfile = generate_hostfile(&placement, 4);
        assert_eq!(
            hostfile,
            "node-0 slots=4\nnode-1 slots=4\nnode-2 slots=4"
        );
    }

    #[test]
    fn test_total_ranks() {
        let spec = make_spec(4, 8);
        assert_eq!(total_ranks(&spec), 32);
    }

    #[test]
    fn test_mpirun_args_openmpi() {
        let spec = make_spec(2, 4);
        let args = mpirun_args(&spec, "/etc/bubo/hostfile");
        assert!(args.contains(&"mpirun".to_string()));
        assert!(args.contains(&"8".to_string())); // np = 2*4
        assert!(args.contains(&"--hostfile".to_string()));
        assert!(args.contains(&"--allow-run-as-root".to_string()));
    }

    #[test]
    fn test_mpirun_args_with_fabric_interface() {
        let mut spec = make_spec(2, 4);
        spec.mpi = Some(MPISpec {
            implementation: "openmpi".to_string(),
            ssh_auth: true,
            fabric_interface: Some("hsn0".to_string()),
        });
        let args = mpirun_args(&spec, "/etc/bubo/hostfile");
        assert!(args.contains(&"btl_tcp_if_include".to_string()));
        assert!(args.contains(&"hsn0".to_string()));
    }

    #[test]
    fn test_job_labels() {
        let labels = job_labels("my-sim", "worker", Some(3));
        assert_eq!(labels["bubo.io/job-name"], "my-sim");
        assert_eq!(labels["bubo.io/role"], "worker");
        assert_eq!(labels["bubo.io/rank"], "3");
    }

    #[test]
    fn test_naming_conventions() {
        assert_eq!(headless_service_name("sim"), "sim-workers");
        assert_eq!(hostfile_configmap_name("sim"), "sim-hostfile");
        assert_eq!(ssh_secret_name("sim"), "sim-ssh");
        assert_eq!(worker_pod_name("sim", 0), "sim-worker-0");
        assert_eq!(launcher_pod_name("sim"), "sim-launcher");
    }
}
