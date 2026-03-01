use wren_core::{WrenJobSpec, Placement};
use std::collections::HashMap;

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
pub fn total_ranks(spec: &WrenJobSpec) -> u32 {
    spec.nodes * spec.tasks_per_node
}

/// Generates the mpirun command prefix based on the MPI spec.
pub fn mpirun_args(spec: &WrenJobSpec, hostfile_path: &str) -> Vec<String> {
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

/// Standard labels applied to all pods created for a WrenJob.
pub fn job_labels(job_name: &str, role: &str, rank: Option<u32>) -> std::collections::BTreeMap<String, String> {
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("app.kubernetes.io/managed-by".to_string(), "wren".to_string());
    labels.insert("wren.io/job-name".to_string(), job_name.to_string());
    labels.insert("wren.io/role".to_string(), role.to_string());
    if let Some(r) = rank {
        labels.insert("wren.io/rank".to_string(), r.to_string());
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

/// Generates an MPI hostfile from actual node hostnames for bare-metal (Reaper) execution.
///
/// Format (compatible with OpenMPI and MPICH):
/// ```text
/// compute-01 slots=4
/// compute-02 slots=4
/// ```
pub fn generate_bare_metal_hostfile(nodes: &[String], tasks_per_node: u32) -> String {
    nodes
        .iter()
        .map(|node| format!("{} slots={}", node, tasks_per_node))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Generates the mpirun/srun command arguments for bare-metal (Reaper) execution.
///
/// Dispatches to the appropriate launcher for each MPI implementation:
/// - `cray-mpich`: generates `srun`-based arguments
/// - `openmpi`: like the container variant but without `--allow-run-as-root`
/// - `intel-mpi`: generates `mpiexec.hydra` arguments
pub fn bare_metal_mpirun_args(spec: &WrenJobSpec, hostfile_path: &str) -> Vec<String> {
    let mpi = spec.mpi.as_ref();
    let impl_name = mpi
        .map(|m| m.implementation.as_str())
        .unwrap_or("openmpi");

    match impl_name {
        "cray-mpich" => srun_args(spec, hostfile_path),
        "intel-mpi" => {
            let np = total_ranks(spec);
            let mut args = vec![
                "mpiexec.hydra".to_string(),
                "-np".to_string(),
                np.to_string(),
                "-machinefile".to_string(),
                hostfile_path.to_string(),
                "-ppn".to_string(),
                spec.tasks_per_node.to_string(),
            ];
            if let Some(mpi_spec) = mpi {
                if let Some(ref iface) = mpi_spec.fabric_interface {
                    args.push("-iface".to_string());
                    args.push(iface.clone());
                }
            }
            args
        }
        // openmpi and unknown implementations
        _ => {
            let np = total_ranks(spec);
            let mut args = vec![
                "mpirun".to_string(),
                "-np".to_string(),
                np.to_string(),
                "--hostfile".to_string(),
                hostfile_path.to_string(),
            ];
            if let Some(mpi_spec) = mpi {
                if let Some(ref iface) = mpi_spec.fabric_interface {
                    args.push("--mca".to_string());
                    args.push("btl_tcp_if_include".to_string());
                    args.push(iface.clone());
                }
            }
            args
        }
    }
}

/// Returns environment variables needed for bare-metal MPI execution via the Reaper backend.
///
/// Always includes:
/// - `WREN_JOB_NAME`, `WREN_NUM_NODES`, `WREN_TOTAL_RANKS`, `WREN_HOSTFILE`
///
/// For `cray-mpich`: adds `MPICH_OFI_STARTUP_CONNECT`, `MPICH_OFI_NUM_NICS`.
/// When a fabric interface is set: adds `MPICH_OFI_IFNAME` (cray-mpich) or
/// `I_MPI_FABRICS_LIST` / `UCX_NET_DEVICES` (intel-mpi / openmpi).
pub fn bare_metal_env_vars(
    job_name: &str,
    spec: &WrenJobSpec,
    nodes: &[String],
    hostfile_path: &str,
) -> HashMap<String, String> {
    let mpi = spec.mpi.as_ref();
    let impl_name = mpi
        .map(|m| m.implementation.as_str())
        .unwrap_or("openmpi");

    let total = total_ranks(spec);

    let mut env = HashMap::new();
    env.insert("WREN_JOB_NAME".to_string(), job_name.to_string());
    env.insert("WREN_NUM_NODES".to_string(), nodes.len().to_string());
    env.insert("WREN_TOTAL_RANKS".to_string(), total.to_string());
    env.insert("WREN_HOSTFILE".to_string(), hostfile_path.to_string());

    match impl_name {
        "cray-mpich" => {
            // Eagerly establish connections at startup to avoid first-message latency
            env.insert("MPICH_OFI_STARTUP_CONNECT".to_string(), "1".to_string());
            // Allow MPICH to use multiple NICs when available
            env.insert("MPICH_OFI_NUM_NICS".to_string(), "1".to_string());
            if let Some(mpi_spec) = mpi {
                if let Some(ref iface) = mpi_spec.fabric_interface {
                    env.insert("MPICH_OFI_IFNAME".to_string(), iface.clone());
                }
            }
        }
        "intel-mpi" => {
            if let Some(mpi_spec) = mpi {
                if let Some(ref iface) = mpi_spec.fabric_interface {
                    env.insert("I_MPI_FABRICS_LIST".to_string(), iface.clone());
                }
            }
        }
        // openmpi and others
        _ => {
            if let Some(mpi_spec) = mpi {
                if let Some(ref iface) = mpi_spec.fabric_interface {
                    env.insert("UCX_NET_DEVICES".to_string(), iface.clone());
                }
            }
        }
    }

    env
}

/// Generates Slurm-compatible `srun` arguments for cray-mpich bare-metal execution.
pub fn srun_args(spec: &WrenJobSpec, hostfile_path: &str) -> Vec<String> {
    let mut args = vec![
        "srun".to_string(),
        "--nodes".to_string(),
        spec.nodes.to_string(),
        "--ntasks-per-node".to_string(),
        spec.tasks_per_node.to_string(),
        "--hostfile".to_string(),
        hostfile_path.to_string(),
    ];

    if let Some(mpi_spec) = spec.mpi.as_ref() {
        if let Some(ref iface) = mpi_spec.fabric_interface {
            args.push("--network".to_string());
            args.push(iface.clone());
        }
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use wren_core::MPISpec;

    fn make_placement(nodes: &[&str]) -> Placement {
        Placement {
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            score: 1.0,
        }
    }

    fn make_spec(nodes: u32, tasks_per_node: u32) -> WrenJobSpec {
        WrenJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes,
            tasks_per_node,
            backend: wren_core::ExecutionBackendType::Container,
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
        let args = mpirun_args(&spec, "/etc/wren/hostfile");
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
        let args = mpirun_args(&spec, "/etc/wren/hostfile");
        assert!(args.contains(&"btl_tcp_if_include".to_string()));
        assert!(args.contains(&"hsn0".to_string()));
    }

    #[test]
    fn test_job_labels() {
        let labels = job_labels("my-sim", "worker", Some(3));
        assert_eq!(labels["wren.io/job-name"], "my-sim");
        assert_eq!(labels["wren.io/role"], "worker");
        assert_eq!(labels["wren.io/rank"], "3");
    }

    #[test]
    fn test_naming_conventions() {
        assert_eq!(headless_service_name("sim"), "sim-workers");
        assert_eq!(hostfile_configmap_name("sim"), "sim-hostfile");
        assert_eq!(ssh_secret_name("sim"), "sim-ssh");
        assert_eq!(worker_pod_name("sim", 0), "sim-worker-0");
        assert_eq!(launcher_pod_name("sim"), "sim-launcher");
    }

    // --- bare-metal helpers ---

    fn make_nodes(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn make_cray_spec(nodes: u32, tasks_per_node: u32, iface: Option<&str>) -> WrenJobSpec {
        WrenJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes,
            tasks_per_node,
            backend: wren_core::ExecutionBackendType::Reaper,
            container: None,
            reaper: None,
            mpi: Some(MPISpec {
                implementation: "cray-mpich".to_string(),
                ssh_auth: false,
                fabric_interface: iface.map(|s| s.to_string()),
            }),
            topology: None,
            dependencies: vec![],
        }
    }

    fn make_intel_spec(nodes: u32, tasks_per_node: u32, iface: Option<&str>) -> WrenJobSpec {
        WrenJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes,
            tasks_per_node,
            backend: wren_core::ExecutionBackendType::Reaper,
            container: None,
            reaper: None,
            mpi: Some(MPISpec {
                implementation: "intel-mpi".to_string(),
                ssh_auth: false,
                fabric_interface: iface.map(|s| s.to_string()),
            }),
            topology: None,
            dependencies: vec![],
        }
    }

    #[test]
    fn test_generate_bare_metal_hostfile() {
        let nodes = make_nodes(&["compute-01", "compute-02", "compute-03"]);
        let hostfile = generate_bare_metal_hostfile(&nodes, 4);
        assert_eq!(
            hostfile,
            "compute-01 slots=4\ncompute-02 slots=4\ncompute-03 slots=4"
        );
    }

    #[test]
    fn test_generate_bare_metal_hostfile_single_slot() {
        let nodes = make_nodes(&["node-a"]);
        let hostfile = generate_bare_metal_hostfile(&nodes, 1);
        assert_eq!(hostfile, "node-a slots=1");
    }

    #[test]
    fn test_bare_metal_mpirun_args_cray_mpich() {
        let spec = make_cray_spec(2, 4, None);
        let args = bare_metal_mpirun_args(&spec, "/etc/wren/hostfile");
        // cray-mpich delegates to srun
        assert_eq!(args[0], "srun");
        assert!(args.contains(&"--nodes".to_string()));
        assert!(args.contains(&"2".to_string()));
        assert!(args.contains(&"--ntasks-per-node".to_string()));
        assert!(args.contains(&"4".to_string()));
        assert!(args.contains(&"--hostfile".to_string()));
        assert!(args.contains(&"/etc/wren/hostfile".to_string()));
        // no --allow-run-as-root for bare-metal
        assert!(!args.contains(&"--allow-run-as-root".to_string()));
    }

    #[test]
    fn test_bare_metal_mpirun_args_openmpi_no_allow_run_as_root() {
        let spec = make_spec(2, 4);
        let args = bare_metal_mpirun_args(&spec, "/etc/wren/hostfile");
        assert_eq!(args[0], "mpirun");
        assert!(args.contains(&"8".to_string())); // np = 2*4
        assert!(!args.contains(&"--allow-run-as-root".to_string()));
    }

    #[test]
    fn test_bare_metal_mpirun_args_openmpi_with_fabric() {
        let mut spec = make_spec(2, 4);
        spec.mpi = Some(MPISpec {
            implementation: "openmpi".to_string(),
            ssh_auth: false,
            fabric_interface: Some("hsn0".to_string()),
        });
        let args = bare_metal_mpirun_args(&spec, "/etc/wren/hostfile");
        assert!(args.contains(&"btl_tcp_if_include".to_string()));
        assert!(args.contains(&"hsn0".to_string()));
    }

    #[test]
    fn test_bare_metal_mpirun_args_intel_mpi() {
        let spec = make_intel_spec(4, 2, None);
        let args = bare_metal_mpirun_args(&spec, "/etc/wren/hostfile");
        assert_eq!(args[0], "mpiexec.hydra");
        assert!(args.contains(&"-np".to_string()));
        assert!(args.contains(&"8".to_string())); // 4*2
        assert!(args.contains(&"-machinefile".to_string()));
        assert!(args.contains(&"-ppn".to_string()));
        assert!(args.contains(&"2".to_string()));
    }

    #[test]
    fn test_bare_metal_mpirun_args_intel_mpi_with_fabric() {
        let spec = make_intel_spec(4, 2, Some("hsn0"));
        let args = bare_metal_mpirun_args(&spec, "/etc/wren/hostfile");
        assert!(args.contains(&"-iface".to_string()));
        assert!(args.contains(&"hsn0".to_string()));
    }

    #[test]
    fn test_bare_metal_env_vars_common_fields() {
        let spec = make_spec(2, 4);
        let nodes = make_nodes(&["n0", "n1"]);
        let env = bare_metal_env_vars("my-job", &spec, &nodes, "/etc/wren/hostfile");
        assert_eq!(env["WREN_JOB_NAME"], "my-job");
        assert_eq!(env["WREN_NUM_NODES"], "2");
        assert_eq!(env["WREN_TOTAL_RANKS"], "8");
        assert_eq!(env["WREN_HOSTFILE"], "/etc/wren/hostfile");
    }

    #[test]
    fn test_bare_metal_env_vars_cray_mpich() {
        let spec = make_cray_spec(4, 8, None);
        let nodes = make_nodes(&["n0", "n1", "n2", "n3"]);
        let env = bare_metal_env_vars("sim", &spec, &nodes, "/hostfile");
        assert_eq!(env["MPICH_OFI_STARTUP_CONNECT"], "1");
        assert_eq!(env["MPICH_OFI_NUM_NICS"], "1");
        assert!(!env.contains_key("MPICH_OFI_IFNAME"));
    }

    #[test]
    fn test_bare_metal_env_vars_cray_mpich_with_fabric() {
        let spec = make_cray_spec(4, 8, Some("hsn0"));
        let nodes = make_nodes(&["n0", "n1", "n2", "n3"]);
        let env = bare_metal_env_vars("sim", &spec, &nodes, "/hostfile");
        assert_eq!(env["MPICH_OFI_IFNAME"], "hsn0");
    }

    #[test]
    fn test_bare_metal_env_vars_intel_mpi_with_fabric() {
        let spec = make_intel_spec(2, 4, Some("hsn0"));
        let nodes = make_nodes(&["n0", "n1"]);
        let env = bare_metal_env_vars("sim", &spec, &nodes, "/hostfile");
        assert_eq!(env["I_MPI_FABRICS_LIST"], "hsn0");
        assert!(!env.contains_key("UCX_NET_DEVICES"));
    }

    #[test]
    fn test_bare_metal_env_vars_openmpi_with_fabric() {
        let mut spec = make_spec(2, 4);
        spec.mpi = Some(MPISpec {
            implementation: "openmpi".to_string(),
            ssh_auth: false,
            fabric_interface: Some("hsn0".to_string()),
        });
        let nodes = make_nodes(&["n0", "n1"]);
        let env = bare_metal_env_vars("sim", &spec, &nodes, "/hostfile");
        assert_eq!(env["UCX_NET_DEVICES"], "hsn0");
        assert!(!env.contains_key("I_MPI_FABRICS_LIST"));
    }

    #[test]
    fn test_srun_args_basic() {
        let spec = make_cray_spec(4, 8, None);
        let args = srun_args(&spec, "/etc/wren/hostfile");
        assert_eq!(args[0], "srun");
        assert!(args.contains(&"--nodes".to_string()));
        assert!(args.contains(&"4".to_string()));
        assert!(args.contains(&"--ntasks-per-node".to_string()));
        assert!(args.contains(&"8".to_string()));
        assert!(args.contains(&"--hostfile".to_string()));
        assert!(args.contains(&"/etc/wren/hostfile".to_string()));
        assert!(!args.contains(&"--network".to_string()));
    }

    #[test]
    fn test_srun_args_with_fabric_interface() {
        let spec = make_cray_spec(4, 8, Some("hsn0"));
        let args = srun_args(&spec, "/etc/wren/hostfile");
        assert!(args.contains(&"--network".to_string()));
        assert!(args.contains(&"hsn0".to_string()));
    }
}
