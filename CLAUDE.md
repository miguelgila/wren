# Bubo — HPC Job Scheduler for Kubernetes

> A lightweight, Slurm-inspired HPC job scheduler built in Rust for Kubernetes,
> with multi-node MPI support and optional bare-metal execution via [Reaper](https://github.com/miguelgila/reaper).

## Project Vision

Bubo bridges the gap between traditional HPC workload management (Slurm/PBS)
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
                     │  bubo CLI / CRD / API   │
                     └────────────┬─────────────┘
                                  │
                     ┌────────────▼─────────────┐
                     │   Bubo Controller       │
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
bubo/
├── CLAUDE.md                  # This file — project plan and dev guide
├── README.md                  # User-facing documentation
├── Cargo.toml                 # Workspace root
├── Cargo.lock
├── LICENSE                    # Apache-2.0
│
├── crates/
│   ├── bubo-core/           # Shared types, CRD definitions, traits
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── crd.rs         # MPIJob, PodGroup, Queue CRDs
│   │       ├── types.rs       # Placement, JobStatus, TopologyConstraint
│   │       ├── backend.rs     # ExecutionBackend trait
│   │       └── error.rs       # Error types
│   │
│   ├── bubo-scheduler/      # Scheduling algorithms
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── gang.rs        # Gang scheduling (all-or-nothing placement)
│   │       ├── topology.rs    # Topology-aware scoring (switch groups, hops)
│   │       ├── queue.rs       # Priority queue with fair-share
│   │       ├── backfill.rs    # Backfill scheduler (Slurm-style)
│   │       └── resources.rs   # Resource accounting and tracking
│   │
│   ├── bubo-controller/     # Kubernetes controller (main binary)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs        # Entry point, controller setup
│   │       ├── reconciler.rs  # MPIJob reconciliation loop
│   │       ├── node_watcher.rs# Node topology discovery and tracking
│   │       ├── container.rs   # Container execution backend
│   │       ├── reaper.rs      # Reaper execution backend
│   │       ├── mpi.rs         # MPI bootstrap (hostfile, SSH keys, PMIx)
│   │       └── metrics.rs     # Prometheus metrics endpoint
│   │
│   └── bubo-cli/            # CLI tool (bubo submit, queue, cancel, etc.)
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs
│           ├── submit.rs      # bubo submit <job.yaml>
│           ├── queue.rs       # bubo queue (squeue equivalent)
│           ├── cancel.rs      # bubo cancel <job-id>
│           ├── status.rs      # bubo status <job-id>
│           └── logs.rs        # bubo logs <job-id> [--rank N]
│
├── manifests/
│   ├── crds/                  # Generated CRD YAML manifests
│   │   ├── mpijob.yaml
│   │   ├── buboqueue.yaml
│   │   └── podgroup.yaml
│   ├── rbac/                  # ServiceAccount, ClusterRole, ClusterRoleBinding
│   ├── deployment.yaml        # Bubo controller Deployment
│   └── examples/
│       ├── simple-mpi.yaml    # Basic 2-node MPI job
│       ├── gpu-training.yaml  # Multi-node GPU training job
│       ├── reaper-job.yaml    # Bare-metal execution via Reaper
│       └── priority-queues.yaml
│
├── docker/
│   ├── Dockerfile.controller  # Multi-stage build for the controller
│   └── Dockerfile.cli         # CLI container image
│
└── docs/
    ├── architecture.md        # Detailed architecture documentation
    ├── scheduling.md          # Scheduling algorithms explained
    ├── mpi-bootstrap.md       # How MPI jobs are bootstrapped
    ├── reaper-integration.md  # Reaper backend documentation
    └── migration-from-slurm.md# Guide for Slurm users
```

## CRD Definitions

### MPIJob (primary user-facing CRD)

```yaml
apiVersion: hpc.cscs.ch/v1alpha1
kind: MPIJob
metadata:
  name: my-simulation
spec:
  queue: default              # Which BuboQueue to submit to
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
    command: ["mpirun", "-np", "4", "--hostfile", "/etc/bubo/hostfile", "./app"]
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

### BuboQueue

```yaml
apiVersion: hpc.cscs.ch/v1alpha1
kind: BuboQueue
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

### Phase 1: Foundation (v0.1.0)
**Goal:** A working gang scheduler that can run a simple multi-node MPI job.

- [ ] Set up Cargo workspace with all crates
- [ ] Define `MPIJob` CRD with `kube-derive`
- [ ] Implement basic controller reconciliation loop (watch MPIJobs)
- [ ] Implement node resource tracker (watch Nodes, track allocatable resources)
- [ ] Implement FIFO gang scheduler (all-or-nothing placement)
- [ ] Implement container backend:
  - [ ] Headless Service creation for pod DNS discovery
  - [ ] Hostfile ConfigMap generation
  - [ ] Worker Pod creation with shared SSH keys
  - [ ] Launcher Pod that waits for workers, then runs mpirun
- [ ] Implement walltime enforcement (SIGTERM after walltime, SIGKILL after grace)
- [ ] Basic status reporting on the MPIJob status subresource
- [ ] Integration test: 2-node MPI hello world on a kind cluster with OpenMPI
- [ ] Dockerfile for the controller
- [ ] Example manifests

### Phase 2: Smart Scheduling (v0.2.0)
**Goal:** Topology-aware placement and priority queues.

- [ ] Define `BuboQueue` CRD
- [ ] Implement priority queue with multiple queues
- [ ] Implement topology-aware node scoring:
  - [ ] Parse node labels for switch group / rack / zone
  - [ ] Score candidate node sets by network proximity
  - [ ] Support `maxHops` and `preferSameSwitch` constraints
- [ ] Implement resource reservation (prevent double-booking during bind)
- [ ] Add Prometheus metrics:
  - [ ] Queue depth, wait time, scheduling latency
  - [ ] Node utilization, job completion rate
  - [ ] Gang scheduling success/failure ratio
- [ ] `bubo-cli` basics: `submit`, `queue`, `cancel`, `status`

### Phase 3: Reaper Integration (v0.3.0)
**Goal:** Bare-metal execution backend via Reaper.

- [ ] Define Reaper backend trait implementation
- [ ] Implement communication protocol with Reaper agents on nodes
- [ ] Job script distribution to Reaper agents
- [ ] Process lifecycle management (launch, monitor, terminate)
- [ ] MPI bootstrap for bare-metal mode (PMIx or native srun)
- [ ] Shared resource tracking between container and reaper backends
- [ ] Integration test with Reaper

### Phase 4: Advanced Scheduling (v0.4.0)
**Goal:** Slurm-level scheduling sophistication.

- [ ] Backfill scheduler:
  - [ ] Project when blocked high-priority jobs will start
  - [ ] Allow smaller/shorter jobs to backfill without delaying them
- [ ] Fair-share scheduling:
  - [ ] Track per-project/user node-hour usage
  - [ ] Decay historical usage over configurable half-life
  - [ ] Adjust effective priority based on fair-share factor
- [ ] Job dependencies (`afterOk`, `afterAny`, `afterNotOk`)
- [ ] Job arrays (map to indexed jobs)
- [ ] Preemption support (gang preemption — evict entire jobs)
- [ ] Accounting and usage reports

### Phase 5: Production Hardening (v0.5.0)
**Goal:** Ready for production HPC workloads.

- [ ] Leader election for HA controller deployment
- [ ] Comprehensive error handling and retry logic
- [ ] Graceful degradation when nodes disappear mid-job
- [ ] Webhook validation for MPIJob and BuboQueue CRDs
- [ ] Helm chart for deployment
- [ ] Comprehensive documentation
- [ ] Performance benchmarking (scheduling latency at scale)
- [ ] CI/CD pipeline (GitHub Actions: test, lint, build, container image)

## Coding Conventions

- Use `thiserror` for error types, `anyhow` only in binaries
- Use `tracing` for all logging (structured, with spans for each reconciliation)
- All public API types must derive `Serialize, Deserialize, Clone, Debug`
- CRD types additionally derive `JsonSchema` and `CustomResource`
- Async everywhere with `tokio` runtime
- Tests: unit tests in each module, integration tests in `tests/` directories
- Keep scheduling algorithms in `bubo-scheduler` pure (no K8s dependencies) so
  they can be tested without a cluster

## Key Design Decisions

1. **Separate scheduler crate from controller** — scheduling algorithms should be
   testable as pure functions without needing a Kubernetes cluster. The controller
   crate wires them to the K8s API.

2. **Backend trait abstraction** — the `ExecutionBackend` trait allows clean
   separation between container and bare-metal execution. New backends can be added
   without touching scheduling logic.

3. **Single CRD user experience** — users interact with `MPIJob` only. PodGroups
   and internal resources are implementation details managed by the controller.

4. **Hostfile-based MPI bootstrap (v1)** — simpler than PMIx, works with all MPI
   implementations. Can be enhanced with PMIx support later for Reaper backend.

5. **No sidecar injection** — unlike the MPI Operator, Bubo manages the full
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

Integration tests validate the full lifecycle of MPIJobs running against a real
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
- [ ] Verify CRDs are registered: `kubectl get crd mpijobs.hpc.cscs.ch`
- [ ] Verify CRDs have correct print columns and status subresource

#### 1.2 Controller Startup
- [ ] Build controller container image
- [ ] Deploy controller to kind cluster (Deployment + RBAC)
- [ ] Verify controller pod reaches `Running` state
- [ ] Verify `/healthz` and `/readyz` endpoints respond 200
- [ ] Verify `/metrics` endpoint returns Prometheus metrics

#### 1.3 MPIJob Lifecycle (without real MPI)
- [ ] Create an MPIJob with `backend: container` and a simple `busybox` image
- [ ] Verify status transitions: `Pending` → `Scheduling` → `Running`
- [ ] Verify headless Service is created (`<job>-workers`)
- [ ] Verify hostfile ConfigMap is created with correct content
- [ ] Verify worker Pods are created on correct nodes with correct labels
- [ ] Verify launcher Pod is created with mpirun command
- [ ] Verify `bubo status <job>` shows running state via CLI
- [ ] Verify `bubo queue` lists the job
- [ ] Cancel job with `bubo cancel <job>` and verify cleanup:
  - [ ] Pods deleted
  - [ ] Service deleted
  - [ ] ConfigMap deleted
  - [ ] Status updated to `Cancelled`

#### 1.4 Error Handling
- [ ] Submit MPIJob with 0 nodes → verify immediate `Failed` status
- [ ] Submit MPIJob without container spec → verify `Failed` with validation message
- [ ] Submit MPIJob requesting more nodes than available → verify stays in `Scheduling`
- [ ] Submit MPIJob with walltime "1s" → verify `WalltimeExceeded` after timeout

#### 1.5 BuboQueue
- [ ] Create a BuboQueue CRD
- [ ] Submit jobs to the queue
- [ ] Verify queue depth metrics update

### Tier 2: MPI Workload Tests

#### 2.1 Simple MPI Hello World
- [ ] Deploy a 2-node kind cluster with OpenMPI-capable images
- [ ] Submit a 2-node MPI job that runs `mpi_hello_world`
- [ ] Verify all ranks report output
- [ ] Verify job transitions to `Succeeded`
- [ ] Verify `bubo logs <job>` returns output from all ranks
- [ ] Verify `bubo logs <job> --rank 0` filters correctly

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
        cluster_name: bubo-test
        node_image: kindest/node:v1.31.0

    - name: Build controller image
      run: |
        docker build -f docker/Dockerfile.controller -t bubo-controller:test .
        kind load docker-image bubo-controller:test --name bubo-test

    - name: Install CRDs
      run: kubectl apply -f manifests/crds/

    - name: Deploy controller
      run: |
        kubectl apply -f manifests/rbac/
        kubectl apply -f manifests/deployment.yaml
        kubectl wait --for=condition=available deployment/bubo-controller --timeout=60s

    - name: Run integration tests
      run: cargo test --test integration -- --test-threads=1
```

### Test Utilities

Create a `tests/common/` module with helpers:
- `setup_kind_cluster()` — ensure kind cluster is running
- `install_crds()` — apply CRD manifests
- `wait_for_status(job_name, expected_state, timeout)` — poll job status
- `create_mpijob(spec)` — helper to create jobs from Rust code
- `cleanup_job(job_name)` — delete job and all owned resources
- `assert_pods_with_labels(labels, expected_count)` — verify pod creation
