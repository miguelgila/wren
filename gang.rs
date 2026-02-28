use bubo_core::{ClusterState, NodeResources, Placement, BuboError};
use tracing::debug;

/// Gang scheduler: finds a set of N nodes that can satisfy the job's requirements.
/// All-or-nothing — either all nodes are available or the job is not scheduled.
pub struct GangScheduler;

impl GangScheduler {
    /// Attempt to find a placement for a job requiring `node_count` nodes,
    /// each needing the specified resources.
    pub fn schedule(
        cluster: &ClusterState,
        node_count: u32,
        cpu_per_node_millis: u64,
        memory_per_node_bytes: u64,
        gpus_per_node: u32,
    ) -> Result<Placement, BuboError> {
        let mut feasible: Vec<&NodeResources> = cluster
            .nodes
            .iter()
            .filter(|node| {
                let alloc = cluster
                    .allocations
                    .get(&node.name)
                    .cloned()
                    .unwrap_or_default();

                let avail_cpu = node.allocatable_cpu_millis.saturating_sub(alloc.used_cpu_millis);
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
            feasible_count = feasible.len(),
            required = node_count,
            "Gang scheduler: found feasible nodes"
        );

        if (feasible.len() as u32) < node_count {
            return Err(BuboError::NoFeasiblePlacement {
                job_name: String::new(), // Caller fills this in
                reason: format!(
                    "need {} nodes, only {} feasible",
                    node_count,
                    feasible.len()
                ),
            });
        }

        // For v0.1: simple greedy selection — take the first N feasible nodes.
        // Phase 2 will add topology-aware scoring here.
        feasible.truncate(node_count as usize);

        Ok(Placement {
            nodes: feasible.iter().map(|n| n.name.clone()).collect(),
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

    #[test]
    fn test_basic_gang_schedule() {
        let cluster = ClusterState {
            nodes: vec![
                make_node("node-0", 8000, 16_000_000_000, 2),
                make_node("node-1", 8000, 16_000_000_000, 2),
                make_node("node-2", 8000, 16_000_000_000, 2),
                make_node("node-3", 8000, 16_000_000_000, 2),
            ],
            allocations: HashMap::new(),
        };

        let placement = GangScheduler::schedule(&cluster, 3, 4000, 8_000_000_000, 1).unwrap();
        assert_eq!(placement.nodes.len(), 3);
    }

    #[test]
    fn test_insufficient_nodes() {
        let cluster = ClusterState {
            nodes: vec![
                make_node("node-0", 8000, 16_000_000_000, 2),
                make_node("node-1", 8000, 16_000_000_000, 2),
            ],
            allocations: HashMap::new(),
        };

        let result = GangScheduler::schedule(&cluster, 4, 4000, 8_000_000_000, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_respects_existing_allocations() {
        let mut allocations = HashMap::new();
        allocations.insert(
            "node-0".to_string(),
            bubo_core::NodeAllocation {
                used_cpu_millis: 7000,
                used_memory_bytes: 15_000_000_000,
                used_gpus: 0,
                jobs: vec!["other-job".to_string()],
            },
        );

        let cluster = ClusterState {
            nodes: vec![
                make_node("node-0", 8000, 16_000_000_000, 0),
                make_node("node-1", 8000, 16_000_000_000, 0),
                make_node("node-2", 8000, 16_000_000_000, 0),
            ],
            allocations,
        };

        // node-0 only has 1000m CPU free, so it shouldn't be selected
        let placement = GangScheduler::schedule(&cluster, 2, 4000, 8_000_000_000, 0).unwrap();
        assert_eq!(placement.nodes.len(), 2);
        assert!(!placement.nodes.contains(&"node-0".to_string()));
    }
}
