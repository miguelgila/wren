use std::collections::HashMap;

use wren_core::QueuedJob;
use chrono::{DateTime, Duration, Utc};
use tracing::debug;

/// Tracks per-user/project resource usage and computes fair-share factors.
pub struct FairShareManager {
    /// Per-user usage records keyed by user identifier (namespace).
    usage: HashMap<String, UsageRecord>,
    /// Half-life for exponential decay of historical usage.
    decay_half_life: Duration,
    /// Weights for the multi-factor priority calculation.
    weights: FairShareWeights,
}

/// Accumulated resource usage for a single user.
pub struct UsageRecord {
    /// Total node-seconds consumed (decayed over time).
    pub node_seconds: f64,
    /// Total GPU-seconds consumed (decayed over time).
    pub gpu_seconds: f64,
    /// Last time usage was updated (used to compute decay intervals).
    pub last_updated: DateTime<Utc>,
}

/// Weights for the multi-factor effective priority formula.
///
/// Each weight controls how strongly the corresponding factor influences the
/// final effective priority.  They do not have to sum to 1.0.
#[derive(Clone, Debug)]
pub struct FairShareWeights {
    /// Weight applied to the job-age factor (0.0–1.0 recommended).
    pub age: f64,
    /// Weight applied to the job-size factor (0.0–1.0 recommended).
    pub size: f64,
    /// Weight applied to the fair-share factor (0.0–1.0 recommended).
    pub fair_share: f64,
}

impl FairShareWeights {
    /// Equal weighting across all three factors.
    pub fn equal() -> Self {
        Self {
            age: 0.333,
            size: 0.333,
            fair_share: 0.333,
        }
    }
}

impl FairShareManager {
    /// Creates a new `FairShareManager`.
    ///
    /// * `decay_half_life` — how quickly historical usage decays.  A half-life
    ///   of 7 days means a user's usage from 7 days ago counts half as much now.
    /// * `weights` — relative importance of age, size, and fair-share factors.
    pub fn new(decay_half_life: Duration, weights: FairShareWeights) -> Self {
        Self {
            usage: HashMap::new(),
            decay_half_life,
            weights,
        }
    }

    /// Record that a user consumed resources.
    ///
    /// Should be called when a job finishes.  The usage is added directly
    /// (before the next decay pass).
    pub fn record_usage(&mut self, user: &str, node_seconds: f64, gpu_seconds: f64) {
        debug!(user, node_seconds, gpu_seconds, "recording usage");
        let record = self.usage.entry(user.to_string()).or_insert_with(|| UsageRecord {
            node_seconds: 0.0,
            gpu_seconds: 0.0,
            last_updated: Utc::now(),
        });
        record.node_seconds += node_seconds;
        record.gpu_seconds += gpu_seconds;
        record.last_updated = Utc::now();
    }

    /// Apply exponential decay to all usage records.
    ///
    /// Uses the formula:
    /// ```text
    /// decayed = current * 2^(-elapsed / half_life)
    /// ```
    ///
    /// Call this periodically (e.g., once per scheduling cycle) or immediately
    /// before computing priorities to get an up-to-date picture.
    pub fn decay_usage(&mut self) {
        let now = Utc::now();
        let half_life_secs = self.decay_half_life.num_seconds() as f64;

        for (user, record) in self.usage.iter_mut() {
            let elapsed_secs = (now - record.last_updated).num_seconds() as f64;
            if elapsed_secs <= 0.0 {
                continue;
            }
            let factor = (-elapsed_secs / half_life_secs * std::f64::consts::LN_2).exp();
            debug!(
                user,
                elapsed_secs,
                factor,
                before_node = record.node_seconds,
                before_gpu = record.gpu_seconds,
                "decaying usage"
            );
            record.node_seconds *= factor;
            record.gpu_seconds *= factor;
            record.last_updated = now;
        }
    }

    /// Compute the fair-share factor for a user.
    ///
    /// Returns a value in `[-1.0, 1.0]`:
    /// * Positive: user has consumed less than the average — deserves a boost.
    /// * Negative: user has consumed more than the average — should be penalised.
    /// * `0.0` if there is only one user or no usage history at all.
    pub fn fair_share_factor(&self, user: &str) -> f64 {
        if self.usage.is_empty() {
            return 0.0;
        }

        let total_node_seconds: f64 = self.usage.values().map(|r| r.node_seconds).sum();
        let num_users = self.usage.len() as f64;

        // Equal-share baseline for this user.
        let equal_share = if num_users > 0.0 {
            total_node_seconds / num_users
        } else {
            0.0
        };

        let user_usage = self.usage.get(user).map_or(0.0, |r| r.node_seconds);

        if total_node_seconds == 0.0 {
            // Nobody has used anything; all users are equal.
            return 0.0;
        }

        // Normalise: how far from the equal share is this user?
        // Result is clamped to [-1, 1].
        let deviation = (equal_share - user_usage) / total_node_seconds;
        deviation.clamp(-1.0, 1.0)
    }

    /// Compute the age factor for a job (0–1000).
    ///
    /// Scales linearly with wait time, capped at 7 days = 1000.
    fn age_factor(job: &QueuedJob) -> f64 {
        let max_wait_secs = 7.0 * 24.0 * 3600.0; // 7 days
        let waited_secs = (Utc::now() - job.submit_time).num_seconds().max(0) as f64;
        (waited_secs / max_wait_secs * 1000.0).min(1000.0)
    }

    /// Compute the size factor for a job (0–1000).
    ///
    /// Smaller jobs receive a higher factor (inverse of node count), normalised
    /// to [0, 1000].  A single-node job scores 1000; a 64+ node job approaches
    /// 0.
    fn size_factor(job: &QueuedJob) -> f64 {
        // Use a reference scale of 64 nodes for the lower bound.
        let reference_max_nodes = 64.0_f64;
        let nodes = (job.nodes as f64).max(1.0);
        let inverse = reference_max_nodes / nodes;
        // Clamp: single-node job = 1000, 64-node job = ~15.6, >64 is still valid but low.
        (inverse / reference_max_nodes * 1000.0).clamp(0.0, 1000.0)
    }

    /// Compute the effective priority for a job.
    ///
    /// Formula:
    /// ```text
    /// effective = base_priority
    ///           + age_factor    * weight_age
    ///           + size_factor   * weight_size
    ///           + fair_share_factor * 1000 * weight_fair_share
    /// ```
    ///
    /// The fair-share factor is scaled by 1000 so it is on the same order of
    /// magnitude as the age and size factors.
    pub fn effective_priority(&self, job: &QueuedJob) -> f64 {
        let base = job.priority as f64;
        let age = Self::age_factor(job) * self.weights.age;
        let size = Self::size_factor(job) * self.weights.size;
        // fair_share_factor is in [-1, 1]; scale to [-1000, 1000] before weighting.
        let fs = self.fair_share_factor(&job.namespace) * 1000.0 * self.weights.fair_share;

        let effective = base + age + size + fs;
        debug!(
            job = %job.name,
            base,
            age,
            size,
            fair_share = fs,
            effective,
            "computed effective priority"
        );
        effective
    }

    /// Sort a slice of jobs by effective priority, highest first.
    pub fn sort_by_effective_priority(&self, jobs: &mut [QueuedJob]) {
        jobs.sort_by(|a, b| {
            let pa = self.effective_priority(a);
            let pb = self.effective_priority(b);
            pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    /// Returns a usage summary for all tracked users.
    ///
    /// Each entry is `(user, node_seconds, gpu_seconds)`.
    pub fn usage_summary(&self) -> Vec<(String, f64, f64)> {
        let mut summary: Vec<(String, f64, f64)> = self
            .usage
            .iter()
            .map(|(user, record)| (user.clone(), record.node_seconds, record.gpu_seconds))
            .collect();
        summary.sort_by(|a, b| a.0.cmp(&b.0));
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_job(name: &str, namespace: &str, priority: i32, nodes: u32, waited_secs: i64) -> QueuedJob {
        QueuedJob {
            name: name.to_string(),
            namespace: namespace.to_string(),
            queue: "default".to_string(),
            priority,
            nodes,
            tasks_per_node: 1,
            cpu_per_node_millis: 1000,
            memory_per_node_bytes: 1_000_000_000,
            gpus_per_node: 0,
            walltime: None,
            submit_time: Utc::now() - Duration::seconds(waited_secs),
        }
    }

    fn default_weights() -> FairShareWeights {
        FairShareWeights {
            age: 0.25,
            size: 0.25,
            fair_share: 0.50,
        }
    }

    fn manager() -> FairShareManager {
        FairShareManager::new(Duration::days(7), default_weights())
    }

    // --- Basic usage recording ---

    #[test]
    fn test_record_usage_adds_to_empty() {
        let mut mgr = manager();
        mgr.record_usage("alice", 3600.0, 0.0);

        let summary = mgr.usage_summary();
        assert_eq!(summary.len(), 1);
        let (user, ns, gs) = &summary[0];
        assert_eq!(user, "alice");
        assert!((ns - 3600.0).abs() < 1e-6);
        assert!((gs - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_record_usage_accumulates() {
        let mut mgr = manager();
        mgr.record_usage("alice", 1000.0, 500.0);
        mgr.record_usage("alice", 2000.0, 250.0);

        let summary = mgr.usage_summary();
        let (_, ns, gs) = &summary[0];
        assert!((ns - 3000.0).abs() < 1e-6);
        assert!((gs - 750.0).abs() < 1e-6);
    }

    #[test]
    fn test_record_usage_multiple_users() {
        let mut mgr = manager();
        mgr.record_usage("alice", 1000.0, 0.0);
        mgr.record_usage("bob", 2000.0, 100.0);

        let summary = mgr.usage_summary();
        assert_eq!(summary.len(), 2);
        // summary is sorted by name
        assert_eq!(summary[0].0, "alice");
        assert_eq!(summary[1].0, "bob");
    }

    // --- Exponential decay ---

    #[test]
    fn test_decay_half_life_exact() {
        // Manually set last_updated to exactly one half-life ago.
        let half_life = Duration::days(7);
        let mut mgr = FairShareManager::new(half_life, default_weights());
        mgr.record_usage("alice", 1000.0, 0.0);

        // Backdating last_updated by exactly one half-life.
        let record = mgr.usage.get_mut("alice").unwrap();
        record.last_updated = Utc::now() - half_life;

        mgr.decay_usage();

        let (_, ns, _) = &mgr.usage_summary()[0];
        // After one half-life, usage should be ~500.
        assert!((ns - 500.0).abs() < 1.0, "expected ~500 after one half-life, got {ns}");
    }

    #[test]
    fn test_decay_two_half_lives() {
        let half_life = Duration::days(7);
        let mut mgr = FairShareManager::new(half_life, default_weights());
        mgr.record_usage("alice", 1000.0, 0.0);

        let record = mgr.usage.get_mut("alice").unwrap();
        record.last_updated = Utc::now() - Duration::days(14);

        mgr.decay_usage();

        let (_, ns, _) = &mgr.usage_summary()[0];
        // After two half-lives, usage should be ~250.
        assert!((ns - 250.0).abs() < 1.0, "expected ~250 after two half-lives, got {ns}");
    }

    #[test]
    fn test_decay_zero_elapsed_no_change() {
        let mut mgr = manager();
        mgr.record_usage("alice", 500.0, 0.0);
        // Don't backdate — last_updated is now, so elapsed < 1s.
        mgr.decay_usage();

        let (_, ns, _) = &mgr.usage_summary()[0];
        // Should be effectively unchanged (tiny sub-second decay is negligible).
        assert!(*ns > 499.0);
    }

    // --- Fair-share factor ---

    #[test]
    fn test_fair_share_factor_no_usage_history() {
        let mgr = manager();
        // No usage recorded at all — factor should be 0.
        assert_eq!(mgr.fair_share_factor("alice"), 0.0);
    }

    #[test]
    fn test_fair_share_factor_equal_users() {
        let mut mgr = manager();
        mgr.record_usage("alice", 1000.0, 0.0);
        mgr.record_usage("bob", 1000.0, 0.0);

        // Both users consumed exactly equal share — factor should be 0.
        let fa = mgr.fair_share_factor("alice");
        let fb = mgr.fair_share_factor("bob");
        assert!(fa.abs() < 1e-6, "alice factor should be ~0, got {fa}");
        assert!(fb.abs() < 1e-6, "bob factor should be ~0, got {fb}");
    }

    #[test]
    fn test_fair_share_factor_heavy_user_penalised() {
        let mut mgr = manager();
        mgr.record_usage("alice", 100.0, 0.0);   // light user
        mgr.record_usage("bob", 900.0, 0.0);     // heavy user

        let fa = mgr.fair_share_factor("alice");
        let fb = mgr.fair_share_factor("bob");

        // alice used less than average → positive factor
        assert!(fa > 0.0, "alice (light user) should have positive factor, got {fa}");
        // bob used more than average → negative factor
        assert!(fb < 0.0, "bob (heavy user) should have negative factor, got {fb}");
    }

    #[test]
    fn test_fair_share_factor_unknown_user_treated_as_zero_usage() {
        let mut mgr = manager();
        mgr.record_usage("alice", 1000.0, 0.0);

        // "charlie" has no history — treated as 0 usage, equal share = 500.
        // charlie used 0 vs equal share 500, over total 1000 → deviation = (500-0)/1000 = 0.5
        let fc = mgr.fair_share_factor("charlie");
        assert!(fc > 0.0, "unknown user should get a positive fair-share factor");
    }

    // --- Effective priority ---

    #[test]
    fn test_effective_priority_base_only_with_zero_weights() {
        let mgr = FairShareManager::new(
            Duration::days(7),
            FairShareWeights { age: 0.0, size: 0.0, fair_share: 0.0 },
        );
        let job = make_job("j", "alice", 100, 1, 0);
        let ep = mgr.effective_priority(&job);
        // With zero weights, effective_priority equals base priority.
        assert!((ep - 100.0).abs() < 1.0, "expected ~100, got {ep}");
    }

    #[test]
    fn test_effective_priority_higher_for_light_user() {
        let mut mgr = manager();
        mgr.record_usage("heavy", 900.0, 0.0);
        mgr.record_usage("light", 100.0, 0.0);

        let job_heavy = make_job("jh", "heavy", 50, 1, 0);
        let job_light = make_job("jl", "light", 50, 1, 0);

        let ep_heavy = mgr.effective_priority(&job_heavy);
        let ep_light = mgr.effective_priority(&job_light);

        assert!(
            ep_light > ep_heavy,
            "light user job ({ep_light}) should have higher effective priority than heavy user job ({ep_heavy})"
        );
    }

    // --- Age factor ---

    #[test]
    fn test_age_factor_fresh_job_near_zero() {
        let job = make_job("j", "alice", 50, 1, 0); // submitted just now
        let age = FairShareManager::age_factor(&job);
        assert!(age < 1.0, "brand-new job should have near-zero age factor, got {age}");
    }

    #[test]
    fn test_age_factor_week_old_job_at_max() {
        let job = make_job("j", "alice", 50, 1, 7 * 24 * 3600); // 7 days ago
        let age = FairShareManager::age_factor(&job);
        assert!((age - 1000.0).abs() < 1.0, "7-day-old job should have age factor ~1000, got {age}");
    }

    #[test]
    fn test_age_factor_capped_at_1000() {
        let job = make_job("j", "alice", 50, 1, 30 * 24 * 3600); // 30 days ago
        let age = FairShareManager::age_factor(&job);
        assert!((age - 1000.0).abs() < 1.0, "age factor should be capped at 1000, got {age}");
    }

    // --- Size factor ---

    #[test]
    fn test_size_factor_single_node_at_max() {
        let job = make_job("j", "alice", 50, 1, 0);
        let sf = FairShareManager::size_factor(&job);
        assert!((sf - 1000.0).abs() < 1.0, "1-node job should have size factor 1000, got {sf}");
    }

    #[test]
    fn test_size_factor_decreases_with_node_count() {
        let small = make_job("s", "alice", 50, 4, 0);
        let large = make_job("l", "alice", 50, 32, 0);
        let sf_small = FairShareManager::size_factor(&small);
        let sf_large = FairShareManager::size_factor(&large);
        assert!(
            sf_small > sf_large,
            "smaller job ({sf_small}) should have higher size factor than larger job ({sf_large})"
        );
    }

    // --- Sort by effective priority ---

    #[test]
    fn test_sort_by_effective_priority() {
        let mut mgr = manager();
        mgr.record_usage("heavy", 900.0, 0.0);
        mgr.record_usage("light", 100.0, 0.0);

        // weights: age=0.25, size=0.25, fair_share=0.50
        // heavy fs_factor = (500-900)/1000 = -0.4  → fs contribution = -0.4*1000*0.5 = -200
        // light fs_factor = (500-100)/1000 = +0.4  → fs contribution = +0.4*1000*0.5 = +200
        //
        // light-job (base 50):   50 + 0 + 62.5 + 200  = 312.5  ← highest
        // high-prio (base 200):  200 + 0 + 62.5 - 200 = 62.5
        // heavy-job (base 50):   50 + 0 + 62.5 - 200  = -87.5  ← lowest
        let mut jobs = vec![
            make_job("heavy-job", "heavy", 50, 4, 0),
            make_job("light-job", "light", 50, 4, 0),
            make_job("high-prio", "heavy", 200, 4, 0),
        ];

        mgr.sort_by_effective_priority(&mut jobs);

        // light user's fair-share boost lifts it above high-prio despite equal base priority.
        assert_eq!(jobs[0].name, "light-job");
        // high-prio is second: higher base partially offsets the heavy-user penalty.
        assert_eq!(jobs[1].name, "high-prio");
        // heavy-job is last: low base + heavy-user penalty.
        assert_eq!(jobs[2].name, "heavy-job");
    }

    // --- Edge cases ---

    #[test]
    fn test_single_user_fair_share_factor() {
        let mut mgr = manager();
        mgr.record_usage("alice", 5000.0, 100.0);

        // With a single user, equal_share == user_usage, deviation = 0.
        let fa = mgr.fair_share_factor("alice");
        assert!(fa.abs() < 1e-6, "single user should have factor ~0, got {fa}");
    }

    #[test]
    fn test_usage_summary_sorted_alphabetically() {
        let mut mgr = manager();
        mgr.record_usage("charlie", 300.0, 0.0);
        mgr.record_usage("alice", 100.0, 0.0);
        mgr.record_usage("bob", 200.0, 0.0);

        let summary = mgr.usage_summary();
        assert_eq!(summary[0].0, "alice");
        assert_eq!(summary[1].0, "bob");
        assert_eq!(summary[2].0, "charlie");
    }
}
