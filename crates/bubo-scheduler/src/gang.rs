use bubo_core::{BuboError, ClusterState, NodeResources, Placement, TopologySpec};
use tracing::debug;

use crate::topology::TopologyScorer;

/// Maximum number of candidate placements to evaluate when topology is enabled.
const MAX_CANDIDATES: usize = 10;

/// Gang scheduler: finds a set of N nodes that can satisfy the job's requirements.
/// All-or-nothing — either all nodes are available or the job is not scheduled.
pub struct GangScheduler;

impl GangScheduler {
    /// Attempt to find a placement for a job requiring `node_count` nodes,
    /// each needing the specified resources.
    ///
    /// When `topology` is `Some`, up to `MAX_CANDIDATES` feasible combinations
    /// are scored and the best constraint-satisfying placement is returned.
    /// When `topology` is `None`, the original greedy first-N selection is used.
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
        topology: Option<&TopologySpec>,
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

        let n = node_count as usize;

        let Some(topo_spec) = topology else {
            // No topology: greedy first-N selection.
            let selected: Vec<String> =
                feasible.iter().take(n).map(|node| node.name.clone()).collect();
            return Ok(Placement { nodes: selected, score: 1.0 });
        };

        // Topology-aware path: generate candidates and score them.
        let scorer = TopologyScorer::new(&cluster.nodes);
        let candidates = Self::generate_candidates(&feasible, n, MAX_CANDIDATES);

        debug!(
            job = job_name,
            candidate_count = candidates.len(),
            "Gang scheduler: evaluating topology candidates"
        );

        // Filter by constraints, then pick highest score.
        let best = candidates
            .into_iter()
            .filter(|candidate| scorer.check_constraints(candidate, topo_spec))
            .map(|candidate| {
                let score = scorer.score(&candidate, Some(topo_spec));
                (candidate, score)
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        match best {
            Some((nodes, score)) => {
                debug!(job = job_name, score, "Gang scheduler: topology-aware placement found");
                Ok(Placement { nodes, score })
            }
            None => Err(BuboError::NoFeasiblePlacement {
                job_name: job_name.to_string(),
                reason: "no feasible placement satisfies topology constraints".to_string(),
            }),
        }
    }

    /// Generate up to `max_candidates` combinations of `n` nodes from `feasible`.
    ///
    /// Uses a simple sliding-window strategy: it iterates through all contiguous
    /// windows of size `n` first (good locality for sorted/grouped node lists),
    /// then samples additional combinations spaced evenly through the list, up to
    /// `max_candidates` total.
    fn generate_candidates(
        feasible: &[&NodeResources],
        n: usize,
        max_candidates: usize,
    ) -> Vec<Vec<String>> {
        if feasible.len() < n || n == 0 {
            return if n == 0 {
                vec![vec![]]
            } else {
                vec![]
            };
        }

        let mut candidates: Vec<Vec<String>> = Vec::new();
        let total = feasible.len();

        // Strategy 1: contiguous windows (captures co-located node groups).
        for start in 0..=(total - n) {
            if candidates.len() >= max_candidates {
                break;
            }
            let window: Vec<String> =
                feasible[start..start + n].iter().map(|node| node.name.clone()).collect();
            candidates.push(window);
        }

        // Strategy 2: strided combinations to cover non-contiguous diversity.
        // Simple approach: pick every k-th node to form a spread-out group.
        if candidates.len() < max_candidates && total > n {
            let stride = (total / n).max(1);
            for offset in 1..stride {
                if candidates.len() >= max_candidates {
                    break;
                }
                let strided: Vec<String> = (0..n)
                    .map(|i| feasible[(offset + i * stride) % total].name.clone())
                    .collect();
                // Deduplicate within the candidate.
                let mut seen = std::collections::HashSet::new();
                if strided.iter().all(|name| seen.insert(name.clone())) {
                    candidates.push(strided);
                }
            }
        }

        candidates
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

    fn make_node_topo(
        name: &str,
        switch: Option<&str>,
        rack: Option<&str>,
    ) -> NodeResources {
        NodeResources {
            name: name.to_string(),
            allocatable_cpu_millis: 8000,
            allocatable_memory_bytes: 16_000_000_000,
            allocatable_gpus: 0,
            labels: HashMap::new(),
            switch_group: switch.map(str::to_string),
            rack: rack.map(str::to_string),
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
            GangScheduler::schedule(&cluster, "test-job", 3, 4000, 8_000_000_000, 1, None)
                .unwrap();
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
            GangScheduler::schedule(&cluster, "test-job", 2, 4000, 8_000_000_000, 0, None)
                .unwrap();
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

        let result =
            GangScheduler::schedule(&cluster, "test-job", 4, 4000, 8_000_000_000, 0, None);
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
            make_node("node-0", 2000, 16_000_000_000, 0),
            make_node("node-1", 2000, 16_000_000_000, 0),
        ]);

        let result =
            GangScheduler::schedule(&cluster, "test-job", 1, 4000, 8_000_000_000, 0, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_insufficient_memory() {
        let cluster = make_cluster(vec![
            make_node("node-0", 8000, 4_000_000_000, 0),
            make_node("node-1", 8000, 4_000_000_000, 0),
        ]);

        let result =
            GangScheduler::schedule(&cluster, "test-job", 1, 4000, 8_000_000_000, 0, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_insufficient_gpus() {
        let cluster = make_cluster(vec![
            make_node("node-0", 8000, 16_000_000_000, 1),
            make_node("node-1", 8000, 16_000_000_000, 1),
        ]);

        let result =
            GangScheduler::schedule(&cluster, "test-job", 1, 4000, 8_000_000_000, 2, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_respects_existing_allocations() {
        let mut cluster = make_cluster(vec![
            make_node("node-0", 8000, 16_000_000_000, 0),
            make_node("node-1", 8000, 16_000_000_000, 0),
            make_node("node-2", 8000, 16_000_000_000, 0),
        ]);

        cluster.allocate("node-0", 7000, 15_000_000_000, 0, "other-job");

        let placement =
            GangScheduler::schedule(&cluster, "test-job", 2, 4000, 8_000_000_000, 0, None)
                .unwrap();
        assert_eq!(placement.nodes.len(), 2);
        assert!(!placement.nodes.contains(&"node-0".to_string()));
        assert!(placement.nodes.contains(&"node-1".to_string()));
        assert!(placement.nodes.contains(&"node-2".to_string()));
    }

    #[test]
    fn test_empty_cluster() {
        let cluster = make_cluster(vec![]);

        let result =
            GangScheduler::schedule(&cluster, "test-job", 1, 4000, 8_000_000_000, 0, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_zero_node_count() {
        let cluster = make_cluster(vec![make_node("node-0", 8000, 16_000_000_000, 0)]);

        let placement =
            GangScheduler::schedule(&cluster, "test-job", 0, 4000, 8_000_000_000, 0, None)
                .unwrap();
        assert_eq!(placement.nodes.len(), 0);
    }

    #[test]
    fn test_no_gpu_requirement_ignores_gpu_capacity() {
        let cluster = make_cluster(vec![
            make_node("node-0", 8000, 16_000_000_000, 0),
            make_node("node-1", 8000, 16_000_000_000, 0),
        ]);

        let placement =
            GangScheduler::schedule(&cluster, "test-job", 2, 4000, 8_000_000_000, 0, None)
                .unwrap();
        assert_eq!(placement.nodes.len(), 2);
    }

    // ---- topology-aware tests ----

    #[test]
    fn test_topology_aware_scheduling() {
        // 4 nodes: n0+n1 on sw-1/rack-A, n2+n3 on sw-2/rack-B.
        // Requesting 2 nodes without topology → greedy picks n0,n1.
        // With topology (prefer_same_switch) → should still return 2 co-located nodes.
        let cluster = make_cluster(vec![
            make_node_topo("n0", Some("sw-1"), Some("rack-A")),
            make_node_topo("n1", Some("sw-1"), Some("rack-A")),
            make_node_topo("n2", Some("sw-2"), Some("rack-B")),
            make_node_topo("n3", Some("sw-2"), Some("rack-B")),
        ]);

        let spec = TopologySpec {
            prefer_same_switch: true,
            max_hops: None,
            topology_key: None,
        };

        let placement =
            GangScheduler::schedule(&cluster, "topo-job", 2, 4000, 8_000_000_000, 0, Some(&spec))
                .unwrap();
        assert_eq!(placement.nodes.len(), 2);

        let scorer = crate::topology::TopologyScorer::new(&cluster.nodes);
        assert!(
            scorer.check_prefer_same_switch(&placement.nodes),
            "nodes should share the same switch: {:?}",
            placement.nodes
        );
        // Score should be near 1.0 for perfectly co-located nodes.
        assert!(placement.score > 0.9, "expected high score, got {}", placement.score);
    }

    #[test]
    fn test_topology_constraint_max_hops() {
        // n0 and n1 are on different racks (3 hops apart).
        // max_hops = 1 should filter this out entirely → NoFeasiblePlacement.
        let cluster = make_cluster(vec![
            make_node_topo("n0", Some("sw-1"), Some("rack-A")),
            make_node_topo("n1", Some("sw-2"), Some("rack-B")),
        ]);

        let spec = TopologySpec {
            prefer_same_switch: false,
            max_hops: Some(1),
            topology_key: None,
        };

        let result =
            GangScheduler::schedule(&cluster, "hop-job", 2, 4000, 8_000_000_000, 0, Some(&spec));
        assert!(
            result.is_err(),
            "expected NoFeasiblePlacement when max_hops=1 and nodes are 3 hops apart"
        );
    }

    #[test]
    fn test_topology_max_hops_satisfied() {
        // n0 and n1 on same switch (1 hop) — should succeed with max_hops=1.
        let cluster = make_cluster(vec![
            make_node_topo("n0", Some("sw-1"), Some("rack-A")),
            make_node_topo("n1", Some("sw-1"), Some("rack-A")),
            make_node_topo("n2", Some("sw-2"), Some("rack-B")),
        ]);

        let spec = TopologySpec {
            prefer_same_switch: false,
            max_hops: Some(1),
            topology_key: None,
        };

        let placement =
            GangScheduler::schedule(&cluster, "hop-job", 2, 4000, 8_000_000_000, 0, Some(&spec))
                .unwrap();
        assert_eq!(placement.nodes.len(), 2);
        // Must not contain n2 (it would be 3 hops from sw-1 nodes).
        assert!(!placement.nodes.contains(&"n2".to_string()));
    }
}
