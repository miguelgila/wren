use std::collections::HashMap;

use bubo_core::{BuboError, QueuedJob, WalltimeDuration};

use crate::queue::PriorityQueue;

/// Configuration for a managed queue.
pub struct QueueConfig {
    pub max_nodes: u32,
    pub max_walltime: Option<WalltimeDuration>,
    pub max_jobs_per_user: Option<u32>,
    pub default_priority: i32,
}

/// Runtime stats snapshot for a single queue.
pub struct QueueStats {
    pub pending_jobs: usize,
    pub active_jobs: u32,
    pub used_nodes: u32,
    pub max_nodes: u32,
}

/// Internal state for a single named queue.
struct ManagedQueue {
    queue: PriorityQueue,
    config: QueueConfig,
    active_jobs: u32,
    used_nodes: u32,
    /// Per-user active job counts (keyed by namespace, used as user proxy).
    user_job_counts: HashMap<String, u32>,
}

/// Manages multiple named priority queues with policy enforcement.
pub struct QueueManager {
    queues: HashMap<String, ManagedQueue>,
    /// Name of the queue used when a job does not specify one.
    #[allow(dead_code)]
    default_queue: String,
}

impl QueueManager {
    /// Creates a new `QueueManager` with a single "default" queue.
    ///
    /// The default queue has no node/walltime/user limits.
    pub fn new(default_queue: &str) -> Self {
        let mut queues = HashMap::new();
        queues.insert(
            default_queue.to_string(),
            ManagedQueue {
                queue: PriorityQueue::new(),
                config: QueueConfig {
                    max_nodes: u32::MAX,
                    max_walltime: None,
                    max_jobs_per_user: None,
                    default_priority: 50,
                },
                active_jobs: 0,
                used_nodes: 0,
                user_job_counts: HashMap::new(),
            },
        );
        Self {
            queues,
            default_queue: default_queue.to_string(),
        }
    }

    /// Registers or replaces a named queue with the given configuration.
    ///
    /// If the queue already exists, its config is updated while preserving
    /// runtime counters (active_jobs, used_nodes, user_job_counts).
    pub fn register_queue(&mut self, name: &str, config: QueueConfig) {
        if let Some(mq) = self.queues.get_mut(name) {
            mq.config = config;
        } else {
            self.queues.insert(
                name.to_string(),
                ManagedQueue {
                    queue: PriorityQueue::new(),
                    config,
                    active_jobs: 0,
                    used_nodes: 0,
                    user_job_counts: HashMap::new(),
                },
            );
        }
    }

    /// Removes a queue by name. Returns an error if the queue has pending jobs.
    pub fn remove_queue(&mut self, name: &str) -> Result<(), BuboError> {
        match self.queues.get(name) {
            None => Err(BuboError::QueueNotFound {
                queue_name: name.to_string(),
            }),
            Some(mq) if !mq.queue.is_empty() => Err(BuboError::ValidationError {
                reason: format!("queue '{}' is not empty", name),
            }),
            _ => {
                self.queues.remove(name);
                Ok(())
            }
        }
    }

    /// Validates job against queue policies and enqueues it.
    ///
    /// Validation order:
    /// 1. Queue exists
    /// 2. Job node count does not exceed queue max_nodes
    /// 3. Adding this job would not push used_nodes over max_nodes
    /// 4. Job walltime does not exceed queue max_walltime
    /// 5. User has not hit max_jobs_per_user limit
    pub fn submit_job(&mut self, job: QueuedJob) -> Result<(), BuboError> {
        let queue_name = job.queue.clone();
        let mq = self.queues.get(&queue_name).ok_or_else(|| BuboError::QueueNotFound {
            queue_name: queue_name.clone(),
        })?;

        // Check job node count fits within queue maximum.
        if job.nodes > mq.config.max_nodes {
            return Err(BuboError::ValidationError {
                reason: format!(
                    "job '{}' requests {} nodes, but queue '{}' allows at most {}",
                    job.name, job.nodes, queue_name, mq.config.max_nodes
                ),
            });
        }

        // Check adding job would not exceed available capacity.
        if mq.used_nodes + job.nodes > mq.config.max_nodes {
            return Err(BuboError::ValidationError {
                reason: format!(
                    "job '{}' requests {} nodes, but queue '{}' only has {} of {} nodes available",
                    job.name,
                    job.nodes,
                    queue_name,
                    mq.config.max_nodes.saturating_sub(mq.used_nodes),
                    mq.config.max_nodes
                ),
            });
        }

        // Check walltime constraint.
        if let Some(ref max_wt) = mq.config.max_walltime {
            if let Some(ref job_wt) = job.walltime {
                if job_wt.seconds > max_wt.seconds {
                    return Err(BuboError::ValidationError {
                        reason: format!(
                            "job '{}' walltime {} exceeds queue '{}' maximum {}",
                            job.name, job_wt, queue_name, max_wt
                        ),
                    });
                }
            }
        }

        // Check per-user job limit.
        if let Some(limit) = mq.config.max_jobs_per_user {
            let current = mq.user_job_counts.get(&job.namespace).copied().unwrap_or(0);
            if current >= limit {
                return Err(BuboError::ValidationError {
                    reason: format!(
                        "user '{}' has reached the max_jobs_per_user limit of {} in queue '{}'",
                        job.namespace, limit, queue_name
                    ),
                });
            }
        }

        // All checks passed — enqueue.
        let mq = self.queues.get_mut(&queue_name).unwrap();
        mq.queue.push(job);
        Ok(())
    }

    /// Pops the highest-priority job across ALL queues.
    ///
    /// Compares the top of each queue's heap and returns the globally best job.
    pub fn next_job(&mut self) -> Option<QueuedJob> {
        // Find the queue name with the highest-priority pending job.
        let best_queue = self
            .queues
            .iter()
            .filter_map(|(name, mq)| mq.queue.peek().map(|job| (name.clone(), job.priority, job.submit_time)))
            .max_by(|a, b| {
                // Higher priority wins; tie-break on earlier submit time.
                match a.1.cmp(&b.1) {
                    std::cmp::Ordering::Equal => b.2.cmp(&a.2),
                    ord => ord,
                }
            })
            .map(|(name, _, _)| name)?;

        self.queues.get_mut(&best_queue)?.queue.pop()
    }

    /// Pops the highest-priority job from a specific queue.
    pub fn next_job_from(&mut self, queue_name: &str) -> Option<QueuedJob> {
        self.queues.get_mut(queue_name)?.queue.pop()
    }

    /// Removes a job by name from any queue. Returns the job if found.
    pub fn cancel_job(&mut self, job_name: &str) -> Option<QueuedJob> {
        for mq in self.queues.values_mut() {
            if let Some(job) = mq.queue.remove_by_name(job_name) {
                return Some(job);
            }
        }
        None
    }

    /// Records that a job has started running.
    ///
    /// Increments `active_jobs`, `used_nodes`, and the per-user job count.
    pub fn record_job_started(&mut self, queue_name: &str, nodes: u32, namespace: &str) {
        if let Some(mq) = self.queues.get_mut(queue_name) {
            mq.active_jobs += 1;
            mq.used_nodes += nodes;
            *mq.user_job_counts.entry(namespace.to_string()).or_insert(0) += 1;
        }
    }

    /// Records that a running job has finished.
    ///
    /// Decrements `active_jobs`, `used_nodes`, and the per-user job count.
    pub fn record_job_finished(&mut self, queue_name: &str, nodes: u32, namespace: &str) {
        if let Some(mq) = self.queues.get_mut(queue_name) {
            mq.active_jobs = mq.active_jobs.saturating_sub(1);
            mq.used_nodes = mq.used_nodes.saturating_sub(nodes);
            let count = mq.user_job_counts.entry(namespace.to_string()).or_insert(0);
            *count = count.saturating_sub(1);
        }
    }

    /// Returns the number of pending jobs in a specific queue.
    pub fn queue_depth(&self, queue_name: &str) -> usize {
        self.queues.get(queue_name).map_or(0, |mq| mq.queue.len())
    }

    /// Returns the total number of pending jobs across all queues.
    pub fn total_depth(&self) -> usize {
        self.queues.values().map(|mq| mq.queue.len()).sum()
    }

    /// Returns a sorted list of registered queue names.
    pub fn queue_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.queues.keys().cloned().collect();
        names.sort();
        names
    }

    /// Returns a stats snapshot for a given queue, or `None` if not found.
    pub fn queue_stats(&self, queue_name: &str) -> Option<QueueStats> {
        let mq = self.queues.get(queue_name)?;
        Some(QueueStats {
            pending_jobs: mq.queue.len(),
            active_jobs: mq.active_jobs,
            used_nodes: mq.used_nodes,
            max_nodes: mq.config.max_nodes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_job(name: &str, queue: &str, namespace: &str, priority: i32, nodes: u32) -> QueuedJob {
        QueuedJob {
            name: name.to_string(),
            namespace: namespace.to_string(),
            queue: queue.to_string(),
            priority,
            nodes,
            tasks_per_node: 1,
            cpu_per_node_millis: 1000,
            memory_per_node_bytes: 1_000_000_000,
            gpus_per_node: 0,
            walltime: None,
            submit_time: Utc::now(),
        }
    }

    fn make_job_with_walltime(name: &str, queue: &str, walltime_secs: u64) -> QueuedJob {
        QueuedJob {
            name: name.to_string(),
            namespace: "default".to_string(),
            queue: queue.to_string(),
            priority: 50,
            nodes: 1,
            tasks_per_node: 1,
            cpu_per_node_millis: 1000,
            memory_per_node_bytes: 1_000_000_000,
            gpus_per_node: 0,
            walltime: Some(WalltimeDuration { seconds: walltime_secs }),
            submit_time: Utc::now(),
        }
    }

    fn default_config(max_nodes: u32) -> QueueConfig {
        QueueConfig {
            max_nodes,
            max_walltime: None,
            max_jobs_per_user: None,
            default_priority: 50,
        }
    }

    #[test]
    fn test_register_and_list_queues() {
        let mut mgr = QueueManager::new("default");
        mgr.register_queue("gpu", default_config(64));
        mgr.register_queue("high", default_config(32));

        let names = mgr.queue_names();
        assert!(names.contains(&"default".to_string()));
        assert!(names.contains(&"gpu".to_string()));
        assert!(names.contains(&"high".to_string()));
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn test_submit_to_default_queue() {
        let mut mgr = QueueManager::new("default");
        let job = make_job("job-1", "default", "user-a", 50, 2);
        mgr.submit_job(job).unwrap();
        assert_eq!(mgr.queue_depth("default"), 1);
        assert_eq!(mgr.total_depth(), 1);
    }

    #[test]
    fn test_submit_to_named_queue() {
        let mut mgr = QueueManager::new("default");
        mgr.register_queue("gpu", default_config(128));

        let job = make_job("job-1", "gpu", "user-a", 100, 4);
        mgr.submit_job(job).unwrap();

        assert_eq!(mgr.queue_depth("gpu"), 1);
        assert_eq!(mgr.queue_depth("default"), 0);
    }

    #[test]
    fn test_submit_to_unknown_queue_fails() {
        let mut mgr = QueueManager::new("default");
        let job = make_job("job-1", "nonexistent", "user-a", 50, 1);
        let result = mgr.submit_job(job);
        assert!(matches!(result, Err(BuboError::QueueNotFound { .. })));
    }

    #[test]
    fn test_max_nodes_enforcement() {
        let mut mgr = QueueManager::new("default");
        mgr.register_queue("small", default_config(8));

        // Job requests more nodes than the queue allows.
        let job = make_job("big-job", "small", "user-a", 50, 16);
        let result = mgr.submit_job(job);
        assert!(
            matches!(result, Err(BuboError::ValidationError { .. })),
            "expected ValidationError"
        );
    }

    #[test]
    fn test_max_nodes_capacity() {
        let mut mgr = QueueManager::new("default");
        mgr.register_queue("limited", default_config(10));

        // Consume 8 nodes via a running job.
        mgr.record_job_started("limited", 8, "user-a");

        // Now submit a job that would push past capacity (8 + 4 = 12 > 10).
        let job = make_job("overflow", "limited", "user-a", 50, 4);
        let result = mgr.submit_job(job);
        assert!(
            matches!(result, Err(BuboError::ValidationError { .. })),
            "expected ValidationError for capacity overflow"
        );

        // But a smaller job fits (8 + 2 = 10 <= 10).
        let job2 = make_job("fits", "limited", "user-a", 50, 2);
        mgr.submit_job(job2).unwrap();
    }

    #[test]
    fn test_max_walltime_enforcement() {
        let mut mgr = QueueManager::new("default");
        mgr.register_queue("short", QueueConfig {
            max_nodes: 100,
            max_walltime: Some(WalltimeDuration { seconds: 3600 }), // 1h
            max_jobs_per_user: None,
            default_priority: 50,
        });

        // Job with walltime exactly at the limit — should pass.
        let ok_job = make_job_with_walltime("ok-job", "short", 3600);
        mgr.submit_job(ok_job).unwrap();

        // Job exceeding the limit — should fail.
        let bad_job = make_job_with_walltime("long-job", "short", 7200);
        let result = mgr.submit_job(bad_job);
        assert!(
            matches!(result, Err(BuboError::ValidationError { .. })),
            "expected ValidationError for walltime exceed"
        );
    }

    #[test]
    fn test_max_jobs_per_user() {
        let mut mgr = QueueManager::new("default");
        mgr.register_queue("fair", QueueConfig {
            max_nodes: 1000,
            max_walltime: None,
            max_jobs_per_user: Some(2),
            default_priority: 50,
        });

        // Simulate two active jobs for user-a.
        mgr.record_job_started("fair", 1, "user-a");
        mgr.record_job_started("fair", 1, "user-a");

        // Third submission from user-a should be rejected.
        let job = make_job("job-3", "fair", "user-a", 50, 1);
        let result = mgr.submit_job(job);
        assert!(
            matches!(result, Err(BuboError::ValidationError { .. })),
            "expected ValidationError for user job limit"
        );

        // user-b should still be able to submit.
        let job_b = make_job("job-b", "fair", "user-b", 50, 1);
        mgr.submit_job(job_b).unwrap();
    }

    #[test]
    fn test_next_job_across_queues() {
        let mut mgr = QueueManager::new("default");
        mgr.register_queue("gpu", default_config(128));

        // Submit lower-priority job to gpu queue and higher-priority to default.
        mgr.submit_job(make_job("low", "gpu", "user-a", 10, 1)).unwrap();
        mgr.submit_job(make_job("high", "default", "user-a", 100, 1)).unwrap();
        mgr.submit_job(make_job("mid", "gpu", "user-a", 50, 1)).unwrap();

        // next_job should return the globally highest-priority job.
        let job = mgr.next_job().unwrap();
        assert_eq!(job.name, "high");

        let job = mgr.next_job().unwrap();
        assert_eq!(job.name, "mid");

        let job = mgr.next_job().unwrap();
        assert_eq!(job.name, "low");

        assert!(mgr.next_job().is_none());
    }

    #[test]
    fn test_next_job_from_specific_queue() {
        let mut mgr = QueueManager::new("default");
        mgr.register_queue("gpu", default_config(128));

        mgr.submit_job(make_job("gpu-job", "gpu", "user-a", 100, 1)).unwrap();
        mgr.submit_job(make_job("default-job", "default", "user-a", 200, 1)).unwrap();

        // Drain gpu queue specifically — should not touch default.
        let job = mgr.next_job_from("gpu").unwrap();
        assert_eq!(job.name, "gpu-job");

        assert_eq!(mgr.queue_depth("default"), 1);
        assert_eq!(mgr.queue_depth("gpu"), 0);
    }

    #[test]
    fn test_cancel_job() {
        let mut mgr = QueueManager::new("default");
        mgr.register_queue("gpu", default_config(128));

        mgr.submit_job(make_job("job-a", "default", "user-a", 50, 1)).unwrap();
        mgr.submit_job(make_job("job-b", "gpu", "user-a", 50, 1)).unwrap();

        // Cancel from a different queue than where it lives.
        let removed = mgr.cancel_job("job-b");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().name, "job-b");

        // job-a should still be there.
        assert_eq!(mgr.total_depth(), 1);

        // Cancelling a non-existent job returns None.
        assert!(mgr.cancel_job("nonexistent").is_none());
    }

    #[test]
    fn test_record_job_lifecycle() {
        let mut mgr = QueueManager::new("default");

        mgr.record_job_started("default", 4, "user-a");
        mgr.record_job_started("default", 2, "user-a");

        let stats = mgr.queue_stats("default").unwrap();
        assert_eq!(stats.active_jobs, 2);
        assert_eq!(stats.used_nodes, 6);

        mgr.record_job_finished("default", 4, "user-a");

        let stats = mgr.queue_stats("default").unwrap();
        assert_eq!(stats.active_jobs, 1);
        assert_eq!(stats.used_nodes, 2);

        mgr.record_job_finished("default", 2, "user-a");

        let stats = mgr.queue_stats("default").unwrap();
        assert_eq!(stats.active_jobs, 0);
        assert_eq!(stats.used_nodes, 0);
    }

    #[test]
    fn test_queue_stats() {
        let mut mgr = QueueManager::new("default");
        mgr.register_queue("test", default_config(50));

        mgr.submit_job(make_job("j1", "test", "user-a", 50, 3)).unwrap();
        mgr.submit_job(make_job("j2", "test", "user-a", 50, 2)).unwrap();
        mgr.record_job_started("test", 10, "user-b");

        let stats = mgr.queue_stats("test").unwrap();
        assert_eq!(stats.pending_jobs, 2);
        assert_eq!(stats.active_jobs, 1);
        assert_eq!(stats.used_nodes, 10);
        assert_eq!(stats.max_nodes, 50);

        assert!(mgr.queue_stats("unknown").is_none());
    }
}
