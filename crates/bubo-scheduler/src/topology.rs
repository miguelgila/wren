use bubo_core::{NodeResources, TopologySpec};
use std::collections::HashMap;

/// Weights for topology scoring.
const WEIGHT_SWITCH: f64 = 0.7;
const WEIGHT_RACK: f64 = 0.3;

/// Scores candidate placements based on network proximity.
pub struct TopologyScorer<'a> {
    /// Map from node name to its resource/topology info.
    node_map: HashMap<&'a str, &'a NodeResources>,
}

impl<'a> TopologyScorer<'a> {
    /// Build a scorer from a slice of available nodes.
    pub fn new(nodes: &'a [NodeResources]) -> Self {
        let node_map = nodes.iter().map(|n| (n.name.as_str(), n)).collect();
        Self { node_map }
    }

    /// Score a set of node names in [0.0, 1.0].
    ///
    /// 1.0 means all nodes share the same switch group (best locality).
    /// Score degrades as nodes span more switch groups or racks.
    ///
    /// Formula:
    ///   score = (largest_switch_group / total) * WEIGHT_SWITCH
    ///         + (largest_rack_group   / total) * WEIGHT_RACK
    pub fn score(&self, node_names: &[String], topology_spec: Option<&TopologySpec>) -> f64 {
        let total = node_names.len();
        if total == 0 {
            return 0.0;
        }

        let mut switch_counts: HashMap<String, usize> = HashMap::new();
        let mut rack_counts: HashMap<String, usize> = HashMap::new();
        let mut nodes_with_switch = 0usize;
        let mut nodes_with_rack = 0usize;

        for name in node_names {
            if let Some(node) = self.node_map.get(name.as_str()) {
                // Resolve switch group: prefer explicit switch_group, fall back to
                // topology_key label lookup.
                let switch = node
                    .switch_group
                    .clone()
                    .or_else(|| {
                        topology_spec
                            .and_then(|t| t.topology_key.as_deref())
                            .and_then(|key| node.labels.get(key).cloned())
                    });

                if let Some(sw) = switch {
                    *switch_counts.entry(sw).or_insert(0) += 1;
                    nodes_with_switch += 1;
                }

                if let Some(rack) = &node.rack {
                    *rack_counts.entry(rack.clone()).or_insert(0) += 1;
                    nodes_with_rack += 1;
                }
            }
        }

        // If no topology info is available at all, return 0.0.
        if nodes_with_switch == 0 && nodes_with_rack == 0 {
            return 0.0;
        }

        let switch_score = if nodes_with_switch > 0 {
            let largest = switch_counts.values().copied().max().unwrap_or(0);
            (largest as f64) / (total as f64)
        } else {
            0.0
        };

        let rack_score = if nodes_with_rack > 0 {
            let largest = rack_counts.values().copied().max().unwrap_or(0);
            (largest as f64) / (total as f64)
        } else {
            0.0
        };

        switch_score * WEIGHT_SWITCH + rack_score * WEIGHT_RACK
    }

    /// Estimate network hops between two nodes.
    ///
    /// - Same switch_group → 1 hop
    /// - Same rack, different switch → 2 hops
    /// - Different rack → 3 hops
    fn hops_between(&self, a: &str, b: &str) -> u32 {
        let node_a = self.node_map.get(a);
        let node_b = self.node_map.get(b);

        match (node_a, node_b) {
            (Some(na), Some(nb)) => {
                let same_switch = na.switch_group.is_some()
                    && nb.switch_group.is_some()
                    && na.switch_group == nb.switch_group;

                if same_switch {
                    return 1;
                }

                let same_rack = na.rack.is_some()
                    && nb.rack.is_some()
                    && na.rack == nb.rack;

                if same_rack { 2 } else { 3 }
            }
            // If we have no topology info, assume worst case.
            _ => 3,
        }
    }

    /// Check that all pairs of nodes are within `max_hops` of each other.
    pub fn check_max_hops(&self, node_names: &[String], max_hops: u32) -> bool {
        for i in 0..node_names.len() {
            for j in (i + 1)..node_names.len() {
                if self.hops_between(&node_names[i], &node_names[j]) > max_hops {
                    return false;
                }
            }
        }
        true
    }

    /// Check that all nodes share the same switch_group.
    pub fn check_prefer_same_switch(&self, node_names: &[String]) -> bool {
        if node_names.is_empty() {
            return true;
        }

        // Find the switch group of the first node that has one.
        let reference = node_names
            .iter()
            .find_map(|name| self.node_map.get(name.as_str())?.switch_group.as_deref());

        let Some(ref_switch) = reference else {
            // No node has switch info → constraint cannot be satisfied.
            return false;
        };

        node_names.iter().all(|name| {
            self.node_map
                .get(name.as_str())
                .and_then(|n| n.switch_group.as_deref())
                == Some(ref_switch)
        })
    }

    /// Check whether a candidate placement satisfies all constraints in `spec`.
    pub fn check_constraints(&self, node_names: &[String], spec: &TopologySpec) -> bool {
        if let Some(max_hops) = spec.max_hops {
            if !self.check_max_hops(node_names, max_hops) {
                return false;
            }
        }

        if spec.prefer_same_switch && !self.check_prefer_same_switch(node_names) {
            return false;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn node(name: &str, switch: Option<&str>, rack: Option<&str>) -> NodeResources {
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

    fn node_with_label(name: &str, key: &str, value: &str) -> NodeResources {
        let mut labels = HashMap::new();
        labels.insert(key.to_string(), value.to_string());
        NodeResources {
            name: name.to_string(),
            allocatable_cpu_millis: 8000,
            allocatable_memory_bytes: 16_000_000_000,
            allocatable_gpus: 0,
            labels,
            switch_group: None,
            rack: None,
        }
    }

    fn names(ns: &[&str]) -> Vec<String> {
        ns.iter().map(|s| s.to_string()).collect()
    }

    // ---- score tests ----

    #[test]
    fn test_score_all_same_switch() {
        let nodes = vec![
            node("n0", Some("sw-1"), Some("rack-A")),
            node("n1", Some("sw-1"), Some("rack-A")),
            node("n2", Some("sw-1"), Some("rack-A")),
        ];
        let scorer = TopologyScorer::new(&nodes);
        let score = scorer.score(&names(&["n0", "n1", "n2"]), None);
        // All nodes on sw-1 → switch score = 1.0, rack score = 1.0 → total 1.0
        assert!((score - 1.0).abs() < 1e-9, "expected ~1.0, got {score}");
    }

    #[test]
    fn test_score_mixed_switches() {
        let nodes = vec![
            node("n0", Some("sw-1"), Some("rack-A")),
            node("n1", Some("sw-1"), Some("rack-A")),
            node("n2", Some("sw-2"), Some("rack-B")),
            node("n3", Some("sw-2"), Some("rack-B")),
        ];
        let scorer = TopologyScorer::new(&nodes);
        let score = scorer.score(&names(&["n0", "n1", "n2", "n3"]), None);
        // Largest switch group = 2/4 = 0.5; largest rack group = 2/4 = 0.5
        // score = 0.5 * 0.7 + 0.5 * 0.3 = 0.35 + 0.15 = 0.50
        assert!((score - 0.5).abs() < 1e-9, "expected 0.5, got {score}");
    }

    #[test]
    fn test_score_no_topology_info() {
        let nodes = vec![
            node("n0", None, None),
            node("n1", None, None),
        ];
        let scorer = TopologyScorer::new(&nodes);
        let score = scorer.score(&names(&["n0", "n1"]), None);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn test_score_uses_topology_key_label() {
        // Nodes without switch_group but with a topology label.
        let nodes = vec![
            node_with_label("n0", "topology.kubernetes.io/zone", "zone-a"),
            node_with_label("n1", "topology.kubernetes.io/zone", "zone-a"),
            node_with_label("n2", "topology.kubernetes.io/zone", "zone-b"),
        ];
        let spec = TopologySpec {
            prefer_same_switch: false,
            max_hops: None,
            topology_key: Some("topology.kubernetes.io/zone".to_string()),
        };
        let scorer = TopologyScorer::new(&nodes);
        let score = scorer.score(&names(&["n0", "n1", "n2"]), Some(&spec));
        // Largest group = 2/3 via switch proxy; rack = 0.0
        // score = (2/3) * 0.7 = ~0.467
        assert!(score > 0.4 && score < 0.5, "expected ~0.467, got {score}");
    }

    // ---- hop / constraint tests ----

    #[test]
    fn test_max_hops_same_switch() {
        let nodes = vec![
            node("n0", Some("sw-1"), Some("rack-A")),
            node("n1", Some("sw-1"), Some("rack-A")),
        ];
        let scorer = TopologyScorer::new(&nodes);
        // Same switch = 1 hop → passes for max_hops >= 1
        assert!(scorer.check_max_hops(&names(&["n0", "n1"]), 1));
        assert!(scorer.check_max_hops(&names(&["n0", "n1"]), 2));
    }

    #[test]
    fn test_max_hops_different_rack() {
        let nodes = vec![
            node("n0", Some("sw-1"), Some("rack-A")),
            node("n1", Some("sw-2"), Some("rack-B")),
        ];
        let scorer = TopologyScorer::new(&nodes);
        // Different rack = 3 hops → fails for max_hops < 3
        assert!(!scorer.check_max_hops(&names(&["n0", "n1"]), 1));
        assert!(!scorer.check_max_hops(&names(&["n0", "n1"]), 2));
        assert!(scorer.check_max_hops(&names(&["n0", "n1"]), 3));
    }

    #[test]
    fn test_max_hops_same_rack_different_switch() {
        let nodes = vec![
            node("n0", Some("sw-1"), Some("rack-A")),
            node("n1", Some("sw-2"), Some("rack-A")),
        ];
        let scorer = TopologyScorer::new(&nodes);
        // Same rack, different switch = 2 hops
        assert!(!scorer.check_max_hops(&names(&["n0", "n1"]), 1));
        assert!(scorer.check_max_hops(&names(&["n0", "n1"]), 2));
    }

    #[test]
    fn test_prefer_same_switch_satisfied() {
        let nodes = vec![
            node("n0", Some("sw-1"), None),
            node("n1", Some("sw-1"), None),
            node("n2", Some("sw-1"), None),
        ];
        let scorer = TopologyScorer::new(&nodes);
        assert!(scorer.check_prefer_same_switch(&names(&["n0", "n1", "n2"])));
    }

    #[test]
    fn test_prefer_same_switch_violated() {
        let nodes = vec![
            node("n0", Some("sw-1"), None),
            node("n1", Some("sw-2"), None),
        ];
        let scorer = TopologyScorer::new(&nodes);
        assert!(!scorer.check_prefer_same_switch(&names(&["n0", "n1"])));
    }

    #[test]
    fn test_prefer_same_switch_no_info() {
        let nodes = vec![node("n0", None, None), node("n1", None, None)];
        let scorer = TopologyScorer::new(&nodes);
        // No switch info → constraint cannot be satisfied.
        assert!(!scorer.check_prefer_same_switch(&names(&["n0", "n1"])));
    }

    #[test]
    fn test_prefer_same_switch_empty() {
        let nodes: Vec<NodeResources> = vec![];
        let scorer = TopologyScorer::new(&nodes);
        assert!(scorer.check_prefer_same_switch(&names(&[])));
    }

    #[test]
    fn test_gang_scheduler_with_topology() {
        use crate::gang::GangScheduler;
        use bubo_core::{ClusterState, TopologySpec};

        let mut nodes = Vec::new();
        for i in 0..4 {
            nodes.push(NodeResources {
                name: format!("n{i}"),
                allocatable_cpu_millis: 8000,
                allocatable_memory_bytes: 16_000_000_000,
                allocatable_gpus: 0,
                labels: HashMap::new(),
                switch_group: Some(if i < 2 { "sw-1" } else { "sw-2" }.to_string()),
                rack: Some(if i < 2 { "rack-A" } else { "rack-B" }.to_string()),
            });
        }

        let cluster = ClusterState {
            nodes,
            allocations: HashMap::new(),
        };

        let spec = TopologySpec {
            prefer_same_switch: true,
            max_hops: Some(1),
            topology_key: None,
        };

        // Requesting 2 nodes with prefer_same_switch + max_hops=1 should pick
        // the two nodes on sw-1 (or sw-2), not one from each.
        let placement =
            GangScheduler::schedule(&cluster, "topo-job", 2, 4000, 8_000_000_000, 0, Some(&spec))
                .unwrap();
        assert_eq!(placement.nodes.len(), 2);

        // Both selected nodes must be on the same switch.
        let scorer = TopologyScorer::new(&cluster.nodes);
        assert!(scorer.check_prefer_same_switch(&placement.nodes));
        assert!(placement.score > 0.9, "score should be near 1.0, got {}", placement.score);
    }
}
