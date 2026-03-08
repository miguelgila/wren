# Wren

[![CI](https://github.com/miguelgila/wren/actions/workflows/ci.yaml/badge.svg?branch=main)](https://github.com/miguelgila/wren/actions/workflows/ci.yaml)
[![Integration](https://github.com/miguelgila/wren/actions/workflows/integration.yml/badge.svg?branch=main)](https://github.com/miguelgila/wren/actions/workflows/integration.yml)
[![Coverage](https://codecov.io/gh/miguelgila/wren/branch/main/graph/badge.svg)](https://codecov.io/gh/miguelgila/wren)

**A lightweight, Slurm-inspired HPC job scheduler for Kubernetes — built in Rust.**

Wren provides multi-node gang scheduling, topology-aware placement, priority queues with backfill, and MPI-native job execution as a Kubernetes-native controller. Optionally execute jobs on bare metal via [Reaper](https://github.com/miguelgila/reaper).

## Disclaimer

Wren is an experimental, personal project built to explore HPC scheduling on Kubernetes with AI-assisted development. It is under continuous development with no stability guarantees. No support of any kind is provided. Unless you fully understand what Wren does and how it works, you probably don't want to run it in production. That said, the code is open — feel free to read it and send PRs.

## Why Wren?

Traditional HPC schedulers (Slurm, PBS) are powerful but don't speak Kubernetes.
Kubernetes schedulers don't understand multi-node gang scheduling or HPC network topology.
Wren bridges this gap.

| Feature | Slurm | Volcano | MPI Operator | **Wren** |
|---|---|---|---|---|
| Gang scheduling | Yes | Yes | No | Yes |
| Topology-aware placement | Yes | Partial | No | Yes |
| Backfill scheduling | Yes | No | No | Yes |
| Fair-share | Yes | No | No | Yes |
| Kubernetes-native | No | Yes | Yes | Yes |
| Bare-metal execution | Yes | No | No | Yes (via Reaper) |
| MPI-aware | Yes | Partial | Yes | Yes |

**What Wren provides:**
- Gang scheduling (all-or-nothing multi-node placement)
- Topology-aware node scoring (switch groups, hops, zones)
- Priority queues with backfill scheduling
- Fair-share priority adjustment with exponential decay
- Job dependencies (`afterOk`, `afterAny`, `afterNotOk`)
- MPI bootstrap (hostfile generation, SSH key distribution)
- Walltime enforcement with configurable grace periods
- Prometheus metrics (queue depth, scheduling latency, utilization)
- CLI with Slurm-like UX (`wren submit`, `wren queue`, `wren status`, `wren cancel`, `wren logs`)

## Quick Start

### 1. Spin Up a Playground Cluster

If you don't have a Kubernetes cluster available, the quickstart script creates a local [Kind](https://kind.sigs.k8s.io/) cluster with topology-labeled workers, installs CRDs, builds and deploys the controller, and runs all example jobs:

```bash
./examples/quickstart.sh
```

Options:

```bash
./examples/quickstart.sh --release 0.3.0  # Use a specific release version
./examples/quickstart.sh --dev            # Build controller from source
./examples/quickstart.sh --no-cluster     # Skip cluster creation (reuse existing)
./examples/quickstart.sh --cleanup        # Delete the Kind cluster
```

If you already have a cluster and prefer to set things up manually, continue with the steps below.

### 2. Install CRDs and Deploy the Controller

**Using Helm (recommended):**

```bash
helm install wren charts/wren --namespace wren-system --create-namespace
```

**Using raw manifests:**

```bash
kubectl apply -f manifests/crds/
kubectl apply -f manifests/rbac/
kubectl apply -f manifests/deployment.yaml
```

### 3. Create a Queue

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: default
spec:
  maxNodes: 128
  maxWalltime: "24h"
  defaultPriority: 50
  backfill:
    enabled: true
    lookAhead: "2h"
```

```bash
kubectl apply -f queue.yaml
```

### 4. Register Users

Every job requires a valid user identity — Wren never runs jobs as root or anonymous. Create a `WrenUser` for each person who will submit jobs:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: miguel              # must match the Kubernetes username
spec:
  uid: 1001                 # Unix UID for file ownership
  gid: 1001                 # Primary GID
  supplementalGroups:       # Additional groups (project groups, etc.)
    - 1001
    - 5000
  homeDir: "/home/miguel"   # Sets HOME env var in pods
  defaultProject: "climate-sim"
```

```bash
kubectl apply -f wrenuser.yaml
```

WrenUser is cluster-scoped (no namespace) — one per user, shared across all namespaces. The controller uses it to set `runAsUser`/`runAsGroup` on pods and inject `USER`/`HOME` env vars.

**Adding users in bulk** — for teams with existing LDAP/AD, a CronJob can sync users automatically:

```bash
# One-liner: export LDAP users to WrenUser CRDs
ldapsearch -x -b "ou=people,dc=example,dc=org" \
  "(objectClass=posixAccount)" uid uidNumber gidNumber homeDirectory | \
  awk '/^uid:/{u=$2} /^uidNumber:/{n=$2} /^gidNumber:/{g=$2} /^homeDirectory:/{h=$2; \
    printf "apiVersion: wren.giar.dev/v1alpha1\nkind: WrenUser\nmetadata:\n  name: %s\nspec:\n  uid: %s\n  gid: %s\n  homeDir: \"%s\"\n---\n", u, n, g, h}' | \
  kubectl apply -f -
```

Or define users in Helm values for GitOps-managed clusters:

```yaml
# values.yaml
users:
  - name: miguel
    uid: 1001
    gid: 1001
    homeDir: /home/miguel
  - name: alice
    uid: 1002
    gid: 1002
    homeDir: /home/alice
```

### 5. Submit a Job

Create a `WrenJob` — the primary user-facing CRD (equivalent to Slurm's `sbatch`):

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: my-simulation
spec:
  queue: default
  nodes: 4
  tasksPerNode: 1
  walltime: "4h"
  container:
    image: my-registry/my-mpi-app:latest
    command: ["mpirun", "-np", "4", "--hostfile", "/etc/wren/hostfile", "./app"]
    resources:
      limits:
        memory: "64Gi"
  mpi:
    implementation: openmpi
    sshAuth: true
  topology:
    preferSameSwitch: true
```

```bash
# Submit the job
wren submit job.yaml

# Check the queue
wren queue

# Check job status
wren status my-simulation

# View logs (all ranks or a specific one)
wren logs my-simulation
wren logs my-simulation --rank 0

# Cancel a job
wren cancel my-simulation
```

### 6. Simple (Non-MPI) Jobs

For single-node or non-MPI workloads, just omit the `mpi` section. Wren runs the command directly without a launcher/worker pattern:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: hello-world
spec:
  nodes: 1
  container:
    image: busybox:latest
    command: ["echo"]
    args: ["Hello from Wren"]
```

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
              ▼                   ▼                   ▼
         ┌─────────┐        ┌─────────┐        ┌─────────┐
         │ Node 0  │◄──────►│ Node 1  │◄──────►│ Node N  │
         │ rank 0  │  MPI   │ rank 1  │  MPI   │ rank N  │
         └─────────┘        └─────────┘        └─────────┘
```

Wren is structured as a Cargo workspace with four crates:

| Crate | Purpose |
|-------|---------|
| `wren-core` | CRD definitions (`WrenJob`, `WrenQueue`, `WrenUser`), shared types, backend trait |
| `wren-scheduler` | Pure scheduling algorithms (gang, topology, backfill, fair-share, dependencies) — no K8s deps |
| `wren-controller` | Kubernetes controller: reconciliation loop, pod/service management, metrics |
| `wren-cli` | CLI tool (`wren submit`, `queue`, `cancel`, `status`, `logs`) |

**Key design decisions:**
- Scheduling algorithms are pure Rust functions — testable without a cluster
- `ExecutionBackend` trait abstracts container vs. bare-metal (Reaper) execution
- Single CRD UX — users interact with `WrenJob` only; internal resources (Services, ConfigMaps, Pods) are managed automatically
- Hostfile-based MPI bootstrap works with all MPI implementations

## Features

- **Gang scheduling** — all-or-nothing multi-node placement
- **Topology-aware placement** — score nodes by network proximity (switch group, rack, zone)
- **Priority queues** — multiple queues with configurable limits and priorities
- **Backfill scheduling** — smaller jobs can run ahead without delaying higher-priority reservations
- **Fair-share** — per-user/project priority adjustment with exponential usage decay
- **Job dependencies** — `afterOk`, `afterAny`, `afterNotOk` with cycle detection
- **MPI bootstrap** — automatic hostfile generation, SSH key distribution, headless Service
- **Multi-user identity** — WrenUser CRD maps K8s users to Unix UID/GID; pods run as the correct user
- **Walltime enforcement** — SIGTERM after walltime, SIGKILL after grace period
- **Completed job preservation** — 24h TTL for log retrieval after completion
- **Leader election** — HA controller deployment
- **Prometheus metrics** — queue depth, wait time, scheduling latency, utilization
- **Helm chart** — deploy with `helm install`
- **Container image** — amd64 on GHCR
- **Integration tests** — end-to-end validated with Kind cluster

## Examples

The [`manifests/examples/`](manifests/examples/) directory contains ready-to-use job definitions:

| Example | Description |
|---------|-------------|
| [`simple-mpi.yaml`](manifests/examples/simple-mpi.yaml) | Basic 2-node job with busybox |
| [`gpu-training.yaml`](manifests/examples/gpu-training.yaml) | Multi-node GPU training job |
| [`reaper-job.yaml`](manifests/examples/reaper-job.yaml) | Bare-metal execution via Reaper |
| [`wrenuser.yaml`](manifests/examples/wrenuser.yaml) | User identity mapping (UID/GID) |

## Requirements

**Cluster:**
- Kubernetes 1.28+ with a working container runtime
- `kubectl` access with permissions to create CRDs and ClusterRoles

**Building from source:**
- [Rust](https://www.rust-lang.org/tools/install) (2021 edition, stable)
- Docker (for container image builds and cross-compilation)

**Integration tests:**
- [kind](https://kind.sigs.k8s.io/) v0.20+
- `kubectl`
- Docker

## Building

```bash
# Build all crates
cargo build --workspace

# Run unit tests
cargo test --workspace

# Lint
cargo clippy --workspace --all-targets -- -D warnings

# Build container image (multi-arch)
docker build -f docker/Dockerfile.controller -t wren-controller:latest .
```

Cross-compile static musl binaries (for Linux nodes, from macOS):

```bash
docker run --rm -v "$(pwd)":/home/rust/src \
  messense/rust-musl-cross:x86_64-musl \
  cargo build --workspace --release
```

## Testing

```bash
# Unit tests
cargo test --workspace

# Integration tests (spins up a Kind cluster)
./scripts/run-integration-tests.sh
```

## Releases

Every merged PR automatically triggers a patch version bump, changelog generation (via [git-cliff](https://git-cliff.org/)), and a GitHub Release with:
- Static musl binaries for `x86_64` and `aarch64`
- SHA-256 checksums signed with [cosign](https://github.com/sigstore/cosign) (keyless, via GitHub OIDC)
- Container image pushed to GHCR (amd64)

To skip the automatic release, add the `skip-release` label to the PR. For minor/major bumps, use the **Manual Release** workflow.

## Contributing

See [CLAUDE.md](./CLAUDE.md) for the full project plan, architecture, CRD specifications, and development roadmap.

## License

GPL-3.0-or-later
