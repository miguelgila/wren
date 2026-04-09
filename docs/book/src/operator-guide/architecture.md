# Architecture

Wren is a Kubernetes-native HPC job scheduler written in Rust. This page describes its internal architecture, component interactions, and execution flow.

## System Overview

```
                     ┌──────────────────────────┐
                     │     User Interface        │
                     │  wren CLI / CRD / API   │
                     └────────────┬─────────────┘
                                  │
                     ┌────────────▼─────────────┐
                     │   Wren Controller       │
                     │        (Rust)             │
                     │                           │
                     │  ┌───────┐ ┌───────────┐  │
                     │  │Queue  │ │Gang       │  │
                     │  │Manager│ │Scheduler  │  │
                     │  └───────┘ └───────────┘  │
                     │  ┌───────────────────────┐│
                     │  │Node Topology Tracker  ││
                     │  └───────────────────────┘│
                     │          │                 │
                     │   ┌──────┴──────┐         │
                     │   ▼             ▼         │
                     │ ┌──────┐  ┌─────────┐    │
                     │ │Contai│  │Reaper   │    │
                     │ │ner   │  │Backend  │    │
                     │ │Backend  └─────────┘    │
                     │ └──────┘                  │
                     └──────────────────────────┘
                                  │
              ┌───────────────────┼───────────────────┐
              ▼                   ▼                   ▼
         ┌─────────┐        ┌─────────┐        ┌─────────┐
         │ Node 0  │◄──────►│ Node 1  │◄──────►│ Node N  │
         │ rank 0  │  MPI   │ rank 1  │  MPI   │ rank N  │
         └─────────┘        └─────────┘        └─────────┘
```

## Core Components

### Wren Controller

The main controller runs as a Kubernetes Deployment and handles the full lifecycle of WrenJob objects:

- **Reconciliation Loop** — watches WrenJob objects, validates specs, and drives state transitions
- **Queue Manager** — manages multiple priority queues and enforces job submission limits
- **Scheduler** — implements gang scheduling with topology awareness and backfill
- **Node Watcher** — tracks available cluster resources and node labels
- **Execution Backends** — abstract interface for launching jobs (container or bare-metal)

The controller is stateless except for in-memory cluster state. All durable state (WrenJob status, job IDs, node data) lives in etcd via Kubernetes resources.

### Scheduler Crate

The scheduling algorithms live in a pure Rust library (`wren-scheduler`) with no Kubernetes dependencies. This enables:

- **Testability** — scheduling decisions can be tested offline without a cluster
- **Reusability** — scheduling logic can be embedded in other tools
- **Separation of Concerns** — scheduler doesn't know about K8s API or pod creation

The scheduler is called by the controller's reconciliation loop, receives cluster state as input, and returns a Placement (list of nodes) for each job.

### Execution Backends

Wren abstracts job execution via the `ExecutionBackend` trait. Each backend handles:

- **Job Launch** — create Pods, ReaperPods, or other resources
- **Job Status** — poll running workloads and report health
- **Job Termination** — graceful shutdown and resource cleanup

Two backends are provided:

| Backend | How | When |
|---------|-----|------|
| **Container** | Creates Kubernetes Pods | Default; for GPU jobs on shared clusters |
| **Reaper** | Dispatches to bare-metal nodes via ReaperPod CRD | HPC-only clusters with dedicated compute nodes |

New backends can be added by implementing the trait.

### User Identity System

Wren enforces multi-user execution with proper UID/GID isolation:

1. **Mutating Webhook** — stamps `wren.giar.dev/user` annotation from K8s UserInfo
2. **WrenUser CRD** — cluster admin maintains user-to-UID mappings
3. **Container Backend** — sets `securityContext.runAsUser/runAsGroup` on pods
4. **Reaper Backend** — passes UID/GID to bare-metal processes via job request

See [User Identity](../user-guide/user-identity.md) for details.

## Reconciliation Loop

The controller's core is a watch-based reconciliation loop:

```
1. Watch WrenJob (any namespace)
   │
2. On event (create/update/delete):
   ├─ Fetch full WrenJob spec and current status
   ├─ Perform state transition:
   │  ├─ Pending → Scheduling: validate spec, resolve user
   │  ├─ Scheduling → Running: call scheduler, call backend.launch()
   │  ├─ Running → Succeeded/Failed: poll backend status
   │  └─ any state → Deleted: call backend.terminate() and backend.cleanup()
   │
3. Update WrenJob status subresource
   │
4. Requeue for retry if transient error
   │
5. (Optional) Watch managed resources (Pods, ReaperPods)
   └─ Trigger reconciliation on pod status changes for faster feedback
```

Key design points:

- **Idempotent** — reconciling the same job multiple times produces the same result
- **Watched Triggers** — pod label changes trigger job reconciliation for low latency
- **Status is Truth** — `.status` fields drive all state transitions, not `.spec`
- **TTL Cleanup** — completed pods have a 24-hour TTL and are auto-deleted

## Job Lifecycle

### Pending

Initial state. Job enters the system but has not been validated or assigned to nodes.

Transitions to:
- **Scheduling** — if spec is valid and user identity resolves
- **Failed** — if spec validation fails or no valid user identity

### Scheduling

Job is in the scheduler's queue and waiting for resources. The controller:

1. Calls `queue_manager.enqueue(job)` with priority, fair-share factors
2. On next reconciliation, calls `scheduler.find_placement()` with cluster state
3. If placement found → **Running**
4. If resources unavailable and job was waiting too long → may backfill smaller jobs, or job remains in Scheduling
5. If user cancels → **Cancelled**

Metrics tracked: queue depth, wait time, scheduling latency.

### Running

The backend has launched the job's workloads (Pods or ReaperPods). The controller:

1. Calls `backend.status()` periodically
2. Updates `.status.message` with pod readiness, exit codes
3. Enforces walltime limit:
   - Send SIGTERM at `walltime - grace_period`
   - Send SIGKILL at `walltime`
4. If walltime exceeded → **WalltimeExceeded**
5. If pods report completion → **Succeeded** or **Failed**

### Succeeded / Failed / WalltimeExceeded

Job reached terminal state. Pods are preserved for log retrieval (24-hour TTL).

Transitions to:
- **Deleted** — when TTL expires or user manually deletes the job

## Node Topology Tracking

The `NodeWatcher` component maintains cluster-wide resource state:

1. **Initial Sync** — on startup, list all Nodes and extract:
   - Allocatable CPU, memory, GPUs
   - Topology labels (switch group, rack, zone from node.metadata.labels)
2. **Watch Updates** — on Node changes, rebuild the resource snapshot
3. **Pod Watcher** — watch Pods with `app.kubernetes.io/managed-by=wren` to track allocations
4. **Resource Reservation** — during scheduling, reserve resources to prevent double-booking

This state is stored in memory (rebuilt from API on restart) and used by the scheduler to make placement decisions.

## Gang Scheduling

Wren uses **gang scheduling** — all-or-nothing placement:

- A job requires `nodes` Pods on `nodes` different nodes
- The scheduler either finds placement for **all** nodes or **none**
- Jobs do not partially run

This differs from bin-packing schedulers (e.g., Kubernetes default) which greedily schedule individual pods. Gang scheduling is essential for HPC workloads:

- MPI jobs need all ranks to start together
- Partial start causes deadlocks (rank 0 waits for rank 1 which doesn't exist)
- Fair-share and backfill algorithms assume atomic job units

## Topology-Aware Scheduling

When a job has topology constraints, the scheduler scores candidate node sets:

- **Switch Group** — prefer nodes on the same network switch (locality = lower latency)
- **Max Hops** — reject placements where any two nodes are more than N hops apart
- **Score Calculation** — higher score = lower average hop distance

Topology data comes from node labels:

```yaml
metadata:
  labels:
    topology.kubernetes.io/switch-group: "switch-1"
    topology.kubernetes.io/rack: "rack-a"
```

The scheduler reads these labels and computes hop distances assuming a hierarchical topology (switch → rack → region).

## Backfill Scheduling

Wren implements **Slurm-style backfill**:

1. High-priority job arrives but can't fit now
2. Controller places a "reservation" for that job at time T + look_ahead
3. Smaller/shorter lower-priority jobs can run if they finish before T
4. This maximizes cluster utilization without delaying high-priority work

Example: Cluster has 2 idle nodes, high-priority 4-node job waits. A 2-node low-priority job can backfill if it will finish before the 4-node job would start.

## Leader Election

For high availability, Wren supports multiple controller replicas with automatic failover:

- Controllers compete for a Kubernetes Lease (`wren-controller-leader`)
- Only the leader processes jobs; standbys are dormant
- If leader crashes, a standby acquires the lease within ~15 seconds
- Controlled via `WREN_LEADER_ELECTION` env var (default: enabled)

## Metrics & Observability

The controller exports Prometheus metrics on port 8080 at `/metrics`:

- Job counts and active jobs per state and queue
- Queue depth per queue
- Scheduling latency and job wait time
- Node utilization (CPU, memory, GPUs)
- Topology scoring distribution

See [Monitoring](monitoring.md) for the full metrics reference.

## Error Handling

The controller is designed to be **resilient** and **observable**:

- Transient errors (API timeouts) → requeue after exponential backoff
- Validation errors → job transitions to Failed with error message in status
- Resource conflicts (AlreadyExists on pod creation) → tolerated and handled gracefully
- Pod deletion race conditions → controller re-reconciles to fix state

All errors and state transitions are logged with `tracing` structured logging for audit and debugging.
