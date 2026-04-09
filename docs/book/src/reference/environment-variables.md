# Environment Variables Reference

Wren injects environment variables into job containers and ReaperPods. This page describes all variables and their usage.

## Wren Metadata Variables

These variables are injected into every job (container and Reaper backends).

| Variable | Value | Format | Purpose |
|----------|-------|--------|---------|
| `WREN_JOB_NAME` | Job metadata name | string | Job identifier for logging and monitoring |
| `WREN_NUM_NODES` | Number of nodes allocated | uint | Total node count for this job |
| `WREN_TOTAL_RANKS` | nodes × tasksPerNode | uint | Total MPI ranks in the job |
| `WREN_HOSTFILE` | Path to hostfile | string | Path to file listing MPI node slots |

### Example

For a 2-node job with 1 task per node:

```bash
WREN_JOB_NAME=my-mpi-job
WREN_NUM_NODES=2
WREN_TOTAL_RANKS=2
WREN_HOSTFILE=/etc/wren/hostfile
```

## User Identity Variables

These variables provide user information. Set by Wren from WrenUser CRD lookup.

| Variable | Value | Purpose |
|----------|-------|---------|
| `USER` | Username | Unix username (matches WrenUser.metadata.name) |
| `LOGNAME` | Username | Login name (same as USER) |
| `HOME` | Home directory | From WrenUser.spec.homeDir |

### Example

For user `alice` with homeDir `/home/alice`:

```bash
USER=alice
LOGNAME=alice
HOME=/home/alice
```

### Container Backend

The container's `securityContext.runAsUser` and `runAsGroup` are also set from WrenUser spec, but these are not environment variables (they're pod-level settings).

### Reaper Backend

User identity flows through ReaperPod spec fields (`uid`, `gid`, `username`, `homeDir`). The Reaper agent uses these to call `setuid(1001)` before executing the process, achieving the same effect.

## Distributed Training Variables

Used by distributed ML frameworks (PyTorch, TensorFlow, Horovod). Set by Wren for all jobs.

| Variable | Value | Purpose |
|----------|-------|---------|
| `RANK` | Global process rank | 0 to WORLD_SIZE-1, assigned sequentially per node |
| `WORLD_SIZE` | Total process count | nodes × tasksPerNode |
| `MASTER_ADDR` | Hostname of rank 0 | Used by PyTorch, TensorFlow for bootstrapping |
| `MASTER_PORT` | Fixed port (29500) | Used by PyTorch, TensorFlow for bootstrapping |
| `LOCAL_RANK` | Per-node rank | 0 to tasksPerNode-1 (only if tasksPerNode > 1) |

### Example

4-node job with 2 tasks per node (8 ranks total):

```
Node compute-01, rank 0: RANK=0, WORLD_SIZE=8, MASTER_ADDR=compute-01, LOCAL_RANK=0
Node compute-01, rank 1: RANK=1, WORLD_SIZE=8, MASTER_ADDR=compute-01, LOCAL_RANK=1
Node compute-02, rank 2: RANK=2, WORLD_SIZE=8, MASTER_ADDR=compute-01, LOCAL_RANK=0
Node compute-02, rank 3: RANK=3, WORLD_SIZE=8, MASTER_ADDR=compute-01, LOCAL_RANK=1
...
```

### Usage

**PyTorch Distributed:**

```python
import torch.distributed as dist

rank = int(os.environ['RANK'])
world_size = int(os.environ['WORLD_SIZE'])
master_addr = os.environ['MASTER_ADDR']
master_port = int(os.environ['MASTER_PORT'])

dist.init_process_group(
    backend='nccl',
    init_method=f'tcp://{master_addr}:{master_port}',
    rank=rank,
    world_size=world_size
)
```

**TensorFlow MultiWorkerMirroredStrategy:**

```python
import os
os.environ['TF_CONFIG'] = json.dumps({
    'cluster': {
        'worker': [f'{os.environ["MASTER_ADDR"]}:{os.environ["MASTER_PORT"]}']
    },
    'task': {
        'type': 'worker',
        'index': int(os.environ['RANK'])
    }
})

strategy = tf.distribute.MultiWorkerMirroredStrategy()
```

**Horovod:**

```bash
horovodrun -np 8 python train.py
# Automatically uses RANK, WORLD_SIZE, MASTER_ADDR, MASTER_PORT
```

## MPI Variables

Set based on `spec.mpi.implementation` and other MPI configuration. Used to configure MPI runtime behavior.

### OpenMPI

| Variable | Condition | Value | Purpose |
|----------|-----------|-------|---------|
| `OMPI_MCA_btl_tcp_if_include` | if fabricInterface set | Interface name | Restrict MPI to specific network interface |

### Cray-MPICH

| Variable | Condition | Value | Purpose |
|----------|-----------|-------|---------|
| `MPICH_OFI_STARTUP_CONNECT` | Always | `all` | Connect to all ranks at startup (for OFI provider) |
| `MPICH_OFI_NUM_NICS` | Always | `1` | Number of network interfaces (OFI specific) |
| `MPICH_OFI_IFNAME` | if fabricInterface set | Interface name | Network interface for OFI (e.g., `hsn0` for Slingshot) |

### Intel MPI

| Variable | Condition | Value | Purpose |
|----------|-----------|-------|---------|
| `I_MPI_FABRICS_LIST` | if fabricInterface set | Interface name | Network fabric selection |

### Example

Job with `mpi.implementation=cray-mpich` and `mpi.fabricInterface=hsn0`:

```bash
MPICH_OFI_STARTUP_CONNECT=all
MPICH_OFI_NUM_NICS=1
MPICH_OFI_IFNAME=hsn0
```

## Custom Environment Variables

Users can inject custom variables via the job spec:

### Container Backend

```yaml
spec:
  container:
    env:
      - name: MY_VAR
        value: my_value
      - name: CUDA_VISIBLE_DEVICES
        value: "0,1"
```

These are added to all pod containers.

### Reaper Backend

```yaml
spec:
  reaper:
    environment:
      MY_VAR: my_value
      CUDA_VISIBLE_DEVICES: "0,1"
      SCRATCH: /scratch/project
```

These are merged with Wren-injected variables.

### Precedence

1. Wren-injected variables (highest priority, cannot be overridden)
2. User-specified variables
3. Image defaults (lowest priority)

If user specifies a variable that conflicts with a Wren variable, Wren's value wins (e.g., user can't override `RANK`).

## Hostfile

The hostfile is a text file listing MPI nodes and slots, distributed to all processes.

### Format

```
node-0 slots=1
node-1 slots=1
node-2 slots=1
node-3 slots=1
```

For `tasksPerNode=2`:

```
node-0 slots=2
node-1 slots=2
node-2 slots=2
node-3 slots=2
```

### Location

Hostfile path is in `WREN_HOSTFILE` environment variable.

**Container Backend:** Mounted as ConfigMap at `/etc/wren/hostfile` (or custom path via volumeMounts)

**Reaper Backend:** Distributed via ConfigMap volume

### Usage

**OpenMPI mpirun:**

```bash
mpirun -np $WREN_TOTAL_RANKS --hostfile $WREN_HOSTFILE ./app
```

**Cray-MPICH srun:**

```bash
srun --overlap ./app
# Or with explicit node list:
srun -N $WREN_NUM_NODES -n $WREN_TOTAL_RANKS ./app
```

**Intel MPI mpiexec:**

```bash
mpiexec.hydra -f $WREN_HOSTFILE -np $WREN_TOTAL_RANKS ./app
```

## SSH Variables (Container Backend Only)

When `mpi.sshAuth=true`, MPI bootstrap uses SSH keys.

| Variable | Value | Purpose |
|----------|-------|---------|
| `SSH_AUTH_SOCK` | /run/secrets/ssh-auth-socket | Socket for SSH agent (if configured) |

The SSH keys are mounted from a Secret and available to all pods for inter-node communication.

## Reaper-Specific Variables

Reaper backend receives additional information in the ReaperPod spec (not env vars):

| Field | Value | Purpose |
|-------|-------|---------|
| `spec.uid` | User ID | Unix UID for process execution |
| `spec.gid` | Group ID | Unix GID for process execution |
| `spec.username` | Username | For reference and HOME env var |
| `spec.homeDir` | Path | Home directory path |
| `spec.walltime` | Seconds | Maximum runtime before SIGKILL |
| `spec.gracePeriod` | Seconds | Grace period between SIGTERM and SIGKILL |

The Reaper agent uses these to:
1. Call `setuid(uid)` and `setgid(gid)` before exec
2. Set environment variables (`USER`, `HOME`, `LOGNAME`)
3. Enforce walltime and cleanup

## Environment Variable Complete Example

A complete example for a 4-node PyTorch distributed training job:

```bash
# Wren metadata
WREN_JOB_NAME=pytorch-ddp-training
WREN_NUM_NODES=4
WREN_TOTAL_RANKS=8
WREN_HOSTFILE=/etc/wren/hostfile

# User identity
USER=alice
LOGNAME=alice
HOME=/home/alice

# Distributed training (rank 0 on compute-01)
RANK=0
WORLD_SIZE=8
MASTER_ADDR=compute-01
MASTER_PORT=29500
LOCAL_RANK=0

# MPI (Cray-MPICH with Slingshot)
MPICH_OFI_STARTUP_CONNECT=all
MPICH_OFI_NUM_NICS=1
MPICH_OFI_IFNAME=hsn0

# Custom variables
CUDA_VISIBLE_DEVICES=0,1
SCRATCH=/scratch/alice/training
NCCL_DEBUG=INFO
```

## Querying Variables at Runtime

### Container

```bash
# From pod spec
kubectl exec <pod> -- env | grep WREN_
kubectl exec <pod> -- printenv RANK
```

### Reaper

```bash
# From ReaperPod status
kubectl get reaperpod <name> -o yaml | grep -A20 spec.environment
```

## Changing Variables

### At Job Submission

Set custom variables in job spec:

**Container:**
```yaml
container:
  env:
    - name: MY_VAR
      value: my_value
```

**Reaper:**
```yaml
reaper:
  environment:
    MY_VAR: my_value
```

### At Runtime

Cannot modify Wren-injected variables. To change custom variables, create a new job.

### In a Config File

Use ConfigMap for large configuration:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: training-config
data:
  config.yaml: |
    learning_rate: 0.001
    batch_size: 32
---
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: pytorch-training
spec:
  container:
    volumeMounts:
      - name: config
        mountPath: /config
    volumes:
      - name: config
        configMap:
          name: training-config
  ...
```

Then in application code:
```python
with open('/config/config.yaml') as f:
    config = yaml.safe_load(f)
```

## Troubleshooting Missing Variables

### Variable Not Set

```bash
# Check what variables are actually set
kubectl exec <pod> -- env | sort

# Check job spec
kubectl get wrenjob <name> -o yaml
```

### MPI Variables Missing

Verify `spec.mpi` is configured:

```bash
kubectl get wrenjob <name> -o yaml | grep -A5 mpi
```

If empty, add MPI configuration to job spec.

### User Variables Missing

Check WrenUser CRD:

```bash
kubectl get wrenuser <username>

# If not found, create it
kubectl apply -f wrenuser.yaml
```

### RANK Always 0

With `tasksPerNode=1` (default), all processes get `RANK=0`. To run multiple ranks per node:

```yaml
spec:
  tasksPerNode: 2
```

This gives `RANK=0,1,2,3` across nodes with `LOCAL_RANK=0,1` per node.
