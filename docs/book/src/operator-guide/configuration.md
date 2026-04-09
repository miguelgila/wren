# Controller Configuration

The Wren controller is configured via environment variables and the Helm chart values. This page describes all configuration options.

## Environment Variables

### Logging

| Variable | Default | Description |
|----------|---------|-------------|
| `RUST_LOG` | `info` | Log level for the controller. Set to `debug` for detailed tracing, `warn` for minimal output. Supports per-module filtering (e.g., `wren_controller=debug,wren_scheduler=info`). |

### Leader Election

| Variable | Default | Description |
|----------|---------|-------------|
| `WREN_LEADER_ELECTION` | `true` | Enable leader election for HA deployments. Set to `false` to disable. When enabled, only one controller replica will process jobs; others remain idle. |
| `WREN_LEADER_ELECTION_IDENTITY` | hostname | Unique identifier for this controller instance. Defaults to the pod hostname. |
| `WREN_LEADER_ELECTION_LEASE_NAMESPACE` | `wren-system` | Namespace where the leader election Lease is stored. |
| `WREN_LEADER_ELECTION_LEASE_NAME` | `wren-controller-leader` | Name of the Lease resource. |
| `WREN_LEADER_ELECTION_LEASE_DURATION` | `15s` | Duration the leader holds the lease. |
| `WREN_LEADER_ELECTION_RENEW_DEADLINE` | `10s` | Deadline for renewing the lease before giving it up. |
| `WREN_LEADER_ELECTION_RETRY_PERIOD` | `2s` | Interval for attempting to acquire the lease when not leader. |

### Metrics Server

| Variable | Default | Description |
|----------|---------|-------------|
| `WREN_METRICS_PORT` | `8080` | Port where Prometheus metrics are exposed at `/metrics`. |
| `WREN_METRICS_ADDR` | `0.0.0.0` | Address to bind metrics server to. |
| `WREN_HEALTH_PROBE_PORT` | `8080` | Port for liveness (`/healthz`) and readiness (`/readyz`) probes. |

### Kubernetes API

| Variable | Default | Description |
|----------|---------|-------------|
| `KUBECONFIG` | (in-cluster) | Path to kubeconfig file. When running in a pod, uses in-cluster config automatically. |

## Helm Chart Values

The Wren Helm chart (`charts/wren/`) provides a production-ready way to deploy the controller. Key values:

### Image Configuration

```yaml
image:
  repository: ghcr.io/miguelgila/wren-controller
  tag: ""              # Defaults to Chart.appVersion
  pullPolicy: IfNotPresent
```

### Replicas and Scaling

```yaml
replicaCount: 1        # Typically 1 with leader election enabled
                       # Use 3+ for HA without leader election (requires additional state management)
```

### ServiceAccount and RBAC

```yaml
serviceAccount:
  create: true         # Automatically create a ServiceAccount
  name: ""            # If empty, uses the fullname helper (wren-controller)
  annotations: {}     # Additional annotations for the service account
```

The controller requires cluster-wide permissions to:
- Watch and list WrenJob, WrenQueue, WrenUser (all namespaces)
- Create, get, patch Pods (all namespaces)
- Create, list, watch Services, ConfigMaps, Secrets (all namespaces)
- Read Node (resource discovery)
- Create Lease (leader election)

All permissions are defined in `charts/wren/templates/rbac.yaml`.

### Leader Election

```yaml
leaderElection:
  enabled: true        # Recommended for production HA
```

When enabled:
- Only one controller replica becomes leader
- Others enter a dormant state, polling for leader changes
- If leader disappears, a new leader is elected within ~15 seconds
- No additional configuration needed; uses the Lease resource in the controller's namespace

### Metrics

```yaml
metrics:
  enabled: true        # Expose Prometheus metrics
  port: 8080          # Where to serve /metrics and /healthz
  serviceMonitor:
    enabled: false    # Set to true if you have prometheus-operator installed
    interval: 30s     # How often Prometheus scrapes metrics
    scrapeTimeout: 10s
    additionalLabels: {}
```

When `serviceMonitor.enabled: true`, a ServiceMonitor resource is created automatically for the Prometheus Operator to discover and scrape metrics.

### Webhooks

```yaml
webhook:
  enabled: false      # Admission webhooks (currently scaffolded, not in use)
  port: 9443
```

### Resource Limits

```yaml
resources:
  limits:
    cpu: 500m
    memory: 256Mi
  requests:
    cpu: 100m
    memory: 128Mi
```

Typical for a cluster with 100-200 jobs. Scale up for larger clusters:
- 500-1000 jobs: `500m` CPU, `512Mi` memory
- 1000+ jobs: `1000m` CPU, `1Gi` memory

### Probes

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

- **Liveness** — if `/healthz` fails 3 times in a row (30 seconds), Kubernetes restarts the pod
- **Readiness** — if `/readyz` fails 3 times (15 seconds), the pod is removed from the service and doesn't receive requests
- Both are required for stable operation with leader election

### Affinity, Tolerations, Node Selection

```yaml
nodeSelector: {}     # Restrict controller to nodes with specific labels
tolerations: []      # Allow controller on tainted nodes
affinity: {}         # Advanced pod affinity rules
```

Example: run controller on control plane only

```yaml
tolerations:
  - key: node-role.kubernetes.io/control-plane
    operator: Exists
    effect: NoSchedule

nodeSelector:
  node-role.kubernetes.io/control-plane: ""
```

### Pod Security

```yaml
podSecurityContext: {}    # (Empty) — no special pod-level security context

securityContext:
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true    # /tmp and /var/run are emptyDir volumes
  runAsNonRoot: true
  runAsUser: 65532               # "nonroot" user in distroless images
  capabilities:
    drop:
      - ALL
```

These enforce a restrictive security posture (non-root, read-only filesystem, no capabilities).

### Pod Annotations

```yaml
podAnnotations:
  prometheus.io/scrape: "true"
  prometheus.io/port: "8080"
  prometheus.io/path: "/metrics"
```

For Prometheus scrape configs that use pod annotations (not ServiceMonitor).

### Reaper Subchart

```yaml
reaper:
  enabled: false      # Enable to deploy Reaper alongside Wren
```

When enabled, Wren can dispatch jobs to bare-metal nodes via the Reaper backend. See [Reaper Integration](reaper-integration.md).

## Log Level Configuration

The controller uses the `tracing` crate for structured logging. Set `RUST_LOG` to control verbosity:

| Level | Use Case |
|-------|----------|
| `error` | Only failures (operator on-call) |
| `warn` | Recoverable issues (job scheduling failures) |
| `info` | State transitions (job created, scheduling began, completed) |
| `debug` | Detailed scheduling decisions, API calls, resource allocations |
| `trace` | Everything (not recommended for production) |

### Per-Module Configuration

```bash
RUST_LOG=wren_controller=debug,wren_scheduler=info,wren_core=warn
```

This sets different levels for different crates. Useful for debugging specific components.

## Health Checks

The controller exposes two health probes:

### GET /healthz (Liveness)

Returns `200 OK` if the controller is running and able to process jobs. Indicates that Kubernetes should not restart the pod.

### GET /readyz (Readiness)

Returns `200 OK` once the controller has:
1. Acquired the leader lease (if leader election enabled)
2. Connected to the Kubernetes API server
3. Synced initial node and job lists

During startup or if leadership is lost, readiness fails and Kubernetes removes the pod from load balancing.

## Kubernetes Cluster Requirements

Wren requires a Kubernetes cluster with:

- **API Server**: Standard kube-apiserver
- **Node Labels**: Recommended (for topology-aware scheduling):
  - `topology.kubernetes.io/zone` (for zone-level topology)
  - Optional custom labels like `topology.kubernetes.io/switch-group` for network topology
- **Resource Quotas** (optional): If enabled, Wren respects namespace quotas when scheduling jobs
- **Pod Security Policies** (optional): The controller runs as non-root with read-only filesystem
- **RBAC**: Required; the controller needs permissions to manage Pods, Services, ConfigMaps, and Secrets

## Example Helm Deployment

```bash
# Deploy with default settings
helm install wren charts/wren/ --namespace wren-system --create-namespace

# Deploy with custom image tag and 3 replicas for HA
helm install wren charts/wren/ \
  --namespace wren-system \
  --create-namespace \
  --set image.tag=v0.3.0 \
  --set replicaCount=3 \
  --set leaderElection.enabled=true

# Deploy with Reaper backend enabled
helm install wren charts/wren/ \
  --namespace wren-system \
  --create-namespace \
  --set reaper.enabled=true
```

## Troubleshooting Configuration Issues

### Controller pod not starting

Check logs:
```bash
kubectl logs -n wren-system deployment/wren-controller
```

Common causes:
- Missing RBAC permissions (check ClusterRole)
- kubeconfig invalid (if not using in-cluster config)
- Metrics port already in use on the node

### Leader election stuck

Check the Lease:
```bash
kubectl get lease -n wren-system wren-controller-leader -o yaml
```

If multiple replicas think they're leader, delete the Lease and restart the controller:
```bash
kubectl delete lease -n wren-system wren-controller-leader
kubectl rollout restart deployment/wren-controller -n wren-system
```

### High CPU usage

Increase log level to reduce tracing overhead:
```bash
kubectl set env deployment/wren-controller -n wren-system RUST_LOG=warn
```

Or check if the scheduler is running many passes per second (indicates thrashing):
```bash
kubectl logs -n wren-system deployment/wren-controller | grep scheduling_attempts
```
