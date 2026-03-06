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
│   │       ├── crd.rs         # WrenJob, WrenQueue CRDs
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

### Phase 3: Reaper Integration (v0.3.0)
**Goal:** Bare-metal execution backend via Reaper.

- [ ] Define Reaper backend trait implementation
- [ ] Implement communication protocol with Reaper agents on nodes
- [ ] Job script distribution to Reaper agents
- [ ] Process lifecycle management (launch, monitor, terminate)
- [ ] MPI bootstrap for bare-metal mode (PMIx or native srun)
- [ ] Shared resource tracking between container and reaper backends
- [ ] Integration test with Reaper

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

Kubernetes doesn't natively embed "who created this resource" on the object.
The recommended approach is a **validating/mutating admission webhook** that
stamps `wren.io/user` from the K8s API request's `UserInfo`:

| Approach | Pros | Cons |
|----------|------|------|
| **Annotation** (`wren.io/user`) | Simple, CLI sets it | Easy to forge |
| **Namespace = tenant** | K8s RBAC enforces it | One namespace per user is rigid |
| **ServiceAccount name** | Already in request context | Requires webhook to extract |
| **Webhook + UserInfo** | Tamper-proof | Requires admission webhook |

**Chosen approach:** Webhook stamps `wren.io/user` from `UserInfo`, CLI sets it
as a convenience annotation, webhook overrides if present. This mirrors how
Slurm's `sbatch` knows who you are.

- [ ] Add `user` and `project` fields to `WrenJobSpec`:
  ```yaml
  spec:
    user: "miguel"         # stamped by webhook, not user-settable
    project: "climate-sim" # optional, for fair-share grouping
    queue: "gpu"
  ```
- [ ] Implement mutating webhook to stamp `user` from `UserInfo`
- [ ] CLI sends `wren.io/user` annotation; webhook overrides with real identity

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

The beauty of the current design is that **scheduling stays pure** (no K8s deps
in `wren-scheduler`). Multi-user support flows cleanly:

```
Webhook stamps user → Reconciler reads user →
  FairShareManager adjusts priority →
    GangScheduler sees adjusted priority → placement
```

The scheduler crate stays clean — it just sees priority numbers. The controller
crate handles user identity plumbing.

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
