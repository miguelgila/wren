use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Execution backend type for a BuboJob.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionBackendType {
    #[default]
    Container,
    Reaper,
}

/// State of a BuboJob in its lifecycle.
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
            node.allocatable_cpu_millis.saturating_sub(alloc.used_cpu_millis),
            node.allocatable_memory_bytes.saturating_sub(alloc.used_memory_bytes),
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
    pub fn parse(input: &str) -> std::result::Result<Self, crate::error::BuboError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(crate::error::BuboError::InvalidWalltime {
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
                        .map_err(|_| crate::error::BuboError::InvalidWalltime {
                            input: input.to_string(),
                        })?;
                current_num.clear();

                match ch {
                    'd' => total_seconds += num * 86400,
                    'h' => total_seconds += num * 3600,
                    'm' => total_seconds += num * 60,
                    's' => total_seconds += num,
                    _ => {
                        return Err(crate::error::BuboError::InvalidWalltime {
                            input: input.to_string(),
                        })
                    }
                }
            }
        }

        if !current_num.is_empty() {
            return Err(crate::error::BuboError::InvalidWalltime {
                input: input.to_string(),
            });
        }

        if total_seconds == 0 {
            return Err(crate::error::BuboError::InvalidWalltime {
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
            write!(f, "{}d{}h{}m", s / 86400, (s % 86400) / 3600, (s % 3600) / 60)
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
}
