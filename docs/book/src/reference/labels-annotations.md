# Labels and Annotations Reference

Wren uses Kubernetes labels and annotations to organize and identify resources. This page documents all labels and annotations used by Wren.

## Annotations

Annotations are used to attach metadata that affects job behavior or identity. Unlike labels, they are not indexed and can contain arbitrary data.

### wren.giar.dev/user

**Applied to:** WrenJob

**Value:** Username (string)

**Set by:** Mutating webhook (automatically from Kubernetes UserInfo)

**Purpose:** Identifies the user who submitted the job. Used for:
- User identity resolution (lookup WrenUser CRD for UID/GID)
- Fair-share accounting
- Quota enforcement
- Audit logging

**Example:**

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: my-job
  annotations:
    wren.giar.dev/user: alice
spec:
  ...
```

**How it's set:**

1. User submits job via kubectl or API
2. Kubernetes API server populates `UserInfo` from authentication backend (x509 cert, OIDC, ServiceAccount)
3. Wren's mutating webhook intercepts the job creation
4. Webhook reads `UserInfo.username` and stamps the annotation
5. The annotation is immutable (webhook always overrides user-specified values)

**Important:** Every job must have a valid non-root user. If the annotation is missing or the WrenUser CRD doesn't exist, the job immediately transitions to `Failed`.

### Custom Annotations

Users can add arbitrary annotations:

```yaml
metadata:
  annotations:
    wren.giar.dev/user: alice
    team: data-science
    project: climate-modeling
    owner: alice@example.com
```

These are preserved but not interpreted by Wren.

---

## Labels

Labels are indexed key-value pairs used to organize and select resources. Wren uses labels for:
- Resource ownership tracking
- Filtering in queries
- Status reporting

### Standard Kubernetes Labels

#### app.kubernetes.io/managed-by

**Applied to:** All Pods created by Wren (workers, launchers, etc.)

**Value:** `wren`

**Purpose:** Identifies that a Pod is managed by the Wren controller (not by users directly)

**Used for:**

```bash
# List all Wren-managed pods
kubectl get pods -l app.kubernetes.io/managed-by=wren

# Watch for changes (reconciliation trigger)
kubectl watch pods -l app.kubernetes.io/managed-by=wren
```

**Example Pod:**

```yaml
apiVersion: v1
kind: Pod
metadata:
  labels:
    app.kubernetes.io/managed-by: wren
    wren.giar.dev/job-name: my-job
    wren.giar.dev/role: worker
    wren.giar.dev/rank: "0"
```

---

### Wren-Specific Labels

All Wren-specific labels use the `wren.giar.dev/` prefix.

#### wren.giar.dev/job-name

**Applied to:** Pods, Services, ConfigMaps, Secrets (all resources for a job)

**Value:** WrenJob metadata.name (string)

**Purpose:** Links all resources to their parent job

**Used for:**

```bash
# List all pods for a job
kubectl get pods -l wren.giar.dev/job-name=my-job

# Delete all resources for a job (except the job itself)
kubectl delete pods,svc,cm,secret -l wren.giar.dev/job-name=my-job

# Stream logs from all pods
kubectl logs -l wren.giar.dev/job-name=my-job --all-containers=true -f
```

**Example:**

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: my-job-worker-0
  labels:
    wren.giar.dev/job-name: my-job
```

#### wren.giar.dev/role

**Applied to:** Pods (worker or launcher role)

**Value:** `worker` or `launcher` (string)

**Purpose:** Distinguishes pod roles within a job

**Values:**

| Value | Purpose |
|-------|---------|
| `worker` | Compute rank pod (runs MPI rank or standalone process) |
| `launcher` | Orchestration pod (runs mpirun, waits for workers, collects logs) |

**Used for:**

```bash
# List only worker pods
kubectl get pods -l wren.giar.dev/role=worker

# Watch launcher pod only
kubectl watch pod -l wren.giar.dev/job-name=my-job,wren.giar.dev/role=launcher
```

**Example:**

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: my-job-worker-0
  labels:
    wren.giar.dev/job-name: my-job
    wren.giar.dev/role: worker
---
apiVersion: v1
kind: Pod
metadata:
  name: my-job-launcher
  labels:
    wren.giar.dev/job-name: my-job
    wren.giar.dev/role: launcher
```

#### wren.giar.dev/rank

**Applied to:** Worker pods (not launcher)

**Value:** Numeric rank (string), 0-based index

**Purpose:** Identifies which MPI rank this pod represents

**Values:**

- `0` — first MPI rank (world rank 0)
- `1` — second MPI rank
- `N` — Nth rank

**Used for:**

```bash
# Get pod for specific rank
kubectl get pod -l wren.giar.dev/job-name=my-job,wren.giar.dev/rank=0

# Get logs for rank 2
kubectl logs -l wren.giar.dev/job-name=my-job,wren.giar.dev/rank=2

# Port-forward to rank 0 for debugging
kubectl port-forward -l wren.giar.dev/job-name=my-job,wren.giar.dev/rank=0 9000:9000
```

**Example:**

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: my-job-worker-0
  labels:
    wren.giar.dev/job-name: my-job
    wren.giar.dev/role: worker
    wren.giar.dev/rank: "0"
---
apiVersion: v1
kind: Pod
metadata:
  name: my-job-worker-1
  labels:
    wren.giar.dev/job-name: my-job
    wren.giar.dev/role: worker
    wren.giar.dev/rank: "1"
```

**Note:** Launcher pod does not have this label (no rank).

---

## Label Combinations

Common label combinations used in queries:

### All pods for a job

```bash
kubectl get pods -l wren.giar.dev/job-name=my-job
```

### Worker pods only

```bash
kubectl get pods -l wren.giar.dev/job-name=my-job,wren.giar.dev/role=worker
```

### Launcher pod only

```bash
kubectl get pod -l wren.giar.dev/job-name=my-job,wren.giar.dev/role=launcher
```

### Specific rank

```bash
kubectl get pod -l wren.giar.dev/job-name=my-job,wren.giar.dev/rank=0
```

### All Wren-managed resources

```bash
kubectl get all -l app.kubernetes.io/managed-by=wren
```

### Multiple jobs (example)

```bash
kubectl get pods -l "wren.giar.dev/job-name in (job-1, job-2, job-3)"
```

---

## Resource Naming Conventions

Wren uses consistent naming for derived resources (auto-generated, not user-specified).

### Pods

**Worker Pod:**
```
{job-name}-worker-{rank}
```

Example: `my-job-worker-0`, `my-job-worker-1`

**Launcher Pod:**
```
{job-name}-launcher
```

Example: `my-job-launcher`

### Services

**Headless Service (for pod DNS):**
```
{job-name}-workers
```

Example: `my-job-workers`

DNS entries: `my-job-worker-0.my-job-workers.default.svc.cluster.local`

### ConfigMaps

**Hostfile ConfigMap:**
```
{job-name}-hostfile
```

Contains the MPI hostfile (node list).

**SSH Secret:**
```
{job-name}-ssh
```

Contains SSH keys for inter-node MPI communication (if enabled).

### ReaperPod (Bare-Metal)

**ReaperPod:**
```
{job-name}-rank-{rank}
```

Example: `mpi-on-reaper-rank-0`, `mpi-on-reaper-rank-1`

---

## Node Labels (Input)

Wren reads the following node labels for topology-aware scheduling. These are not set by Wren; they come from the cluster or infrastructure provider.

### Standard Kubernetes Labels

| Label | Provider | Example | Purpose |
|-------|----------|---------|---------|
| `kubernetes.io/hostname` | kubelet | `compute-01` | Node name |
| `topology.kubernetes.io/region` | cloud provider | `us-west-2` | Geographic region |
| `topology.kubernetes.io/zone` | cloud provider | `us-west-2a` | Availability zone |
| `node.kubernetes.io/instance-type` | cloud provider | `p3.8xlarge` | Machine type (AWS) |

### Custom Topology Labels

| Label | Set by | Example | Purpose |
|-------|--------|---------|---------|
| `topology.kubernetes.io/switch-group` | operator | `switch-1` | Network switch (custom, site-specific) |
| `topology.kubernetes.io/rack` | operator | `rack-a` | Physical rack (custom, site-specific) |

**Example Node:**

```yaml
apiVersion: v1
kind: Node
metadata:
  name: compute-01
  labels:
    kubernetes.io/hostname: compute-01
    topology.kubernetes.io/region: us-west
    topology.kubernetes.io/zone: us-west-2a
    topology.kubernetes.io/switch-group: switch-1
    topology.kubernetes.io/rack: rack-a
    node-type: compute
    gpu: nvidia-v100
```

### Using Node Labels in Jobs

Specify topology constraints:

```yaml
spec:
  topology:
    preferSameSwitch: true
    maxHops: 2
    topologyKey: topology.kubernetes.io/switch-group
```

Wren reads node labels and uses them to score placements.

---

## Example: Complete Label Usage

A complete job with all associated resources and labels:

```yaml
# WrenJob (user-created)
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ml-training
  namespace: default
  annotations:
    wren.giar.dev/user: alice
spec:
  nodes: 2
  tasksPerNode: 1
  container:
    image: pytorch:latest
  ...
---
# Headless Service (auto-created by Wren)
apiVersion: v1
kind: Service
metadata:
  name: ml-training-workers
  labels:
    app.kubernetes.io/managed-by: wren
    wren.giar.dev/job-name: ml-training
spec:
  clusterIP: None
  selector:
    wren.giar.dev/job-name: ml-training
    wren.giar.dev/role: worker
---
# Hostfile ConfigMap (auto-created by Wren)
apiVersion: v1
kind: ConfigMap
metadata:
  name: ml-training-hostfile
  labels:
    app.kubernetes.io/managed-by: wren
    wren.giar.dev/job-name: ml-training
data:
  hostfile: |
    compute-01 slots=1
    compute-02 slots=1
---
# Worker Pod 0 (auto-created by Wren)
apiVersion: v1
kind: Pod
metadata:
  name: ml-training-worker-0
  labels:
    app.kubernetes.io/managed-by: wren
    wren.giar.dev/job-name: ml-training
    wren.giar.dev/role: worker
    wren.giar.dev/rank: "0"
spec:
  ...
---
# Worker Pod 1 (auto-created by Wren)
apiVersion: v1
kind: Pod
metadata:
  name: ml-training-worker-1
  labels:
    app.kubernetes.io/managed-by: wren
    wren.giar.dev/job-name: ml-training
    wren.giar.dev/role: worker
    wren.giar.dev/rank: "1"
spec:
  ...
---
# Launcher Pod (auto-created by Wren)
apiVersion: v1
kind: Pod
metadata:
  name: ml-training-launcher
  labels:
    app.kubernetes.io/managed-by: wren
    wren.giar.dev/job-name: ml-training
    wren.giar.dev/role: launcher
spec:
  ...
```

---

## Querying Resources by Labels

### kubectl Examples

```bash
# All resources for a job
kubectl get all -l wren.giar.dev/job-name=ml-training

# Only Wren-managed pods
kubectl get pods -l app.kubernetes.io/managed-by=wren

# Worker pods for all Wren jobs
kubectl get pods -l "app.kubernetes.io/managed-by=wren,wren.giar.dev/role=worker"

# Jobs by user (via annotation)
kubectl get wrenjob -l wren.giar.dev/user=alice  # Note: this searches labels, not annotations
# To search annotations, use field selectors:
kubectl get wrenjob --field-selector metadata.annotations.wren\.giar\.dev/user=alice
```

### Programmatic Access (kubectl API)

```go
// Get all worker pods for a job
pods, _ := clientset.CoreV1().Pods("default").List(context.Background(), metav1.ListOptions{
    LabelSelector: "wren.giar.dev/job-name=ml-training,wren.giar.dev/role=worker",
})

// Get pods by multiple labels
pods, _ := clientset.CoreV1().Pods("").List(context.Background(), metav1.ListOptions{
    LabelSelector: "app.kubernetes.io/managed-by=wren,wren.giar.dev/rank=0",
})
```

---

## Label Selectors for Operators

When writing Kubernetes operators that interact with Wren:

```bash
# Trigger reconciliation on pod changes
kubebuilder:rbac:groups="",resources=pods,verbs=get;list;watch,selectors=app.kubernetes.io/managed-by=wren

# Watch only worker pods
watch.Watch(pods, metav1.ListOptions{
    LabelSelector: "wren.giar.dev/role=worker",
})
```

---

## Modifying Labels and Annotations

### User-Defined Labels

Users can add custom labels to WrenJob:

```bash
kubectl label wrenjob ml-training team=data-science priority=high

# Query by custom label
kubectl get wrenjob -l team=data-science
```

### System Labels (Read-Only)

These labels are set by Wren and should not be modified:
- `app.kubernetes.io/managed-by=wren`
- `wren.giar.dev/job-name`
- `wren.giar.dev/role`
- `wren.giar.dev/rank`

Modifying these may cause reconciliation conflicts.

### User Annotation

The `wren.giar.dev/user` annotation is set by the webhook and immutable. Do not try to override it.

---

## API Aggregation

When listing resources, labels enable efficient filtering without API calls:

```bash
# Instead of:
for job in $(kubectl get wrenjob -o name); do
  kubectl get pods -l wren.giar.dev/job-name=$job
done

# Use:
kubectl get pods -l app.kubernetes.io/managed-by=wren
```

This single query retrieves all Wren-managed pods across all jobs and namespaces.
