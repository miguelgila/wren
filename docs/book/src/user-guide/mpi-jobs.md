# MPI Jobs

Wren is designed for multi-node MPI workloads. This guide covers MPI configuration, hostfile generation, SSH bootstrap, and supported implementations.

## MPI Configuration

The `mpi` section specifies MPI implementation and bootstrap settings:

```yaml
mpi:
  implementation: openmpi  # or cray-mpich, intel-mpi
  sshAuth: true             # Use SSH for mpirun bootstrap
  fabricInterface: hsn0      # Network interface for MPI traffic
```

All MPI fields are optional. Defaults:
- `implementation: openmpi`
- `sshAuth: true`
- `fabricInterface: none` (use default)

## Supported MPI Implementations

### OpenMPI

Standard open-source MPI. Works on any Kubernetes cluster with ssh-enabled container images.

```yaml
mpi:
  implementation: openmpi
  sshAuth: true
```

Wren injects:
- `OMPI_COMM_WORLD_RANK` — process rank
- `OMPI_COMM_WORLD_SIZE` — total processes

### Cray MPICH (HPE Slingshot)

HPC-specific MPI for Slingshot interconnect. Common on Perlmutter, Frontier, other HPC systems.

```yaml
mpi:
  implementation: cray-mpich
  sshAuth: false  # Use Cray-specific bootstrap (PMI, Cray PMIx)
  fabricInterface: hsn0  # Slingshot interface
```

Wren injects:
- `MPICH_OFI_PROVIDER=gni` — use Cray OFI GNI provider
- `MPICH_NETMOD=ofi` — use OFI netmod
- `MPICH_PORT_RANGE=50000:59999` — port range for communication

When `sshAuth: false`, Cray MPICH uses PMIx bootstrap instead of SSH.

### Intel MPI

Intel's commercial MPI implementation.

```yaml
mpi:
  implementation: intel-mpi
  sshAuth: true
```

Wren injects:
- `I_MPI_RANK` — process rank
- `I_MPI_WORLD_SIZE` — total processes
- `I_MPI_FABRICS=shm:ofi` — fabric selection

## SSH Bootstrap (Container Backend)

When `sshAuth: true`, Wren:
1. Generates SSH key pairs
2. Mounts them into all pods at `/etc/wren/ssh`
3. Configures `authorized_keys` for passwordless login
4. Allows `mpirun` to ssh to other nodes to start ranks

Requirement: Container image must have `sshd` running or `openssh-client` available.

Example with Dockerfile:

```dockerfile
FROM nvcr.io/nvidia/pytorch:24.01-py3
RUN apt-get update && apt-get install -y openssh-server openssh-client
RUN mkdir -p /run/sshd
COPY entrypoint.sh /
ENTRYPOINT ["/entrypoint.sh"]
```

Entrypoint:

```bash
#!/bin/bash
# Start SSH daemon in background
/usr/sbin/sshd -D &

# Run user application
exec "$@"
```

## Hostfile Generation

Wren creates a hostfile ConfigMap with pod hostnames and distributes it to all containers at `/etc/wren/hostfile`.

Example hostfile for 4-node job:

```
my-job-worker-0.my-job-workers.default.svc.cluster.local slots=1
my-job-worker-1.my-job-workers.default.svc.cluster.local slots=1
my-job-worker-2.my-job-workers.default.svc.cluster.local slots=1
my-job-worker-3.my-job-workers.default.svc.cluster.local slots=1
```

Usage:

```yaml
container:
  command: ["mpirun", "-np", "4", "--hostfile", "/etc/wren/hostfile", "./app"]
```

The `slots=1` means 1 rank per node. For `tasksPerNode > 1` (Phase 3b), this will be `slots=N`.

## Container Backend Example: OpenMPI on GPU

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ddp-training
spec:
  nodes: 4
  tasksPerNode: 1
  queue: gpu
  priority: 100
  walltime: "8h"

  container:
    image: "nvcr.io/nvidia/pytorch:24.01-py3"
    command: [
      "mpirun",
      "-np", "4",
      "--hostfile", "/etc/wren/hostfile",
      "python", "train_ddp.py"
    ]
    hostNetwork: true
    resources:
      limits:
        nvidia.com/gpu: "4"
        memory: "64Gi"
      requests:
        cpu: "16000m"
        memory: "64Gi"

    env:
      - name: NCCL_DEBUG
        value: INFO
      - name: CUDA_LAUNCH_BLOCKING
        value: "0"
      - name: NCCL_IB_DISABLE
        value: "0"
      - name: NCCL_SOCKET_IFNAME
        value: eth0

  mpi:
    implementation: openmpi
    sshAuth: true
    fabricInterface: eth0

  topology:
    preferSameSwitch: true
    maxHops: 1
```

## Container Backend Example: Cray MPICH on Slingshot

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: hpl-benchmark
spec:
  nodes: 8
  tasksPerNode: 1
  queue: compute
  priority: 50
  walltime: "2h"

  container:
    image: "ghcr.io/hpc/hpl-cray:latest"
    command: [
      "srun",
      "-n", "8",
      "-N", "8",
      "--ntasks-per-node=1",
      "/opt/hpl/xhpl"
    ]
    hostNetwork: true
    resources:
      limits:
        memory: "128Gi"
      requests:
        cpu: "32000m"
        memory: "128Gi"

    env:
      - name: MPICH_OFI_PROVIDER
        value: gni

  mpi:
    implementation: cray-mpich
    sshAuth: false
    fabricInterface: hsn0

  topology:
    preferSameSwitch: true
    maxHops: 0  # All on same Slingshot switch for best performance
```

## Reaper Backend Example: Bare-Metal MPI

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: gromacs-sim
spec:
  nodes: 16
  tasksPerNode: 1
  backend: reaper
  queue: batch
  walltime: "12h"

  reaper:
    script: |
      #!/bin/bash
      set -e
      module load cray-mpich
      module load gromacs/2024

      # Wren injects MASTER_ADDR, RANK, WORLD_SIZE
      echo "Running on rank $RANK / $WORLD_SIZE"
      echo "Master at $MASTER_ADDR"

      # Use Cray launcher (recommended on HPC systems)
      srun --ntasks=$WORLD_SIZE gmx_mpi mdrun -s simulation.tpr

    workingDir: /scratch/user/gromacs

    environment:
      MPICH_OFI_PROVIDER: gni
      MPICH_NETMOD: ofi

  mpi:
    implementation: cray-mpich
    sshAuth: false

  topology:
    preferSameSwitch: true
    maxHops: 2
```

## Environment Variables Injected by Wren

### All Backends

Wren always injects:

```
USER=<from WrenUser>
LOGNAME=<from WrenUser>
HOME=<from WrenUser homeDir>
```

### Distributed Training (PyTorch, TensorFlow)

For multi-node distributed training, Wren injects:

```
MASTER_ADDR=<launcher pod IP or node IP>
MASTER_PORT=29500
RANK=<process rank>
WORLD_SIZE=<total processes>
LOCAL_RANK=<rank on this node>
```

Example PyTorch usage:

```python
import torch
import torch.distributed as dist
import os

rank = int(os.environ['RANK'])
world_size = int(os.environ['WORLD_SIZE'])
master_addr = os.environ['MASTER_ADDR']
master_port = os.environ['MASTER_PORT']

# Initialize distributed training
dist.init_process_group(
    backend='nccl',
    init_method=f'tcp://{master_addr}:{master_port}',
    rank=rank,
    world_size=world_size
)

# Your training code...
```

### OpenMPI-Specific

When `implementation: openmpi`, Wren injects:

```
OMPI_COMM_WORLD_RANK=<rank>
OMPI_COMM_WORLD_SIZE=<world_size>
```

### Cray MPICH-Specific

When `implementation: cray-mpich`:

```
MPICH_OFI_PROVIDER=gni
MPICH_NETMOD=ofi
MPICH_PORT_RANGE=50000:59999
```

### Intel MPI-Specific

When `implementation: intel-mpi`:

```
I_MPI_RANK=<rank>
I_MPI_WORLD_SIZE=<world_size>
I_MPI_FABRICS=shm:ofi
```

## MPI Debugging

### Enable MPI Debug Output

```yaml
container:
  env:
    - name: OMPI_DEBUG
      value: "1"  # OpenMPI
    - name: MPICH_RANK_REORDER
      value: "0"  # Cray MPICH
    - name: I_MPI_DEBUG
      value: "5"  # Intel MPI
```

### Check Hostfile

```bash
kubectl exec -it <pod-name> -- cat /etc/wren/hostfile
```

### SSH Access to Verify Connectivity

```bash
# List pods
kubectl get pods -l wren.giar.dev/job=my-job

# Login to worker
kubectl exec -it my-job-worker-0 -- bash

# Test SSH to peer
ssh my-job-worker-1.my-job-workers.default.svc.cluster.local hostname
```

### View MPI Process Output

```bash
kubectl logs my-job-launcher
# or for container backend with per-rank logs
wren logs my-job --rank 0
```

## Network Interface Selection

The `fabricInterface` field tells Wren and MPI which network to use:

- `hsn0` — Cray Slingshot (HPE Cray systems)
- `ib0` — InfiniBand
- `eth0` — Ethernet (default)
- `enp2s0` — Specific NIC

For Slingshot systems:

```yaml
mpi:
  fabricInterface: hsn0
```

Wren passes this to MPI implementations:
- OpenMPI: `--mca btl_openib_if_include hsn0`
- Cray MPICH: `MPICH_OFI_INTERFACE=hsn0`

## Common Issues

### "mpirun: command not found"

Container image lacks `mpirun`. Install OpenMPI or use a base image that includes it:

```dockerfile
FROM ubuntu:22.04
RUN apt-get update && apt-get install -y openmpi-bin libopenmpi-dev openssh-server
```

### "SSH connection refused"

- Ensure sshd is running in container
- Check `/etc/wren/ssh` is mounted correctly
- Verify SSH keys are distributed to all pods

### "Hostfile not found"

- Check `--hostfile /etc/wren/hostfile` path in mpirun command
- Verify ConfigMap is created: `kubectl get cm <job-name>-hostfile`

### "All ranks on same node"

- Set `tasksPerNode: 1` explicitly
- Ensure job requests multiple nodes: `nodes: 4`
- Check topology didn't force all ranks to one node

## Next: Multi-Task MPI (Phase 3b)

In the future, Wren will support `tasksPerNode > 1` with:
- CPU affinity binding
- GPU device selection
- NUMA-aware placement
- Per-rank local rank env vars

Today, use `tasksPerNode: 1` (one rank per node) and launch multiple ranks manually in your script if needed.
