//! Backfill scheduler — Slurm-style algorithm that allows smaller/shorter jobs to
//! fill idle resource gaps without delaying higher-priority blocked jobs.
//!
//! ## Algorithm outline
//!
//! 1. Build a [`ResourceTimeline`] from running jobs: for each node, record when
//!    each running job's resources will be released (based on walltime).
//! 2. The highest-priority pending job gets a *reservation*: compute the earliest
//!    time enough nodes will be free for it, and mark those nodes/times reserved.
//! 3. For every lower-priority pending job (in priority order), check whether it
//!    can start *now* (resources available on enough nodes) **and** finish before
//!    it would displace the reserved job's start time.
//! 4. Return a [`BackfillDecision`] for each job that passes the check.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::{debug, trace};

use wren_core::{ClusterState, NodeResources, Placement, QueuedJob, WalltimeDuration};

/// Info about a currently running job needed for timeline projection.
#[derive(Clone, Debug)]
pub struct RunningJobInfo {
    pub name: String,
    /// Nodes the job occupies.
    pub nodes: Vec<String>,
    pub cpu_per_node_millis: u64,
    pub memory_per_node_bytes: u64,
    pub gpus_per_node: u32,
    pub started_at: DateTime<Utc>,
    /// If `None` the job is treated as running indefinitely (not releasable within the
    /// look-ahead window).
    pub walltime: Option<WalltimeDuration>,
}

impl RunningJobInfo {
    /// Estimated end time. Returns `None` if the job has no walltime.
    pub fn estimated_end(&self) -> Option<DateTime<Utc>> {
        self.walltime
            .as_ref()
            .map(|wt| self.started_at + chrono::Duration::seconds(wt.seconds as i64))
    }
}

// ---------------------------------------------------------------------------
// ResourceTimeline
// ---------------------------------------------------------------------------

/// A time-ordered sequence of resource-release events for a single node.
#[derive(Clone, Debug)]
struct NodeTimeline {
    #[allow(dead_code)]
    node_name: String,
    /// Sorted ascending by release_at. Each entry describes resources becoming
    /// available at that instant.
    events: Vec<ReleaseEvent>,
}

#[derive(Clone, Debug)]
struct ReleaseEvent {
    /// When these resources will be freed.
    release_at: DateTime<Utc>,
    cpu_millis: u64,
    memory_bytes: u64,
    gpus: u32,
}

/// Cluster-wide timeline of when resources will become available.
///
/// Constructed from the set of currently-running jobs and used to project
/// whether (and when) a pending job could start.
pub struct ResourceTimeline {
    /// Per-node timelines, keyed by node name.
    node_timelines: HashMap<String, NodeTimeline>,
    /// Reference to cluster topology/allocatable resources (not modified).
    #[allow(dead_code)]
    nodes: Vec<NodeResources>,
    /// When the timeline was built (i.e. "now").
    #[allow(dead_code)]
    built_at: DateTime<Utc>,
}

impl ResourceTimeline {
    /// Compute resources currently available on `node_name` *plus* any resources
    /// that will be released at or before `at`.
    ///
    /// Returns `(cpu_millis, memory_bytes, gpus)`.
    pub fn available_at(
        &self,
        node_name: &str,
        cluster: &ClusterState,
        at: DateTime<Utc>,
    ) -> (u64, u64, u32) {
        // Start from current available resources.
        let (cur_cpu, cur_mem, cur_gpu) =
            cluster.available_resources(node_name).unwrap_or((0, 0, 0));

        // Add back resources released by jobs finishing before `at`.
        let extra = self
            .node_timelines
            .get(node_name)
            .map(|tl| {
                tl.events.iter().filter(|ev| ev.release_at <= at).fold(
                    (0u64, 0u64, 0u32),
                    |acc, ev| {
                        (
                            acc.0 + ev.cpu_millis,
                            acc.1 + ev.memory_bytes,
                            acc.2 + ev.gpus,
                        )
                    },
                )
            })
            .unwrap_or((0, 0, 0));

        (cur_cpu + extra.0, cur_mem + extra.1, cur_gpu + extra.2)
    }

    /// Find the earliest `DateTime` at or after `not_before` when at least
    /// `node_count` nodes can simultaneously satisfy the per-node resource
    /// requirements.
    ///
    /// Returns `None` if no such time exists within the look-ahead window
    /// (`not_before + look_ahead`).
    #[allow(clippy::too_many_arguments)]
    pub fn earliest_start(
        &self,
        cluster: &ClusterState,
        node_count: u32,
        cpu_per_node: u64,
        memory_per_node: u64,
        gpus_per_node: u32,
        not_before: DateTime<Utc>,
        look_ahead: Duration,
    ) -> Option<DateTime<Utc>> {
        let deadline = not_before + chrono::Duration::from_std(look_ahead).ok()?;

        // Collect all candidate "check points": now + every release event within
        // the window. The earliest start must coincide with one of these.
        let mut check_points: Vec<DateTime<Utc>> = vec![not_before];
        for tl in self.node_timelines.values() {
            for ev in &tl.events {
                if ev.release_at > not_before && ev.release_at <= deadline {
                    check_points.push(ev.release_at);
                }
            }
        }
        check_points.sort();
        check_points.dedup();

        for t in check_points {
            let feasible_count = cluster
                .nodes
                .iter()
                .filter(|n| {
                    let (cpu, mem, gpu) = self.available_at(&n.name, cluster, t);
                    cpu >= cpu_per_node && mem >= memory_per_node && gpu >= gpus_per_node
                })
                .count();

            if feasible_count >= node_count as usize {
                return Some(t);
            }
        }

        None
    }
}

// ---------------------------------------------------------------------------
// BackfillScheduler
// ---------------------------------------------------------------------------

/// Result of a backfill feasibility check for a single pending job.
#[derive(Clone, Debug)]
pub struct BackfillDecision {
    pub job_name: String,
    /// Nodes selected for this job.
    pub placement: Placement,
    /// `true` when this job can start immediately without delaying any
    /// higher-priority job's computed reservation time.
    pub safe_to_backfill: bool,
}

/// Slurm-inspired backfill scheduler.
///
/// Stateless: all methods are pure functions over the supplied inputs.
pub struct BackfillScheduler;

impl BackfillScheduler {
    /// Build a [`ResourceTimeline`] from the current cluster state and the list
    /// of running jobs.
    ///
    /// Jobs without a walltime are ignored in timeline projection (they hold
    /// resources indefinitely as far as the algorithm is concerned).
    pub fn build_timeline(
        cluster: &ClusterState,
        running_jobs: &[RunningJobInfo],
        now: DateTime<Utc>,
    ) -> ResourceTimeline {
        let mut node_timelines: HashMap<String, NodeTimeline> = HashMap::new();

        for job in running_jobs {
            let Some(end_time) = job.estimated_end() else {
                trace!(job = job.name, "No walltime; skipping timeline entry");
                continue;
            };

            for node_name in &job.nodes {
                let tl = node_timelines
                    .entry(node_name.clone())
                    .or_insert_with(|| NodeTimeline {
                        node_name: node_name.clone(),
                        events: Vec::new(),
                    });
                tl.events.push(ReleaseEvent {
                    release_at: end_time,
                    cpu_millis: job.cpu_per_node_millis,
                    memory_bytes: job.memory_per_node_bytes,
                    gpus: job.gpus_per_node,
                });
            }
        }

        // Sort each node's events chronologically.
        for tl in node_timelines.values_mut() {
            tl.events.sort_by_key(|ev| ev.release_at);
        }

        debug!(node_count = node_timelines.len(), "Built resource timeline");

        ResourceTimeline {
            node_timelines,
            nodes: cluster.nodes.clone(),
            built_at: now,
        }
    }

    /// Find jobs that can be backfilled into available resource gaps.
    ///
    /// `pending_jobs` **must** be sorted by priority descending (highest first).
    /// `look_ahead` limits how far into the future we project reservations.
    ///
    /// Returns a list of [`BackfillDecision`]s for jobs that can start
    /// immediately without delaying higher-priority reserved jobs.
    pub fn find_backfill_candidates(
        cluster: &ClusterState,
        running_jobs: &[RunningJobInfo],
        pending_jobs: &[&QueuedJob],
        look_ahead: Duration,
        now: DateTime<Utc>,
    ) -> Vec<BackfillDecision> {
        if pending_jobs.is_empty() {
            return Vec::new();
        }

        let timeline = Self::build_timeline(cluster, running_jobs, now);

        // Reservation: the earliest start time for the highest-priority job.
        // This is the "protected" slot we must not displace.
        let top_job = pending_jobs[0];
        let reservation_start = timeline.earliest_start(
            cluster,
            top_job.nodes,
            top_job.cpu_per_node_millis,
            top_job.memory_per_node_bytes,
            top_job.gpus_per_node,
            now,
            look_ahead,
        );

        debug!(
            top_job = top_job.name,
            reservation_start = ?reservation_start,
            "Computed reservation for highest-priority job"
        );

        let mut decisions: Vec<BackfillDecision> = Vec::new();

        // Shadow cluster state to track tentative backfill allocations so we
        // don't double-book nodes within this scheduling pass.
        let mut shadow = cluster.clone();

        for job in pending_jobs {
            // Select nodes currently available (greedy: first N feasible).
            let selected = Self::select_nodes_now(&shadow, job);
            let Some(nodes) = selected else {
                trace!(job = job.name, "No nodes available now; skipping backfill");
                continue;
            };

            let safe = Self::is_safe_to_backfill(job, now, reservation_start);

            if safe {
                debug!(
                    job = job.name,
                    nodes = ?nodes,
                    "Job approved for backfill"
                );

                // Record shadow allocation so subsequent jobs don't over-book.
                for node_name in &nodes {
                    shadow.allocate(
                        node_name,
                        job.cpu_per_node_millis,
                        job.memory_per_node_bytes,
                        job.gpus_per_node,
                        &job.name,
                    );
                }

                decisions.push(BackfillDecision {
                    job_name: job.name.clone(),
                    placement: Placement { nodes, score: 1.0 },
                    safe_to_backfill: true,
                });
            }
        }

        decisions
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Attempt to greedily select `job.nodes` feasible nodes from `cluster` right now.
    ///
    /// Returns `None` if fewer than the required number of nodes are available.
    fn select_nodes_now(cluster: &ClusterState, job: &QueuedJob) -> Option<Vec<String>> {
        let selected: Vec<String> = cluster
            .nodes
            .iter()
            .filter(|n| {
                let alloc = cluster
                    .allocations
                    .get(&n.name)
                    .cloned()
                    .unwrap_or_default();
                let avail_cpu = n
                    .allocatable_cpu_millis
                    .saturating_sub(alloc.used_cpu_millis);
                let avail_mem = n
                    .allocatable_memory_bytes
                    .saturating_sub(alloc.used_memory_bytes);
                let avail_gpu = n.allocatable_gpus.saturating_sub(alloc.used_gpus);
                avail_cpu >= job.cpu_per_node_millis
                    && avail_mem >= job.memory_per_node_bytes
                    && avail_gpu >= job.gpus_per_node
            })
            .take(job.nodes as usize)
            .map(|n| n.name.clone())
            .collect();

        if selected.len() < job.nodes as usize {
            None
        } else {
            Some(selected)
        }
    }

    /// A job is safe to backfill when:
    ///   (a) It has a walltime, and
    ///   (b) Its projected completion (`now + walltime`) is at or before the
    ///       reservation start for the top-priority job — or there is no
    ///       reservation (the top job can never start within the look-ahead).
    fn is_safe_to_backfill(
        job: &QueuedJob,
        now: DateTime<Utc>,
        reservation_start: Option<DateTime<Utc>>,
    ) -> bool {
        let Some(ref wt) = job.walltime else {
            // No walltime: cannot guarantee we won't delay the reserved job.
            return false;
        };

        let Some(reserved_at) = reservation_start else {
            // The top-priority job has no feasible start within the window.
            // Any job with a walltime can safely run.
            return true;
        };

        let completion = now + chrono::Duration::seconds(wt.seconds as i64);
        completion <= reserved_at
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ----- helpers ----------------------------------------------------------

    fn node(name: &str, cpu: u64, mem: u64, gpus: u32) -> NodeResources {
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

    fn cluster(nodes: Vec<NodeResources>) -> ClusterState {
        ClusterState {
            nodes,
            allocations: HashMap::new(),
        }
    }

    fn queued_job(name: &str, priority: i32, nodes: u32, walltime_secs: Option<u64>) -> QueuedJob {
        QueuedJob {
            name: name.to_string(),
            namespace: "default".to_string(),
            queue: "default".to_string(),
            priority,
            nodes,
            tasks_per_node: 1,
            cpu_per_node_millis: 2000,
            memory_per_node_bytes: 4_000_000_000,
            gpus_per_node: 0,
            walltime: walltime_secs.map(|s| WalltimeDuration { seconds: s }),
            submit_time: Utc::now(),
        }
    }

    fn running_job(
        name: &str,
        nodes: &[&str],
        started_secs_ago: i64,
        walltime_secs: Option<u64>,
    ) -> RunningJobInfo {
        RunningJobInfo {
            name: name.to_string(),
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            cpu_per_node_millis: 2000,
            memory_per_node_bytes: 4_000_000_000,
            gpus_per_node: 0,
            started_at: Utc::now() - chrono::Duration::seconds(started_secs_ago),
            walltime: walltime_secs.map(|s| WalltimeDuration { seconds: s }),
        }
    }

    // ----- build_timeline ---------------------------------------------------

    #[test]
    fn test_build_timeline_empty() {
        let c = cluster(vec![node("n0", 8000, 16_000_000_000, 0)]);
        let tl = BackfillScheduler::build_timeline(&c, &[], Utc::now());
        assert!(tl.node_timelines.is_empty());
    }

    #[test]
    fn test_build_timeline_job_without_walltime_ignored() {
        let c = cluster(vec![node("n0", 8000, 16_000_000_000, 0)]);
        let jobs = vec![running_job("j1", &["n0"], 60, None)];
        let tl = BackfillScheduler::build_timeline(&c, &jobs, Utc::now());
        // Job with no walltime should not appear in timeline.
        assert!(!tl.node_timelines.contains_key("n0"));
    }

    #[test]
    fn test_build_timeline_records_release_event() {
        let c = cluster(vec![node("n0", 8000, 16_000_000_000, 0)]);
        // Job started 30 s ago with a 60 s walltime → frees up in 30 s.
        let jobs = vec![running_job("j1", &["n0"], 30, Some(60))];
        let tl = BackfillScheduler::build_timeline(&c, &jobs, Utc::now());
        let events = &tl.node_timelines["n0"].events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cpu_millis, 2000);
    }

    #[test]
    fn test_build_timeline_multiple_jobs_same_node() {
        let c = cluster(vec![node("n0", 8000, 16_000_000_000, 0)]);
        let jobs = vec![
            running_job("j1", &["n0"], 10, Some(60)),
            running_job("j2", &["n0"], 5, Some(120)),
        ];
        let tl = BackfillScheduler::build_timeline(&c, &jobs, Utc::now());
        // Two events on n0, sorted by release_at.
        assert_eq!(tl.node_timelines["n0"].events.len(), 2);
        let evs = &tl.node_timelines["n0"].events;
        assert!(evs[0].release_at <= evs[1].release_at);
    }

    // ----- available_at -----------------------------------------------------

    #[test]
    fn test_available_at_current_only() {
        let c = cluster(vec![node("n0", 8000, 16_000_000_000, 0)]);
        let tl = BackfillScheduler::build_timeline(&c, &[], Utc::now());
        let now = Utc::now();
        let (cpu, mem, gpu) = tl.available_at("n0", &c, now);
        assert_eq!(cpu, 8000);
        assert_eq!(mem, 16_000_000_000);
        assert_eq!(gpu, 0);
    }

    #[test]
    fn test_available_at_adds_released_resources() {
        // n0 has 8000m CPU total; 2000m in use by a job finishing in 30 s.
        let mut c = cluster(vec![node("n0", 8000, 16_000_000_000, 0)]);
        c.allocate("n0", 2000, 4_000_000_000, 0, "j1");

        let jobs = vec![running_job("j1", &["n0"], 30, Some(60))];
        let now = Utc::now();
        let tl = BackfillScheduler::build_timeline(&c, &jobs, now);

        // At now: only 6000m available (j1 still running).
        let (cpu_now, _, _) = tl.available_at("n0", &c, now);
        assert_eq!(cpu_now, 6000);

        // 60 s from now j1 has ended → 8000m available.
        let future = now + chrono::Duration::seconds(60);
        let (cpu_future, _, _) = tl.available_at("n0", &c, future);
        assert_eq!(cpu_future, 8000);
    }

    // ----- is_safe_to_backfill ----------------------------------------------

    #[test]
    fn test_safe_to_backfill_no_walltime_is_unsafe() {
        let job = queued_job("j", 10, 1, None);
        let now = Utc::now();
        let reservation = Some(now + chrono::Duration::seconds(3600));
        assert!(!BackfillScheduler::is_safe_to_backfill(
            &job,
            now,
            reservation
        ));
    }

    #[test]
    fn test_safe_to_backfill_completes_before_reservation() {
        let job = queued_job("j", 10, 1, Some(1800)); // 30 min walltime
        let now = Utc::now();
        let reservation = Some(now + chrono::Duration::seconds(3600)); // reserved in 1 h
        assert!(BackfillScheduler::is_safe_to_backfill(
            &job,
            now,
            reservation
        ));
    }

    #[test]
    fn test_safe_to_backfill_would_overlap_reservation() {
        let job = queued_job("j", 10, 1, Some(7200)); // 2 h walltime
        let now = Utc::now();
        let reservation = Some(now + chrono::Duration::seconds(3600)); // reserved in 1 h
        assert!(!BackfillScheduler::is_safe_to_backfill(
            &job,
            now,
            reservation
        ));
    }

    #[test]
    fn test_safe_to_backfill_no_reservation_means_safe() {
        let job = queued_job("j", 10, 1, Some(7200));
        let now = Utc::now();
        // No reservation (top job can't start in window)
        assert!(BackfillScheduler::is_safe_to_backfill(&job, now, None));
    }

    // ----- find_backfill_candidates -----------------------------------------

    #[test]
    fn test_no_pending_jobs_returns_empty() {
        let c = cluster(vec![node("n0", 8000, 16_000_000_000, 0)]);
        let decisions = BackfillScheduler::find_backfill_candidates(
            &c,
            &[],
            &[],
            Duration::from_secs(7200),
            Utc::now(),
        );
        assert!(decisions.is_empty());
    }

    #[test]
    fn test_single_job_no_resources_not_scheduled() {
        // Cluster fully occupied.
        let mut c = cluster(vec![node("n0", 8000, 16_000_000_000, 0)]);
        c.allocate("n0", 8000, 16_000_000_000, 0, "blocker");

        let job = queued_job("small", 10, 1, Some(1800));
        let pending = vec![&job];

        let decisions = BackfillScheduler::find_backfill_candidates(
            &c,
            &[],
            &pending,
            Duration::from_secs(7200),
            Utc::now(),
        );
        assert!(decisions.is_empty());
    }

    #[test]
    fn test_backfill_approved_when_safe() {
        // 2 nodes free, 1 high-priority job blocked waiting for 2 nodes,
        // 1 small low-priority job needs 1 node with short walltime.
        let c = cluster(vec![
            node("n0", 8000, 16_000_000_000, 0),
            node("n1", 8000, 16_000_000_000, 0),
        ]);

        // High-priority job needs both nodes; mark n0 as occupied so it can't
        // start yet (only 1 node free wouldn't satisfy a 2-node job in tests —
        // but here both are free so it *would* start; let's use a 3-node job).
        let high = queued_job("high", 100, 3, Some(3600)); // needs 3 nodes — blocked
        let low = queued_job("low", 10, 1, Some(1800)); // needs 1 node, 30 min

        // high is sorted first (highest priority).
        let pending: Vec<&QueuedJob> = vec![&high, &low];

        let decisions = BackfillScheduler::find_backfill_candidates(
            &c,
            &[],
            &pending,
            Duration::from_secs(7200),
            Utc::now(),
        );

        // "low" should be approved; "high" can't run (only 2 nodes available).
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].job_name, "low");
        assert!(decisions[0].safe_to_backfill);
    }

    #[test]
    fn test_no_walltime_job_not_backfilled() {
        let c = cluster(vec![node("n0", 8000, 16_000_000_000, 0)]);
        // A high-priority job that will eventually need n0.
        let high = queued_job("high", 100, 2, Some(3600)); // blocked (only 1 node)
        let low_no_wt = queued_job("low", 10, 1, None); // no walltime

        let pending: Vec<&QueuedJob> = vec![&high, &low_no_wt];

        let decisions = BackfillScheduler::find_backfill_candidates(
            &c,
            &[],
            &pending,
            Duration::from_secs(7200),
            Utc::now(),
        );

        // "low_no_wt" must not be backfilled because it has no walltime.
        assert!(decisions.is_empty());
    }

    #[test]
    fn test_shadow_allocation_prevents_double_booking() {
        // 2 nodes, 2 small jobs each needing 1 node + 1 large job needing 2.
        let c = cluster(vec![
            node("n0", 4000, 8_000_000_000, 0),
            node("n1", 4000, 8_000_000_000, 0),
        ]);

        let big = queued_job("big", 100, 4, Some(3600)); // blocked
        let mut small1 = queued_job("small1", 50, 1, Some(1800));
        small1.cpu_per_node_millis = 4000;
        small1.memory_per_node_bytes = 8_000_000_000;
        let mut small2 = queued_job("small2", 40, 1, Some(1800));
        small2.cpu_per_node_millis = 4000;
        small2.memory_per_node_bytes = 8_000_000_000;

        let pending: Vec<&QueuedJob> = vec![&big, &small1, &small2];

        let decisions = BackfillScheduler::find_backfill_candidates(
            &c,
            &[],
            &pending,
            Duration::from_secs(7200),
            Utc::now(),
        );

        // Both small jobs should be scheduled (each gets one node).
        assert_eq!(decisions.len(), 2);
        // Nodes must be distinct.
        let n0 = &decisions[0].placement.nodes[0];
        let n1 = &decisions[1].placement.nodes[0];
        assert_ne!(n0, n1);
    }
}
