use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Execution backend type for a WrenJob.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionBackendType {
    #[default]
    Container,
    Reaper,
}

/// State of a WrenJob in its lifecycle.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq, Default)]
pub enum JobState {
    #[default]
    Pending,
    Scheduling,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    WalltimeExceeded,
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobState::Pending => write!(f, "Pending"),
            JobState::Scheduling => write!(f, "Scheduling"),
            JobState::Running => write!(f, "Running"),
            JobState::Succeeded => write!(f, "Succeeded"),
            JobState::Failed => write!(f, "Failed"),
            JobState::Cancelled => write!(f, "Cancelled"),
            JobState::WalltimeExceeded => write!(f, "WalltimeExceeded"),
        }
    }
}

/// Tracks the total resources available on a single node.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeResources {
    pub name: String,
    pub allocatable_cpu_millis: u64,
    pub allocatable_memory_bytes: u64,
    pub allocatable_gpus: u32,
    pub labels: HashMap<String, String>,
    /// Network switch group this node belongs to (for topology-aware scheduling).
    pub switch_group: Option<String>,
    /// Rack identifier.
    pub rack: Option<String>,
}

/// Tracks how much of a node's resources are currently consumed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeAllocation {
    pub used_cpu_millis: u64,
    pub used_memory_bytes: u64,
    pub used_gpus: u32,
    /// Job names currently allocated on this node.
    pub jobs: Vec<String>,
}

/// Snapshot of cluster-wide resource state used by the scheduler.
#[derive(Clone, Debug)]
pub struct ClusterState {
    pub nodes: Vec<NodeResources>,
    pub allocations: HashMap<String, NodeAllocation>,
}

impl ClusterState {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            allocations: HashMap::new(),
        }
    }

    /// Returns available resources on a node after subtracting allocations.
    pub fn available_resources(&self, node_name: &str) -> Option<(u64, u64, u32)> {
        let node = self.nodes.iter().find(|n| n.name == node_name)?;
        let alloc = self.allocations.get(node_name).cloned().unwrap_or_default();
        Some((
            node.allocatable_cpu_millis
                .saturating_sub(alloc.used_cpu_millis),
            node.allocatable_memory_bytes
                .saturating_sub(alloc.used_memory_bytes),
            node.allocatable_gpus.saturating_sub(alloc.used_gpus),
        ))
    }

    /// Records a resource allocation on a node for a given job.
    pub fn allocate(
        &mut self,
        node_name: &str,
        cpu_millis: u64,
        memory_bytes: u64,
        gpus: u32,
        job_name: &str,
    ) {
        let alloc = self.allocations.entry(node_name.to_string()).or_default();
        alloc.used_cpu_millis += cpu_millis;
        alloc.used_memory_bytes += memory_bytes;
        alloc.used_gpus += gpus;
        alloc.jobs.push(job_name.to_string());
    }

    /// Releases a job's allocation from a node.
    pub fn deallocate(
        &mut self,
        node_name: &str,
        cpu_millis: u64,
        memory_bytes: u64,
        gpus: u32,
        job_name: &str,
    ) {
        if let Some(alloc) = self.allocations.get_mut(node_name) {
            alloc.used_cpu_millis = alloc.used_cpu_millis.saturating_sub(cpu_millis);
            alloc.used_memory_bytes = alloc.used_memory_bytes.saturating_sub(memory_bytes);
            alloc.used_gpus = alloc.used_gpus.saturating_sub(gpus);
            alloc.jobs.retain(|j| j != job_name);
        }
    }
}

impl Default for ClusterState {
    fn default() -> Self {
        Self::new()
    }
}

/// A scheduling decision: which nodes to place a job on.
#[derive(Clone, Debug)]
pub struct Placement {
    pub nodes: Vec<String>,
    /// Topology score (higher = better placement). Used when comparing alternatives.
    pub score: f64,
}

/// Parsed walltime duration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalltimeDuration {
    pub seconds: u64,
}

impl WalltimeDuration {
    /// Parse a walltime string like "4h", "30m", "1d", "2h30m", "3600" (seconds).
    pub fn parse(input: &str) -> std::result::Result<Self, crate::error::WrenError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(crate::error::WrenError::InvalidWalltime {
                input: input.to_string(),
            });
        }

        // Try pure seconds
        if let Ok(secs) = input.parse::<u64>() {
            return Ok(Self { seconds: secs });
        }

        let mut total_seconds: u64 = 0;
        let mut current_num = String::new();

        for ch in input.chars() {
            if ch.is_ascii_digit() {
                current_num.push(ch);
            } else {
                let num: u64 =
                    current_num
                        .parse()
                        .map_err(|_| crate::error::WrenError::InvalidWalltime {
                            input: input.to_string(),
                        })?;
                current_num.clear();

                match ch {
                    'd' => total_seconds += num * 86400,
                    'h' => total_seconds += num * 3600,
                    'm' => total_seconds += num * 60,
                    's' => total_seconds += num,
                    _ => {
                        return Err(crate::error::WrenError::InvalidWalltime {
                            input: input.to_string(),
                        })
                    }
                }
            }
        }

        if !current_num.is_empty() {
            return Err(crate::error::WrenError::InvalidWalltime {
                input: input.to_string(),
            });
        }

        if total_seconds == 0 {
            return Err(crate::error::WrenError::InvalidWalltime {
                input: input.to_string(),
            });
        }

        Ok(Self {
            seconds: total_seconds,
        })
    }

    pub fn as_duration(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.seconds)
    }
}

impl std::fmt::Display for WalltimeDuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.seconds;
        if s >= 86400 {
            write!(
                f,
                "{}d{}h{}m",
                s / 86400,
                (s % 86400) / 3600,
                (s % 3600) / 60
            )
        } else if s >= 3600 {
            write!(f, "{}h{}m", s / 3600, (s % 3600) / 60)
        } else if s >= 60 {
            write!(f, "{}m", s / 60)
        } else {
            write!(f, "{}s", s)
        }
    }
}

/// Describes a pending job for the scheduler queue.
#[derive(Clone, Debug)]
pub struct QueuedJob {
    pub name: String,
    pub namespace: String,
    pub queue: String,
    pub priority: i32,
    pub nodes: u32,
    pub tasks_per_node: u32,
    pub cpu_per_node_millis: u64,
    pub memory_per_node_bytes: u64,
    pub gpus_per_node: u32,
    pub walltime: Option<WalltimeDuration>,
    pub submit_time: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_walltime_parse_hours() {
        let wt = WalltimeDuration::parse("4h").unwrap();
        assert_eq!(wt.seconds, 14400);
    }

    #[test]
    fn test_walltime_parse_minutes() {
        let wt = WalltimeDuration::parse("30m").unwrap();
        assert_eq!(wt.seconds, 1800);
    }

    #[test]
    fn test_walltime_parse_days() {
        let wt = WalltimeDuration::parse("1d").unwrap();
        assert_eq!(wt.seconds, 86400);
    }

    #[test]
    fn test_walltime_parse_combined() {
        let wt = WalltimeDuration::parse("2h30m").unwrap();
        assert_eq!(wt.seconds, 9000);
    }

    #[test]
    fn test_walltime_parse_seconds_number() {
        let wt = WalltimeDuration::parse("3600").unwrap();
        assert_eq!(wt.seconds, 3600);
    }

    #[test]
    fn test_walltime_parse_invalid() {
        assert!(WalltimeDuration::parse("").is_err());
        assert!(WalltimeDuration::parse("abc").is_err());
        assert!(WalltimeDuration::parse("4x").is_err());
    }

    #[test]
    fn test_walltime_display() {
        assert_eq!(WalltimeDuration { seconds: 14400 }.to_string(), "4h0m");
        assert_eq!(WalltimeDuration { seconds: 90000 }.to_string(), "1d1h0m");
        assert_eq!(WalltimeDuration { seconds: 300 }.to_string(), "5m");
        assert_eq!(WalltimeDuration { seconds: 45 }.to_string(), "45s");
    }

    #[test]
    fn test_cluster_state_available_resources() {
        let mut state = ClusterState::new();
        state.nodes.push(NodeResources {
            name: "node-0".to_string(),
            allocatable_cpu_millis: 8000,
            allocatable_memory_bytes: 16_000_000_000,
            allocatable_gpus: 4,
            labels: HashMap::new(),
            switch_group: None,
            rack: None,
        });
        state.allocate("node-0", 2000, 4_000_000_000, 1, "job-1");

        let (cpu, mem, gpu) = state.available_resources("node-0").unwrap();
        assert_eq!(cpu, 6000);
        assert_eq!(mem, 12_000_000_000);
        assert_eq!(gpu, 3);
    }

    #[test]
    fn test_cluster_state_deallocate() {
        let mut state = ClusterState::new();
        state.nodes.push(NodeResources {
            name: "node-0".to_string(),
            allocatable_cpu_millis: 8000,
            allocatable_memory_bytes: 16_000_000_000,
            allocatable_gpus: 4,
            labels: HashMap::new(),
            switch_group: None,
            rack: None,
        });
        state.allocate("node-0", 2000, 4_000_000_000, 1, "job-1");
        state.deallocate("node-0", 2000, 4_000_000_000, 1, "job-1");

        let (cpu, mem, gpu) = state.available_resources("node-0").unwrap();
        assert_eq!(cpu, 8000);
        assert_eq!(mem, 16_000_000_000);
        assert_eq!(gpu, 4);
        assert!(state.allocations["node-0"].jobs.is_empty());
    }

    #[test]
    fn test_job_state_display() {
        assert_eq!(JobState::Pending.to_string(), "Pending");
        assert_eq!(JobState::Running.to_string(), "Running");
        assert_eq!(JobState::Failed.to_string(), "Failed");
    }

    #[test]
    fn test_job_state_display_all_variants() {
        assert_eq!(JobState::Scheduling.to_string(), "Scheduling");
        assert_eq!(JobState::Succeeded.to_string(), "Succeeded");
        assert_eq!(JobState::Cancelled.to_string(), "Cancelled");
        assert_eq!(JobState::WalltimeExceeded.to_string(), "WalltimeExceeded");
    }

    #[test]
    fn test_job_state_default() {
        assert_eq!(JobState::default(), JobState::Pending);
    }

    #[test]
    fn test_execution_backend_type_default() {
        assert_eq!(
            ExecutionBackendType::default(),
            ExecutionBackendType::Container
        );
    }

    #[test]
    fn test_execution_backend_type_serde() {
        let json = serde_json::to_string(&ExecutionBackendType::Reaper).unwrap();
        assert_eq!(json, r#""reaper""#);
        let parsed: ExecutionBackendType = serde_json::from_str(r#""container""#).unwrap();
        assert_eq!(parsed, ExecutionBackendType::Container);
    }

    #[test]
    fn test_cluster_state_default() {
        let state = ClusterState::default();
        assert!(state.nodes.is_empty());
        assert!(state.allocations.is_empty());
    }

    #[test]
    fn test_cluster_state_available_resources_unknown_node() {
        let state = ClusterState::new();
        assert!(state.available_resources("nonexistent").is_none());
    }

    #[test]
    fn test_cluster_state_available_resources_no_allocation() {
        let mut state = ClusterState::new();
        state.nodes.push(NodeResources {
            name: "node-0".to_string(),
            allocatable_cpu_millis: 4000,
            allocatable_memory_bytes: 8_000_000_000,
            allocatable_gpus: 2,
            labels: HashMap::new(),
            switch_group: None,
            rack: None,
        });
        let (cpu, mem, gpu) = state.available_resources("node-0").unwrap();
        assert_eq!(cpu, 4000);
        assert_eq!(mem, 8_000_000_000);
        assert_eq!(gpu, 2);
    }

    #[test]
    fn test_cluster_state_multiple_allocations() {
        let mut state = ClusterState::new();
        state.nodes.push(NodeResources {
            name: "n0".to_string(),
            allocatable_cpu_millis: 8000,
            allocatable_memory_bytes: 16_000_000_000,
            allocatable_gpus: 4,
            labels: HashMap::new(),
            switch_group: None,
            rack: None,
        });
        state.allocate("n0", 1000, 2_000_000_000, 1, "job-1");
        state.allocate("n0", 2000, 4_000_000_000, 1, "job-2");

        let (cpu, mem, gpu) = state.available_resources("n0").unwrap();
        assert_eq!(cpu, 5000);
        assert_eq!(mem, 10_000_000_000);
        assert_eq!(gpu, 2);

        let alloc = &state.allocations["n0"];
        assert_eq!(alloc.jobs, vec!["job-1", "job-2"]);
    }

    #[test]
    fn test_cluster_state_deallocate_nonexistent_node() {
        let mut state = ClusterState::new();
        // Should not panic
        state.deallocate("nonexistent", 1000, 1000, 1, "job-1");
    }

    #[test]
    fn test_cluster_state_deallocate_saturating() {
        let mut state = ClusterState::new();
        state.nodes.push(NodeResources {
            name: "n0".to_string(),
            allocatable_cpu_millis: 8000,
            allocatable_memory_bytes: 16_000_000_000,
            allocatable_gpus: 4,
            labels: HashMap::new(),
            switch_group: None,
            rack: None,
        });
        state.allocate("n0", 1000, 1_000_000_000, 1, "job-1");
        // Deallocate more than was allocated — should saturate to 0
        state.deallocate("n0", 5000, 5_000_000_000, 5, "job-1");

        let alloc = &state.allocations["n0"];
        assert_eq!(alloc.used_cpu_millis, 0);
        assert_eq!(alloc.used_memory_bytes, 0);
        assert_eq!(alloc.used_gpus, 0);
    }

    #[test]
    fn test_walltime_parse_seconds_suffix() {
        let wt = WalltimeDuration::parse("30s").unwrap();
        assert_eq!(wt.seconds, 30);
    }

    #[test]
    fn test_walltime_parse_complex() {
        let wt = WalltimeDuration::parse("1d2h30m").unwrap();
        assert_eq!(wt.seconds, 86400 + 7200 + 1800);
    }

    #[test]
    fn test_walltime_parse_zero_duration() {
        assert!(WalltimeDuration::parse("0h").is_err());
    }

    #[test]
    fn test_walltime_parse_trailing_number() {
        assert!(WalltimeDuration::parse("4h30").is_err());
    }

    #[test]
    fn test_walltime_as_duration() {
        let wt = WalltimeDuration { seconds: 3600 };
        assert_eq!(wt.as_duration(), std::time::Duration::from_secs(3600));
    }

    #[test]
    fn test_walltime_display_seconds() {
        assert_eq!(WalltimeDuration { seconds: 5 }.to_string(), "5s");
    }

    #[test]
    fn test_walltime_display_days() {
        assert_eq!(WalltimeDuration { seconds: 90061 }.to_string(), "1d1h1m");
    }

    #[test]
    fn test_node_resources_with_topology() {
        let node = NodeResources {
            name: "gpu-node-0".to_string(),
            allocatable_cpu_millis: 64000,
            allocatable_memory_bytes: 256_000_000_000,
            allocatable_gpus: 8,
            labels: HashMap::from([("zone".to_string(), "us-east-1a".to_string())]),
            switch_group: Some("sw-rack-01".to_string()),
            rack: Some("rack-01".to_string()),
        };
        assert_eq!(node.switch_group.as_deref(), Some("sw-rack-01"));
        assert_eq!(node.rack.as_deref(), Some("rack-01"));
        assert_eq!(node.labels["zone"], "us-east-1a");
    }

    #[test]
    fn test_node_allocation_default() {
        let alloc = NodeAllocation::default();
        assert_eq!(alloc.used_cpu_millis, 0);
        assert_eq!(alloc.used_memory_bytes, 0);
        assert_eq!(alloc.used_gpus, 0);
        assert!(alloc.jobs.is_empty());
    }

    #[test]
    fn test_placement_clone() {
        let p = Placement {
            nodes: vec!["n0".to_string(), "n1".to_string()],
            score: 0.95,
        };
        let c = p.clone();
        assert_eq!(c.nodes, vec!["n0", "n1"]);
        assert!((c.score - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn test_job_state_serde_roundtrip() {
        for state in [
            JobState::Pending,
            JobState::Scheduling,
            JobState::Running,
            JobState::Succeeded,
            JobState::Failed,
            JobState::Cancelled,
            JobState::WalltimeExceeded,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let parsed: JobState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn test_walltime_parse_whitespace_trimmed() {
        let wt = WalltimeDuration::parse("  4h  ").unwrap();
        assert_eq!(wt.seconds, 14400);
    }

    #[test]
    fn test_walltime_parse_only_whitespace_is_error() {
        assert!(WalltimeDuration::parse("   ").is_err());
    }

    #[test]
    fn test_walltime_parse_unknown_suffix_is_error() {
        assert!(WalltimeDuration::parse("4w").is_err()); // weeks not supported
        assert!(WalltimeDuration::parse("4y").is_err());
    }

    #[test]
    fn test_walltime_display_minutes_exact() {
        assert_eq!(WalltimeDuration { seconds: 120 }.to_string(), "2m");
        assert_eq!(WalltimeDuration { seconds: 3600 }.to_string(), "1h0m");
    }

    #[test]
    fn test_walltime_as_duration_zero() {
        // WalltimeDuration with 0 seconds (not normally constructible via parse
        // but representable as struct).
        let wt = WalltimeDuration { seconds: 0 };
        assert_eq!(wt.as_duration(), std::time::Duration::from_secs(0));
    }

    #[test]
    fn test_cluster_state_allocate_same_node_multiple_jobs() {
        let mut state = ClusterState::new();
        state.nodes.push(NodeResources {
            name: "n0".to_string(),
            allocatable_cpu_millis: 16000,
            allocatable_memory_bytes: 32_000_000_000,
            allocatable_gpus: 8,
            labels: HashMap::new(),
            switch_group: None,
            rack: None,
        });

        state.allocate("n0", 4000, 8_000_000_000, 2, "job-1");
        state.allocate("n0", 4000, 8_000_000_000, 2, "job-2");

        let (cpu, mem, gpu) = state.available_resources("n0").unwrap();
        assert_eq!(cpu, 8000);
        assert_eq!(mem, 16_000_000_000);
        assert_eq!(gpu, 4);

        let alloc = &state.allocations["n0"];
        assert!(alloc.jobs.contains(&"job-1".to_string()));
        assert!(alloc.jobs.contains(&"job-2".to_string()));
    }

    #[test]
    fn test_cluster_state_deallocate_only_removes_one_job_name() {
        let mut state = ClusterState::new();
        state.nodes.push(NodeResources {
            name: "n0".to_string(),
            allocatable_cpu_millis: 16000,
            allocatable_memory_bytes: 32_000_000_000,
            allocatable_gpus: 0,
            labels: HashMap::new(),
            switch_group: None,
            rack: None,
        });

        state.allocate("n0", 4000, 0, 0, "job-1");
        state.allocate("n0", 4000, 0, 0, "job-2");
        state.deallocate("n0", 4000, 0, 0, "job-1");

        let alloc = &state.allocations["n0"];
        assert!(!alloc.jobs.contains(&"job-1".to_string()));
        assert!(alloc.jobs.contains(&"job-2".to_string()));
        assert_eq!(alloc.used_cpu_millis, 4000);
    }

    #[test]
    fn test_node_resources_no_switch_no_rack() {
        let node = NodeResources {
            name: "bare-node".to_string(),
            allocatable_cpu_millis: 4000,
            allocatable_memory_bytes: 8_000_000_000,
            allocatable_gpus: 0,
            labels: HashMap::new(),
            switch_group: None,
            rack: None,
        };
        assert!(node.switch_group.is_none());
        assert!(node.rack.is_none());
    }

    #[test]
    fn test_execution_backend_type_reaper_serde() {
        let json = serde_json::to_string(&ExecutionBackendType::Reaper).unwrap();
        assert_eq!(json, r#""reaper""#);
        let parsed: ExecutionBackendType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ExecutionBackendType::Reaper);
    }

    #[test]
    fn test_queued_job_fields() {
        let now = chrono::Utc::now();
        let job = QueuedJob {
            name: "test-job".to_string(),
            namespace: "default".to_string(),
            queue: "gpu".to_string(),
            priority: 100,
            nodes: 4,
            tasks_per_node: 8,
            cpu_per_node_millis: 8000,
            memory_per_node_bytes: 16_000_000_000,
            gpus_per_node: 4,
            walltime: Some(WalltimeDuration { seconds: 3600 }),
            submit_time: now,
        };
        assert_eq!(job.name, "test-job");
        assert_eq!(job.nodes, 4);
        assert_eq!(job.tasks_per_node, 8);
        assert_eq!(job.gpus_per_node, 4);
        assert_eq!(job.walltime.unwrap().seconds, 3600);
        assert_eq!(job.submit_time, now);
    }
}
