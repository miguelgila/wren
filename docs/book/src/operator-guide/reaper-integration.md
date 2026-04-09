# Reaper Integration

Wren can dispatch jobs to bare-metal compute nodes via the [Reaper](https://github.com/miguelgila/reaper) backend. This page describes how to deploy and configure Reaper alongside Wren.

## Overview

The **Reaper backend** allows Wren to execute MPI jobs directly on bare-metal nodes without Kubernetes Pods. This is useful for:

- **True HPC clusters** with tightly-integrated hardware (InfiniBand, custom topologies)
- **Avoiding container overhead** in performance-critical workloads
- **Native MPI execution** without container runtime complications
- **Slingshot/RDMA networks** that work best with direct process access

Wren schedules the job, Reaper executes it.

## Architecture

```
┌──────────────────────────────────────────┐
│  Wren Controller (K8s)                   │
│  - Schedules jobs (placement algorithm)  │
│  - Monitors job status                   │
│  - Enforces walltime, cleanup            │
└─────────────────┬────────────────────────┘
                  │
                  │ Creates ReaperPod CRDs
                  │ (1 per rank)
                  ▼
     ┌────────────────────────────┐
     │  ReaperPod CRD             │
     │  (k8s custom resource)     │
     └────────────┬───────────────┘
                  │
                  │ Observed by Reaper agent
                  │ on each bare-metal node
                  ▼
     ┌────────────────────────────┐
     │  Reaper Agent              │
     │  (per compute node)        │
     │  - Launches processes      │
     │  - Tracks status           │
     │  - Reports back            │
     └────────────┬───────────────┘
                  │
                  │ Executes on bare metal
                  ▼
     ┌────────────────────────────┐
     │  MPI Job (actual process)  │
     │  running on compute node   │
     └────────────────────────────┘
```

## Prerequisites

- Kubernetes cluster with etcd (standard)
- At least one non-Kubernetes compute node with:
  - Linux kernel (any modern version)
  - SSH access from controller (for setup only)
  - Network connectivity to Kubernetes cluster
  - Optional: MPI implementation (OpenMPI, Cray-MPICH, Intel-MPI)

## Installation

### 1. Deploy Reaper Helm Subchart

Enable the Reaper subchart when installing Wren:

```bash
helm install wren charts/wren/ \
  --namespace wren-system \
  --create-namespace \
  --set reaper.enabled=true
```

This deploys:
- Wren controller (same as before)
- Reaper DaemonSet (one agent per compute node)
- RBAC for Reaper (permissions to create/read ReaperPods)
- ConfigMap with Reaper configuration

### 2. Label Compute Nodes (Optional)

If you want to restrict Reaper agents to specific nodes (not master/control nodes):

```bash
kubectl label node compute-01 compute-01 compute-02 wren-reaper=enabled

# Then in values.yaml or Helm:
helm install wren charts/wren/ \
  --set reaper.enabled=true \
  --set reaper.nodeSelector.wren-reaper=enabled
```

This prevents Reaper agents from running on control plane nodes.

### 3. Verify Reaper Agents are Running

```bash
kubectl get pods -n wren-system -l app.kubernetes.io/name=reaper

# Check logs
kubectl logs -n wren-system -l app.kubernetes.io/name=reaper -f
```

Once agents report "ready", you can submit Reaper-backed jobs.

## Submitting a Reaper Job

Use `backend: reaper` in your WrenJob spec:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: mpi-on-reaper
spec:
  queue: default
  nodes: 2
  tasksPerNode: 1
  backend: reaper

  reaper:
    # Command to run on each node
    command: ["python3"]
    args: ["/opt/train.py"]

    # Or use a shell script
    # script: |
    #   #!/bin/bash
    #   module load cray-mpich
    #   srun ./my_app

    # Environment variables
    environment:
      MPI_IMPL: cray-mpich
      SCRATCH: /scratch/project

    # Working directory (default: /root)
    working_dir: /opt

    # Volume mounts
    volumes:
      - name: hostfile
        config_map: mpi-hostfile
        mount_path: /etc/wren
        read_only: true

  mpi:
    implementation: cray-mpich
    ssh_auth: false         # Reaper doesn't need SSH
    fabric_interface: hsn0  # High-speed network interface

  topology:
    prefer_same_switch: true
    max_hops: 2
```

Submit:

```bash
kubectl apply -f mpi-on-reaper.yaml

# Monitor
wren status mpi-on-reaper
wren logs mpi-on-reaper
```

## ReaperPod CRD

When a job is scheduled for Reaper execution, Wren creates one **ReaperPod** per rank. Example:

```yaml
apiVersion: reaper.giar.dev/v1alpha1
kind: ReaperPod
metadata:
  name: mpi-on-reaper-rank-0
  namespace: default
spec:
  # Which node to execute on
  nodeName: compute-01

  # User identity
  username: alice
  uid: 1001
  gid: 1001
  supplementalGroups:
    - 1001
    - 5000
  homeDir: /home/alice

  # Command to execute
  command:
    - python3
    - /opt/train.py
  args: []

  # Environment variables (includes MPI vars)
  environment:
    - name: MPI_IMPL
      value: cray-mpich
    - name: SCRATCH
      value: /scratch/project
    - name: RANK
      value: "0"
    - name: WORLD_SIZE
      value: "2"
    - name: MASTER_ADDR
      value: compute-01
    - name: MASTER_PORT
      value: "29500"
    - name: WREN_HOSTFILE
      value: /etc/wren/hostfile

  # Working directory
  workingDir: /opt

  # Volumes
  volumes:
    - name: hostfile
      configMap:
        name: mpi-on-reaper-hostfile
      mountPath: /etc/wren
      readOnly: true

  # Walltime enforcement
  walltime: 3600  # seconds
  gracePeriod: 60

status:
  # Updated by Reaper agent
  phase: Running        # Pending, Running, Succeeded, Failed, Terminated
  exitCode: null
  startTime: "2025-04-09T12:00:00Z"
  message: "Process running (pid 1234)"
```

The Reaper agent on `compute-01` observes this resource, launches the process, and updates `.status` continuously.

## Environment Variables Injected

Wren automatically injects these environment variables into every ReaperPod:

### Wren Metadata

| Variable | Example | Purpose |
|----------|---------|---------|
| `WREN_JOB_NAME` | `mpi-on-reaper` | Job identifier |
| `WREN_NUM_NODES` | `2` | Number of nodes in the job |
| `WREN_TOTAL_RANKS` | `2` | Total MPI ranks (nodes × tasksPerNode) |
| `WREN_HOSTFILE` | `/etc/wren/hostfile` | Path to hostfile (if provided) |

### User Identity

| Variable | Example | Purpose |
|----------|---------|---------|
| `USER` | `alice` | Unix username |
| `LOGNAME` | `alice` | Login name (same as USER) |
| `HOME` | `/home/alice` | Home directory |

### Distributed Training (PyTorch, TensorFlow)

| Variable | Example | Purpose |
|----------|---------|---------|
| `RANK` | `0` | Global rank of this process (0 to WORLD_SIZE-1) |
| `WORLD_SIZE` | `2` | Total number of processes |
| `MASTER_ADDR` | `compute-01` | Hostname/IP of rank 0 (for bootstrapping) |
| `MASTER_PORT` | `29500` | Port on rank 0 (for bootstrapping) |
| `LOCAL_RANK` | `0` | Rank on this node (only with tasksPerNode > 1) |

### MPI Implementation-Specific

**For Cray-MPICH:**

| Variable | Example |
|----------|---------|
| `MPICH_OFI_STARTUP_CONNECT` | `all` |
| `MPICH_OFI_NUM_NICS` | `1` |
| `MPICH_OFI_IFNAME` | `hsn0` (if mpi.fabricInterface set) |

**For OpenMPI:**

| Variable | Example |
|----------|---------|
| `I_MPI_FABRICS_LIST` | `hsn0` (if mpi.fabricInterface set) |

**For Intel MPI:**

| Variable | Example |
|----------|---------|
| `I_MPI_FABRICS_LIST` | `hsn0` (if mpi.fabricInterface set) |

## Hostfile Format

For MPI jobs, Wren distributes a hostfile listing all nodes and ranks:

```
compute-01 slots=1
compute-02 slots=1
```

The file is mounted at the path specified in `WREN_HOSTFILE` and is accessible to the process.

Use with `mpirun`:

```bash
mpirun -np 2 --hostfile $WREN_HOSTFILE ./app
```

Or with `srun` (Cray):

```bash
srun --overlap ./app
```

## Reaper Configuration

The Reaper agents are configured via the Helm values:

```yaml
reaper:
  enabled: true
  image:
    repository: ghcr.io/miguelgila/reaper-agent
    tag: ""         # Defaults to Chart.appVersion
    pullPolicy: IfNotPresent

  # Node selector (to restrict to compute nodes only)
  nodeSelector: {}

  # Tolerations (to run on tainted nodes)
  tolerations: []

  # Resource requests
  resources:
    requests:
      cpu: 100m
      memory: 128Mi
    limits:
      cpu: 500m
      memory: 256Mi
```

## Job Lifecycle with Reaper

### Submission

```
WrenJob created
  ↓
Wren validates spec (backend: reaper)
  ↓
Wren resolves user identity (WrenUser CRD)
  ↓
Wren state → Scheduling
```

### Scheduling

```
Scheduler finds placement (e.g., compute-01, compute-02)
  ↓
Wren state → Running
```

### Execution

```
Wren creates ReaperPod for rank 0 on compute-01
Wren creates ReaperPod for rank 1 on compute-02
  ↓
Reaper agent on compute-01 observes ReaperPod for rank 0
Reaper agent on compute-02 observes ReaperPod for rank 1
  ↓
Agents launch processes (respecting uid/gid/environment)
  ↓
Agents update ReaperPod.status with phase=Running, pid=XXXX
  ↓
Wren polls ReaperPod.status, sees Running, maintains job state
```

### Walltime Enforcement

```
Wren calculates deadline = creationTime + walltime
  ↓
At (deadline - grace_period):
  Wren sends SIGTERM to process via Reaper agent
  ↓
At deadline:
  Wren sends SIGKILL to process via Reaper agent
  ↓
Reaper reports process terminated
  ↓
Wren state → WalltimeExceeded
```

### Termination

```
User cancels job (wren cancel) or job completes
  ↓
Wren deletes ReaperPod resources
  ↓
Reaper agent sees deletion, cleans up process
  ↓
Wren calls backend.cleanup()
  ↓
Wren state → Cancelled / Succeeded / Failed
```

## Overlays and Distributed Filesystems

For HPC workloads, compute nodes often need access to a shared filesystem (Lustre, NFS, GPFS). The Reaper backend supports:

### 1. Host Path Mount

Mount a directory from the node's local filesystem:

```yaml
volumes:
  - name: scratch
    mount_path: /scratch
    host_path: /scratch
    read_only: false
```

### 2. ConfigMap (for small files)

Distribute config files:

```yaml
volumes:
  - name: hostfile
    mount_path: /etc/wren
    config_map: mpi-hostfile
    read_only: true
```

### 3. Overlay Filesystem

For shared state across ranks (e.g., distributed checkpoints), use a named overlay:

```yaml
reaper:
  overlay_name: my-job-overlay
```

Wren creates an overlay filesystem, mounts it on each node, and each rank can read/write to it. On job completion, the overlay can be archived or discarded.

(Overlay support requires Reaper v0.2+)

## RBAC for Reaper

The Reaper DaemonSet needs permissions to:

| Resource | Verbs | Why |
|----------|-------|-----|
| `reaperpods` | `get, list, watch, create, delete` | Observe and manage job tasks |
| `reaperpods/status` | `get, patch, update` | Report status changes |
| `configmaps` | `get, list, watch` | Read mounted config (hostfile, etc.) |
| `secrets` | `get, list, watch` | Read mounted secrets (if needed) |

The Helm chart automatically creates a ClusterRole with these permissions.

## Troubleshooting Reaper Jobs

### Job stuck in Scheduling

```bash
kubectl get wrenjob mpi-on-reaper -o yaml
# Check spec.conditions for validation errors
# Check if topology constraints are satisfied
```

### ReaperPod not transitioning to Running

```bash
# Check if Reaper agent is running
kubectl get pods -n wren-system -l app=reaper -o wide

# Check Reaper agent logs
kubectl logs -n wren-system -l app=reaper | grep ReaperPod

# Check ReaperPod status
kubectl get reaperpod -o yaml
```

Common causes:
- Reaper agent not running on the scheduled node
- Reaper agent incompatible with ReaperPod version
- User identity not resolvable on compute node (e.g., missing LDAP)

### Process launches but exits immediately

```bash
# Check job logs
wren logs mpi-on-reaper --rank 0

# Check ReaperPod status message
kubectl get reaperpod mpi-on-reaper-rank-0 -o yaml | grep status.message
```

Common causes:
- Script not found or not executable
- Walltime too short
- Working directory doesn't exist
- MPI implementation not installed on node

### Performance is worse than native

```bash
# Check if process is actually running on bare metal
ps aux | grep train.py

# Check network performance
iperf3 between nodes

# Check if MPI fabric interface is correct
ethtool -i hsn0  # verify interface exists
```

Common causes:
- Container overhead still present (verify backend: reaper is used)
- Network interface name wrong (mpi.fabricInterface mismatch)
- MPI binding disabled (check MPI env vars)

## Performance Considerations

Reaper backends are optimized for HPC:

- **No Container Overhead** — processes run directly on the OS, no cgroup/namespace overhead
- **Native MPI** — uses system MPI library, optimized for InfiniBand/Slingshot
- **Direct Access** — processes can use RDMA, fine-grained process management
- **Scaling** — tested with 100+ nodes, thousands of ranks

Typical improvements over Pod-based execution:
- 5-10% lower latency (no container init overhead)
- 10-15% higher throughput (direct NIC access)
- Negligible memory overhead (no kubelet proxy)

## Migration from Container to Reaper

To migrate existing jobs:

```bash
# Before: container backend
backend: container
container:
  image: mycompany/mpi-image:latest
  command: ["mpirun", "-np", "4", "--hostfile", "/etc/wren/hostfile", "./app"]

# After: reaper backend
backend: reaper
reaper:
  command: ["mpirun", "-np", "4", "--hostfile", "$WREN_HOSTFILE", "./app"]
  environment:
    PATH: /usr/local/bin:/usr/bin:$PATH  # Ensure MPI is in PATH
```

Ensure:
1. MPI is installed on compute nodes (not in container)
2. Application is built for compute node architecture
3. Shared filesystem is mounted and accessible
4. User identity is available on compute nodes (LDAP or WrenUser CRD)
