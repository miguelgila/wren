use wren_core::{WrenError, ClusterState, NodeResources, Placement};
use tracing::{debug, warn};

/// Resource tracker that wraps `ClusterState` and provides atomic
/// try_allocate / release operations for the scheduler.
pub struct ResourceTracker {
    state: ClusterState,
}

impl ResourceTracker {
    pub fn new(state: ClusterState) -> Self {
        Self { state }
    }

    /// Returns a reference to the underlying cluster state (for the gang scheduler).
    pub fn cluster_state(&self) -> &ClusterState {
        &self.state
    }

    /// Atomically check that all nodes in `placement` have sufficient resources,
    /// then record the allocation for `job_name`.
    ///
    /// If any node cannot satisfy the requirements the entire operation is rolled
    /// back and `WrenError::NoFeasiblePlacement` is returned.
    pub fn try_allocate(
        &mut self,
        job_name: &str,
        placement: &Placement,
        cpu_per_node_millis: u64,
        memory_per_node_bytes: u64,
        gpus_per_node: u32,
    ) -> Result<(), WrenError> {
        // --- check phase (no mutations) ---
        for node_name in &placement.nodes {
            let (avail_cpu, avail_mem, avail_gpu) =
                self.state.available_resources(node_name).ok_or_else(|| {
                    WrenError::NoFeasiblePlacement {
                        job_name: job_name.to_string(),
                        reason: format!("node '{}' not found in cluster state", node_name),
                    }
                })?;

            if avail_cpu < cpu_per_node_millis
                || avail_mem < memory_per_node_bytes
                || avail_gpu < gpus_per_node
            {
                warn!(
                    job = job_name,
                    node = node_name,
                    avail_cpu,
                    avail_mem,
                    avail_gpu,
                    req_cpu = cpu_per_node_millis,
                    req_mem = memory_per_node_bytes,
                    req_gpu = gpus_per_node,
                    "Resource check failed during try_allocate"
                );
                return Err(WrenError::NoFeasiblePlacement {
                    job_name: job_name.to_string(),
                    reason: format!(
                        "node '{}' has insufficient resources \
                         (cpu: {}/{}, mem: {}/{}, gpu: {}/{})",
                        node_name,
                        avail_cpu,
                        cpu_per_node_millis,
                        avail_mem,
                        memory_per_node_bytes,
                        avail_gpu,
                        gpus_per_node,
                    ),
                });
            }
        }

        // --- commit phase ---
        for node_name in &placement.nodes {
            self.state.allocate(
                node_name,
                cpu_per_node_millis,
                memory_per_node_bytes,
                gpus_per_node,
                job_name,
            );
        }

        debug!(
            job = job_name,
            nodes = ?placement.nodes,
            "Allocated resources for job"
        );
        Ok(())
    }

    /// Release a job's allocation from all nodes in `placement`.
    pub fn release(
        &mut self,
        job_name: &str,
        placement: &Placement,
        cpu_per_node_millis: u64,
        memory_per_node_bytes: u64,
        gpus_per_node: u32,
    ) {
        for node_name in &placement.nodes {
            self.state.deallocate(
                node_name,
                cpu_per_node_millis,
                memory_per_node_bytes,
                gpus_per_node,
                job_name,
            );
        }
        debug!(
            job = job_name,
            nodes = ?placement.nodes,
            "Released resources for job"
        );
    }

    /// Total allocatable CPU millis across all nodes.
    pub fn total_cpu_millis(&self) -> u64 {
        self.state.nodes.iter().map(|n| n.allocatable_cpu_millis).sum()
    }

    /// Total allocatable memory bytes across all nodes.
    pub fn total_memory_bytes(&self) -> u64 {
        self.state.nodes.iter().map(|n| n.allocatable_memory_bytes).sum()
    }

    /// Total allocatable GPUs across all nodes.
    pub fn total_gpus(&self) -> u32 {
        self.state.nodes.iter().map(|n| n.allocatable_gpus).sum()
    }

    /// Used CPU millis across all nodes.
    pub fn used_cpu_millis(&self) -> u64 {
        self.state.allocations.values().map(|a| a.used_cpu_millis).sum()
    }

    /// Used memory bytes across all nodes.
    pub fn used_memory_bytes(&self) -> u64 {
        self.state.allocations.values().map(|a| a.used_memory_bytes).sum()
    }

    /// Used GPUs across all nodes.
    pub fn used_gpus(&self) -> u32 {
        self.state.allocations.values().map(|a| a.used_gpus).sum()
    }

    /// CPU utilization in [0.0, 1.0]. Returns 0.0 for an empty cluster.
    pub fn cpu_utilization(&self) -> f64 {
        let total = self.total_cpu_millis();
        if total == 0 {
            return 0.0;
        }
        self.used_cpu_millis() as f64 / total as f64
    }

    /// Memory utilization in [0.0, 1.0]. Returns 0.0 for an empty cluster.
    pub fn memory_utilization(&self) -> f64 {
        let total = self.total_memory_bytes();
        if total == 0 {
            return 0.0;
        }
        self.used_memory_bytes() as f64 / total as f64
    }

    /// Add a node to the tracked cluster (e.g. when the node watcher discovers a new node).
    pub fn add_node(&mut self, node: NodeResources) {
        self.state.nodes.push(node);
    }

    /// Remove a node from the tracked cluster by name.
    pub fn remove_node(&mut self, node_name: &str) {
        self.state.nodes.retain(|n| n.name != node_name);
        self.state.allocations.remove(node_name);
    }

    /// Number of nodes in the cluster.
    pub fn node_count(&self) -> usize {
        self.state.nodes.len()
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

    fn make_placement(nodes: &[&str]) -> Placement {
        Placement {
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            score: 1.0,
        }
    }

    fn two_node_tracker() -> ResourceTracker {
        let mut state = ClusterState::new();
        state.nodes.push(make_node("node-0", 8000, 16_000_000_000, 4));
        state.nodes.push(make_node("node-1", 8000, 16_000_000_000, 4));
        ResourceTracker::new(state)
    }

    #[test]
    fn test_try_allocate_success() {
        let mut tracker = two_node_tracker();
        let placement = make_placement(&["node-0", "node-1"]);

        tracker
            .try_allocate("job-1", &placement, 4000, 8_000_000_000, 2)
            .unwrap();

        assert_eq!(tracker.used_cpu_millis(), 8000);
        assert_eq!(tracker.used_memory_bytes(), 16_000_000_000);
        assert_eq!(tracker.used_gpus(), 4);
    }

    #[test]
    fn test_try_allocate_insufficient_cpu() {
        let mut tracker = two_node_tracker();
        let placement = make_placement(&["node-0"]);

        // Require more CPU than available
        let result = tracker.try_allocate("job-1", &placement, 16000, 8_000_000_000, 0);
        assert!(result.is_err());
        // No allocation should have occurred
        assert_eq!(tracker.used_cpu_millis(), 0);
    }

    #[test]
    fn test_try_allocate_insufficient_memory() {
        let mut tracker = two_node_tracker();
        let placement = make_placement(&["node-0"]);

        let result = tracker.try_allocate("job-1", &placement, 4000, 32_000_000_000, 0);
        assert!(result.is_err());
        assert_eq!(tracker.used_memory_bytes(), 0);
    }

    #[test]
    fn test_try_allocate_insufficient_gpus() {
        let mut tracker = two_node_tracker();
        let placement = make_placement(&["node-0"]);

        let result = tracker.try_allocate("job-1", &placement, 4000, 8_000_000_000, 8);
        assert!(result.is_err());
        assert_eq!(tracker.used_gpus(), 0);
    }

    #[test]
    fn test_try_allocate_unknown_node() {
        let mut tracker = two_node_tracker();
        let placement = make_placement(&["node-99"]);

        let result = tracker.try_allocate("job-1", &placement, 4000, 8_000_000_000, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_release_restores_resources() {
        let mut tracker = two_node_tracker();
        let placement = make_placement(&["node-0", "node-1"]);

        tracker
            .try_allocate("job-1", &placement, 4000, 8_000_000_000, 2)
            .unwrap();
        tracker.release("job-1", &placement, 4000, 8_000_000_000, 2);

        assert_eq!(tracker.used_cpu_millis(), 0);
        assert_eq!(tracker.used_memory_bytes(), 0);
        assert_eq!(tracker.used_gpus(), 0);
    }

    #[test]
    fn test_multiple_jobs_accumulate() {
        let mut tracker = two_node_tracker();

        let p0 = make_placement(&["node-0"]);
        let p1 = make_placement(&["node-1"]);

        tracker.try_allocate("job-1", &p0, 4000, 8_000_000_000, 0).unwrap();
        tracker.try_allocate("job-2", &p1, 2000, 4_000_000_000, 0).unwrap();

        assert_eq!(tracker.used_cpu_millis(), 6000);
        assert_eq!(tracker.used_memory_bytes(), 12_000_000_000);
    }

    #[test]
    fn test_atomicity_partial_failure() {
        // If the second node of a two-node placement fails the check,
        // no allocation should be recorded on the first node either.
        let mut state = ClusterState::new();
        state.nodes.push(make_node("node-0", 8000, 16_000_000_000, 0));
        state.nodes.push(make_node("node-1", 2000, 16_000_000_000, 0)); // weak CPU
        let mut tracker = ResourceTracker::new(state);

        let placement = make_placement(&["node-0", "node-1"]);
        let result = tracker.try_allocate("job-1", &placement, 4000, 8_000_000_000, 0);
        assert!(result.is_err());
        // node-0 must not have been mutated
        assert_eq!(tracker.used_cpu_millis(), 0);
    }

    #[test]
    fn test_utilization_metrics() {
        let mut tracker = two_node_tracker();
        // Total: 16000m CPU, 32 GB, 8 GPUs across two nodes
        assert_eq!(tracker.total_cpu_millis(), 16000);
        assert_eq!(tracker.total_memory_bytes(), 32_000_000_000);
        assert_eq!(tracker.total_gpus(), 8);
        assert_eq!(tracker.cpu_utilization(), 0.0);

        let placement = make_placement(&["node-0"]);
        tracker.try_allocate("job-1", &placement, 8000, 16_000_000_000, 0).unwrap();

        // 8000 / 16000 = 0.5
        assert!((tracker.cpu_utilization() - 0.5).abs() < 1e-9);
        assert!((tracker.memory_utilization() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_add_remove_node() {
        let mut tracker = two_node_tracker();
        assert_eq!(tracker.node_count(), 2);

        tracker.add_node(make_node("node-2", 8000, 16_000_000_000, 0));
        assert_eq!(tracker.node_count(), 3);
        assert_eq!(tracker.total_cpu_millis(), 24000);

        tracker.remove_node("node-2");
        assert_eq!(tracker.node_count(), 2);
        assert_eq!(tracker.total_cpu_millis(), 16000);
    }

    #[test]
    fn test_empty_cluster_utilization() {
        let tracker = ResourceTracker::new(ClusterState::new());
        assert_eq!(tracker.cpu_utilization(), 0.0);
        assert_eq!(tracker.memory_utilization(), 0.0);
    }
}
