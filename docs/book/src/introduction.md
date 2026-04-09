# Introduction to Wren

Wren is a lightweight, Slurm-inspired HPC job scheduler for Kubernetes. It brings traditional HPC workload management to cloud-native clusters — enabling multi-node gang scheduling, topology-aware placement, and MPI-native execution without sacrificing Kubernetes principles.

## Why Wren?

Traditional HPC schedulers (Slurm, PBS) are powerful but don't speak Kubernetes. Kubernetes schedulers don't understand multi-node gang scheduling or HPC network topology. Wren bridges this gap by combining:

- **HPC concepts** — gang scheduling, backfill, fair-share, walltime enforcement
- **Kubernetes-native design** — CRDs, controllers, service-based architecture
- **Performance** — Rust-based scheduling algorithms, topology-aware placement
- **Flexibility** — execute jobs in containers (Pods) or on bare metal (via Reaper)

## Key Features

**Scheduling**
- Gang scheduling — all-or-nothing multi-node placement (no partial allocations)
- Topology-aware placement — score nodes by network proximity (switch, rack, zone)
- Priority queues with backfill — larger jobs don't block smaller ones
- Fair-share scheduling — per-user/project priority adjustment with exponential usage decay
- Job dependencies — `afterOk`, `afterAny`, `afterNotOk` with cycle detection

**Execution**
- Multi-node MPI jobs with automatic hostfile generation and SSH key distribution
- Container backend — Kubernetes Pods with user identity mapping (UID/GID)
- Bare-metal backend — execute directly on nodes via Reaper
- Walltime enforcement — SIGTERM after walltime, SIGKILL after grace period

**Operations**
- Multi-user support — WrenUser CRD maps Kubernetes identities to Unix UID/GID
- Leader election for HA controller deployment
- Prometheus metrics — queue depth, wait time, scheduling latency, utilization
- Helm chart for easy deployment

## Comparison with Other Schedulers

| Feature | Slurm | Volcano | MPI Operator | Wren |
|---|---|---|---|---|
| Gang scheduling | Yes | Yes | No | Yes |
| Topology-aware placement | Yes | Partial | No | Yes |
| Backfill scheduling | Yes | No | No | Yes |
| Fair-share | Yes | No | No | Yes |
| Kubernetes-native | No | Yes | Yes | Yes |
| Bare-metal execution | Yes | No | No | Yes (via Reaper) |
| MPI-aware | Yes | Partial | Yes | Yes |

**When to use Wren:**
- Running HPC simulations and scientific computing workloads on Kubernetes
- Multi-node MPI applications requiring gang scheduling
- Clusters with HPC fabrics (Slingshot, InfiniBand, RDMA)
- Teams wanting Slurm-like scheduling without Slurm operations overhead
- Hybrid setups with both container and bare-metal execution

## Architecture Overview

```
                     ┌──────────────────────────┐
                     │     User Interface        │
                     │  wren CLI / CRD / API     │
                     └────────────┬─────────────┘
                                  │
                     ┌────────────▼─────────────┐
                     │   Wren Controller         │
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
              │                   │                   │
              ▼                   ▼                   ▼
         ┌─────────┐        ┌─────────┐        ┌─────────┐
         │ Node 0  │        │ Node 1  │        │ Node N  │
         │ rank 0  │◄─────►│ rank 1  │◄─────►│ rank N  │
         │ (MPI)   │  Fast  │ (MPI)   │  Fast  │ (MPI)   │
         └─────────┘ Network └─────────┘ Network └─────────┘
```

The architecture is built as a Cargo workspace with four crates:

| Crate | Purpose |
|-------|---------|
| `wren-core` | CRD definitions (WrenJob, WrenQueue, WrenUser), shared types, backend trait |
| `wren-scheduler` | Pure scheduling algorithms (no K8s deps) — testable without a cluster |
| `wren-controller` | Kubernetes controller: reconciliation loop, pod/service management, metrics |
| `wren-cli` | CLI tool (`wren submit`, `queue`, `cancel`, `status`, `logs`) |

**Key design decisions:**
- Scheduling algorithms are pure Rust functions — testable without a cluster
- `ExecutionBackend` trait abstracts container vs. bare-metal execution
- Single CRD user experience — users interact with `WrenJob` only
- Hostfile-based MPI bootstrap works with all MPI implementations

## What's Inside a WrenJob

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: my-simulation
  annotations:
    wren.giar.dev/user: alice          # User identity (stamped by webhook)
spec:
  queue: default                       # Which WrenQueue to use
  nodes: 4                             # Number of nodes required
  tasksPerNode: 1                      # MPI ranks per node
  walltime: "4h"                       # Maximum runtime
  priority: 100                        # Higher = more important
  project: climate-sim                 # For fair-share tracking

  # Container execution (Pods)
  container:
    image: nvcr.io/nvidia/pytorch:24.01
    command: ["mpirun", "-np", "4", "--hostfile", "/etc/wren/hostfile", "./train.py"]
    resources:
      limits:
        nvidia.com/gpu: 4
        memory: 64Gi

  # MPI configuration
  mpi:
    implementation: cray-mpich
    sshAuth: true
    fabricInterface: hsn0

  # Topology preferences (optional)
  topology:
    preferSameSwitch: true
    maxHops: 2
    topologyKey: "topology.kubernetes.io/zone"

  # Job dependencies (optional)
  dependencies:
    - type: afterOk
      job: data-preprocessing
```

## User Identity & Security

Every job requires a valid, non-root user identity. The controller enforces this:

1. **Identity capture** — mutating webhook stamps `wren.giar.dev/user` from Kubernetes UserInfo
2. **UID resolution** — controller looks up WrenUser CRD to get Unix UID/GID
3. **Execution identity** — pods/containers run as the correct user (files owned correctly on shared filesystems)

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: alice                    # Must match Kubernetes username
spec:
  uid: 1002                      # Unix UID for file ownership
  gid: 1002                      # Primary GID
  supplementalGroups:            # Additional groups (projects, etc.)
    - 1002
    - 5000
  homeDir: "/home/alice"         # Sets HOME env var in pods
  defaultProject: "climate-sim"  # Default for fair-share accounting
```

This makes compute nodes **stateless from an identity perspective** — no LDAP client, no SSSD. Identity flows entirely from the Wren controller.

## Disclaimer

Wren is an experimental, personal project under continuous development with no stability guarantees. No support of any kind is provided. Unless you fully understand what Wren does and how it works, you probably don't want to run it in production.

That said, the code is open — read it, send PRs, and build on it.

## Next Steps

- **[Installation](getting-started/installation.md)** — Set up Wren in your cluster
- **[Quick Start](getting-started/quick-start.md)** — Run your first job in 5 minutes
- **[Submitting Jobs](../user-guide/submitting-jobs.md)** — Learn the WrenJob CRD
