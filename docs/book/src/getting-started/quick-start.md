# Quick Start

Get Wren running and submit your first job in 10 minutes.

## Fastest Path: One-Liner Quickstart

If you don't have a Kubernetes cluster yet, this script creates one:

```bash
git clone https://github.com/miguelgila/wren.git
cd wren
./examples/quickstart.sh --dev
```

This:
- Creates a local Kind cluster with topology labels
- Installs CRDs and deploys the controller
- Creates a WrenUser and default queue
- Runs example jobs
- Prints connection info

If you already have a cluster, use `--no-cluster`:

```bash
./examples/quickstart.sh --dev --no-cluster
```

Otherwise, follow the manual steps below.

## Manual Setup (5 Minutes)

### Step 1: Create a Kubernetes Cluster

If you don't have one, create a local Kind cluster:

```bash
# Install kind if needed
brew install kind  # macOS
# or: go install sigs.k8s.io/kind@latest

# Create a cluster with topology labels (for scheduling awareness)
cat > kind-config.yaml << 'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
  - role: worker
    labels:
      topology.kubernetes.io/zone: zone-a
      topology.kubernetes.io/region: us-west
  - role: worker
    labels:
      topology.kubernetes.io/zone: zone-b
      topology.kubernetes.io/region: us-west
EOF

kind create cluster --config kind-config.yaml --name wren-demo
```

Verify:

```bash
kubectl cluster-info
kubectl get nodes -L topology.kubernetes.io/zone
```

### Step 2: Install Wren

**Option A: Using Helm (recommended)**

```bash
# Clone the repo
git clone https://github.com/miguelgila/wren.git
cd wren

# Install the Helm chart
helm install wren charts/wren \
  --namespace wren-system \
  --create-namespace
```

**Option B: Using raw manifests**

```bash
git clone https://github.com/miguelgila/wren.git
cd wren

kubectl create namespace wren-system
kubectl apply -f manifests/crds/
kubectl apply -f manifests/rbac/
kubectl apply -f manifests/deployment.yaml
```

### Step 3: Wait for the Controller to Start

```bash
# Watch the controller pod
kubectl -n wren-system get pods --watch

# Once it shows Running, proceed
kubectl -n wren-system logs deployment/wren-controller
```

### Step 4: Create a Default Queue

```bash
kubectl apply -f - << 'EOF'
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: default
spec:
  maxNodes: 10
  maxWalltime: "24h"
  maxJobsPerUser: 10
  defaultPriority: 50
  backfill:
    enabled: true
    lookAhead: "2h"
EOF
```

Verify:

```bash
kubectl get wrenqueues
```

### Step 5: Create a User

Every job requires a valid user identity. Create one:

```bash
kubectl apply -f - << 'EOF'
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: demo-user
spec:
  uid: 1001
  gid: 1001
  supplementalGroups:
    - 1001
  homeDir: /home/demo-user
  defaultProject: demo
EOF
```

Verify:

```bash
kubectl get wrenusers
```

### Step 6: Submit Your First Job

Create a simple hello-world job:

```bash
kubectl apply -f - << 'EOF'
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: hello-world
  annotations:
    wren.giar.dev/user: demo-user
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "10m"
  container:
    image: busybox:latest
    command: ["sh", "-c"]
    args: ["echo 'Hello from Wren!'; sleep 5"]
EOF
```

### Step 7: Check Job Status

**Using kubectl:**

```bash
# Watch the job transition through states
kubectl get wrenjobs --watch

# Get detailed status
kubectl get wrenjob hello-world -o yaml
kubectl describe wrenjob hello-world
```

**Using the wren CLI:**

First, install the CLI:

```bash
# If building from source
cargo build --release -p wren-cli
cp target/release/wren /usr/local/bin/

# Or download a pre-built binary from GitHub releases
```

Then:

```bash
# List all jobs
wren queue

# Check specific job
wren status hello-world

# View logs
wren logs hello-world
```

### Step 8: Run an MPI Job

Here's a simple 2-node MPI example. Note: you need at least 2 worker nodes.

```bash
kubectl apply -f manifests/examples/simple-mpi.yaml
```

Check status:

```bash
kubectl get wrenjob hello-mpi -o wide
kubectl describe wrenjob hello-mpi
```

The job creates:
- A headless Service for pod DNS discovery
- A ConfigMap with the MPI hostfile
- Worker Pods (one per node)

Watch the pods:

```bash
kubectl get pods -o wide
```

View logs:

```bash
kubectl logs -l app.kubernetes.io/name=wren,app.kubernetes.io/instance=hello-mpi -f
```

## Understanding What Happened

When you submitted a WrenJob, Wren:

1. **Validated** the job (correct user, non-root, valid queue)
2. **Scheduled** it (found available nodes)
3. **Allocated resources** (reserved nodes, created a placement)
4. **Created infrastructure**:
   - Headless Service for pod DNS
   - ConfigMap with the MPI hostfile (for MPI jobs)
5. **Launched** the job:
   - Created Pods on scheduled nodes
   - Injected environment variables (USER, HOME, MPI vars)
   - Set correct UID/GID (from WrenUser)
6. **Monitored** execution:
   - Watched for completion or timeout
   - Enforced walltime (SIGTERM, then SIGKILL)
   - Preserved completed jobs for log retrieval

## Next Examples

Try these examples from the repo:

```bash
# Simple MPI job (2 nodes, busybox)
kubectl apply -f manifests/examples/simple-mpi.yaml

# GPU training job (multi-node)
kubectl apply -f manifests/examples/gpu-training.yaml

# Bare-metal execution via Reaper (if enabled)
kubectl apply -f manifests/examples/reaper-job.yaml
```

## Cleanup

To delete everything:

```bash
# Delete your jobs
kubectl delete wrenjob --all

# Delete the queue
kubectl delete wrenqueue default

# Uninstall the controller
helm uninstall wren --namespace wren-system
# or: kubectl delete -f manifests/

# Delete the namespace
kubectl delete namespace wren-system

# Delete the kind cluster (if you created one)
kind delete cluster --name wren-demo
```

## Troubleshooting

**Controller pod won't start:**

```bash
kubectl -n wren-system describe pod <pod-name>
kubectl -n wren-system logs <pod-name>
```

**Job stuck in Pending/Scheduling:**

```bash
# Check controller logs
kubectl -n wren-system logs deployment/wren-controller

# Check if nodes have enough resources
kubectl describe nodes

# Check the job status
kubectl describe wrenjob <job-name>
```

**Pods aren't being created:**

```bash
# Verify the user exists
kubectl get wrenuser demo-user

# Verify the queue exists
kubectl get wrenqueue default

# Check RBAC permissions
kubectl describe clusterrolebinding wren-controller
```

**View detailed metrics:**

```bash
# Port-forward to metrics
kubectl -n wren-system port-forward svc/wren-controller 8080:8080

# Query Prometheus metrics
curl http://localhost:8080/metrics
```

## Next Steps

- **[Installation Guide](installation.md)** — Full installation options
- **[Cluster Setup](cluster-setup.md)** — Configure topology labels
- **[Submitting Jobs](../user-guide/submitting-jobs.md)** — Learn WrenJob specification
- **[User Identity](../user-guide/user-identity.md)** — Register users and manage identity
- **[MPI Jobs](../user-guide/mpi-jobs.md)** — Configure MPI-specific settings
