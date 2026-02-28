use bubo_core::{BuboError, ClusterState, NodeResources, Placement};
use tracing::debug;

/// Gang scheduler: finds a set of N nodes that can satisfy the job's requirements.
/// All-or-nothing — either all nodes are available or the job is not scheduled.
pub struct GangScheduler;

impl GangScheduler {
    /// Attempt to find a placement for a job requiring `node_count` nodes,
    /// each needing the specified resources.
    ///
    /// Returns a `Placement` with the selected node names on success, or
    /// `BuboError::NoFeasiblePlacement` if insufficient nodes are available.
    pub fn schedule(
        cluster: &ClusterState,
        job_name: &str,
        node_count: u32,
        cpu_per_node_millis: u64,
        memory_per_node_bytes: u64,
        gpus_per_node: u32,
    ) -> Result<Placement, BuboError> {
        let feasible: Vec<&NodeResources> = cluster
            .nodes
            .iter()
            .filter(|node| {
                let alloc = cluster
                    .allocations
                    .get(&node.name)
                    .cloned()
                    .unwrap_or_default();

                let avail_cpu =
                    node.allocatable_cpu_millis.saturating_sub(alloc.used_cpu_millis);
                let avail_mem = node
                    .allocatable_memory_bytes
                    .saturating_sub(alloc.used_memory_bytes);
                let avail_gpu = node.allocatable_gpus.saturating_sub(alloc.used_gpus);

                avail_cpu >= cpu_per_node_millis
                    && avail_mem >= memory_per_node_bytes
                    && avail_gpu >= gpus_per_node
            })
            .collect();

        debug!(
            job = job_name,
            feasible_count = feasible.len(),
            required = node_count,
            "Gang scheduler: found feasible nodes"
        );

        if (feasible.len() as u32) < node_count {
            return Err(BuboError::NoFeasiblePlacement {
                job_name: job_name.to_string(),
                reason: format!(
                    "need {} nodes, only {} feasible",
                    node_count,
                    feasible.len()
                ),
            });
        }

        // For v0.1: simple greedy selection — take the first N feasible nodes.
        // Phase 2 will add topology-aware scoring here.
        let selected: Vec<String> = feasible
            .iter()
            .take(node_count as usize)
            .map(|n| n.name.clone())
            .collect();

        Ok(Placement {
            nodes: selected,
            score: 1.0, // No scoring yet
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_node(name: &str, cpu: u64, mem: u64, gpus: u32) -> NodeResources {
        NodeResources {
            name: name.to_string(),
            allocatable_cpu_millis: cpu,
            allocatable_memory_bytes: mem,
            allocatable_gpus: gpus,
            labels: HashMap::new(),
            switch_group: None,
            rack: None,
        }
    }

    fn make_cluster(nodes: Vec<NodeResources>) -> ClusterState {
        ClusterState {
            nodes,
            allocations: HashMap::new(),
        }
    }

    #[test]
    fn test_basic_gang_schedule() {
        let cluster = make_cluster(vec![
            make_node("node-0", 8000, 16_000_000_000, 2),
            make_node("node-1", 8000, 16_000_000_000, 2),
            make_node("node-2", 8000, 16_000_000_000, 2),
            make_node("node-3", 8000, 16_000_000_000, 2),
        ]);

        let placement =
            GangScheduler::schedule(&cluster, "test-job", 3, 4000, 8_000_000_000, 1).unwrap();
        assert_eq!(placement.nodes.len(), 3);
        assert_eq!(placement.score, 1.0);
    }

    #[test]
    fn test_exact_node_count() {
        let cluster = make_cluster(vec![
            make_node("node-0", 8000, 16_000_000_000, 0),
            make_node("node-1", 8000, 16_000_000_000, 0),
        ]);

        let placement =
            GangScheduler::schedule(&cluster, "test-job", 2, 4000, 8_000_000_000, 0).unwrap();
        assert_eq!(placement.nodes.len(), 2);
        assert!(placement.nodes.contains(&"node-0".to_string()));
        assert!(placement.nodes.contains(&"node-1".to_string()));
    }

    #[test]
    fn test_insufficient_nodes() {
        let cluster = make_cluster(vec![
            make_node("node-0", 8000, 16_000_000_000, 2),
            make_node("node-1", 8000, 16_000_000_000, 2),
        ]);

        let result = GangScheduler::schedule(&cluster, "test-job", 4, 4000, 8_000_000_000, 0);
        assert!(result.is_err());
        match result.unwrap_err() {
            BuboError::NoFeasiblePlacement { job_name, reason } => {
                assert_eq!(job_name, "test-job");
                assert!(reason.contains("need 4"));
                assert!(reason.contains("only 2"));
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn test_insufficient_cpu() {
        let cluster = make_cluster(vec![
            make_node("node-0", 2000, 16_000_000_000, 0), // only 2 CPU cores
            make_node("node-1", 2000, 16_000_000_000, 0),
        ]);

        // Require 4000m per node — neither node qualifies
        let result = GangScheduler::schedule(&cluster, "test-job", 1, 4000, 8_000_000_000, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_insufficient_memory() {
        let cluster = make_cluster(vec![
            make_node("node-0", 8000, 4_000_000_000, 0), // only 4 GB
            make_node("node-1", 8000, 4_000_000_000, 0),
        ]);

        let result = GangScheduler::schedule(&cluster, "test-job", 1, 4000, 8_000_000_000, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_insufficient_gpus() {
        let cluster = make_cluster(vec![
            make_node("node-0", 8000, 16_000_000_000, 1), // only 1 GPU
            make_node("node-1", 8000, 16_000_000_000, 1),
        ]);

        // Require 2 GPUs per node
        let result = GangScheduler::schedule(&cluster, "test-job", 1, 4000, 8_000_000_000, 2);
        assert!(result.is_err());
    }

    #[test]
    fn test_respects_existing_allocations() {
        let mut cluster = make_cluster(vec![
            make_node("node-0", 8000, 16_000_000_000, 0),
            make_node("node-1", 8000, 16_000_000_000, 0),
            make_node("node-2", 8000, 16_000_000_000, 0),
        ]);

        // node-0 is mostly used up
        cluster.allocate("node-0", 7000, 15_000_000_000, 0, "other-job");

        // node-0 only has 1000m CPU and 1 GB free — shouldn't be selected for 4000m requirement
        let placement =
            GangScheduler::schedule(&cluster, "test-job", 2, 4000, 8_000_000_000, 0).unwrap();
        assert_eq!(placement.nodes.len(), 2);
        assert!(!placement.nodes.contains(&"node-0".to_string()));
        assert!(placement.nodes.contains(&"node-1".to_string()));
        assert!(placement.nodes.contains(&"node-2".to_string()));
    }

    #[test]
    fn test_empty_cluster() {
        let cluster = make_cluster(vec![]);

        let result = GangScheduler::schedule(&cluster, "test-job", 1, 4000, 8_000_000_000, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_zero_node_count() {
        let cluster = make_cluster(vec![make_node("node-0", 8000, 16_000_000_000, 0)]);

        // Requesting 0 nodes should succeed with an empty placement
        let placement =
            GangScheduler::schedule(&cluster, "test-job", 0, 4000, 8_000_000_000, 0).unwrap();
        assert_eq!(placement.nodes.len(), 0);
    }

    #[test]
    fn test_no_gpu_requirement_ignores_gpu_capacity() {
        let cluster = make_cluster(vec![
            make_node("node-0", 8000, 16_000_000_000, 0), // no GPUs
            make_node("node-1", 8000, 16_000_000_000, 0),
        ]);

        // gpus_per_node = 0 means no GPU required — should succeed
        let placement =
            GangScheduler::schedule(&cluster, "test-job", 2, 4000, 8_000_000_000, 0).unwrap();
        assert_eq!(placement.nodes.len(), 2);
    }
}
