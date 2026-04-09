# Helm Chart

The Wren Helm chart (`charts/wren/`) provides a production-ready, customizable way to deploy the controller and optional Reaper backend to a Kubernetes cluster.

## Chart Structure

```
charts/wren/
├── Chart.yaml              # Chart metadata and version
├── values.yaml             # Default configuration values
├── values.schema.json      # JSON schema for value validation
└── templates/
    ├── deployment.yaml     # Wren controller Deployment
    ├── service.yaml        # Metrics Service (ClusterIP)
    ├── servicemonitor.yaml # Prometheus Operator integration
    ├── rbac.yaml           # ServiceAccount, ClusterRole, ClusterRoleBinding
    ├── configmap.yaml      # (Optional) additional config
    └── _helpers.tpl        # Helm templating helpers
```

## Installation

### Prerequisites

- Kubernetes 1.20+
- Helm 3.0+
- kubectl configured to access your cluster

### Basic Installation

```bash
helm repo add wren https://charts.example.com/wren  # (replace with actual repo)
helm repo update

helm install wren wren/wren \
  --namespace wren-system \
  --create-namespace
```

This creates:
- Namespace `wren-system`
- ServiceAccount `wren-controller`
- ClusterRole and ClusterRoleBinding with necessary permissions
- Deployment `wren-controller` with 1 replica
- Service `wren-controller-metrics` on port 8080
- Leader election Lease `wren-controller-leader` in the same namespace

### Verify Installation

```bash
kubectl get all -n wren-system
kubectl logs -n wren-system deployment/wren-controller
```

Once the controller is running, you can submit jobs:

```bash
kubectl apply -f - <<EOF
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: hello-world
spec:
  queue: default
  nodes: 2
  tasksPerNode: 1
  container:
    image: busybox
    command: ["sh", "-c"]
    args: ["echo 'Hello from $(hostname)'"]
EOF
```

## Configuration Values

All configuration is in `values.yaml`. Override values at install time with `--set` or `-f`:

```bash
# Single value override
helm install wren charts/wren/ --set replicaCount=3

# Multiple values from file
helm install wren charts/wren/ -f custom-values.yaml
```

### Image Configuration

Control the container image for the controller:

```yaml
image:
  repository: ghcr.io/miguelgila/wren-controller
  tag: ""              # Empty defaults to Chart.appVersion (e.g., v0.3.0)
  pullPolicy: IfNotPresent
```

Override to use a specific version:

```bash
helm install wren charts/wren/ --set image.tag=v0.3.0
```

### Replicas

```yaml
replicaCount: 1
```

For high availability, increase to 3+ replicas. With leader election enabled (default), only one will be active; the others are standby.

### Service Account

```yaml
serviceAccount:
  create: true         # Create ServiceAccount automatically
  name: ""            # If empty, uses {{ include "wren.fullname" . }}
  annotations: {}     # Add annotations (e.g., IAM role on AWS EKS)
```

The ServiceAccount needs cluster-wide permissions granted by the ClusterRole in `rbac.yaml`.

### Resource Requests and Limits

```yaml
resources:
  limits:
    cpu: 500m          # Hard limit; pod is killed if exceeded
    memory: 256Mi
  requests:
    cpu: 100m          # Reserved on node; Kubernetes uses for scheduling
    memory: 128Mi
```

Recommendation:
- For small clusters (< 100 nodes): `100m` CPU request, `500m` limit, `128Mi` / `256Mi` memory
- For large clusters (> 500 nodes): `500m` CPU request, `1000m` limit, `512Mi` / `1Gi` memory

Test with your workload and scale based on observed usage in metrics.

### Leader Election

```yaml
leaderElection:
  enabled: true
```

When enabled (recommended for production):
- Multiple replicas compete for leadership
- Only the leader processes jobs; others are dormant
- Automatic failover if leader pod crashes
- No additional setup required

When disabled (for testing):
- Any replica will process jobs
- Allows faster job scheduling but loses HA
- Use only in non-critical environments

### Logging

```yaml
logLevel: info        # One of: trace, debug, info, warn, error
```

Maps to the `RUST_LOG` environment variable. Set to `debug` for detailed troubleshooting.

### Liveness and Readiness Probes

```yaml
livenessProbe:
  httpGet:
    path: /healthz
    port: metrics
  initialDelaySeconds: 5
  periodSeconds: 10
  failureThreshold: 3

readinessProbe:
  httpGet:
    path: /readyz
    port: metrics
  initialDelaySeconds: 3
  periodSeconds: 5
  failureThreshold: 3
```

- Liveness failing → Kubernetes restarts the pod
- Readiness failing → pod is removed from service (stopped accepting requests)

Adjust `initialDelaySeconds` if your cluster is slow; increase `periodSeconds` to reduce probe overhead.

### Metrics

```yaml
metrics:
  enabled: true       # Expose /metrics endpoint
  port: 8080         # Port for metrics and health probes
  serviceMonitor:
    enabled: false    # Set true if you have prometheus-operator
    interval: 30s
    scrapeTimeout: 10s
    additionalLabels: {}  # Labels for ServiceMonitor discovery
```

#### With Prometheus Operator

If your cluster has prometheus-operator installed:

```bash
helm install wren charts/wren/ \
  --set metrics.serviceMonitor.enabled=true \
  --set metrics.serviceMonitor.additionalLabels.release=prometheus
```

This creates a ServiceMonitor that Prometheus automatically discovers and scrapes.

#### With Prometheus Config

If you manage Prometheus scrape configs manually, use pod annotations:

```yaml
podAnnotations:
  prometheus.io/scrape: "true"
  prometheus.io/port: "8080"
  prometheus.io/path: "/metrics"
```

Then configure your Prometheus to scrape based on these annotations.

### Pod Security

```yaml
podSecurityContext: {}    # No special pod-level security context

securityContext:
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true   # Read-only except /tmp and /var/run
  runAsNonRoot: true
  runAsUser: 65532               # "nonroot" user in distroless images
  capabilities:
    drop:
      - ALL
```

These are security best practices. Do not weaken them in production.

### Node Affinity and Tolerations

```yaml
nodeSelector: {}      # Restrict to nodes with labels
tolerations: []       # Allow on tainted nodes
affinity: {}         # Pod affinity rules
```

Example: run on control plane only

```bash
helm install wren charts/wren/ \
  --set 'tolerations[0].key=node-role.kubernetes.io/control-plane' \
  --set 'tolerations[0].operator=Exists' \
  --set 'tolerations[0].effect=NoSchedule' \
  --set 'nodeSelector.node-role\.kubernetes\.io/control-plane=""'
```

### Reaper Backend

```yaml
reaper:
  enabled: false      # Set to true to deploy Reaper subchart
```

When enabled, a Reaper agent is deployed on each compute node. This allows Wren to dispatch jobs to bare-metal via the Reaper backend. See [Reaper Integration](reaper-integration.md) for details.

```bash
helm install wren charts/wren/ --set reaper.enabled=true
```

## RBAC Permissions

The ClusterRole grants the controller:

| Resource | Verbs | Scope | Why |
|----------|-------|-------|-----|
| `wrenjobs` | `get, list, watch, create, patch, update, delete` | all namespaces | Core job management |
| `wrenqueues` | `get, list, watch` | all namespaces | Queue configuration |
| `wrenusers` | `get, list, watch` | cluster-scoped | User identity resolution |
| `pods` | `get, list, watch, create, delete` | all namespaces | Pod lifecycle (container backend) |
| `services` | `get, list, watch, create, delete` | all namespaces | Headless services for MPI |
| `configmaps` | `get, list, watch, create, delete` | all namespaces | Hostfile and secrets distribution |
| `secrets` | `get, list, watch, create, delete` | all namespaces | SSH keys, credentials |
| `nodes` | `get, list, watch` | cluster-scoped | Topology discovery |
| `leases` | `get, create, update, delete` | controller's namespace | Leader election |

## Helm Upgrade

Update to a new version:

```bash
helm repo update
helm upgrade wren wren/wren --namespace wren-system
```

Rolling update: old replicas are replaced one by one, with zero downtime for running jobs.

## Helm Values Reference

For a complete reference of all available values, see `charts/wren/values.yaml` or run:

```bash
helm show values wren/wren
```

## Common Deployment Patterns

### Development (Single Replica, No Leader Election)

```bash
helm install wren charts/wren/ \
  --namespace wren-system \
  --create-namespace \
  --set leaderElection.enabled=false \
  --set replicaCount=1 \
  --set logLevel=debug
```

Use for testing and development. Fast scheduling, easy to restart.

### Production HA

```bash
helm install wren charts/wren/ \
  --namespace wren-system \
  --create-namespace \
  --set replicaCount=3 \
  --set leaderElection.enabled=true \
  --set logLevel=info \
  --set resources.requests.cpu=500m \
  --set resources.requests.memory=512Mi \
  --set resources.limits.cpu=1000m \
  --set resources.limits.memory=1Gi \
  --set metrics.serviceMonitor.enabled=true
```

Use for production. Three replicas with automatic failover, monitoring enabled.

### HPC Cluster with Reaper

```bash
helm install wren charts/wren/ \
  --namespace wren-system \
  --create-namespace \
  --set reaper.enabled=true \
  --set replicaCount=3 \
  --set leaderElection.enabled=true
```

Use for bare-metal HPC clusters. Deploys Wren controller and Reaper backend for job execution.

## Uninstallation

```bash
helm uninstall wren --namespace wren-system
kubectl delete namespace wren-system  # Remove namespace and all resources
```

This removes the controller, RBAC, and all related resources. Running jobs are terminated.

## Chart Versioning

Chart versions follow semantic versioning:
- Patch version: bug fixes, no breaking changes
- Minor version: new features, backwards compatible
- Major version: breaking changes (CRD updates, API changes)

Check `Chart.yaml` for the current version:

```bash
helm search repo wren
```
