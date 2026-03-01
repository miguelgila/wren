use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, warn};
use wren_core::Placement;

/// A resource reservation that prevents double-booking during the
/// scheduling → launch window. Reservations expire after a timeout
/// to avoid permanent resource leaks if launch fails silently.
#[derive(Clone, Debug)]
struct Reservation {
    job_name: String,
    placement: Placement,
    cpu_per_node_millis: u64,
    memory_per_node_bytes: u64,
    gpus_per_node: u32,
    created_at: Instant,
}

/// Manages temporary resource reservations during the scheduling → launch phase.
///
/// When the scheduler finds a placement, we reserve resources immediately
/// so that concurrent scheduling attempts don't double-book the same nodes.
/// The reservation is released when:
/// - The job is successfully launched (committed to cluster state)
/// - The reservation expires (timeout, default 60s)
/// - The launch fails (explicitly released)
pub struct ReservationManager {
    reservations: HashMap<String, Reservation>,
    timeout: Duration,
}

impl ReservationManager {
    pub fn new(timeout: Duration) -> Self {
        Self {
            reservations: HashMap::new(),
            timeout,
        }
    }

    /// Create a reservation for a job, blocking those resources.
    pub fn reserve(
        &mut self,
        job_name: &str,
        placement: &Placement,
        cpu_per_node_millis: u64,
        memory_per_node_bytes: u64,
        gpus_per_node: u32,
    ) {
        debug!(job = job_name, nodes = ?placement.nodes, "creating resource reservation");
        self.reservations.insert(
            job_name.to_string(),
            Reservation {
                job_name: job_name.to_string(),
                placement: placement.clone(),
                cpu_per_node_millis,
                memory_per_node_bytes,
                gpus_per_node,
                created_at: Instant::now(),
            },
        );
    }

    /// Release a reservation (job launched successfully or failed).
    pub fn release(&mut self, job_name: &str) -> bool {
        let removed = self.reservations.remove(job_name).is_some();
        if removed {
            debug!(job = job_name, "released resource reservation");
        }
        removed
    }

    /// Get the total reserved resources on a specific node across all active reservations.
    pub fn reserved_on_node(&self, node_name: &str) -> (u64, u64, u32) {
        let mut cpu = 0u64;
        let mut mem = 0u64;
        let mut gpu = 0u32;

        for reservation in self.reservations.values() {
            if reservation.placement.nodes.contains(&node_name.to_string()) {
                cpu += reservation.cpu_per_node_millis;
                mem += reservation.memory_per_node_bytes;
                gpu += reservation.gpus_per_node;
            }
        }

        (cpu, mem, gpu)
    }

    /// Expire stale reservations and return the names of expired jobs.
    pub fn expire_stale(&mut self) -> Vec<String> {
        let now = Instant::now();
        let expired: Vec<String> = self
            .reservations
            .iter()
            .filter(|(_, r)| now.duration_since(r.created_at) > self.timeout)
            .map(|(name, _)| name.clone())
            .collect();

        for name in &expired {
            warn!(job = %name, "reservation expired, releasing resources");
            self.reservations.remove(name);
        }

        expired
    }

    /// Number of active reservations.
    pub fn active_count(&self) -> usize {
        self.reservations.len()
    }

    /// Check if a job has an active reservation.
    pub fn has_reservation(&self, job_name: &str) -> bool {
        self.reservations.contains_key(job_name)
    }

    /// List all reserved job names.
    pub fn reserved_jobs(&self) -> Vec<String> {
        self.reservations.keys().cloned().collect()
    }
}

impl Default for ReservationManager {
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_placement(nodes: &[&str]) -> Placement {
        Placement {
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            score: 1.0,
        }
    }

    #[test]
    fn test_reserve_and_release() {
        let mut mgr = ReservationManager::default();
        let placement = make_placement(&["node-0", "node-1"]);

        mgr.reserve("job-1", &placement, 4000, 8_000_000_000, 2);
        assert_eq!(mgr.active_count(), 1);
        assert!(mgr.has_reservation("job-1"));

        assert!(mgr.release("job-1"));
        assert_eq!(mgr.active_count(), 0);
        assert!(!mgr.has_reservation("job-1"));
    }

    #[test]
    fn test_release_nonexistent() {
        let mut mgr = ReservationManager::default();
        assert!(!mgr.release("nonexistent"));
    }

    #[test]
    fn test_reserved_on_node() {
        let mut mgr = ReservationManager::default();

        mgr.reserve(
            "job-1",
            &make_placement(&["node-0", "node-1"]),
            4000,
            8_000_000_000,
            2,
        );
        mgr.reserve(
            "job-2",
            &make_placement(&["node-0", "node-2"]),
            2000,
            4_000_000_000,
            1,
        );

        // node-0 has reservations from both jobs
        let (cpu, mem, gpu) = mgr.reserved_on_node("node-0");
        assert_eq!(cpu, 6000);
        assert_eq!(mem, 12_000_000_000);
        assert_eq!(gpu, 3);

        // node-1 only from job-1
        let (cpu, mem, gpu) = mgr.reserved_on_node("node-1");
        assert_eq!(cpu, 4000);
        assert_eq!(mem, 8_000_000_000);
        assert_eq!(gpu, 2);

        // node-2 only from job-2
        let (cpu, mem, gpu) = mgr.reserved_on_node("node-2");
        assert_eq!(cpu, 2000);
        assert_eq!(mem, 4_000_000_000);
        assert_eq!(gpu, 1);

        // node-3 has no reservations
        let (cpu, mem, gpu) = mgr.reserved_on_node("node-3");
        assert_eq!(cpu, 0);
        assert_eq!(mem, 0);
        assert_eq!(gpu, 0);
    }

    #[test]
    fn test_expire_stale() {
        let mut mgr = ReservationManager::new(Duration::from_millis(10));
        mgr.reserve(
            "job-1",
            &make_placement(&["node-0"]),
            4000,
            8_000_000_000,
            0,
        );

        // Reservation should be active immediately
        assert_eq!(mgr.active_count(), 1);
        assert!(mgr.expire_stale().is_empty());

        // Wait for expiration
        std::thread::sleep(Duration::from_millis(20));

        let expired = mgr.expire_stale();
        assert_eq!(expired, vec!["job-1".to_string()]);
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn test_multiple_reservations() {
        let mut mgr = ReservationManager::default();
        mgr.reserve(
            "job-1",
            &make_placement(&["node-0"]),
            4000,
            8_000_000_000,
            0,
        );
        mgr.reserve(
            "job-2",
            &make_placement(&["node-1"]),
            4000,
            8_000_000_000,
            0,
        );
        mgr.reserve(
            "job-3",
            &make_placement(&["node-2"]),
            4000,
            8_000_000_000,
            0,
        );

        assert_eq!(mgr.active_count(), 3);

        let mut jobs = mgr.reserved_jobs();
        jobs.sort();
        assert_eq!(jobs, vec!["job-1", "job-2", "job-3"]);

        mgr.release("job-2");
        assert_eq!(mgr.active_count(), 2);
        assert!(!mgr.has_reservation("job-2"));
    }

    #[test]
    fn test_overwrite_reservation() {
        let mut mgr = ReservationManager::default();
        mgr.reserve(
            "job-1",
            &make_placement(&["node-0"]),
            4000,
            8_000_000_000,
            0,
        );
        mgr.reserve(
            "job-1",
            &make_placement(&["node-1"]),
            2000,
            4_000_000_000,
            0,
        );

        // Should overwrite — only 1 reservation
        assert_eq!(mgr.active_count(), 1);

        // node-0 should no longer be reserved
        let (cpu, _, _) = mgr.reserved_on_node("node-0");
        assert_eq!(cpu, 0);

        // node-1 should have the new reservation
        let (cpu, _, _) = mgr.reserved_on_node("node-1");
        assert_eq!(cpu, 2000);
    }
}
