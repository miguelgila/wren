use bubo_core::{ClusterState, NodeResources};
use k8s_openapi::api::core::v1::Node;
use kube::{Api, Client};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

/// Watches Kubernetes nodes and maintains an up-to-date ClusterState.
pub struct NodeWatcher {
    client: Client,
    state: Arc<RwLock<ClusterState>>,
}

impl NodeWatcher {
    pub fn new(client: Client, state: Arc<RwLock<ClusterState>>) -> Self {
        Self { client, state }
    }

    /// Performs a one-shot sync of all nodes into the cluster state.
    pub async fn sync_nodes(&self) -> Result<(), kube::Error> {
        let nodes_api: Api<Node> = Api::all(self.client.clone());
        let node_list = nodes_api.list(&Default::default()).await?;

        let mut resources = Vec::new();
        for node in &node_list.items {
            if let Some(nr) = Self::parse_node(node) {
                resources.push(nr);
            }
        }

        info!(count = resources.len(), "synced nodes");

        let mut state = self.state.write().await;
        state.nodes = resources;
        Ok(())
    }

    /// Parse a Kubernetes Node object into our internal NodeResources.
    fn parse_node(node: &Node) -> Option<NodeResources> {
        let name = node.metadata.name.as_deref()?;
        let status = node.status.as_ref()?;
        let allocatable = status.allocatable.as_ref()?;

        let cpu_millis = parse_cpu(allocatable.get("cpu")?);
        let memory_bytes = parse_memory(allocatable.get("memory")?);
        let gpus = allocatable
            .get("nvidia.com/gpu")
            .map(|q| parse_integer(q))
            .unwrap_or(0);

        let btree_labels = node
            .metadata
            .labels
            .clone()
            .unwrap_or_default();

        let switch_group = btree_labels.get("network.bubo.io/switch-group").cloned();
        let rack = btree_labels.get("topology.kubernetes.io/zone").cloned();

        let labels: std::collections::HashMap<String, String> = btree_labels.into_iter().collect();

        // Skip unschedulable nodes
        if let Some(spec) = &node.spec {
            if spec.unschedulable == Some(true) {
                debug!(node = name, "skipping unschedulable node");
                return None;
            }
        }

        // Skip nodes with NoSchedule taints
        if let Some(spec) = &node.spec {
            if let Some(taints) = &spec.taints {
                for taint in taints {
                    if taint.effect.as_str() == "NoSchedule" {
                        debug!(node = name, taint_key = %taint.key, "skipping tainted node");
                        return None;
                    }
                }
            }
        }

        Some(NodeResources {
            name: name.to_string(),
            allocatable_cpu_millis: cpu_millis,
            allocatable_memory_bytes: memory_bytes,
            allocatable_gpus: gpus,
            labels,
            switch_group,
            rack,
        })
    }
}

/// Parse a Kubernetes CPU quantity (e.g., "4", "4000m") into millicores.
fn parse_cpu(quantity: &k8s_openapi::apimachinery::pkg::api::resource::Quantity) -> u64 {
    let s = &quantity.0;
    if let Some(stripped) = s.strip_suffix('m') {
        stripped.parse::<u64>().unwrap_or(0)
    } else {
        s.parse::<u64>().unwrap_or(0) * 1000
    }
}

/// Parse a Kubernetes memory quantity (e.g., "16Gi", "16384Mi", "17179869184") into bytes.
fn parse_memory(quantity: &k8s_openapi::apimachinery::pkg::api::resource::Quantity) -> u64 {
    let s = &quantity.0;
    if let Some(stripped) = s.strip_suffix("Ki") {
        stripped.parse::<u64>().unwrap_or(0) * 1024
    } else if let Some(stripped) = s.strip_suffix("Mi") {
        stripped.parse::<u64>().unwrap_or(0) * 1024 * 1024
    } else if let Some(stripped) = s.strip_suffix("Gi") {
        stripped.parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024
    } else if let Some(stripped) = s.strip_suffix("Ti") {
        stripped.parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024 * 1024
    } else if let Some(stripped) = s.strip_suffix('K') {
        stripped.parse::<u64>().unwrap_or(0) * 1000
    } else if let Some(stripped) = s.strip_suffix('M') {
        stripped.parse::<u64>().unwrap_or(0) * 1_000_000
    } else if let Some(stripped) = s.strip_suffix('G') {
        stripped.parse::<u64>().unwrap_or(0) * 1_000_000_000
    } else if let Some(stripped) = s.strip_suffix('T') {
        stripped.parse::<u64>().unwrap_or(0) * 1_000_000_000_000
    } else {
        s.parse::<u64>().unwrap_or(0)
    }
}

/// Parse an integer quantity (e.g., GPU count).
fn parse_integer(quantity: &k8s_openapi::apimachinery::pkg::api::resource::Quantity) -> u32 {
    quantity.0.parse::<u32>().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

    #[test]
    fn test_parse_cpu_cores() {
        assert_eq!(parse_cpu(&Quantity("4".to_string())), 4000);
        assert_eq!(parse_cpu(&Quantity("16".to_string())), 16000);
    }

    #[test]
    fn test_parse_cpu_millis() {
        assert_eq!(parse_cpu(&Quantity("4000m".to_string())), 4000);
        assert_eq!(parse_cpu(&Quantity("500m".to_string())), 500);
    }

    #[test]
    fn test_parse_memory_gi() {
        assert_eq!(parse_memory(&Quantity("16Gi".to_string())), 17_179_869_184);
    }

    #[test]
    fn test_parse_memory_mi() {
        assert_eq!(parse_memory(&Quantity("512Mi".to_string())), 536_870_912);
    }

    #[test]
    fn test_parse_memory_bytes() {
        assert_eq!(
            parse_memory(&Quantity("17179869184".to_string())),
            17_179_869_184
        );
    }

    #[test]
    fn test_parse_memory_decimal_suffixes() {
        assert_eq!(parse_memory(&Quantity("1G".to_string())), 1_000_000_000);
        assert_eq!(parse_memory(&Quantity("1M".to_string())), 1_000_000);
        assert_eq!(parse_memory(&Quantity("1K".to_string())), 1_000);
    }

    #[test]
    fn test_parse_integer() {
        assert_eq!(parse_integer(&Quantity("4".to_string())), 4);
        assert_eq!(parse_integer(&Quantity("0".to_string())), 0);
    }

    #[test]
    fn test_parse_node_basic() {
        use k8s_openapi::api::core::v1::{NodeSpec, NodeStatus};
        use std::collections::BTreeMap;

        let mut allocatable = BTreeMap::new();
        allocatable.insert("cpu".to_string(), Quantity("8".to_string()));
        allocatable.insert("memory".to_string(), Quantity("32Gi".to_string()));
        allocatable.insert("nvidia.com/gpu".to_string(), Quantity("4".to_string()));

        let mut labels = BTreeMap::new();
        labels.insert(
            "network.bubo.io/switch-group".to_string(),
            "sw-rack-01".to_string(),
        );

        let node = Node {
            metadata: kube::api::ObjectMeta {
                name: Some("gpu-node-0".to_string()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(NodeSpec {
                unschedulable: Some(false),
                ..Default::default()
            }),
            status: Some(NodeStatus {
                allocatable: Some(allocatable),
                ..Default::default()
            }),
        };

        let nr = NodeWatcher::parse_node(&node).unwrap();
        assert_eq!(nr.name, "gpu-node-0");
        assert_eq!(nr.allocatable_cpu_millis, 8000);
        assert_eq!(nr.allocatable_memory_bytes, 34_359_738_368);
        assert_eq!(nr.allocatable_gpus, 4);
        assert_eq!(nr.switch_group.as_deref(), Some("sw-rack-01"));
    }

    #[test]
    fn test_parse_node_skips_unschedulable() {
        use k8s_openapi::api::core::v1::{NodeSpec, NodeStatus};
        use std::collections::BTreeMap;

        let mut allocatable = BTreeMap::new();
        allocatable.insert("cpu".to_string(), Quantity("8".to_string()));
        allocatable.insert("memory".to_string(), Quantity("32Gi".to_string()));

        let node = Node {
            metadata: kube::api::ObjectMeta {
                name: Some("cordoned-node".to_string()),
                ..Default::default()
            },
            spec: Some(NodeSpec {
                unschedulable: Some(true),
                ..Default::default()
            }),
            status: Some(NodeStatus {
                allocatable: Some(allocatable),
                ..Default::default()
            }),
        };

        assert!(NodeWatcher::parse_node(&node).is_none());
    }
}
