# Cluster Setup

Wren works with any Kubernetes 1.28+ cluster, but to unlock topology-aware scheduling and MPI performance, you need to configure node topology labels and network settings.

## Prerequisites

- Kubernetes cluster 1.28 or later
- `kubectl` access with node labeling permissions
- At least 2 worker nodes (1 for controller, 1+ for jobs)

## Topology Labels (Optional but Recommended)

Wren uses node labels to understand your cluster's network topology and make better scheduling decisions. This is especially important for MPI jobs that benefit from network locality.

### Standard Kubernetes Topology Labels

Kubernetes automatically labels nodes with standard topology labels:

```bash
# Check existing topology labels
kubectl get nodes --show-labels | grep topology

# Common labels:
# topology.kubernetes.io/region  - Cloud region (us-west, eu-central, etc.)
# topology.kubernetes.io/zone    - Availability zone (us-west-1a, us-west-1b, etc.)
# topology.kubernetes.io/hostname- Hostname (usually set automatically)
```

Most cloud providers automatically add these labels. If not, add them:

```bash
# Label nodes manually (replace with your values)
kubectl label nodes node-1 topology.kubernetes.io/zone=zone-a
kubectl label nodes node-2 topology.kubernetes.io/zone=zone-a
kubectl label nodes node-3 topology.kubernetes.io/zone=zone-b
kubectl label nodes node-4 topology.kubernetes.io/zone=zone-b

# Or label by role or group
kubectl label nodes -l node-role.kubernetes.io/worker topology.kubernetes.io/region=us-west
```

### Custom Topology Labels for HPC Clusters

For HPC clusters with specialized network topology, add custom labels:

```bash
# Network topology (e.g., Cray Slingshot, InfiniBand)
kubectl label nodes node-1 \
  hpc.io/switch-group=switch-1 \
  hpc.io/rack=rack-1a \
  hpc.io/network-interface=hsn0

kubectl label nodes node-2 \
  hpc.io/switch-group=switch-1 \
  hpc.io/rack=rack-1a \
  hpc.io/network-interface=hsn0

kubectl label nodes node-3 \
  hpc.io/switch-group=switch-2 \
  hpc.io/rack=rack-1b \
  hpc.io/network-interface=hsn0
```

Then use custom topology keys in your WrenJob:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: my-mpi-job
spec:
  nodes: 4
  topology:
    preferSameSwitch: true
    topologyKey: "hpc.io/switch-group"  # Custom key instead of default zone
```

## Setting Up Kind for Local Testing

Kind (Kubernetes in Docker) is great for local development. The quickstart script creates a topology-aware Kind cluster, but you can also set it up manually.

### Create a Kind Cluster with Topology

Create a Kind configuration file with topology labels:

```bash
cat > kind-config.yaml << 'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4

# Number of control plane and worker nodes
nodes:
  - role: control-plane
    labels:
      node-role: control-plane
      topology.kubernetes.io/zone: zone-a
      topology.kubernetes.io/region: us-west

  # First "switch" (zone a)
  - role: worker
    labels:
      topology.kubernetes.io/zone: zone-a
      topology.kubernetes.io/region: us-west
      hpc.io/switch-group: switch-1
      hpc.io/rack: rack-1a

  - role: worker
    labels:
      topology.kubernetes.io/zone: zone-a
      topology.kubernetes.io/region: us-west
      hpc.io/switch-group: switch-1
      hpc.io/rack: rack-1a

  # Second "switch" (zone b)
  - role: worker
    labels:
      topology.kubernetes.io/zone: zone-b
      topology.kubernetes.io/region: us-west
      hpc.io/switch-group: switch-2
      hpc.io/rack: rack-1b

  - role: worker
    labels:
      topology.kubernetes.io/zone: zone-b
      topology.kubernetes.io/region: us-west
      hpc.io/switch-group: switch-2
      hpc.io/rack: rack-1b
EOF

# Create the cluster
kind create cluster --config kind-config.yaml --name wren-demo
```

Verify:

```bash
kubectl get nodes -L topology.kubernetes.io/zone,hpc.io/switch-group
```

## Configuring Network Interfaces

If your cluster has specialized network hardware (InfiniBand, Cray Slingshot, OmniPath), label the network interfaces:

```bash
# Label the network interface for MPI traffic
kubectl label nodes node-1 node-2 node-3 \
  mpi.wren.giar.dev/fabric-interface=hsn0

# Or use a custom annotation to describe network capabilities
kubectl annotate nodes node-1 \
  mpi.wren.giar.dev/capabilities="slingshot-11,ofi" \
  --overwrite
```

Then configure WrenJob to use them:

```yaml
spec:
  mpi:
    fabricInterface: hsn0          # Network interface for MPI traffic
    implementation: cray-mpich     # MPI implementation
```

## Cloud Provider Specific Setup

### AWS EKS

AWS automatically labels nodes with topology information:

```bash
# EKS nodes already have:
# - topology.kubernetes.io/region (e.g., us-west-2)
# - topology.kubernetes.io/zone (e.g., us-west-2a)
# - karpenter.sh/capacity-type (if using Karpenter)
# - karpenter.sh/instance-type (if using Karpenter)

# Verify
kubectl get nodes --show-labels | grep topology
```

For MPI workloads on EC2, ensure:

```bash
# Enable enhanced networking (ENA) on instances
# Enable placement groups for low latency (in CloudFormation/Terraform)
# Label nodes in the same placement group

kubectl label nodes -l karpenter.sh/capacity-type=spot \
  mpi.wren.giar.dev/low-latency=true
```

### Google GKE

GKE labels nodes with:

```bash
# - cloud.google.com/gke-nodepool (node pool name)
# - topology.kubernetes.io/region
# - topology.kubernetes.io/zone
# - cloud.google.com/gke-local-ssd (local SSD info)

kubectl get nodes --show-labels | grep cloud.google
```

For HPC on GKE, use Compute Engine nodes with:

```bash
# - Interconnect for high bandwidth
# - Custom machine types with guaranteed CPU allocation
# - Placement policies for co-location

# Label accordingly
kubectl label nodes node-1 node-2 \
  mpi.wren.giar.dev/interconnect=true
```

### Microsoft AKS

AKS labels nodes with:

```bash
# - topology.kubernetes.io/region
# - topology.kubernetes.io/zone
# - agentpool (managed node pool)
# - kubernetes.azure.com/os (OS type)

kubectl get nodes --show-labels | grep topology
```

For HPC on AKS, ensure:

```bash
# Use RDMA-capable VMs (if available in your region)
# Place all MPI job nodes in the same VMSS (via nodeSelector)

kubectl label nodes -l agentpool=hpc \
  mpi.wren.giar.dev/rdma=true
```

## Bare-Metal Cluster Setup (for Reaper Backend)

If using Wren's bare-metal Reaper backend, ensure:

### 1. SSH Access Between Nodes

Wren's MPI bootstrap uses SSH keys for process coordination:

```bash
# Generate SSH keys for the container user
ssh-keygen -N "" -f /etc/wren/ssh/id_rsa

# Distribute to all compute nodes
for node in node-1 node-2 node-3; do
  ssh-copy-id -i /etc/wren/ssh/id_rsa.pub user@$node
done
```

### 2. Shared Filesystem (Optional)

For efficient job distribution and logging, set up a shared filesystem:

```bash
# Mount NFS on all nodes (example)
sudo mkdir -p /scratch
sudo mount -t nfs nfs-server:/export/scratch /scratch

# Or use Lustre, GPFS, CephFS, etc.
```

Label nodes with shared storage availability:

```bash
kubectl label nodes -l has-nfs=true \
  storage.wren.giar.dev/nfs=true
```

### 3. Resource Discovery

Wren discovers node capabilities automatically, but you can help by labeling:

```bash
# GPU resources
kubectl label nodes gpu-node-1 \
  nvidia.com/gpu=a100 \
  accelerator=nvidia-a100

# CPU features
kubectl label nodes fast-cpu-node \
  cpu.feature.node.kubernetes.io/sse4_2=true \
  cpu.feature.node.kubernetes.io/avx2=true

# Memory
kubectl label nodes highmem-node \
  node.kubernetes.io/memory=large
```

## Network Policy & Security

If running with Kubernetes Network Policies, ensure MPI traffic is allowed:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: allow-mpi
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/part-of: mpi-job
  policyTypes:
    - Ingress
    - Egress
  ingress:
    - from:
        - podSelector:
            matchLabels:
              app.kubernetes.io/part-of: mpi-job
  egress:
    - to:
        - podSelector:
            matchLabels:
              app.kubernetes.io/part-of: mpi-job
    - to:
        - namespaceSelector: {}
      ports:
        - protocol: TCP
          port: 53  # DNS
        - protocol: UDP
          port: 53
```

## Resource Quotas

Set up namespace-level resource quotas if using namespaced job submission:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: hpc-jobs
---
apiVersion: v1
kind: ResourceQuota
metadata:
  name: hpc-quota
  namespace: hpc-jobs
spec:
  hard:
    pods: "100"
    requests.cpu: "200"
    requests.memory: "500Gi"
    limits.cpu: "400"
    limits.memory: "1Ti"
---
apiVersion: v1
kind: LimitRange
metadata:
  name: hpc-limits
  namespace: hpc-jobs
spec:
  limits:
    - type: Pod
      max:
        cpu: "32"
        memory: "256Gi"
```

## Verifying Your Setup

Run this check to verify topology awareness is working:

```bash
# Check all nodes and their topology labels
kubectl get nodes -o custom-columns=\
NAME:.metadata.name,\
ZONE:.metadata.labels.topology\\.kubernetes\\.io/zone,\
REGION:.metadata.labels.topology\\.kubernetes\\.io/region

# Check available resources per node
kubectl describe nodes | grep -A 5 "Allocated resources"

# Test scheduling a pod to a specific zone
kubectl run test-pod --image=busybox \
  --nodeSelector=topology.kubernetes.io/zone=zone-a
```

## Monitoring Node Status

Set up monitoring for node health, which affects scheduling:

```bash
# Watch node status
kubectl get nodes --watch

# Check node conditions
kubectl describe nodes

# Check for NotReady nodes
kubectl get nodes -o wide | grep NotReady

# View node metrics (if metrics-server is installed)
kubectl top nodes
```

## Next Steps

- **[Quick Start](quick-start.md)** — Run your first job
- **[Submitting Jobs](../user-guide/submitting-jobs.md)** — Learn WrenJob specification
- **[Topology-Aware Placement](../user-guide/topology.md)** — Advanced topology configuration
