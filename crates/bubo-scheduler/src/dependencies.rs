use std::collections::{HashMap, VecDeque};

use bubo_core::{BuboError, DependencyType, JobDependency, JobState};
use tracing::debug;

// ---------------------------------------------------------------------------
// DependencyStatus
// ---------------------------------------------------------------------------

/// Result of checking whether a job's dependencies are satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyStatus {
    /// All dependencies are satisfied — the job may proceed.
    Satisfied,
    /// At least one dependency is not yet resolved — the job must wait.
    Waiting { pending_deps: Vec<String> },
    /// A dependency failed in a way that permanently blocks this job.
    /// E.g., an `AfterOk` dependency completed with `Failed` state.
    Failed { reason: String },
}

// ---------------------------------------------------------------------------
// DependencyResolver
// ---------------------------------------------------------------------------

/// Tracks job states and resolves Slurm-style dependency chains.
///
/// Keeps this crate free of Kubernetes dependencies by operating only on
/// `bubo-core` types.
pub struct DependencyResolver {
    /// Known job states, keyed by job name.
    job_states: HashMap<String, JobState>,
    /// Registered dependencies: job_name -> list of its declared dependencies.
    dependencies: HashMap<String, Vec<JobDependency>>,
}

impl DependencyResolver {
    pub fn new() -> Self {
        Self {
            job_states: HashMap::new(),
            dependencies: HashMap::new(),
        }
    }

    /// Register a job and the dependencies it declares.
    ///
    /// Overwrites any previously registered dependency list for the same job.
    pub fn register_job(&mut self, job_name: &str, deps: Vec<JobDependency>) {
        debug!(job = job_name, dep_count = deps.len(), "DependencyResolver: registering job");
        self.dependencies.insert(job_name.to_string(), deps);
        // Ensure the job has an entry in job_states (default Pending) so it
        // appears in the full job graph for cycle detection.
        self.job_states.entry(job_name.to_string()).or_insert(JobState::Pending);
    }

    /// Update the known state of a job.
    pub fn update_job_state(&mut self, job_name: &str, state: JobState) {
        debug!(job = job_name, state = %state, "DependencyResolver: updating job state");
        self.job_states.insert(job_name.to_string(), state);
    }

    /// Check whether all dependencies for `job_name` are satisfied.
    ///
    /// Returns:
    /// - `Satisfied` when every dependency condition is met.
    /// - `Waiting` when one or more dependencies have not yet reached a
    ///   terminal state that satisfies the condition.
    /// - `Failed` when a dependency has reached a state that permanently
    ///   prevents the condition from ever being met.
    pub fn check_dependencies(&self, job_name: &str) -> DependencyStatus {
        let deps = match self.dependencies.get(job_name) {
            Some(d) => d,
            None => {
                // Unknown job — treat as no dependencies registered.
                return DependencyStatus::Satisfied;
            }
        };

        if deps.is_empty() {
            return DependencyStatus::Satisfied;
        }

        let mut pending_deps: Vec<String> = Vec::new();

        for dep in deps {
            let dep_state = self.job_states.get(&dep.job);

            match &dep.dep_type {
                DependencyType::AfterOk => {
                    match dep_state {
                        Some(JobState::Succeeded) => {
                            // Satisfied — continue.
                        }
                        Some(JobState::Failed)
                        | Some(JobState::Cancelled)
                        | Some(JobState::WalltimeExceeded) => {
                            return DependencyStatus::Failed {
                                reason: format!(
                                    "dependency '{}' (afterOk) ended with state '{}', \
                                     which cannot satisfy the condition",
                                    dep.job,
                                    dep_state.unwrap()
                                ),
                            };
                        }
                        _ => {
                            // Pending / Scheduling / Running / unknown
                            pending_deps.push(dep.job.clone());
                        }
                    }
                }

                DependencyType::AfterAny => {
                    match dep_state {
                        Some(JobState::Succeeded)
                        | Some(JobState::Failed)
                        | Some(JobState::Cancelled)
                        | Some(JobState::WalltimeExceeded) => {
                            // Any terminal state satisfies AfterAny.
                        }
                        _ => {
                            pending_deps.push(dep.job.clone());
                        }
                    }
                }

                DependencyType::AfterNotOk => {
                    match dep_state {
                        Some(JobState::Failed)
                        | Some(JobState::Cancelled)
                        | Some(JobState::WalltimeExceeded) => {
                            // Satisfied.
                        }
                        Some(JobState::Succeeded) => {
                            return DependencyStatus::Failed {
                                reason: format!(
                                    "dependency '{}' (afterNotOk) succeeded, \
                                     which cannot satisfy the condition",
                                    dep.job
                                ),
                            };
                        }
                        _ => {
                            pending_deps.push(dep.job.clone());
                        }
                    }
                }
            }
        }

        if pending_deps.is_empty() {
            DependencyStatus::Satisfied
        } else {
            DependencyStatus::Waiting { pending_deps }
        }
    }

    /// Validate that the registered dependency graph contains no cycles.
    ///
    /// Uses Kahn's algorithm (BFS topological sort). Returns
    /// `Err(BuboError::ValidationError)` if a cycle is detected, including
    /// the names of the jobs involved.
    pub fn validate_no_cycles(&self) -> Result<(), BuboError> {
        // Build adjacency list: dep.job -> job_name (dep.job must come before job_name)
        // In-degree counts how many dependencies each job has.
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();

        // Initialise every known job with in-degree 0.
        for job in self.dependencies.keys() {
            in_degree.entry(job.as_str()).or_insert(0);
        }
        for state_job in self.job_states.keys() {
            in_degree.entry(state_job.as_str()).or_insert(0);
        }

        for (job_name, deps) in &self.dependencies {
            for dep in deps {
                // Edge: dep.job -> job_name
                in_degree.entry(dep.job.as_str()).or_insert(0);
                *in_degree.entry(job_name.as_str()).or_insert(0) += 1;
                adj.entry(dep.job.as_str()).or_default().push(job_name.as_str());
            }
        }

        let mut queue: VecDeque<&str> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&job, _)| job)
            .collect();

        let mut visited_count = 0usize;

        while let Some(job) = queue.pop_front() {
            visited_count += 1;
            if let Some(dependents) = adj.get(job) {
                for &dependent in dependents {
                    let deg = in_degree.get_mut(dependent).expect("must exist");
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(dependent);
                    }
                }
            }
        }

        let total_nodes = in_degree.len();
        if visited_count < total_nodes {
            // Collect the jobs still in a cycle (non-zero in-degree).
            let cycle_jobs: Vec<&str> = in_degree
                .iter()
                .filter(|(_, &deg)| deg > 0)
                .map(|(&job, _)| job)
                .collect();
            return Err(BuboError::ValidationError {
                reason: format!(
                    "dependency cycle detected among jobs: [{}]",
                    cycle_jobs.join(", ")
                ),
            });
        }

        Ok(())
    }

    /// Return all registered jobs whose dependencies are currently `Satisfied`.
    pub fn ready_jobs(&self) -> Vec<String> {
        self.dependencies
            .keys()
            .filter(|job| self.check_dependencies(job) == DependencyStatus::Satisfied)
            .cloned()
            .collect()
    }

    /// Remove a job from tracking (call after the job has completed and all
    /// dependent jobs have been updated).
    pub fn remove_job(&mut self, job_name: &str) {
        debug!(job = job_name, "DependencyResolver: removing job");
        self.dependencies.remove(job_name);
        self.job_states.remove(job_name);
    }
}

impl Default for DependencyResolver {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// JobArraySpec / ExpandedJob
// ---------------------------------------------------------------------------

/// Specification for a job array.
///
/// Supported string formats:
/// - `"0-99"`         → indices 0..=99, step 1
/// - `"1-10:2"`       → indices 1, 3, 5, 7, 9, step 2
/// - `"0-99%5"`       → indices 0..=99, step 1, max 5 concurrent
/// - `"1-10:2%3"`     → step and concurrency combined
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobArraySpec {
    pub start: u32,
    /// Inclusive end index.
    pub end: u32,
    /// Step between indices (default 1).
    pub step: u32,
    /// Maximum simultaneously running tasks (None means unlimited).
    pub max_concurrent: Option<u32>,
}

/// A single expanded task from a job array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedJob {
    /// Generated job name, e.g. `"my-job_3"` for array index 3.
    pub name: String,
    /// Array index for this task.
    pub array_index: u32,
    /// Name of the parent array job.
    pub parent_name: String,
}

impl JobArraySpec {
    /// Parse a job array specification string.
    ///
    /// Grammar (informal):
    /// ```text
    /// spec      = range ["%" max_conc]
    /// range     = start "-" end [":" step]
    /// ```
    pub fn parse(input: &str) -> Result<Self, BuboError> {
        let input = input.trim();

        // Split off optional `%max_concurrent` suffix.
        let (range_part, max_concurrent) = if let Some(pos) = input.find('%') {
            let conc_str = &input[pos + 1..];
            let conc: u32 =
                conc_str.parse().map_err(|_| BuboError::ValidationError {
                    reason: format!(
                        "invalid max_concurrent value '{}' in job array spec '{}'",
                        conc_str, input
                    ),
                })?;
            if conc == 0 {
                return Err(BuboError::ValidationError {
                    reason: format!(
                        "max_concurrent must be >= 1 in job array spec '{}'",
                        input
                    ),
                });
            }
            (&input[..pos], Some(conc))
        } else {
            (input, None)
        };

        // Split off optional `:step` suffix from the range.
        let (bounds_part, step) = if let Some(pos) = range_part.find(':') {
            let step_str = &range_part[pos + 1..];
            let step: u32 =
                step_str.parse().map_err(|_| BuboError::ValidationError {
                    reason: format!(
                        "invalid step value '{}' in job array spec '{}'",
                        step_str, input
                    ),
                })?;
            if step == 0 {
                return Err(BuboError::ValidationError {
                    reason: format!("step must be >= 1 in job array spec '{}'", input),
                });
            }
            (&range_part[..pos], step)
        } else {
            (range_part, 1u32)
        };

        // Parse `start-end`.
        let dash_pos = bounds_part.find('-').ok_or_else(|| BuboError::ValidationError {
            reason: format!(
                "missing '-' separator in job array spec '{}'; expected 'start-end'",
                input
            ),
        })?;

        let start_str = &bounds_part[..dash_pos];
        let end_str = &bounds_part[dash_pos + 1..];

        let start: u32 = start_str.parse().map_err(|_| BuboError::ValidationError {
            reason: format!(
                "invalid start index '{}' in job array spec '{}'",
                start_str, input
            ),
        })?;

        let end: u32 = end_str.parse().map_err(|_| BuboError::ValidationError {
            reason: format!(
                "invalid end index '{}' in job array spec '{}'",
                end_str, input
            ),
        })?;

        if end < start {
            return Err(BuboError::ValidationError {
                reason: format!(
                    "end index {} < start index {} in job array spec '{}'",
                    end, start, input
                ),
            });
        }

        Ok(Self { start, end, step, max_concurrent })
    }

    /// Expand the array spec into individual `ExpandedJob` entries.
    pub fn expand(&self, parent_job_name: &str) -> Vec<ExpandedJob> {
        let mut jobs = Vec::new();
        let mut index = self.start;
        while index <= self.end {
            jobs.push(ExpandedJob {
                name: format!("{}_{}", parent_job_name, index),
                array_index: index,
                parent_name: parent_job_name.to_string(),
            });
            index = match index.checked_add(self.step) {
                Some(next) => next,
                None => break, // overflow guard
            };
        }
        jobs
    }

    /// Number of tasks in this array.
    pub fn task_count(&self) -> u32 {
        if self.end < self.start {
            return 0;
        }
        (self.end - self.start) / self.step + 1
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    // ------------------------------------------------------------------
    // Helper
    // ------------------------------------------------------------------

    fn dep(dep_type: DependencyType, job: &str) -> JobDependency {
        JobDependency { dep_type, job: job.to_string() }
    }

    // ------------------------------------------------------------------
    // DependencyResolver — registration and basic checks
    // ------------------------------------------------------------------

    #[test]
    fn test_no_deps_is_satisfied() {
        let mut resolver = DependencyResolver::new();
        resolver.register_job("job-a", vec![]);
        assert_eq!(resolver.check_dependencies("job-a"), DependencyStatus::Satisfied);
    }

    #[test]
    fn test_unknown_job_is_satisfied() {
        let resolver = DependencyResolver::new();
        // A job that was never registered has no deps → Satisfied.
        assert_eq!(resolver.check_dependencies("ghost"), DependencyStatus::Satisfied);
    }

    // ------------------------------------------------------------------
    // AfterOk
    // ------------------------------------------------------------------

    #[test]
    fn test_after_ok_waits_while_pending() {
        let mut resolver = DependencyResolver::new();
        resolver.register_job("dep-job", vec![]);
        resolver.update_job_state("dep-job", JobState::Pending);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterOk, "dep-job")]);

        assert_eq!(
            resolver.check_dependencies("my-job"),
            DependencyStatus::Waiting { pending_deps: vec!["dep-job".to_string()] }
        );
    }

    #[test]
    fn test_after_ok_satisfied_on_succeeded() {
        let mut resolver = DependencyResolver::new();
        resolver.register_job("dep-job", vec![]);
        resolver.update_job_state("dep-job", JobState::Succeeded);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterOk, "dep-job")]);

        assert_eq!(resolver.check_dependencies("my-job"), DependencyStatus::Satisfied);
    }

    #[test]
    fn test_after_ok_fails_when_dep_failed() {
        let mut resolver = DependencyResolver::new();
        resolver.register_job("dep-job", vec![]);
        resolver.update_job_state("dep-job", JobState::Failed);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterOk, "dep-job")]);

        match resolver.check_dependencies("my-job") {
            DependencyStatus::Failed { reason } => {
                assert!(reason.contains("dep-job"));
                assert!(reason.contains("afterOk"));
            }
            other => panic!("expected Failed, got {:?}", other),
        }
    }

    #[test]
    fn test_after_ok_fails_when_dep_cancelled() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("dep-job", JobState::Cancelled);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterOk, "dep-job")]);

        assert!(matches!(resolver.check_dependencies("my-job"), DependencyStatus::Failed { .. }));
    }

    #[test]
    fn test_after_ok_fails_when_dep_walltime_exceeded() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("dep-job", JobState::WalltimeExceeded);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterOk, "dep-job")]);

        assert!(matches!(resolver.check_dependencies("my-job"), DependencyStatus::Failed { .. }));
    }

    // ------------------------------------------------------------------
    // AfterAny
    // ------------------------------------------------------------------

    #[test]
    fn test_after_any_satisfied_on_succeeded() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("dep-job", JobState::Succeeded);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterAny, "dep-job")]);

        assert_eq!(resolver.check_dependencies("my-job"), DependencyStatus::Satisfied);
    }

    #[test]
    fn test_after_any_satisfied_on_failed() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("dep-job", JobState::Failed);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterAny, "dep-job")]);

        assert_eq!(resolver.check_dependencies("my-job"), DependencyStatus::Satisfied);
    }

    #[test]
    fn test_after_any_satisfied_on_cancelled() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("dep-job", JobState::Cancelled);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterAny, "dep-job")]);

        assert_eq!(resolver.check_dependencies("my-job"), DependencyStatus::Satisfied);
    }

    #[test]
    fn test_after_any_waits_while_running() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("dep-job", JobState::Running);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterAny, "dep-job")]);

        assert!(matches!(
            resolver.check_dependencies("my-job"),
            DependencyStatus::Waiting { .. }
        ));
    }

    // ------------------------------------------------------------------
    // AfterNotOk
    // ------------------------------------------------------------------

    #[test]
    fn test_after_not_ok_satisfied_on_failed() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("dep-job", JobState::Failed);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterNotOk, "dep-job")]);

        assert_eq!(resolver.check_dependencies("my-job"), DependencyStatus::Satisfied);
    }

    #[test]
    fn test_after_not_ok_fails_when_dep_succeeded() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("dep-job", JobState::Succeeded);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterNotOk, "dep-job")]);

        match resolver.check_dependencies("my-job") {
            DependencyStatus::Failed { reason } => {
                assert!(reason.contains("dep-job"));
                assert!(reason.contains("afterNotOk"));
            }
            other => panic!("expected Failed, got {:?}", other),
        }
    }

    #[test]
    fn test_after_not_ok_waits_while_scheduling() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("dep-job", JobState::Scheduling);
        resolver.register_job("my-job", vec![dep(DependencyType::AfterNotOk, "dep-job")]);

        assert!(matches!(
            resolver.check_dependencies("my-job"),
            DependencyStatus::Waiting { .. }
        ));
    }

    // ------------------------------------------------------------------
    // Multiple dependencies
    // ------------------------------------------------------------------

    #[test]
    fn test_multiple_deps_all_satisfied() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("a", JobState::Succeeded);
        resolver.update_job_state("b", JobState::Failed);
        resolver.register_job(
            "c",
            vec![
                dep(DependencyType::AfterOk, "a"),
                dep(DependencyType::AfterNotOk, "b"),
            ],
        );

        assert_eq!(resolver.check_dependencies("c"), DependencyStatus::Satisfied);
    }

    #[test]
    fn test_multiple_deps_one_pending() {
        let mut resolver = DependencyResolver::new();
        resolver.update_job_state("a", JobState::Succeeded);
        resolver.update_job_state("b", JobState::Running);
        resolver.register_job(
            "c",
            vec![
                dep(DependencyType::AfterOk, "a"),
                dep(DependencyType::AfterAny, "b"),
            ],
        );

        match resolver.check_dependencies("c") {
            DependencyStatus::Waiting { pending_deps } => {
                assert_eq!(pending_deps, vec!["b".to_string()]);
            }
            other => panic!("expected Waiting, got {:?}", other),
        }
    }

    // ------------------------------------------------------------------
    // Unknown dependency job (state not yet set)
    // ------------------------------------------------------------------

    #[test]
    fn test_unknown_dep_job_treated_as_waiting() {
        let mut resolver = DependencyResolver::new();
        // "dep-job" is never registered or updated — no state entry.
        resolver.register_job("my-job", vec![dep(DependencyType::AfterOk, "dep-job")]);

        assert!(matches!(
            resolver.check_dependencies("my-job"),
            DependencyStatus::Waiting { .. }
        ));
    }

    // ------------------------------------------------------------------
    // ready_jobs
    // ------------------------------------------------------------------

    #[test]
    fn test_ready_jobs() {
        let mut resolver = DependencyResolver::new();
        resolver.register_job("no-deps", vec![]);
        resolver.update_job_state("a", JobState::Succeeded);
        resolver.register_job("after-a", vec![dep(DependencyType::AfterOk, "a")]);
        resolver.update_job_state("b", JobState::Running);
        resolver.register_job("after-b", vec![dep(DependencyType::AfterOk, "b")]);

        let mut ready = resolver.ready_jobs();
        ready.sort();
        assert_eq!(ready, vec!["after-a".to_string(), "no-deps".to_string()]);
    }

    // ------------------------------------------------------------------
    // Cycle detection
    // ------------------------------------------------------------------

    #[test]
    fn test_no_cycle_linear_chain() {
        let mut resolver = DependencyResolver::new();
        resolver.register_job("a", vec![]);
        resolver.register_job("b", vec![dep(DependencyType::AfterOk, "a")]);
        resolver.register_job("c", vec![dep(DependencyType::AfterOk, "b")]);

        assert!(resolver.validate_no_cycles().is_ok());
    }

    #[test]
    fn test_cycle_direct() {
        let mut resolver = DependencyResolver::new();
        // a depends on b AND b depends on a → cycle.
        resolver.register_job("a", vec![dep(DependencyType::AfterOk, "b")]);
        resolver.register_job("b", vec![dep(DependencyType::AfterOk, "a")]);

        let result = resolver.validate_no_cycles();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("cycle"), "expected 'cycle' in: {}", err_msg);
    }

    #[test]
    fn test_self_dependency_is_cycle() {
        let mut resolver = DependencyResolver::new();
        resolver.register_job("a", vec![dep(DependencyType::AfterOk, "a")]);

        assert!(resolver.validate_no_cycles().is_err());
    }

    #[test]
    fn test_cycle_three_nodes() {
        let mut resolver = DependencyResolver::new();
        resolver.register_job("a", vec![dep(DependencyType::AfterOk, "c")]);
        resolver.register_job("b", vec![dep(DependencyType::AfterOk, "a")]);
        resolver.register_job("c", vec![dep(DependencyType::AfterOk, "b")]);

        assert!(resolver.validate_no_cycles().is_err());
    }

    // ------------------------------------------------------------------
    // remove_job
    // ------------------------------------------------------------------

    #[test]
    fn test_remove_job() {
        let mut resolver = DependencyResolver::new();
        resolver.register_job("a", vec![]);
        resolver.update_job_state("a", JobState::Succeeded);
        resolver.register_job("b", vec![dep(DependencyType::AfterOk, "a")]);

        // b is ready because a succeeded.
        assert_eq!(resolver.check_dependencies("b"), DependencyStatus::Satisfied);

        resolver.remove_job("a");

        // After removal a's state is gone; b now sees unknown dep → Waiting.
        assert!(matches!(
            resolver.check_dependencies("b"),
            DependencyStatus::Waiting { .. }
        ));
    }

    // ------------------------------------------------------------------
    // JobArraySpec — parsing
    // ------------------------------------------------------------------

    #[test]
    fn test_parse_simple_range() {
        let spec = JobArraySpec::parse("0-99").unwrap();
        assert_eq!(spec.start, 0);
        assert_eq!(spec.end, 99);
        assert_eq!(spec.step, 1);
        assert_eq!(spec.max_concurrent, None);
    }

    #[test]
    fn test_parse_range_with_step() {
        let spec = JobArraySpec::parse("1-10:2").unwrap();
        assert_eq!(spec.start, 1);
        assert_eq!(spec.end, 10);
        assert_eq!(spec.step, 2);
        assert_eq!(spec.max_concurrent, None);
    }

    #[test]
    fn test_parse_range_with_max_concurrent() {
        let spec = JobArraySpec::parse("0-99%5").unwrap();
        assert_eq!(spec.start, 0);
        assert_eq!(spec.end, 99);
        assert_eq!(spec.step, 1);
        assert_eq!(spec.max_concurrent, Some(5));
    }

    #[test]
    fn test_parse_range_with_step_and_max_concurrent() {
        let spec = JobArraySpec::parse("1-10:2%3").unwrap();
        assert_eq!(spec.start, 1);
        assert_eq!(spec.end, 10);
        assert_eq!(spec.step, 2);
        assert_eq!(spec.max_concurrent, Some(3));
    }

    #[test]
    fn test_parse_error_missing_dash() {
        assert!(JobArraySpec::parse("099").is_err());
    }

    #[test]
    fn test_parse_error_end_before_start() {
        assert!(JobArraySpec::parse("10-5").is_err());
    }

    #[test]
    fn test_parse_error_zero_step() {
        assert!(JobArraySpec::parse("0-9:0").is_err());
    }

    #[test]
    fn test_parse_error_zero_max_concurrent() {
        assert!(JobArraySpec::parse("0-9%0").is_err());
    }

    // ------------------------------------------------------------------
    // JobArraySpec — task_count and expand
    // ------------------------------------------------------------------

    #[test]
    fn test_task_count_simple() {
        let spec = JobArraySpec::parse("0-99").unwrap();
        assert_eq!(spec.task_count(), 100);
    }

    #[test]
    fn test_task_count_with_step() {
        // 1, 3, 5, 7, 9 → 5 tasks
        let spec = JobArraySpec::parse("1-9:2").unwrap();
        assert_eq!(spec.task_count(), 5);
    }

    #[test]
    fn test_task_count_single_element() {
        let spec = JobArraySpec::parse("5-5").unwrap();
        assert_eq!(spec.task_count(), 1);
    }

    #[test]
    fn test_expand_simple() {
        let spec = JobArraySpec::parse("0-2").unwrap();
        let jobs = spec.expand("my-job");
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0], ExpandedJob { name: "my-job_0".to_string(), array_index: 0, parent_name: "my-job".to_string() });
        assert_eq!(jobs[1], ExpandedJob { name: "my-job_1".to_string(), array_index: 1, parent_name: "my-job".to_string() });
        assert_eq!(jobs[2], ExpandedJob { name: "my-job_2".to_string(), array_index: 2, parent_name: "my-job".to_string() });
    }

    #[test]
    fn test_expand_with_step() {
        let spec = JobArraySpec::parse("1-9:2").unwrap();
        let jobs = spec.expand("sim");
        let indices: Vec<u32> = jobs.iter().map(|j| j.array_index).collect();
        assert_eq!(indices, vec![1, 3, 5, 7, 9]);
        assert!(jobs.iter().all(|j| j.parent_name == "sim"));
    }

    #[test]
    fn test_expand_names_formatted_correctly() {
        let spec = JobArraySpec::parse("0-99").unwrap();
        let jobs = spec.expand("wave-sim");
        assert_eq!(jobs[0].name, "wave-sim_0");
        assert_eq!(jobs[99].name, "wave-sim_99");
    }

    #[test]
    fn test_expand_preserves_max_concurrent_in_spec() {
        // max_concurrent is stored on the spec but not applied during expand —
        // the scheduler layer enforces it. Verify the field round-trips.
        let spec = JobArraySpec::parse("0-9%3").unwrap();
        assert_eq!(spec.max_concurrent, Some(3));
        let jobs = spec.expand("job");
        // All 10 are expanded; the scheduler decides how many to submit at once.
        assert_eq!(jobs.len(), 10);
    }

    // ------------------------------------------------------------------
    // Permanent failure short-circuit (first failure wins)
    // ------------------------------------------------------------------

    #[test]
    fn test_failed_dep_short_circuits_before_waiting_dep() {
        let mut resolver = DependencyResolver::new();
        // a failed (AfterOk → permanent failure) and b is still running (pending).
        resolver.update_job_state("a", JobState::Failed);
        resolver.update_job_state("b", JobState::Running);
        resolver.register_job(
            "c",
            vec![
                dep(DependencyType::AfterOk, "a"), // permanent failure
                dep(DependencyType::AfterAny, "b"), // would be waiting
            ],
        );

        // The first dep causes an immediate Failed return.
        assert!(matches!(resolver.check_dependencies("c"), DependencyStatus::Failed { .. }));
    }

    // ------------------------------------------------------------------
    // HashSet deduplication in ready_jobs (no duplicates from HashMap iter)
    // ------------------------------------------------------------------

    #[test]
    fn test_ready_jobs_no_duplicates() {
        let mut resolver = DependencyResolver::new();
        resolver.register_job("j1", vec![]);
        resolver.register_job("j2", vec![]);

        let ready = resolver.ready_jobs();
        let unique: HashSet<_> = ready.iter().cloned().collect();
        assert_eq!(ready.len(), unique.len());
    }
}
