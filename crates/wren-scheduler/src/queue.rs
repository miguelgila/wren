use wren_core::QueuedJob;
use std::collections::BinaryHeap;
use std::cmp::Ordering;

/// Wrapper that makes `QueuedJob` orderable for `BinaryHeap` (max-heap).
/// Ordering: higher priority first; ties broken by earlier submit_time (FIFO).
#[derive(Clone, Debug)]
struct HeapEntry(QueuedJob);

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.0.priority == other.0.priority && self.0.submit_time == other.0.submit_time
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority wins
        match self.0.priority.cmp(&other.0.priority) {
            Ordering::Equal => {
                // Earlier submit_time wins (reverse: smaller time = greater in heap)
                other.0.submit_time.cmp(&self.0.submit_time)
            }
            ord => ord,
        }
    }
}

/// Priority queue for pending jobs.
///
/// Jobs are dequeued in order of:
/// 1. Priority descending (higher value = more important)
/// 2. Submit time ascending (FIFO within the same priority)
pub struct PriorityQueue {
    heap: BinaryHeap<HeapEntry>,
}

impl PriorityQueue {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
        }
    }

    /// Add a job to the queue.
    pub fn push(&mut self, job: QueuedJob) {
        self.heap.push(HeapEntry(job));
    }

    /// Remove and return the highest-priority job, or `None` if empty.
    pub fn pop(&mut self) -> Option<QueuedJob> {
        self.heap.pop().map(|e| e.0)
    }

    /// Peek at the highest-priority job without removing it.
    pub fn peek(&self) -> Option<&QueuedJob> {
        self.heap.peek().map(|e| &e.0)
    }

    /// Remove a job by name. Returns the removed job if found.
    ///
    /// O(n) — rebuilds the heap after removal.
    pub fn remove_by_name(&mut self, name: &str) -> Option<QueuedJob> {
        let entries: Vec<HeapEntry> = self.heap.drain().collect();
        let mut removed = None;
        let remaining: Vec<HeapEntry> = entries
            .into_iter()
            .filter_map(|e| {
                if e.0.name == name && removed.is_none() {
                    removed = Some(e.0);
                    None
                } else {
                    Some(e)
                }
            })
            .collect();
        self.heap = BinaryHeap::from(remaining);
        removed
    }

    /// Number of jobs in the queue.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Returns `true` if the queue contains no jobs.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Returns an iterator over all jobs in the queue in arbitrary order.
    /// Use `pop()` to drain in priority order.
    pub fn iter(&self) -> impl Iterator<Item = &QueuedJob> {
        self.heap.iter().map(|e| &e.0)
    }
}

impl Default for PriorityQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_job(name: &str, priority: i32, offset_secs: i64) -> QueuedJob {
        QueuedJob {
            name: name.to_string(),
            namespace: "default".to_string(),
            queue: "default".to_string(),
            priority,
            nodes: 1,
            tasks_per_node: 1,
            cpu_per_node_millis: 1000,
            memory_per_node_bytes: 1_000_000_000,
            gpus_per_node: 0,
            walltime: None,
            submit_time: Utc::now() + chrono::Duration::seconds(offset_secs),
        }
    }

    #[test]
    fn test_push_pop_single() {
        let mut q = PriorityQueue::new();
        q.push(make_job("job-a", 50, 0));
        assert_eq!(q.len(), 1);
        let job = q.pop().unwrap();
        assert_eq!(job.name, "job-a");
        assert!(q.is_empty());
    }

    #[test]
    fn test_priority_ordering() {
        let mut q = PriorityQueue::new();
        q.push(make_job("low", 10, 0));
        q.push(make_job("high", 100, 0));
        q.push(make_job("mid", 50, 0));

        assert_eq!(q.pop().unwrap().name, "high");
        assert_eq!(q.pop().unwrap().name, "mid");
        assert_eq!(q.pop().unwrap().name, "low");
    }

    #[test]
    fn test_fifo_for_same_priority() {
        let mut q = PriorityQueue::new();
        // Earlier submit time should come first among same-priority jobs
        q.push(make_job("first", 50, -20));  // submitted 20s ago
        q.push(make_job("third", 50, 0));    // submitted now
        q.push(make_job("second", 50, -10)); // submitted 10s ago

        assert_eq!(q.pop().unwrap().name, "first");
        assert_eq!(q.pop().unwrap().name, "second");
        assert_eq!(q.pop().unwrap().name, "third");
    }

    #[test]
    fn test_mixed_priority_and_fifo() {
        let mut q = PriorityQueue::new();
        q.push(make_job("low-early", 10, -100));
        q.push(make_job("high-late", 100, 0));
        q.push(make_job("high-early", 100, -50));

        // high-early comes first (same priority as high-late but earlier)
        assert_eq!(q.pop().unwrap().name, "high-early");
        assert_eq!(q.pop().unwrap().name, "high-late");
        assert_eq!(q.pop().unwrap().name, "low-early");
    }

    #[test]
    fn test_peek_does_not_remove() {
        let mut q = PriorityQueue::new();
        q.push(make_job("job-a", 50, 0));
        q.push(make_job("job-b", 100, 0));

        let peeked = q.peek().unwrap();
        assert_eq!(peeked.name, "job-b");
        assert_eq!(q.len(), 2); // still 2
    }

    #[test]
    fn test_remove_by_name_found() {
        let mut q = PriorityQueue::new();
        q.push(make_job("job-a", 50, 0));
        q.push(make_job("job-b", 100, 0));
        q.push(make_job("job-c", 10, 0));

        let removed = q.remove_by_name("job-b");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().name, "job-b");
        assert_eq!(q.len(), 2);

        // Remaining order should still be correct
        assert_eq!(q.pop().unwrap().name, "job-a");
        assert_eq!(q.pop().unwrap().name, "job-c");
    }

    #[test]
    fn test_remove_by_name_not_found() {
        let mut q = PriorityQueue::new();
        q.push(make_job("job-a", 50, 0));

        let removed = q.remove_by_name("nonexistent");
        assert!(removed.is_none());
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn test_remove_by_name_only_first_match() {
        // Two jobs with the same name (shouldn't happen in practice, but verify behavior)
        let mut q = PriorityQueue::new();
        q.push(make_job("dup", 50, -10));
        q.push(make_job("dup", 50, 0));

        let removed = q.remove_by_name("dup");
        assert!(removed.is_some());
        assert_eq!(q.len(), 1); // Only one removed
    }

    #[test]
    fn test_pop_empty() {
        let mut q = PriorityQueue::new();
        assert!(q.pop().is_none());
    }

    #[test]
    fn test_len_and_is_empty() {
        let mut q = PriorityQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);

        q.push(make_job("job-a", 50, 0));
        assert!(!q.is_empty());
        assert_eq!(q.len(), 1);

        q.pop();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
    }
}
