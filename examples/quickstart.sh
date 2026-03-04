#!/usr/bin/env bash
# =============================================================================
# Wren Quickstart — spin up a local cluster and run example jobs.
#
# Usage:
#   ./examples/quickstart.sh              # Full setup + run examples
#   ./examples/quickstart.sh --no-cluster # Skip cluster creation (reuse existing)
#   ./examples/quickstart.sh --cleanup    # Tear down the cluster
# =============================================================================
set -euo pipefail

CLUSTER_NAME="wren-test"
CONTROLLER_IMAGE="wren-controller:dev"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
fail()  { echo -e "${RED}[FAIL]${NC}  $*"; }

# ---------------------------------------------------------------------------
# Parse arguments
# ---------------------------------------------------------------------------
SKIP_CLUSTER=false
CLEANUP_ONLY=false

for arg in "$@"; do
    case "$arg" in
        --no-cluster) SKIP_CLUSTER=true ;;
        --cleanup)    CLEANUP_ONLY=true ;;
        --help|-h)
            echo "Usage: $0 [--no-cluster] [--cleanup]"
            echo ""
            echo "  --no-cluster  Skip kind cluster creation (reuse existing)"
            echo "  --cleanup     Delete the kind cluster and exit"
            exit 0
            ;;
    esac
done

# ---------------------------------------------------------------------------
# Cleanup mode
# ---------------------------------------------------------------------------
if $CLEANUP_ONLY; then
    info "Deleting kind cluster '$CLUSTER_NAME'..."
    kind delete cluster --name "$CLUSTER_NAME" 2>/dev/null && ok "Cluster deleted" || warn "Cluster not found"
    exit 0
fi

# ---------------------------------------------------------------------------
# Prerequisites check
# ---------------------------------------------------------------------------
info "Checking prerequisites..."

for cmd in kind kubectl docker cargo; do
    if ! command -v "$cmd" &>/dev/null; then
        fail "'$cmd' is required but not found. Please install it first."
        exit 1
    fi
done
ok "All prerequisites found"

# ---------------------------------------------------------------------------
# Step 1: Create kind cluster
# ---------------------------------------------------------------------------
if $SKIP_CLUSTER; then
    info "Skipping cluster creation (--no-cluster)"
else
    if kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
        warn "Cluster '$CLUSTER_NAME' already exists. Reusing it."
        warn "  (Use --cleanup to delete it first, or --no-cluster to skip this step)"
    else
        info "Creating kind cluster '$CLUSTER_NAME' with 4 topology-labeled workers..."
        kind create cluster --name "$CLUSTER_NAME" --config "$ROOT_DIR/kind-config.yaml"
        ok "Cluster created"
    fi
fi

# Point kubectl at the kind cluster
kubectl cluster-info --context "kind-${CLUSTER_NAME}" &>/dev/null || {
    fail "Cannot reach cluster. Is kind running?"
    exit 1
}
ok "Cluster is reachable"

# ---------------------------------------------------------------------------
# Step 2: Install CRDs
# ---------------------------------------------------------------------------
info "Installing CRDs..."
kubectl apply -f "$ROOT_DIR/manifests/crds/"
ok "CRDs installed"

# Verify
kubectl get crd wrenjobs.wren.giar.dev &>/dev/null && ok "  WrenJob CRD registered" || fail "  WrenJob CRD missing"
kubectl get crd wrenqueues.wren.giar.dev &>/dev/null && ok "  WrenQueue CRD registered" || fail "  WrenQueue CRD missing"

# ---------------------------------------------------------------------------
# Step 3: Build and load controller image
# ---------------------------------------------------------------------------
info "Building controller Docker image..."
if docker build -f "$ROOT_DIR/docker/Dockerfile.controller" -t "$CONTROLLER_IMAGE" "$ROOT_DIR" 2>/dev/null; then
    ok "Image built: $CONTROLLER_IMAGE"
    info "Loading image into kind cluster..."
    kind load docker-image "$CONTROLLER_IMAGE" --name "$CLUSTER_NAME"
    ok "Image loaded into cluster"
else
    warn "Docker build failed (disk space?). Skipping controller deployment."
    warn "You can still create CRD objects — they just won't be reconciled."
    SKIP_CONTROLLER=true
fi

# ---------------------------------------------------------------------------
# Step 4: Deploy controller + RBAC
# ---------------------------------------------------------------------------
if [ "${SKIP_CONTROLLER:-false}" != "true" ]; then
    info "Deploying controller..."
    kubectl apply -f "$ROOT_DIR/manifests/rbac/rbac.yaml"
    kubectl apply -f "$ROOT_DIR/manifests/deployment.yaml"

    info "Waiting for controller to be ready..."
    if kubectl wait --for=condition=available deployment/wren-controller \
        -n wren-system --timeout=90s 2>/dev/null; then
        ok "Controller is running"
    else
        warn "Controller did not become ready in 90s. Jobs may not reconcile."
    fi
fi

# ---------------------------------------------------------------------------
# Step 5: Run examples
# ---------------------------------------------------------------------------
echo ""
echo "==========================================="
echo "  Wren is ready! Let's run some examples."
echo "==========================================="
echo ""

# --- Example 1: Hello World ---
info "Example 1: Hello World (single-node job)"
kubectl apply -f "$SCRIPT_DIR/01-hello-world/job.yaml"
ok "Created: hello-wren"
echo "  Check status:  kubectl get wrenjob hello-wren -o wide"
echo "  Watch:         kubectl get wrenjob hello-wren -w"
echo ""

sleep 2

# --- Example 2: Multi-node ---
info "Example 2: Multi-node job (2 workers, gang scheduled)"
kubectl apply -f "$SCRIPT_DIR/02-multi-node/job.yaml"
ok "Created: multi-node-demo"
echo "  Check pods:    kubectl get pods -l wren.giar.dev/job-name=multi-node-demo"
echo "  Check service: kubectl get svc multi-node-demo-workers"
echo ""

sleep 2

# --- Example 3: Queues ---
info "Example 3: Queue with policies"
kubectl apply -f "$SCRIPT_DIR/03-queues/queue.yaml"
kubectl apply -f "$SCRIPT_DIR/03-queues/job.yaml"
ok "Created: batch queue + batch-job"
echo "  List queues:   kubectl get wrenqueues"
echo "  Queue detail:  kubectl get wrenqueue batch -o yaml"
echo ""

sleep 2

# --- Example 4: Topology ---
info "Example 4: Topology-aware placement"
kubectl apply -f "$SCRIPT_DIR/04-topology/job.yaml"
ok "Created: topology-aware-job"
echo "  Check nodes:   kubectl get nodes --show-labels | grep topology.wren.io"
echo "  Check status:  kubectl get wrenjob topology-aware-job -o yaml"
echo ""

sleep 2

# --- Example 5: Priority ---
info "Example 5: Priority scheduling"
kubectl apply -f "$SCRIPT_DIR/05-priority/low-priority.yaml"
kubectl apply -f "$SCRIPT_DIR/05-priority/high-priority.yaml"
ok "Created: low-priority-job + high-priority-job"
echo "  List all jobs: kubectl get wrenjobs"
echo ""

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "==========================================="
echo "  All examples submitted!"
echo "==========================================="
echo ""
echo "Useful commands:"
echo "  kubectl get wrenjobs                         # List all jobs"
echo "  kubectl get wrenjob <name> -o yaml           # Job details"
echo "  kubectl get pods -l wren.giar.dev/job-name=<name> # Job pods"
echo "  kubectl get wrenqueues                      # List queues"
echo "  kubectl get nodes --show-labels             # Node topology"
echo ""
echo "To clean up examples:"
echo "  kubectl delete wrenjob --all"
echo "  kubectl delete wrenqueue --all"
echo ""
echo "To tear down the cluster:"
echo "  ./examples/quickstart.sh --cleanup"
echo ""
