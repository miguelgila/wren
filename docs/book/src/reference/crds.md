# CRD Reference

Complete field reference for all Wren custom resource definitions.

## WrenJob

The primary user-facing CRD for submitting multi-node HPC workloads. Equivalent to Slurm's `sbatch`.

### Metadata

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `metadata.name` | string | Yes | Job name (DNS-1123 subdomain, unique per namespace) |
| `metadata.namespace` | string | No | Kubernetes namespace (default: `default`) |
| `metadata.annotations` | map | No | Key-value pairs; Wren uses `wren.giar.dev/user` |
| `metadata.labels` | map | No | Arbitrary labels for user organization |

### Spec

#### Queue and Scheduling

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `spec.queue` | string | `default` | Which WrenQueue to submit to |
| `spec.priority` | int32 | `50` | Priority for scheduling (higher = more important). Range: 0-100. |
| `spec.walltime` | string | (none) | Maximum runtime before forceful termination. Format: `4h`, `30m`, `1d`, `2h30m`, or integer seconds. |
| `spec.project` | string | (none) | Project identifier for fair-share grouping (optional, user-settable) |

#### Resource Request

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `spec.nodes` | uint32 | Required | Number of nodes required. Gang scheduling: all or nothing. |
| `spec.tasksPerNode` | uint32 | `1` | MPI ranks per node (not yet enforced per-node; affects total ranks). |

#### Backend Selection

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `spec.backend` | string | `container` | Execution backend: `container` (Kubernetes Pods) or `reaper` (bare-metal via ReaperPod). |

#### Container Backend Configuration

Required if `spec.backend == "container"`. Optional otherwise.

```yaml
spec:
  container:
    image: string                 # Required: container image URI
    command: [string]             # Optional: entrypoint override
    args: [string]                # Optional: arguments to entrypoint
    hostNetwork: bool             # Optional (default: false): use host network namespace (required for RDMA)
    env:                          # Optional: environment variables
      - name: string
        value: string
    resources:                     # Optional: resource requests and limits
      requests:
        cpu: string               # e.g., "100m", "1"
        memory: string            # e.g., "128Mi", "1Gi"
        nvidia.com/gpu: int       # e.g., 1, 4
      limits:
        cpu: string
        memory: string
        nvidia.com/gpu: int
    volumeMounts:                 # Optional: volume mounts
      - name: string
        mountPath: string
        readOnly: bool
```

##### ContainerSpec Details

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `image` | string | Yes | Container image URL (e.g., `ubuntu:22.04`, `ghcr.io/org/image:v1.0`) |
| `command` | []string | No | Entrypoint override (replaces image CMD) |
| `args` | []string | No | Arguments to entrypoint |
| `hostNetwork` | bool | No (default: false) | Use host network namespace. Required for Slingshot/RDMA. |
| `env` | []EnvVar | No | Environment variables (in addition to Wren-injected vars) |
| `resources` | ResourceRequirements | No | CPU, memory, GPU requests and limits |
| `volumeMounts` | []VolumeMount | No | Additional volume mounts (ConfigMaps, Secrets, hostPath) |

#### Reaper Backend Configuration

Required if `spec.backend == "reaper"`. Optional otherwise.

```yaml
spec:
  reaper:
    command: [string]             # Optional: command to execute
    args: [string]                # Optional: arguments
    script: string                # Optional: shell script (if command not set)
    workingDir: string            # Optional: working directory
    environment:                  # Optional: environment variables (map)
      KEY1: value1
      KEY2: value2
    volumes:                      # Optional: volume mounts
      - name: string
        mountPath: string
        readOnly: bool
        configMap: string         # Optional: ConfigMap name to mount
        secret: string            # Optional: Secret name to mount
        hostPath: string          # Optional: host path to mount
        emptyDir: bool            # Optional: use emptyDir volume
    dnsMode: string               # Optional: "host" (default) or "kubernetes"
    overlayName: string           # Optional: named overlay filesystem
```

##### ReaperSpec Details

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `command` | []string | No (if script set) | Command to execute on bare metal |
| `args` | []string | No | Arguments to command |
| `script` | string | No (if command set) | Shell script to execute (wrapped in `/bin/sh -c`) |
| `workingDir` | string | No | Working directory for the process |
| `environment` | map[string]string | No | Environment variables |
| `volumes` | []ReaperVolumeSpec | No | Volume mounts for the process |
| `dnsMode` | string | No (default: "host") | DNS resolution mode: "host" (use node's /etc/hosts) or "kubernetes" |
| `overlayName` | string | No | Named overlay filesystem for inter-rank shared state |

#### MPI Configuration

Optional. Use when the job is an MPI application.

```yaml
spec:
  mpi:
    implementation: string        # Optional (default: "openmpi"): openmpi, cray-mpich, intel-mpi
    sshAuth: bool                 # Optional (default: true): mount shared SSH keys for mpirun bootstrap
    fabricInterface: string       # Optional: network interface for MPI traffic (e.g., "hsn0", "eth0")
```

##### MPISpec Details

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `implementation` | string | `openmpi` | MPI implementation: `openmpi`, `cray-mpich`, or `intel-mpi`. Affects `mpirun` args and env vars. |
| `sshAuth` | bool | `true` | Use SSH-based MPI bootstrap (mount shared keys for inter-node communication). Set to `false` for Reaper backend (uses hostfile instead). |
| `fabricInterface` | string | (none) | Network interface name for MPI traffic (e.g., `hsn0` for Slingshot). Sets MPI env vars like `MPICH_OFI_IFNAME`. |

#### Topology-Aware Scheduling

Optional. Use when job requires network locality.

```yaml
spec:
  topology:
    preferSameSwitch: bool        # Optional (default: false): prefer nodes on same network switch
    maxHops: uint32               # Optional: maximum allowed network hops between any two nodes
    topologyKey: string           # Optional (default: "topology.kubernetes.io/zone"): label key for grouping
```

##### TopologySpec Details

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `preferSameSwitch` | bool | `false` | Prefer nodes on the same network switch (higher score for same switch). |
| `maxHops` | uint32 | (no limit) | Maximum allowed network hops between any two nodes in placement. Job fails to schedule if constraint violated. |
| `topologyKey` | string | `topology.kubernetes.io/zone` | Kubernetes node label key for topology grouping. Can be custom (e.g., `topology.kubernetes.io/switch-group`). |

#### Job Dependencies

Optional. Use for workflow jobs.

```yaml
spec:
  dependencies:
    - type: string                # afterOk, afterAny, or afterNotOk
      job: string                 # Name of the job this depends on
```

##### JobDependency Details

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `type` | string | Yes | Dependency type: `afterOk` (wait for success), `afterAny` (wait for completion regardless of status), `afterNotOk` (wait for failure). |
| `job` | string | Yes | Name of the job to depend on (must be in same namespace). |

### Status

| Field | Type | Description |
|-------|------|-------------|
| `status.jobId` | uint64 | Unique sequential job ID (assigned by controller). Example: `12345` |
| `status.state` | string | Current job state: `Pending`, `Scheduling`, `Running`, `Succeeded`, `Failed`, `Cancelled`, `WalltimeExceeded` |
| `status.startTime` | RFC3339 timestamp | When job entered `Running` state |
| `status.completionTime` | RFC3339 timestamp | When job entered terminal state |
| `status.message` | string | Human-readable status message. Example: "Waiting for resources" or "2/4 worker pods running" |
| `status.placement.nodes` | []string | Which nodes the job is running on (assigned by scheduler). Empty during `Scheduling`. |
| `status.conditions` | []Condition | Detailed status conditions |

##### Condition

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | `Ready`, `Failed` |
| `status` | string | `True`, `False`, `Unknown` |
| `reason` | string | Machine-readable reason (e.g., `InsufficientResources`) |
| `message` | string | Human-readable explanation |
| `lastProbeTime` | RFC3339 timestamp | When condition was last checked |
| `lastTransitionTime` | RFC3339 timestamp | When condition last changed |

### Example

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ml-training
  namespace: data-science
  annotations:
    wren.giar.dev/user: alice
spec:
  queue: gpu
  priority: 80
  walltime: 8h
  project: dl-research

  nodes: 4
  tasksPerNode: 2

  backend: container

  container:
    image: pytorch:24.01-cuda12.1-runtime-ubuntu22.04
    command: ["python", "-m", "torch.distributed.launch"]
    args: ["--nproc_per_node=2", "train.py", "--epochs=100"]
    resources:
      requests:
        nvidia.com/gpu: 2
        memory: 64Gi
        cpu: 8
      limits:
        nvidia.com/gpu: 2
        memory: 64Gi
        cpu: 16
    hostNetwork: true
    env:
      - name: CUDA_DEVICE_ORDER
        value: PCI_BUS_ID

  mpi:
    implementation: openmpi
    sshAuth: true

  topology:
    preferSameSwitch: true
    maxHops: 2

  dependencies:
    - type: afterOk
      job: data-prep-job
```

---

## WrenQueue

Defines a priority queue with resource limits and scheduling policies.

### Metadata

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `metadata.name` | string | Yes | Queue name (e.g., `default`, `gpu`, `interactive`) |

### Spec

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `spec.maxNodes` | uint32 | (unlimited) | Maximum nodes that can be allocated to jobs in this queue |
| `spec.maxWalltime` | string | (unlimited) | Maximum walltime allowed for jobs in this queue |
| `spec.maxJobsPerUser` | uint32 | (unlimited) | Maximum concurrent jobs per user |
| `spec.defaultPriority` | int32 | `50` | Default priority for jobs submitted without explicit priority |
| `spec.preemption.enabled` | bool | `false` | Enable job preemption (experimental) |
| `spec.preemption.policy` | string | `gang` | Preemption policy: `gang` (evict entire jobs) or `granular` (individual pods) |
| `spec.backfill.enabled` | bool | `true` | Enable Slurm-style backfill scheduling |
| `spec.backfill.lookAhead` | string | `2h` | How far ahead to project resource availability for backfill |
| `spec.fairShare.enabled` | bool | `true` | Enable fair-share scheduling |
| `spec.fairShare.decayHalfLife` | string | `7d` | Historical usage decay period (older usage counts less) |
| `spec.fairShare.weightAge` | float | `0.25` | Weight of job age in fair-share calculation |
| `spec.fairShare.weightSize` | float | `0.25` | Weight of job size (node count) in fair-share calculation |
| `spec.fairShare.weightFairShare` | float | `0.50` | Weight of fair-share factor in fair-share calculation |

### Example

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: gpu
spec:
  maxNodes: 64
  maxWalltime: 24h
  maxJobsPerUser: 5
  defaultPriority: 50

  backfill:
    enabled: true
    lookAhead: 4h

  fairShare:
    enabled: true
    decayHalfLife: 14d
    weightAge: 0.25
    weightSize: 0.25
    weightFairShare: 0.50
```

---

## WrenUser

Maps a username to Unix UID/GID for job execution identity.

### Metadata

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `metadata.name` | string | Yes | Username (e.g., `alice`, `bob`) |

Note: WrenUser is cluster-scoped (not namespaced).

### Spec

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `spec.uid` | uint32 | Yes | Unix user ID (> 0, not 0 for security) |
| `spec.gid` | uint32 | Yes | Primary Unix group ID |
| `spec.supplementalGroups` | []uint32 | No | Additional group IDs |
| `spec.homeDir` | string | No | Home directory path (e.g., `/home/alice`). Used for `$HOME` env var. |
| `spec.defaultProject` | string | No | Default project for fair-share grouping (user can override via job spec) |

### Example

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: alice
spec:
  uid: 1001
  gid: 1001
  supplementalGroups:
    - 1001       # alice's group
    - 5000       # data-scientists group
    - 5001       # ml-researchers group
  homeDir: /home/alice
  defaultProject: dl-research
```

---

## Field Format Reference

### String Duration

Time durations are strings like `4h`, `30m`, `1d`, `2h30m`, or integer seconds.

Supported units:
- `s` — seconds
- `m` — minutes
- `h` — hours
- `d` — days
- `w` — weeks

Examples:
- `30` — 30 seconds
- `5m` — 5 minutes
- `1h` — 1 hour
- `2d` — 2 days
- `1w` — 1 week
- `2h30m` — 2 hours 30 minutes

### Kubernetes Quantity (CPU, Memory)

Standard Kubernetes resource format.

**CPU:**
- `100m` — 0.1 core (milli-cores)
- `1` — 1 core
- `2` — 2 cores
- `500m` — half core

**Memory:**
- `128Mi` — 128 mebibytes (power of 2)
- `1Gi` — 1 gibibyte
- `128M` — 128 megabytes (decimal, avoid)
- `1G` — 1 gigabyte (decimal, avoid)

Kubernetes uses binary units (Mi, Gi) for precision with container resource limits.

### DNS-1123 Subdomain

Valid Kubernetes resource names (must be DNS-compliant):
- Lowercase alphanumeric and hyphens only
- Start and end with alphanumeric
- Max 63 characters
- Example: `my-job-123`

Invalid: `MY-JOB`, `my_job`, `my job`, `my-job-`

### Timestamps

All timestamps are RFC3339 format:
- `2025-04-09T12:34:56Z`
- `2025-04-09T12:34:56+05:00`

### Label Selectors

Labels are used to filter resources. Format: `key=value,key2=value2`

Example:
```bash
kubectl get wrenjob -l queue=gpu,priority=high
```

---

## Print Columns

When running `kubectl get wrenjob`, these columns are displayed:

| Column | Source | Example |
|--------|--------|---------|
| JobID | `.status.jobId` | `12345` |
| State | `.status.state` | `Running` |
| Nodes | `.spec.nodes` | `4` |
| Queue | `.spec.queue` | `gpu` |
| Age | `.metadata.creationTimestamp` | `2h` |

---

## API Versions

Current API version: **`v1alpha1`**

This is an alpha API. Breaking changes may occur in future versions. Always test upgrades in a dev cluster first.

Version history:
- `v1alpha1` (current) — initial release with Phase 1-4 features
