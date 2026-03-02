# Wren

**A lightweight, Slurm-inspired HPC job scheduler for Kubernetes — built in Rust.**

Wren provides multi-node gang scheduling, topology-aware placement, priority queues with backfill, and MPI-native job execution as a Kubernetes-native controller. Optionally execute jobs on bare metal via [Reaper](https://github.com/miguelgila/reaper).

> **Early development** — Wren is under active development and not yet ready for production use.

## Why Wren?

Traditional HPC schedulers (Slurm, PBS) are powerful but don't speak Kubernetes.
Kubernetes schedulers don't understand multi-node gang scheduling or HPC network topology.
Wren bridges this gap.

| Feature | Slurm | Volcano | MPI Operator | **Wren** |
|---|---|---|---|---|
| Gang scheduling | ✅ | ✅ | ❌ | ✅ |
| Topology-aware placement | ✅ | Partial | ❌ | ✅ |
| Backfill scheduling | ✅ | ❌ | ❌ | ✅ |
| Fair-share | ✅ | ❌ | ❌ | ✅ |
| Kubernetes-native | ❌ | ✅ | ✅ | ✅ |
| Bare-metal execution | ✅ | ❌ | ❌ | ✅ (via Reaper) |
| MPI-aware | ✅ | Partial | ✅ | ✅ |

## Quick Start

```yaml
apiVersion: wren.scops-hpc.com/v1alpha1
kind: MPIJob
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
        memory: 64Gi
  mpi:
    implementation: openmpi
    sshAuth: true
  topology:
    preferSameSwitch: true
```

```bash
# Submit a job
wren submit job.yaml

# Check the queue
wren queue

# Check job status
wren status my-simulation

# View logs for a specific rank
wren logs my-simulation --rank 0

# Cancel a job
wren cancel my-simulation
```

## Architecture

See [CLAUDE.md](./CLAUDE.md) for the full project plan, architecture, and development roadmap.

## Building

```bash
# Build all crates
cargo build --workspace

# Run tests
cargo test --workspace

# Build container image
docker build -f docker/Dockerfile.controller -t wren-controller:latest .
```

## License

Apache-2.0
