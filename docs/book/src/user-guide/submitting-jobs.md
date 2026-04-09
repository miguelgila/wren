# Submitting Jobs

Wren jobs are submitted as WrenJob custom resources. This guide covers the spec fields, execution backends, and common patterns.

## Basic Job Structure

Every WrenJob requires:
- `nodes` — how many nodes the job needs
- `backend` — where to run: `container` (Kubernetes pods) or `reaper` (bare-metal)
- Backend-specific config (`container` or `reaper`)

Optional fields control:
- `queue` — which WrenQueue to submit to (default: `default`)
- `priority` — job priority, higher number runs first (default: 50)
- `walltime` — maximum runtime (e.g., `4h`, `30m`, `1d`)
- `tasksPerNode` — MPI ranks per node (default: 1)
- `mpi` — MPI configuration and bootstrap method
- `topology` — placement preferences
- `dependencies` — job dependencies (afterOk, afterAny, afterNotOk)
- `project` — project name for fair-share grouping

## Container Backend

The container backend runs jobs as Kubernetes Pods. Wren:
- Creates a headless Service for pod DNS discovery
- Generates a hostfile ConfigMap with all pod hostnames
- Creates launcher and worker Pods
- Mounts shared SSH keys for `mpirun` communication
- Enforces walltime via SIGTERM/SIGKILL

### Container Backend Example: Simple MPI Job

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: hello-mpi
spec:
  nodes: 4
  queue: default
  priority: 50
  walltime: "1h"
  tasksPerNode: 1

  container:
    image: "nvcr.io/nvidia/pytorch:24.01-py3"
    command: ["mpirun", "-np", "4", "--hostfile", "/etc/wren/hostfile", "./hello_mpi"]
    hostNetwork: true  # Required for InfiniBand/Slingshot
    resources:
      limits:
        nvidia.com/gpu: "4"
        memory: "64Gi"
      requests:
        cpu: "16000m"

    env:
      - name: NCCL_DEBUG
        value: INFO
      - name: MPICH_OFI_PROVIDER
        value: gni

    volumeMounts:
      - name: shared-data
        mountPath: /data

  mpi:
    implementation: openmpi  # or cray-mpich, intel-mpi
    sshAuth: true
    fabricInterface: hsn0

  topology:
    preferSameSwitch: true
    maxHops: 2
```

### Container Spec Fields

- `image` — container image (required)
- `command` — entry point command, split by spaces
- `args` — additional arguments to command
- `hostNetwork` — use host network namespace (required for RDMA fabrics)
- `resources.limits` — Kubernetes resource limits (cpu, memory, nvidia.com/gpu, etc.)
- `resources.requests` — Kubernetes resource requests
- `env` — environment variables (list of name/value pairs)
- `volumeMounts` — mounts for ConfigMaps, Secrets, hostPath, emptyDir

The controller automatically injects:
- `/etc/wren/hostfile` — ConfigMap with pod hostnames
- `/etc/wren/ssh` — Shared SSH keys for mpirun (when `sshAuth: true`)
- `USER`, `HOME`, `LOGNAME` — from WrenUser identity
- MPI implementation env vars (MPICH_OFI_*, UCX_NET_DEVICES, etc.)
- Distributed training env vars (MASTER_ADDR, RANK, WORLD_SIZE, LOCAL_RANK)

## Reaper Backend

The reaper backend runs jobs on bare-metal nodes via the Reaper agent. Wren:
- Creates ReaperPod CRs for each rank
- Distributes job scripts and environment variables
- Mounts hostfile and SSH keys as ConfigMaps
- Manages process lifecycle via ReaperPod status

### Reaper Backend Example: Shell Script

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: climate-sim
spec:
  nodes: 8
  queue: compute
  priority: 100
  walltime: "24h"
  tasksPerNode: 1
  backend: reaper

  reaper:
    script: |
      #!/bin/bash
      set -e
      module load cray-mpich
      export MPICH_OFI_PROVIDER=gni
      cd $SLURM_SUBMIT_DIR
      srun ./climate_model.x

    workingDir: /scratch/user/climate

    environment:
      SCRATCH: /scratch/project
      MODEL_DATA: /global/data/climate
      OMP_NUM_THREADS: "16"

    volumes:
      - name: model-data
        mountPath: /global/data
        hostPath: /storage/models
      - name: config
        mountPath: /etc/model
        configMap: climate-config
      - name: scratch
        mountPath: /scratch
        emptyDir: true
```

### Reaper Backend Example: Binary Command

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: distributed-training
spec:
  nodes: 4
  tasksPerNode: 1
  backend: reaper
  walltime: "12h"

  reaper:
    command: ["python3", "/opt/train/train.py"]
    args: ["--epochs", "100", "--batch-size", "256"]

    environment:
      PYTHONUNBUFFERED: "1"
      CUDA_VISIBLE_DEVICES: "0,1,2,3"

    volumes:
      - name: training-code
        mountPath: /opt/train
        configMap: pytorch-training
        readOnly: true
      - name: datasets
        mountPath: /data
        hostPath: /mnt/datasets
      - name: checkpoints
        mountPath: /checkpoints
        emptyDir: true
```

### Reaper Spec Fields

- `command` — executable path and arguments (array)
- `args` — additional arguments to command
- `script` — shell script to execute (ignored if `command` is set)
- `workingDir` — working directory for the job
- `environment` — environment variables (map of name: value)
- `volumes` — volumes to mount
  - `name` — volume identifier
  - `mountPath` — where to mount inside the job
  - `readOnly` — mount read-only (default: false)
  - `configMap` — ConfigMap name to mount
  - `secret` — Secret name to mount
  - `hostPath` — host path to mount
  - `emptyDir` — use emptyDir volume
- `dnsMode` — DNS resolution: "host" (default) or "kubernetes"
- `overlayName` — shared overlay filesystem group (for distributed storage)

## Walltime

Walltime specifies the maximum runtime for a job. Formats:
- `30m` — 30 minutes
- `4h` — 4 hours
- `1d` — 1 day
- `2h30m` — 2 hours 30 minutes
- `3600` — raw seconds

When walltime expires:
1. Job receives SIGTERM (graceful termination)
2. After grace period, job receives SIGKILL (forced termination)
3. Job state transitions to `WalltimeExceeded`

## Job Nodes and Tasks Per Node

- `nodes` — number of nodes to allocate (gang scheduling: all-or-nothing)
- `tasksPerNode` — MPI ranks per node (default: 1)

Example: `nodes: 4, tasksPerNode: 2` allocates 4 nodes with 8 total MPI ranks.

For the container backend, `tasksPerNode` is informational (the user's `mpirun` command in the image controls actual rank count).

For the Reaper backend, `tasksPerNode > 1` is planned for Phase 3b (CPU affinity, GPU binding per rank).

## Queue and Priority

- `queue` — submit to a specific WrenQueue (default: `default`)
- `priority` — numeric priority (higher = runs first). Default: 50

Within a queue, jobs are scheduled by:
1. Priority (highest first)
2. Arrival time (FIFO within same priority)
3. Backfill (smaller jobs can run earlier if they don't delay higher-priority reservations)

## Project for Fair-Share

The `project` field groups jobs for fair-share accounting. Optional; if not set, the job is grouped by the user's identity.

```yaml
spec:
  project: climate-sim
```

## Submitting a Job

```bash
kubectl apply -f job.yaml
```

Or via the CLI:

```bash
wren submit job.yaml
```

## Checking Job Status

```bash
# Get the job ID (assigned by Wren)
kubectl get wrenjob hello-mpi
# Output:
# NAME        JOBID   STATE       NODES   QUEUE     AGE
# hello-mpi   42      Running     4       default   2m

# Watch the job
kubectl get wrenjob hello-mpi -w

# Detailed status
kubectl describe wrenjob hello-mpi

# Check pod status (container backend)
kubectl get pods -l wren.giar.dev/job=hello-mpi
```

Via CLI:

```bash
wren status 42
wren queue
wren logs 42
wren logs 42 --rank 0  # specific rank
```

## Cancelling a Job

```bash
kubectl delete wrenjob hello-mpi
# or
wren cancel 42
```

Cancellation:
- Terminates running pods/processes
- Cleans up Service, ConfigMap, and Pod resources
- Updates job status to `Cancelled`

## Job Lifecycle States

- `Pending` — job created, waiting for resources
- `Scheduling` — controller is deciding node placement
- `Running` — pods/processes are executing
- `Succeeded` — job completed successfully (exit code 0)
- `Failed` — job failed (non-zero exit code or error)
- `Cancelled` — job was cancelled by user
- `WalltimeExceeded` — job hit the walltime limit
