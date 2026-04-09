# Queues and Priority

Wren uses queues to organize jobs and enforce cluster policies. This guide covers queue creation, priority scheduling, and how jobs are ordered.

## WrenQueue CRD

A WrenQueue defines a scheduling context with resource limits and policies.

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: default
spec:
  maxNodes: 128
  maxWalltime: "24h"
  maxJobsPerUser: 10
  defaultPriority: 50

  backfill:
    enabled: true
    lookAhead: "2h"

  fairShare:
    enabled: true
    decayHalfLife: "7d"
```

## Queue Spec Fields

- `maxNodes` — maximum total nodes this queue can use (hard limit)
- `maxWalltime` — maximum walltime allowed for jobs (e.g., `24h`)
- `maxJobsPerUser` — maximum concurrent jobs per user
- `defaultPriority` — default priority for jobs that don't specify one (default: 50)
- `backfill.enabled` — enable backfill scheduling (default: false)
- `backfill.lookAhead` — how far ahead to project resource availability (e.g., `2h`)
- `fairShare.enabled` — enable fair-share priority adjustment (default: false)
- `fairShare.decayHalfLife` — usage history decay period (e.g., `7d`)

## Creating Queues

Define separate queues for different workload types:

```yaml
---
# High-priority queue for urgent simulations
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: urgent
spec:
  maxNodes: 64
  maxWalltime: "4h"
  maxJobsPerUser: 2
  defaultPriority: 100

---
# General-purpose batch queue
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: batch
spec:
  maxNodes: 256
  maxWalltime: "72h"
  maxJobsPerUser: 20
  defaultPriority: 50
  backfill:
    enabled: true
    lookAhead: "4h"
  fairShare:
    enabled: true
    decayHalfLife: "14d"

---
# GPU queue with shorter limits
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: gpu
spec:
  maxNodes: 32
  maxWalltime: "12h"
  maxJobsPerUser: 5
  defaultPriority: 50
```

Then submit jobs to a queue:

```yaml
spec:
  queue: gpu
  priority: 100
```

## Priority Scheduling

Jobs are scheduled in priority order: highest priority first, then by arrival time (FIFO).

```yaml
spec:
  priority: 200  # Higher than default (50)
```

The default priority is 50. Jobs with the same priority are scheduled in arrival order.

With fair-share enabled, the scheduler also adjusts priority based on historical usage, so a user who has consumed many node-hours recently gets a lower effective priority.

## Backfill Scheduling

When backfill is enabled, the scheduler allows smaller or shorter jobs to run ahead of their place in the queue if they won't delay higher-priority reservations.

Example:
1. Job A (high-priority, 100 nodes, 4h) arrives and gets placed
2. Job B (medium-priority, 100 nodes, 4h) arrives but must wait (not enough resources)
3. Job C (low-priority, 20 nodes, 2h) arrives
4. Backfill: Job C can run now because it finishes before Job A completes, so it doesn't delay Job B's reservation

Backfill respects:
- The `lookAhead` window (how far ahead to project availability)
- Job dependencies (a job blocked by a dependency won't reserve resources)
- Fair-share priority adjustments

## Fair-Share Scheduling

Fair-share adjusts job priority based on historical resource usage. Projects/users who have consumed fewer resources get higher priority.

With `fairShare.enabled: true`:

- Controller tracks node-hours consumed per user/project
- Historical usage decays over time (half-life = `decayHalfLife`)
- Scheduler adjusts effective priority based on fair-share factor
- New users or those with low usage get a priority boost

Example: A user with high historical usage might see their priority reduced by 20, while a new user with no history gets a +50 boost.

## Default Queue

If not specified, jobs go to the `default` queue:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: default
spec:
  maxNodes: 128
  maxWalltime: "24h"
  defaultPriority: 50
```

To use a different default, update the queue name in your job spec.

## Queue Limits

Jobs are rejected at submission time if they exceed queue limits:

| Limit | Checked | Action |
|-------|---------|--------|
| `maxNodes` | Gang scheduling time | Job stays in Scheduling until resources available |
| `maxWalltime` | Job creation | Job fails immediately with validation error |
| `maxJobsPerUser` | Scheduling time | Job waits in queue until prior jobs complete |

## Viewing Queue Status

```bash
# List all queues
kubectl get wrenqueue

# Get queue details
kubectl describe wrenqueue batch

# Check queue metrics (via Prometheus)
# wren_queue_depth{queue="batch"}
# wren_queue_wait_time_seconds{queue="batch"}
```

Via CLI:

```bash
wren queue
# Shows job count, wait times, and resource usage per queue
```

## Scheduling Order (Container Examples)

### Without Backfill or Fair-Share

Queue: `[JobA(pri=100), JobB(pri=50), JobC(pri=50)]`

Scheduling order: A, then B (arrival first), then C.

### With Backfill

Queue at time 0:
- JobA (pri=100, 100 nodes, 4h) — scheduled now
- JobB (pri=90, 100 nodes, 4h) — waits (not enough resources)
- JobC (pri=10, 20 nodes, 2h) — eligible for backfill

Cluster has 120 nodes. JobA uses 100. Backfill logic:
- JobB reserved at time 4h (when JobA completes)
- JobC can run from time 0-2h without delaying JobB
- JobC is backfilled

### With Fair-Share

User alice:
- 40 node-hours historical usage
- Fair-share factor: -0.25 (reduce priority by 25)

User bob:
- 0 node-hours (new)
- Fair-share factor: +0.50 (increase priority by 50)

JobA (alice, pri=50) → effective priority: 50 - 25 = 25
JobB (bob, pri=50) → effective priority: 50 + 50 = 100

Scheduling order: JobB first (higher effective priority).

## Queue Policies Best Practices

1. **Create separate queues for different SLAs:**
   - `urgent` — critical jobs, short walltime, high priority
   - `batch` — general workloads, longer walltime, backfill enabled
   - `gpu` — resource-specific, limits on GPU node usage

2. **Use fair-share for multi-project clusters:**
   - Prevents one project from dominating the queue
   - New projects get a priority boost
   - Historical usage decays so temporary overuse doesn't lock out others

3. **Set appropriate backfill lookahead:**
   - Shorter lookahead: faster scheduling decisions, less backfill opportunity
   - Longer lookahead: better resource utilization, slower scheduling decisions
   - Start with `lookAhead: "2h"` and tune based on workload

4. **Enforce user limits to prevent queue flooding:**
   - `maxJobsPerUser: 10` prevents a single user from filling the queue
   - Adjust based on cluster size and user count
