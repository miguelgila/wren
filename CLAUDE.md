# Wren — HPC Job Scheduler for Kubernetes

> A lightweight, Slurm-inspired HPC job scheduler built in Rust for Kubernetes,
> with multi-node MPI support and optional bare-metal execution via [Reaper](https://github.com/miguelgila/reaper).

## Project Vision

Wren bridges the gap between traditional HPC workload management (Slurm/PBS)
and cloud-native Kubernetes orchestration. It provides gang scheduling, topology-aware
placement, priority queues with backfill, and MPI-native job execution — all as a
Kubernetes-native controller written in Rust.

**Key differentiators:**
- Multi-node gang scheduling with topology awareness
- Dual execution backends: containers (Pods) or bare-metal (Reaper)
- Slurm-like UX (queues, priorities, walltime, fair-share, backfill)
- Rust for performance-critical scheduling decisions (topology scoring, constraint solving)
- Designed for real HPC fabrics (Slingshot, InfiniBand, RDMA)

## Architecture Overview

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
                     │ └──────┘  └─────────┘    │
                     └──────────────────────────┘
                                  │
              ┌───────────────────┼───────────────────┐
              ▼                   ▼                   ▼
         ┌─────────┐        ┌─────────┐        ┌─────────┐
         │ Node 0  │◄──────►│ Node 1  │◄──────►│ Node N  │
         │ rank 0  │  MPI   │ rank 1  │  MPI   │ rank N  │
         └─────────┘        └─────────┘        └─────────┘
```

## Tech Stack

- **Language:** Rust (2021 edition, stable)
- **K8s client:** `kube-rs` + `kube-runtime` (async, tokio)
- **CRDs:** `kube-derive` with `schemars` for OpenAPI schemas
- **CLI:** `clap` v4
- **Serialization:** `serde` + `serde_json` / `serde_yaml`
- **Logging:** `tracing` + `tracing-subscriber`
- **Metrics:** `prometheus` crate, exposed via axum
- **Testing:** standard Rust tests + `k8s-openapi` test fixtures

## Repository Structure

```
wren/
├── CLAUDE.md                  # This file — project plan and dev guide
├── README.md                  # User-facing documentation
├── Cargo.toml                 # Workspace root
├── Cargo.lock
├── LICENSE                    # GPL-3.0-or-later
│
├── crates/
│   ├── wren-core/           # Shared types, CRD definitions, traits
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── crd.rs         # WrenJob, WrenQueue, WrenUser CRDs
│   │       ├── types.rs       # Placement, JobStatus, TopologyConstraint
│   │       ├── backend.rs     # ExecutionBackend trait
│   │       └── error.rs       # Error types
│   │
│   ├── wren-scheduler/      # Scheduling algorithms
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── gang.rs        # Gang scheduling (all-or-nothing placement)
│   │       ├── topology.rs    # Topology-aware scoring (switch groups, hops)
│   │       ├── queue.rs       # Priority queue
│   │       ├── queue_manager.rs # Multi-queue management
│   │       ├── backfill.rs    # Backfill scheduler (Slurm-style)
│   │       ├── fair_share.rs  # Fair-share priority adjustment
│   │       ├── dependencies.rs# Job dependencies and job arrays
│   │       └── resources.rs   # Resource accounting and tracking
│   │
│   ├── wren-controller/     # Kubernetes controller (main binary)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs        # Entry point, controller setup
│   │       ├── reconciler.rs  # WrenJob reconciliation loop
│   │       ├── job_id.rs      # Sequential job ID allocator (ConfigMap-backed)
│   │       ├── node_watcher.rs# Node topology discovery and tracking
│   │       ├── container.rs   # Container execution backend
│   │       ├── reaper.rs      # Reaper execution backend
│   │       ├── mpi.rs         # MPI bootstrap (hostfile, SSH keys, PMIx)
│   │       ├── metrics.rs     # Prometheus metrics endpoint
│   │       ├── reservation.rs # Resource reservation tracking
│   │       ├── leader_election.rs # HA leader election
│   │       └── webhook.rs     # Admission webhook validation
│   │
│   └── wren-cli/            # CLI tool (wren submit, queue, cancel, etc.)
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs
│           ├── submit.rs      # wren submit <job.yaml>
│           ├── queue.rs       # wren queue (squeue equivalent)
│           ├── cancel.rs      # wren cancel <job-id>
│           ├── status.rs      # wren status <job-id>
│           └── logs.rs        # wren logs <job-id> [--rank N]
│
├── charts/
│   └── wren/                  # Helm chart
│       ├── Chart.yaml
│       ├── values.yaml
│       └── templates/         # Deployment, RBAC, CRDs, Service, ServiceMonitor
│
├── manifests/
│   ├── crds/                  # Generated CRD YAML manifests
│   │   ├── wrenjob.yaml
│   │   └── wrenqueue.yaml
│   ├── rbac/
│   │   └── rbac.yaml          # ServiceAccount, ClusterRole, ClusterRoleBinding
│   ├── deployment.yaml        # Wren controller Deployment
│   └── examples/
│       ├── simple-mpi.yaml    # Basic 2-node job
│       ├── gpu-training.yaml  # Multi-node GPU training job
│       └── reaper-job.yaml    # Bare-metal execution via Reaper
│
├── docker/
│   └── Dockerfile.controller  # Multi-stage build for the controller
│
├── scripts/
│   └── run-integration-tests.sh # Kind cluster integration test runner
│
└── .github/workflows/
    ├── ci.yaml                # Lint, test, Docker build/push, Helm lint
    ├── integration.yml        # Kind cluster integration tests
    ├── auto-release.yml       # PR merge → patch bump → tag → release
    ├── release.yml            # Build static binaries, create GitHub Release
    └── manual-release.yml     # Manual version bump (patch/minor/major)
```

## CRD Definitions

### WrenJob (primary user-facing CRD)

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: my-simulation
spec:
  queue: default              # Which WrenQueue to submit to
  priority: 100               # Job priority (higher = more important)
  walltime: "4h"              # Maximum runtime, enforced with SIGTERM → SIGKILL

  # Gang scheduling
  nodes: 4                    # Number of nodes required
  tasksPerNode: 1             # MPI ranks per node

  # Execution backend: "container" (default) or "reaper"
  backend: container

  # Container backend configuration
  container:
    image: nvcr.io/nvidia/pytorch:24.01-py3
    command: ["mpirun", "-np", "4", "--hostfile", "/etc/wren/hostfile", "./app"]
    resources:
      limits:
        nvidia.com/gpu: 4
        memory: 64Gi
    hostNetwork: true          # Required for Slingshot/RDMA

  # Reaper backend configuration (when backend: reaper)
  reaper:
    script: |
      #!/bin/bash
      module load cray-mpich
      srun ./my_app
    environment:
      SCRATCH: /scratch/project

  # MPI configuration
  mpi:
    implementation: cray-mpich   # cray-mpich | openmpi | intel-mpi
    sshAuth: true                # Mount shared SSH keys for mpirun
    fabricInterface: hsn0        # Network interface for MPI traffic

  # Topology preferences
  topology:
    preferSameSwitch: true
    maxHops: 2                   # Maximum network hops between nodes
    topologyKey: "topology.kubernetes.io/zone"

  # Job dependencies (Slurm-style)
  dependencies:
    - type: afterOk
      job: previous-simulation
```

### WrenQueue

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
  preemption:
    enabled: false
    policy: gang            # Only preempt entire jobs, not individual pods
  backfill:
    enabled: true
    lookAhead: "2h"         # How far ahead to project resource availability
  fairShare:
    enabled: true
    decayHalfLife: "7d"     # Usage history decay period
    weightAge: 0.25
    weightSize: 0.25
    weightFairShare: 0.50
```

### WrenUser (cluster-scoped, maps usernames to Unix UIDs)

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: miguel              # matches wren.giar.dev/user annotation
spec:
  uid: 1001                 # Unix UID for process execution
  gid: 1001                 # Primary Unix GID
  supplementalGroups:       # Additional GIDs (e.g., project groups)
    - 1001
    - 5000
  homeDir: "/home/miguel"   # HOME env var, optional volume mount
  defaultProject: "climate-sim"  # default fair-share project
```

Population: admin-managed or synced from LDAP via CronJob. The controller looks
up the WrenUser when creating pods (securityContext) or dispatching to Reaper
(uid/gid in job request). No LDAP connectivity needed from the controller.

## Development Milestones

### Phase 1: Foundation (v0.1.0) — COMPLETE
**Goal:** A working gang scheduler that can run a simple multi-node MPI job.

- [x] Set up Cargo workspace with all crates
- [x] Define `WrenJob` CRD with `kube-derive` (originally MPIJob, renamed)
- [x] Implement basic controller reconciliation loop (watch WrenJobs)
- [x] Implement node resource tracker (watch Nodes, track allocatable resources)
- [x] Implement FIFO gang scheduler (all-or-nothing placement)
- [x] Implement container backend:
  - [x] Headless Service creation for pod DNS discovery
  - [x] Hostfile ConfigMap generation
  - [x] Worker Pod creation with shared SSH keys
  - [x] Launcher Pod that waits for workers, then runs mpirun
  - [x] Simple mode (no launcher) for non-MPI jobs — runs user command directly
- [x] Implement walltime enforcement (SIGTERM after walltime, SIGKILL after grace)
- [x] Basic status reporting on the WrenJob status subresource
- [x] Pod watcher — `.watches()` on pods with `app.kubernetes.io/managed-by=wren` for fast reconciliation
- [x] Completed pod preservation with 24h TTL for log retrieval
- [x] AlreadyExists (HTTP 409) tolerance on resource creation
- [x] Integration tests: shell smoke tests on kind cluster (10 tests)
- [x] Dockerfile for the controller
- [x] Example manifests (hello-world, multi-node)

### Phase 2: Smart Scheduling (v0.2.0) — COMPLETE
**Goal:** Topology-aware placement and priority queues.

- [x] Define `WrenQueue` CRD
- [x] Implement priority queue with multiple queues
- [x] Implement topology-aware node scoring:
  - [x] Parse node labels for switch group / rack / zone
  - [x] Score candidate node sets by network proximity
  - [x] Support `maxHops` and `preferSameSwitch` constraints
- [x] Implement resource reservation (prevent double-booking during bind)
- [x] Add Prometheus metrics:
  - [x] Queue depth, wait time, scheduling latency
  - [x] Node utilization, job completion rate
  - [x] Gang scheduling success/failure ratio
- [x] `wren-cli` basics: `submit`, `queue`, `cancel`, `status`, `logs`
- [x] Sequential job IDs (Slurm-style, ConfigMap-backed counter)
- [x] CLI integration tests (`--only-cli` flag, 6 tests)

### Phase 3: Reaper Integration (v0.3.0) — IN PROGRESS
**Goal:** Bare-metal execution backend via Reaper.

#### Phase 3a: Core Integration — COMPLETE
- [x] Define Reaper backend trait implementation (`ReaperBackend` with HTTP client)
- [x] Implement communication protocol with Reaper agents on nodes (REST API: POST/GET/DELETE /api/v1/jobs)
- [x] Job script distribution to Reaper agents
- [x] Process lifecycle management (launch, monitor, terminate via HTTP)
- [x] Reaper: accept `uid`/`gid`/`username`/`homeDir` in job request
- [x] Reaper: use `Command::uid()`/`.gid()` to drop privileges before exec (setgroups → setgid → setuid)
- [x] Reaper: inject `USER`/`HOME`/`LOGNAME` env vars (same as container backend)
- [x] MPI environment variables for bare-metal (MPICH_OFI_*, UCX_NET_DEVICES, per-implementation)
- [x] Distributed training env vars (MASTER_ADDR, MASTER_PORT, RANK, WORLD_SIZE, LOCAL_RANK)
- [x] Hostfile distribution (content in request JSON, Reaper writes to disk)
- [x] Shared resource tracking between container and reaper backends
- [ ] Integration test with Reaper

#### Phase 3b: Multi-Task & Resource Binding — PLANNED
**Goal:** Support `tasks_per_node > 1` with CPU affinity and GPU binding (replaces Slurm's `srun` intra-node functionality).

- [ ] Extend `ReaperJobRequest` with `tasks_per_node`, `cpus_per_task`, `gpus_per_task` fields
- [ ] Reaper agent: multi-process spawning (fork N processes per job request)
- [ ] CPU affinity binding via `sched_setaffinity()` or `numactl` (hwloc-aware)
- [ ] GPU device binding via `CUDA_VISIBLE_DEVICES` (NVIDIA), `ROCR_VISIBLE_DEVICES` (AMD), `ZE_AFFINITY_MASK` (Intel)
- [ ] NUMA-aware placement (pin ranks to NUMA domains close to their assigned GPUs)
- [ ] Per-task local rank env vars (`WREN_LOCAL_RANK`, `LOCAL_RANK`)
- [ ] Aggregate status reporting (job running if any task running, failed if any task failed)
- [ ] Per-task exit code tracking
- [ ] Wren controller: topology discovery (query node GPU/NUMA topology via Reaper agent or node labels)

**Design notes:**
- Reaper agent handles intra-node binding decisions locally (can query `/sys/devices/system/node/`, `nvidia-smi`)
- Wren controller specifies *what* resources per task; Reaper decides *which* specific devices
- Phase 3a supports `tasks_per_node=1` only (single rank per node, no binding needed)

#### Phase 3c: MPI Process Management — PLANNED
**Goal:** Full PMI bootstrap for traditional MPI applications without SSH.

- [ ] PMI key-value store service (lightweight, in Wren controller or standalone)
- [ ] PMIx integration for cray-mpich and OpenMPI
- [ ] Shared-filesystem PMI fallback (ranks exchange connection info via files on Lustre/GPFS)
- [ ] Wren-managed process group coordination (barrier, abort propagation)
- [ ] Integration test with actual MPI hello-world on multi-node

**Design rationale:** Wren directly launches MPI ranks on each node via Reaper (replacing `srun`).
No SSH between compute nodes. PMI bootstrap via Wren-managed KVS or shared filesystem.
For ML training workloads (PyTorch/NCCL), MASTER_ADDR/RANK/WORLD_SIZE from Phase 3a suffice.

### Phase 4: Advanced Scheduling (v0.4.0) — MOSTLY COMPLETE
**Goal:** Slurm-level scheduling sophistication.

- [x] Backfill scheduler (`wren-scheduler/src/backfill.rs`):
  - [x] Build resource timelines from running jobs
  - [x] Compute reservation times for blocked high-priority jobs
  - [x] Allow smaller/shorter jobs to backfill without delaying reservations
  - [x] Shadow allocation prevents double-booking during scheduling pass
  - [x] Look-ahead window for future resource projection
- [x] Fair-share scheduling (`wren-scheduler/src/fair_share.rs`):
  - [x] Track per-project/user node-hour usage (node-seconds, GPU-seconds)
  - [x] Exponential decay of historical usage with configurable half-life
  - [x] Multi-factor effective priority (age, size, fair-share weights)
  - [x] Fair-share factor computation (-1 to +1 range)
- [x] Job dependencies (`wren-scheduler/src/dependencies.rs`):
  - [x] `AfterOk`, `AfterAny`, `AfterNotOk` dependency types
  - [x] Cycle detection using Kahn's algorithm
  - [x] Exposed in WrenJobSpec CRD (`dependencies` field)
- [x] Job arrays (`wren-scheduler/src/dependencies.rs`):
  - [x] `JobArraySpec` with parsing (`0-99`, `1-10:2`, `0-99%5`)
  - [x] Concurrency limiting and step support
  - [ ] **Not yet exposed in WrenJobSpec CRD** — scheduler code only
- [ ] Preemption support (gang preemption — evict entire jobs)
- [ ] Accounting and usage reports (API endpoint for querying usage)

**Note:** Fair-share and backfill are implemented as pure scheduler algorithms
but are not yet wired into the controller reconciliation loop. The scheduler
crate is intentionally kept free of K8s dependencies for testability.

### Phase 5: Multi-User & Multi-Tenancy (v0.5.0)
**Goal:** Production multi-user support with identity, quotas, and accounting.

Currently Wren has **no user identity tracking** — every job is anonymous. This
phase adds the plumbing to make it a proper multi-tenant HPC scheduler.

#### 5.1 User Identity

User identity in Wren serves two purposes:

1. **Scheduling identity** — who submitted this job? (quotas, fair-share, limits)
2. **Execution identity** — what UID/GID should the process run as? (filesystem permissions)

Kubernetes only provides (1) natively via `UserInfo`. It has no concept of Unix
UIDs. But HPC shared filesystems (Lustre, GPFS, NFS with AUTH_SYS) need real
UIDs for correct file ownership.

**Design: three layers**

```
               ┌─────────────────┐
               │  kubectl / CLI  │
               │  (user: miguel) │
               └────────┬────────┘
                        │ K8s API request
               ┌────────▼────────┐
  Layer 1:     │ Mutating Webhook │ ← stamps wren.giar.dev/user from UserInfo
               └────────┬────────┘
                        │
               ┌────────▼────────┐
               │ WrenJob CR      │
               │ annotations:    │
               │   wren.giar.dev/user  │
               │ spec.project    │
               └────────┬────────┘
                        │
        ┌───────────────▼───────────────┐
        │       Wren Controller         │
        │                               │
  Layer 2:  lookup wren.giar.dev/user         │
        │       │                       │
        │  ┌────▼─────┐                 │
        │  │ WrenUser  │  ← admin/sync  │
        │  │   CRD     │    maintains   │
        │  │ uid: 1001 │                │
        │  │ gid: 1001 │                │
        │  └────┬──────┘                │
        │       │                       │
  Layer 3:  ┌───▼─────┐  ┌──────────┐  │
        │   │Container│  │ Reaper   │  │
        │   │Backend  │  │ Backend  │  │
        │   └────┬────┘  └────┬─────┘  │
        └────────┼────────────┼────────┘
                 │            │
      runAsUser: 1001    setuid(1001)
      runAsGroup: 1001   + USER/HOME env
                 │            │
            ┌────▼────┐  ┌───▼──────┐
            │  Pod     │  │  Reaper  │
            │ uid=1001 │  │ uid=1001 │
            └─────────┘  └──────────┘
```

**Layer 1 — Identity Capture (Mutating Webhook):**
Webhook stamps `wren.giar.dev/user` from the K8s API request's `UserInfo`. This is
tamper-proof — the API server populates `UserInfo` from the auth backend (OIDC,
x509, ServiceAccount). CLI sets `wren.giar.dev/user` as a convenience; webhook always
overrides with the real identity.

**Layer 2 — UID Resolution (WrenUser CRD):**
A cluster-scoped `WrenUser` CRD maps usernames to Unix UID/GID. The controller
looks up the WrenUser when creating pods or dispatching to Reaper. This avoids
the controller needing direct LDAP connectivity.

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: miguel
spec:
  uid: 1001
  gid: 1001
  supplementalGroups: [1001, 5000]
  homeDir: "/home/miguel"
  defaultProject: "climate-sim"
```

Population options (pick what fits the site):
- **Manual** — admin creates WrenUser CRDs (small teams)
- **CronJob sync** — `ldapsearch | kubectl apply` (most HPC sites)
- **LDAP controller** — watches LDAP changes, syncs WrenUser CRDs (large sites)
- **Helm values** — define users in `values.yaml` for GitOps

| | Controller → LDAP | WrenUser CRD |
|---|---|---|
| Runtime dependency | LDAP must be reachable | None (etcd) |
| Failure mode | LDAP down = can't schedule | Always available |
| Caching | Manual TTL cache | K8s informer, watch-based |
| Configuration | Bind DN, TLS, schema | Just a CRD |
| Auditability | LDAP query logs | `kubectl get wrenusers` |

**Layer 3 — Execution Identity:**
- **Container backend** — sets `securityContext.runAsUser`/`runAsGroup` on pods.
  Files written to NFS/Lustre are owned by the correct UID. No NSS needed in
  the container.
- **Reaper backend** — receives numeric UID/GID from controller. Reaper agent
  calls `CommandExt::uid()`/`.gid()` before exec. Sets `USER`, `HOME`,
  `LOGNAME` env vars from WrenUser CRD. No LDAP/NSS/PAM needed on the compute
  node — identity flows entirely from the controller.

This makes compute nodes **stateless from an identity perspective**: no LDAP
client, no SSSD, no nscd. The same identity plumbing works for both container
and bare-metal backends. Nodes are dumb executors; all identity comes from Wren.

**Reaper change required:** Add `uid`/`gid`/`username`/`home_dir` fields to the
job request, use `Command::uid()`/`.gid()` before exec, inject env vars. Small,
self-contained change — Reaper stays a dumb executor.

##### Implementation checklist

- [ ] Define `WrenUser` CRD in `wren-core/src/crd.rs` (cluster-scoped)
- [ ] Add `project` field to `WrenJobSpec` (user-settable, for fair-share)
- [ ] Implement mutating webhook to stamp `wren.giar.dev/user` from `UserInfo`
- [ ] CLI sends `wren.giar.dev/user` annotation; webhook overrides with real identity
- [ ] Controller: look up WrenUser on reconcile, resolve UID/GID
- [ ] Container backend: set `securityContext.runAsUser`/`runAsGroup` on pods
- [ ] Container backend: set `USER`/`HOME` env vars from WrenUser
- [ ] Reaper backend: pass UID/GID/username/homeDir in job request
- [ ] Reaper: accept UID/GID, use `Command::uid()`/`.gid()`, set env vars
- [ ] Add LDAP sync script example (`scripts/sync-ldap-users.sh`)
- [ ] CRD manifest and RBAC for WrenUser

#### 5.2 Per-User Limits & Quotas

- [ ] Enforce `max_jobs_per_user` in reconciler (field already exists in `WrenQueueSpec`)
- [ ] Add per-user/project resource quotas to `WrenQueue`:
  ```yaml
  spec:
    quotas:
      - user: "miguel"
        maxNodes: 32
        maxGpuHours: 1000    # per month
      - project: "climate-sim"
        maxNodes: 64
  ```
- [ ] Burst allowances (use more when cluster is idle)
- [ ] Max concurrent nodes per user enforcement

#### 5.3 Fair-Share Wiring

The `FairShareManager` exists in `wren-scheduler` but isn't connected to the
controller yet. This sub-phase wires it up:

- [ ] Feed job completion data into `FairShareManager` (record `node_hours = nodes * walltime`)
- [ ] Key usage by `(user, project)` instead of just namespace
- [ ] Adjust effective priority in the scheduling loop using fair-share factor
- [ ] Expose usage summaries via metrics endpoint (`/metrics`)

#### 5.4 Accounting & Reports

- [ ] Persist usage records (currently in-memory only)
- [ ] Add `wren accounting` CLI command (like Slurm's `sacct`)
- [ ] Per-user/project usage reports (node-hours, GPU-hours, job count)
- [ ] Expose usage via Prometheus metrics for Grafana dashboards

#### Architecture Flow

The scheduler crate stays **pure** (no K8s deps). Multi-user support flows
cleanly through three paths:

**Scheduling path** (identity → priority):
```
Webhook stamps user → Reconciler reads user →
  FairShareManager adjusts priority →
    GangScheduler sees adjusted priority → placement
```

**Execution path** (identity → UID):
```
Reconciler reads user → WrenUser CRD lookup (uid, gid, homeDir) →
  Container backend: securityContext.runAsUser/runAsGroup
  Reaper backend: pass uid/gid in job request → setuid before exec
```

The scheduler crate sees only priority numbers. The controller crate handles
user identity plumbing. Both backends receive the same UID/GID — files written
to shared filesystems have correct ownership regardless of execution mode.

#### Security: No Anonymous or Root Execution

**Hard rule: every job must have a valid, non-root user identity.**

The controller enforces this at scheduling time:
- Job enters `Scheduling` → controller resolves `wren.giar.dev/user` annotation → looks up WrenUser CRD
- If no annotation, no matching WrenUser, or WrenUser has `uid: 0` → job is **Failed** immediately
- Jobs never run as root (uid=0) or as an anonymous/default user

This means:
- The mutating webhook stamps `wren.giar.dev/user` from K8s UserInfo (automatic for kubectl users)
- An admin must create a `WrenUser` CRD for every user before they can submit jobs
- The CLI should send `wren.giar.dev/user` as a convenience; the webhook always overrides with the real identity

**Future: API Gateway for Job Submission**

When adding a REST API gateway (Phase 7+), the gateway will:
1. Authenticate users via OIDC/token (independent of K8s RBAC)
2. Stamp `wren.giar.dev/user` on the WrenJob before creating it via the K8s API
3. The controller's existing enforcement (require valid WrenUser, reject uid=0) provides defense-in-depth
4. The gateway can add additional checks: rate limiting, quota pre-validation, project authorization

The current architecture (annotation + WrenUser CRD lookup + controller enforcement) is designed to
work unchanged behind an API gateway — the gateway just becomes another source of the annotation.

### Phase 6: Production Hardening (v0.6.0)
**Goal:** Ready for production HPC workloads.

- [x] Leader election for HA controller deployment
- [ ] Comprehensive error handling and retry logic
- [ ] Graceful degradation when nodes disappear mid-job
- [x] Webhook validation for WrenJob and WrenQueue CRDs (scaffolded)
- [x] Helm chart for deployment (`charts/wren/`)
- [ ] Comprehensive documentation
- [ ] Performance benchmarking (scheduling latency at scale)
- [x] CI/CD pipeline (GitHub Actions: test, lint, build, container image)

### Phase 7: API Gateway & REST Interface (future)
**Goal:** REST API for job submission independent of kubectl/K8s RBAC.

- [ ] API gateway service (axum-based, separate binary or mode)
- [ ] OIDC/token authentication (map external identity to `wren.giar.dev/user`)
- [ ] REST endpoints: `POST /jobs`, `GET /jobs`, `DELETE /jobs/{id}`, `GET /jobs/{id}/logs`
- [ ] Rate limiting and quota pre-validation at the gateway level
- [ ] Project authorization (which users can submit to which projects)
- [ ] WebSocket endpoint for job status streaming
- [ ] Gateway stamps `wren.giar.dev/user` annotation, controller enforces identity as today

## Coding Conventions

- Use `thiserror` for error types, `anyhow` only in binaries
- Use `tracing` for all logging (structured, with spans for each reconciliation)
- All public API types must derive `Serialize, Deserialize, Clone, Debug`
- CRD types additionally derive `JsonSchema` and `CustomResource`
- Async everywhere with `tokio` runtime
- Tests: unit tests in each module, integration tests in `tests/` directories
- Keep scheduling algorithms in `wren-scheduler` pure (no K8s dependencies) so
  they can be tested without a cluster

## Key Design Decisions

1. **Separate scheduler crate from controller** — scheduling algorithms should be
   testable as pure functions without needing a Kubernetes cluster. The controller
   crate wires them to the K8s API.

2. **Backend trait abstraction** — the `ExecutionBackend` trait allows clean
   separation between container and bare-metal execution. New backends can be added
   without touching scheduling logic.

3. **Single CRD user experience** — users interact with `WrenJob` only. PodGroups
   and internal resources are implementation details managed by the controller.

4. **Hostfile-based MPI bootstrap (v1)** — simpler than PMIx, works with all MPI
   implementations. Can be enhanced with PMIx support later for Reaper backend.

5. **No sidecar injection** — unlike the MPI Operator, Wren manages the full
   lifecycle directly. This gives us more control over the scheduling and execution
   flow.

## References

- [kube-rs documentation](https://kube.rs/)
- [Volcano scheduler](https://volcano.sh/) — most mature K8s gang scheduler
- [MPI Operator](https://github.com/kubeflow/mpi-operator) — study launcher/worker pattern
- [Slurm documentation](https://slurm.schedmd.com/) — scheduling algorithms (backfill, fair-share)
- [HPK](https://github.com/CARV-ICS-FORTH/HPK) — inverse approach (K8s on Slurm)
- [Reaper](https://github.com/miguelgila/reaper) — bare-metal execution backend

## Integration Test Plan

### Overview

Integration tests validate the full lifecycle of WrenJobs running against a real
Kubernetes API. They are split into two tiers:

- **Tier 1 — API-level tests** (run in CI): use a `kind` cluster to test CRD
  installation, controller startup, and object lifecycle without running real MPI
  workloads.
- **Tier 2 — MPI workload tests** (manual / nightly): run actual multi-node MPI
  hello-world jobs on a cluster with 2+ worker nodes.

### Prerequisites

- `kind` v0.20+ (or `k3d`)
- `kubectl`
- `docker` (for building controller image)
- Rust toolchain (for building test binaries)

### Tier 1: API-Level Integration Tests

Located in `tests/integration/` at the workspace root.

#### 1.1 CRD Installation
- [ ] Generate CRD manifests from Rust types (`cargo run --bin crd-gen`)
- [ ] Apply CRDs to a fresh kind cluster
- [ ] Verify CRDs are registered: `kubectl get crd wrenjobs.wren.giar.dev`
- [ ] Verify CRDs have correct print columns and status subresource

#### 1.2 Controller Startup
- [ ] Build controller container image
- [ ] Deploy controller to kind cluster (Deployment + RBAC)
- [ ] Verify controller pod reaches `Running` state
- [ ] Verify `/healthz` and `/readyz` endpoints respond 200
- [ ] Verify `/metrics` endpoint returns Prometheus metrics

#### 1.3 WrenJob Lifecycle (without real MPI)
- [ ] Create a WrenJob with `backend: container` and a simple `busybox` image
- [ ] Verify status transitions: `Pending` → `Scheduling` → `Running`
- [ ] Verify headless Service is created (`<job>-workers`)
- [ ] Verify hostfile ConfigMap is created with correct content
- [ ] Verify worker Pods are created on correct nodes with correct labels
- [ ] Verify launcher Pod is created with mpirun command
- [ ] Verify `wren status <job>` shows running state via CLI
- [ ] Verify `wren queue` lists the job
- [ ] Cancel job with `wren cancel <job>` and verify cleanup:
  - [ ] Pods deleted
  - [ ] Service deleted
  - [ ] ConfigMap deleted
  - [ ] Status updated to `Cancelled`

#### 1.4 Error Handling
- [ ] Submit WrenJob with 0 nodes → verify immediate `Failed` status
- [ ] Submit WrenJob without container spec → verify `Failed` with validation message
- [ ] Submit WrenJob requesting more nodes than available → verify stays in `Scheduling`
- [ ] Submit WrenJob with walltime "1s" → verify `WalltimeExceeded` after timeout

#### 1.5 WrenQueue
- [ ] Create a WrenQueue CRD
- [ ] Submit jobs to the queue
- [ ] Verify queue depth metrics update

### Tier 2: MPI Workload Tests

#### 2.1 Simple MPI Hello World
- [ ] Deploy a 2-node kind cluster with OpenMPI-capable images
- [ ] Submit a 2-node MPI job that runs `mpi_hello_world`
- [ ] Verify all ranks report output
- [ ] Verify job transitions to `Succeeded`
- [ ] Verify `wren logs <job>` returns output from all ranks
- [ ] Verify `wren logs <job> --rank 0` filters correctly

#### 2.2 Gang Scheduling Validation
- [ ] Submit a job requiring 3 nodes on a 4-node cluster
- [ ] Submit a second job requiring 3 nodes
- [ ] Verify only first job is scheduled (gang: all-or-nothing)
- [ ] Cancel first job → verify second job gets scheduled

#### 2.3 Priority Scheduling
- [ ] Submit low-priority job (priority 10)
- [ ] Submit high-priority job (priority 100)
- [ ] With limited resources, verify high-priority job is scheduled first

### CI Pipeline Integration

Add to `.github/workflows/integration.yml`:

```yaml
integration-tests:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - uses: helm/kind-action@v1
      with:
        cluster_name: wren-test
        node_image: kindest/node:v1.31.0

    - name: Build controller image
      run: |
        docker build -f docker/Dockerfile.controller -t wren-controller:test .
        kind load docker-image wren-controller:test --name wren-test

    - name: Install CRDs
      run: kubectl apply -f manifests/crds/

    - name: Deploy controller
      run: |
        kubectl apply -f manifests/rbac/
        kubectl apply -f manifests/deployment.yaml
        kubectl wait --for=condition=available deployment/wren-controller --timeout=60s

    - name: Run integration tests
      run: cargo test --test integration -- --test-threads=1
```

### Test Utilities

Create a `tests/common/` module with helpers:
- `setup_kind_cluster()` — ensure kind cluster is running
- `install_crds()` — apply CRD manifests
- `wait_for_status(job_name, expected_state, timeout)` — poll job status
- `create_wrenjob(spec)` — helper to create jobs from Rust code
- `cleanup_job(job_name)` — delete job and all owned resources
- `assert_pods_with_labels(labels, expected_count)` — verify pod creation
