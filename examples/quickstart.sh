#!/usr/bin/env bash
# =============================================================================
# Wren Quickstart — spin up a local cluster and run example jobs.
#
# Usage:
#   ./examples/quickstart.sh                  # Full setup using GHCR latest image
#   ./examples/quickstart.sh --release 0.3.0  # Use a specific release version
#   ./examples/quickstart.sh --dev            # Build controller from source
#   ./examples/quickstart.sh --no-cluster     # Skip cluster creation (reuse existing)
#   ./examples/quickstart.sh --cleanup        # Tear down the cluster
# =============================================================================
set -euo pipefail

CLUSTER_NAME="wren-test"
KUBE_CONTEXT="kind-${CLUSTER_NAME}"
GHCR_IMAGE="ghcr.io/miguelgila/wren-controller"
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
BUILD_MODE="release"
RELEASE_TAG="latest"

usage() {
    echo "Usage: $0 [--no-cluster] [--cleanup] [--dev] [--release [VERSION]]"
    echo ""
    echo "  --no-cluster        Skip kind cluster creation (reuse existing)"
    echo "  --cleanup           Delete the kind cluster and exit"
    echo "  --dev               Build controller from source (requires cargo)"
    echo "  --release [VERSION] Pull pre-built image from GHCR (default: latest)"
    echo "  -h, --help          Show this help message"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --no-cluster) SKIP_CLUSTER=true ;;
        --cleanup)    CLEANUP_ONLY=true ;;
        --dev)        BUILD_MODE="dev" ;;
        --release)
            BUILD_MODE="release"
            if [ $# -ge 2 ] && [[ "$2" != --* ]]; then
                RELEASE_TAG="$2"
                shift
            fi
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            fail "Unknown option: $1"
            usage
            exit 1
            ;;
    esac
    shift
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

REQUIRED_CMDS="kind kubectl docker"
if [ "$BUILD_MODE" = "dev" ]; then
    REQUIRED_CMDS="$REQUIRED_CMDS cargo"
fi

for cmd in $REQUIRED_CMDS; do
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

# Verify the kind cluster is reachable (without changing default context)
kubectl cluster-info --context "$KUBE_CONTEXT" &>/dev/null || {
    fail "Cannot reach cluster. Is kind running?"
    exit 1
}
ok "Cluster is reachable (context: $KUBE_CONTEXT)"

# ---------------------------------------------------------------------------
# Step 2: Install CRDs
# ---------------------------------------------------------------------------
info "Installing CRDs..."
kubectl --context "$KUBE_CONTEXT" apply -f "$ROOT_DIR/manifests/crds/"
ok "CRDs installed"

# Verify
kubectl --context "$KUBE_CONTEXT" get crd wrenjobs.wren.giar.dev &>/dev/null && ok "  WrenJob CRD registered" || fail "  WrenJob CRD missing"
kubectl --context "$KUBE_CONTEXT" get crd wrenqueues.wren.giar.dev &>/dev/null && ok "  WrenQueue CRD registered" || fail "  WrenQueue CRD missing"

# ---------------------------------------------------------------------------
# Step 3: Get controller image
# ---------------------------------------------------------------------------
if [ "$BUILD_MODE" = "dev" ]; then
    info "Building controller Docker image from source..."
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
else
    # GHCR images are amd64-only; detect arch mismatch and fall back to --dev
    HOST_ARCH="$(docker info --format '{{.Architecture}}' 2>/dev/null || uname -m)"
    if [[ "$HOST_ARCH" != "x86_64" && "$HOST_ARCH" != "amd64" ]]; then
        warn "Pre-built GHCR image is amd64-only, but this machine is $HOST_ARCH."
        warn "Falling back to building from source (--dev mode)."
        if command -v cargo &>/dev/null; then
            BUILD_MODE="dev"
            info "Building controller Docker image from source..."
            if docker build -f "$ROOT_DIR/docker/Dockerfile.controller" -t "$CONTROLLER_IMAGE" "$ROOT_DIR" 2>/dev/null; then
                ok "Image built: $CONTROLLER_IMAGE"
                info "Loading image into kind cluster..."
                kind load docker-image "$CONTROLLER_IMAGE" --name "$CLUSTER_NAME"
                ok "Image loaded into cluster"
            else
                warn "Docker build failed. Skipping controller deployment."
                SKIP_CONTROLLER=true
            fi
        else
            fail "cargo is required to build from source on $HOST_ARCH. Install Rust or use an amd64 machine."
            exit 1
        fi
    else
        CONTROLLER_IMAGE="${GHCR_IMAGE}:${RELEASE_TAG}"
        info "Pulling controller image ${CONTROLLER_IMAGE}..."
        if docker pull "$CONTROLLER_IMAGE"; then
            ok "Image pulled: $CONTROLLER_IMAGE"
            info "Loading image into kind cluster..."
            kind load docker-image "$CONTROLLER_IMAGE" --name "$CLUSTER_NAME"
            ok "Image loaded into cluster"
        else
            fail "Failed to pull $CONTROLLER_IMAGE. Check the version tag exists on GHCR."
            exit 1
        fi
    fi
fi

# ---------------------------------------------------------------------------
# Step 4: Deploy controller + RBAC
# ---------------------------------------------------------------------------
if [ "${SKIP_CONTROLLER:-false}" != "true" ]; then
    info "Deploying controller..."
    # deployment.yaml creates the wren-system namespace; apply it before RBAC
    kubectl --context "$KUBE_CONTEXT" apply -f "$ROOT_DIR/manifests/deployment.yaml"
    kubectl --context "$KUBE_CONTEXT" apply -f "$ROOT_DIR/manifests/rbac/rbac.yaml"

    # Patch the image if using a release build
    if [ "$BUILD_MODE" != "dev" ]; then
        kubectl --context "$KUBE_CONTEXT" set image deployment/wren-controller \
            controller="$CONTROLLER_IMAGE" -n wren-system
    fi

    info "Waiting for controller to be ready..."
    if kubectl --context "$KUBE_CONTEXT" wait --for=condition=available deployment/wren-controller \
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

# Build the wren CLI if cargo is available
WREN_CLI=""
if command -v cargo &>/dev/null; then
    info "Building wren CLI..."
    if cargo build --release --bin wren -q 2>/dev/null; then
        WREN_CLI="$ROOT_DIR/target/release/wren"
        ok "CLI built: $WREN_CLI"
        info "To use it: export PATH=\"$ROOT_DIR/target/release:\$PATH\""
    else
        warn "CLI build failed; falling back to kubectl for examples."
    fi
fi

CTX="--context $KUBE_CONTEXT"

# --- Example 1: Hello World ---
info "Example 1: Hello World (single-node job)"
kubectl --context "$KUBE_CONTEXT" apply -f "$SCRIPT_DIR/01-hello-world/job.yaml"
ok "Created: hello-wren"
echo "  Check status:  wren status hello-wren"
echo "  Or with kubectl: kubectl $CTX get wrenjob hello-wren -o wide"
echo ""

sleep 2

# --- Example 2: Multi-node ---
info "Example 2: Multi-node job (2 workers, gang scheduled)"
kubectl --context "$KUBE_CONTEXT" apply -f "$SCRIPT_DIR/02-multi-node/job.yaml"
ok "Created: multi-node-demo"
echo "  Check status:  wren status multi-node-demo"
echo "  Check pods:    kubectl $CTX get pods -l wren.giar.dev/job-name=multi-node-demo"
echo ""

sleep 2

# --- Example 3: Queues ---
info "Example 3: Queue with policies"
kubectl --context "$KUBE_CONTEXT" apply -f "$SCRIPT_DIR/03-queues/queue.yaml"
kubectl --context "$KUBE_CONTEXT" apply -f "$SCRIPT_DIR/03-queues/job.yaml"
ok "Created: batch queue + batch-job"
echo "  List queues:   wren queue"
echo "  Queue detail:  kubectl $CTX get wrenqueue batch -o yaml"
echo ""

sleep 2

# --- Example 4: Topology ---
info "Example 4: Topology-aware placement"
kubectl --context "$KUBE_CONTEXT" apply -f "$SCRIPT_DIR/04-topology/job.yaml"
ok "Created: topology-aware-job"
echo "  Check nodes:   kubectl $CTX get nodes --show-labels | grep topology.wren.giar.dev"
echo "  Check status:  wren status topology-aware-job"
echo ""

sleep 2

# --- Example 5: Priority ---
info "Example 5: Priority scheduling"
kubectl --context "$KUBE_CONTEXT" apply -f "$SCRIPT_DIR/05-priority/low-priority.yaml"
kubectl --context "$KUBE_CONTEXT" apply -f "$SCRIPT_DIR/05-priority/high-priority.yaml"
ok "Created: low-priority-job + high-priority-job"
echo "  List all jobs: wren queue"
echo ""

sleep 2

# --- Example 6: Dependencies ---
info "Example 6: Job dependencies (preprocess → train → evaluate)"
kubectl --context "$KUBE_CONTEXT" apply -f "$SCRIPT_DIR/06-dependencies/pipeline.yaml"
ok "Created: preprocess + train + evaluate (dependency chain)"
echo "  List all jobs: wren queue"
echo "  NOTE: Dependencies are defined in the CRD but not yet reconciled."
echo ""

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "==========================================="
echo "  All examples submitted!"
echo "==========================================="
echo ""
if [ -n "$WREN_CLI" ]; then
    echo "Add the wren CLI to your PATH:"
    echo "  export PATH=\"$ROOT_DIR/target/release:\$PATH\""
    echo ""
fi
echo "Useful commands (wren CLI):"
echo "  wren queue                                   # List all jobs"
echo "  wren status <name>                           # Job details"
echo "  wren logs <name>                             # Job logs"
echo "  wren logs <name> --rank 0                    # Logs for a specific rank"
echo "  wren cancel <name>                           # Cancel a job"
echo ""
echo "Useful commands (kubectl):"
echo "  kubectl $CTX get wrenjobs                         # List all jobs"
echo "  kubectl $CTX get wrenjob <name> -o yaml           # Job details"
echo "  kubectl $CTX get pods -l wren.giar.dev/job-name=<name> # Job pods"
echo "  kubectl $CTX get wrenqueues                       # List queues"
echo "  kubectl $CTX get nodes --show-labels              # Node topology"
echo ""
echo "To clean up examples:"
echo "  kubectl $CTX delete wrenjob --all"
echo "  kubectl $CTX delete wrenqueue --all"
echo ""
echo "To tear down the cluster:"
echo "  ./examples/quickstart.sh --cleanup"
echo ""
