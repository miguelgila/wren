# Installation

This guide covers installing Wren in your Kubernetes cluster. Choose the installation method that best fits your workflow.

## Prerequisites

**Cluster:**
- Kubernetes 1.28 or later with a working container runtime
- `kubectl` access with permissions to create CRDs and ClusterRoles
- At least 2 worker nodes for meaningful scheduling (1 for controller, 1+ for jobs)

**Client Tools:**
- `kubectl` (any recent version)
- `helm` 3.0+ (for Helm-based installation, recommended)
- `docker` (optional, only needed to build the controller image from source)

## Option 1: Install via Helm Chart (Recommended)

The Helm chart is the simplest way to deploy Wren to a cluster. It installs CRDs, creates RBAC resources, and deploys the controller.

### 1. Add the Helm repository (if applicable)

Wren is hosted on GitHub. You can use the OCI registry:

```bash
helm repo add wren oci://ghcr.io/miguelgila/charts --insecure
helm repo update
```

Or use a local checkout:

```bash
git clone https://github.com/miguelgila/wren.git
cd wren
```

### 2. Install the Wren Helm chart

**Basic installation:**

```bash
helm install wren charts/wren \
  --namespace wren-system \
  --create-namespace
```

**With custom values (e.g., Reaper backend enabled):**

```bash
helm install wren charts/wren \
  --namespace wren-system \
  --create-namespace \
  --set reaper.enabled=true \
  --set metrics.enabled=true
```

**From a release (if using remote registry):**

```bash
helm install wren wren/wren \
  --namespace wren-system \
  --create-namespace \
  --version 0.3.0
```

### 3. Verify the installation

```bash
# Check that the namespace exists
kubectl get ns wren-system

# Verify CRDs are installed
kubectl get crd wrenjobs.wren.giar.dev wrenjobs.wren.giar.dev wrenqueues.wren.giar.dev wrenusers.wren.giar.dev

# Verify the controller is running
kubectl -n wren-system get deployment wren-controller

# Watch the controller pod startup
kubectl -n wren-system logs -f deployment/wren-controller
```

The controller should reach `Running` status within 10-30 seconds.

### Helm Chart Options

Key values in `values.yaml`:

```yaml
replicaCount: 1                    # Number of controller replicas

image:
  repository: ghcr.io/miguelgila/wren-controller
  tag: "0.3.0"                     # Use chart appVersion if empty
  pullPolicy: IfNotPresent

leaderElection:
  enabled: true                    # Required for HA (multiple replicas)

metrics:
  enabled: true
  port: 8080
  serviceMonitor:
    enabled: false                 # Set true if prometheus-operator is installed

webhook:
  enabled: false                   # Enable mutating/validating webhooks (see advanced setup)

reaper:
  enabled: false                   # Enable bare-metal execution backend

logLevel: info                      # RUST_LOG env var

resources:
  limits:
    cpu: 500m
    memory: 256Mi
  requests:
    cpu: 100m
    memory: 128Mi
```

For a complete list of options, see the Helm chart's `values.yaml`:

```bash
helm show values wren/wren  # if using remote registry
# or
helm show values charts/wren  # if using local checkout
```

## Option 2: Install via Raw Manifests

For manual control or environments without Helm, apply the manifests directly:

```bash
# Create the wren-system namespace
kubectl create namespace wren-system

# Install CRDs
kubectl apply -f manifests/crds/

# Install RBAC (ServiceAccount, ClusterRole, ClusterRoleBinding)
kubectl apply -f manifests/rbac/

# Install the controller Deployment
kubectl apply -f manifests/deployment.yaml

# Optional: install the metrics Service
kubectl apply -f manifests/service.yaml
```

Verify:

```bash
kubectl -n wren-system get all
kubectl get crd | grep wren
```

## Option 3: Build and Install from Source

For development or custom builds:

### 1. Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable, 2021 edition or later)
- `docker` or equivalent container runtime
- `cargo` (comes with Rust)

### 2. Clone the repository

```bash
git clone https://github.com/miguelgila/wren.git
cd wren
```

### 3. Build the controller binary

```bash
# Build all binaries
cargo build --workspace --release

# Or just the controller
cargo build --release -p wren-controller
```

The binary is at `target/release/wren-controller`.

### 4. Build the container image

```bash
# Build locally
docker build -f docker/Dockerfile.controller -t wren-controller:dev .

# Load into kind (if using kind for testing)
kind load docker-image wren-controller:dev --name wren-test

# Or push to a registry
docker tag wren-controller:dev my-registry/wren-controller:latest
docker push my-registry/wren-controller:latest
```

### 5. Deploy

Use the manifests or Helm chart, pointing to your custom image:

**With Helm:**

```bash
helm install wren charts/wren \
  --namespace wren-system \
  --create-namespace \
  --set image.repository=my-registry/wren-controller \
  --set image.tag=latest
```

**With manifests:**

Edit `manifests/deployment.yaml` and change the image line:

```yaml
spec:
  containers:
    - name: wren-controller
      image: my-registry/wren-controller:latest
      imagePullPolicy: Always
```

Then apply:

```bash
kubectl apply -f manifests/rbac/
kubectl apply -f manifests/crds/
kubectl apply -f manifests/deployment.yaml
```

## Cross-Compiling for Linux

If building on macOS for Linux deployment:

```bash
# Using musl for fully static binaries
docker run --rm -v "$(pwd)":/home/rust/src \
  messense/rust-musl-cross:x86_64-musl \
  cargo build --workspace --release

# Binary is at target/x86_64-unknown-linux-musl/release/wren-controller
```

## Verify the Installation

After any installation method, verify:

```bash
# Namespace exists
kubectl get ns wren-system

# CRDs are registered
kubectl get crd | grep wren

# Controller is running
kubectl -n wren-system get deployment wren-controller
kubectl -n wren-system get pods

# Controller is healthy
kubectl -n wren-system logs deployment/wren-controller
kubectl -n wren-system get events --sort-by='.lastTimestamp'
```

## What Gets Installed

| Resource | Type | Namespace | Purpose |
|---|---|---|---|
| `wren-controller` | Deployment | wren-system | Main Wren controller pod |
| `wren-controller` | ServiceAccount | wren-system | Pod identity |
| `wren-controller` | ClusterRole | cluster-wide | Permissions to watch/create resources |
| `wren-controller` | ClusterRoleBinding | cluster-wide | Binds role to service account |
| `wren-controller` | Service | wren-system | Exposes metrics endpoint (port 8080) |
| `wrenjobs.wren.giar.dev` | CustomResourceDefinition | cluster-wide | WrenJob CRD |
| `wrenqueues.wren.giar.dev` | CustomResourceDefinition | cluster-wide | WrenQueue CRD |
| `wrenusers.wren.giar.dev` | CustomResourceDefinition | cluster-wide | WrenUser CRD |

## Next Steps

1. **[Quick Start](quick-start.md)** — Submit your first job
2. **[Cluster Setup](cluster-setup.md)** — Configure topology labels for better scheduling
3. **[User Identity](../user-guide/user-identity.md)** — Register users and configure identity mapping
