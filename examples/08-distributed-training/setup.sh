#!/usr/bin/env bash
# 08-distributed-training: PyTorch DDP training across 2 bare-metal nodes.
#
# This demo creates a Kind cluster with Reaper installed, installs PyTorch on
# each worker node, and runs a distributed training job using ReaperPod CRDs.
#
# No custom HTTP API — ReaperPod → Pod (runtimeClassName: reaper-v2) → bare metal.
#
# Usage:
#   ./examples/08-distributed-training/setup.sh              # Run demo
#   ./examples/08-distributed-training/setup.sh --cleanup     # Delete cluster
#
# Prerequisites:
#   - Docker running
#   - kind (https://kind.sigs.k8s.io/)
#   - kubectl
#   - Reaper repo cloned alongside wren (../reaper)
set -euo pipefail

CLUSTER_NAME="wren-distributed-training"
KUBECONFIG_PATH="/tmp/wren-${CLUSTER_NAME}-kubeconfig"
LOG_FILE="/tmp/wren-${CLUSTER_NAME}-setup.log"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WREN_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
REAPER_ROOT="$(cd "$WREN_ROOT/../reaper" && pwd)"

# ---------------------------------------------------------------------------
# Colors (respect NO_COLOR)
# ---------------------------------------------------------------------------
if [[ -t 1 && "${NO_COLOR:-}" == "" ]]; then
  C_GREEN="\033[0;32m"; C_YELLOW="\033[0;33m"; C_CYAN="\033[0;36m"
  C_RED="\033[0;31m"; C_BOLD="\033[1m"; C_DIM="\033[0;37m"; C_RESET="\033[0m"
else
  C_GREEN=""; C_YELLOW=""; C_CYAN=""; C_RED=""; C_BOLD=""; C_DIM=""; C_RESET=""
fi

info()  { printf "${C_CYAN}[INFO]${C_RESET}  %s\n" "$*"; }
ok()    { printf "${C_GREEN}[OK]${C_RESET}    %s\n" "$*"; }
warn()  { printf "${C_YELLOW}[WARN]${C_RESET}  %s\n" "$*"; }
fail()  { printf "${C_RED}[FAIL]${C_RESET}  %s\n" "$*"; exit 1; }

# ---------------------------------------------------------------------------
# Cleanup mode
# ---------------------------------------------------------------------------
if [[ "${1:-}" == "--cleanup" ]]; then
  info "Deleting Kind cluster '${CLUSTER_NAME}'..."
  kind delete cluster --name "$CLUSTER_NAME" 2>/dev/null || true
  rm -f "$KUBECONFIG_PATH"
  ok "Cleanup complete."
  exit 0
fi

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------
for cmd in docker kind kubectl; do
  command -v "$cmd" >/dev/null || fail "Required tool '$cmd' not found"
done

if [[ ! -d "$REAPER_ROOT/scripts" ]]; then
  fail "Reaper repo not found at $REAPER_ROOT. Clone it alongside wren."
fi

# ---------------------------------------------------------------------------
# Create Kind cluster (1 control-plane + 2 workers)
# ---------------------------------------------------------------------------
KIND_CONFIG=$(mktemp)
cat > "$KIND_CONFIG" <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
  - role: worker
  - role: worker
EOF

if kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
  info "Cluster '${CLUSTER_NAME}' already exists, reusing."
else
  info "Creating Kind cluster '${CLUSTER_NAME}' (1 control-plane + 2 workers)..."
  kind create cluster --name "$CLUSTER_NAME" --config "$KIND_CONFIG" >> "$LOG_FILE" 2>&1
  ok "Cluster created."
fi
rm -f "$KIND_CONFIG"

kind get kubeconfig --name "$CLUSTER_NAME" > "$KUBECONFIG_PATH"
export KUBECONFIG="$KUBECONFIG_PATH"

# ---------------------------------------------------------------------------
# Install Reaper runtime + ReaperPod controller
# ---------------------------------------------------------------------------
info "Installing Reaper runtime on all nodes..."
cd "$REAPER_ROOT"
./scripts/setup-playground.sh --cluster-name "$CLUSTER_NAME" --skip-build >> "$LOG_FILE" 2>&1 || \
  ./scripts/setup-playground.sh --cluster-name "$CLUSTER_NAME" >> "$LOG_FILE" 2>&1
ok "Reaper runtime installed."

info "Installing ReaperPod CRD and controller..."
kubectl apply -f "$REAPER_ROOT/deploy/kubernetes/crds/reaperpods.reaper.io.yaml" >> "$LOG_FILE" 2>&1

# Build and load controller image if not already present
if ! docker image inspect ghcr.io/miguelgila/reaper-controller:latest > /dev/null 2>&1; then
  ./scripts/build-controller-image.sh --cluster-name "$CLUSTER_NAME" >> "$LOG_FILE" 2>&1
else
  kind load docker-image ghcr.io/miguelgila/reaper-controller:latest --name "$CLUSTER_NAME" >> "$LOG_FILE" 2>&1
fi
kubectl apply -f "$REAPER_ROOT/deploy/kubernetes/reaper-controller.yaml" >> "$LOG_FILE" 2>&1
kubectl rollout status deployment/reaper-controller -n reaper-system --timeout=120s >> "$LOG_FILE" 2>&1
ok "ReaperPod controller running."
cd "$WREN_ROOT"

# ---------------------------------------------------------------------------
# Verify RuntimeClass
# ---------------------------------------------------------------------------
info "Verifying RuntimeClass reaper-v2..."
for i in $(seq 1 15); do
  kubectl get runtimeclass reaper-v2 &>/dev/null && break
  sleep 1
done
kubectl get runtimeclass reaper-v2 &>/dev/null || fail "RuntimeClass reaper-v2 not found"
ok "RuntimeClass ready."

# ---------------------------------------------------------------------------
# Install Python3 + PyTorch (CPU-only) on worker nodes
# ---------------------------------------------------------------------------
WORKERS=($(kubectl get nodes --no-headers -o custom-columns=NAME:.metadata.name | grep worker | sort))
if [[ ${#WORKERS[@]} -lt 2 ]]; then
  fail "Expected at least 2 worker nodes, found ${#WORKERS[@]}"
fi

info "Installing Python3 + PyTorch (CPU) on worker nodes..."
info "(This may take a few minutes on first run)"

for worker in "${WORKERS[@]}"; do
  if docker exec "$worker" python3 -c "import torch" 2>/dev/null; then
    ok "$worker already has PyTorch"
  else
    info "  Installing on $worker..."
    docker exec "$worker" bash -c "
      apt-get update -qq && \
      apt-get install -y -qq python3 python3-pip > /dev/null 2>&1 && \
      pip3 install --break-system-packages --quiet \
        torch --index-url https://download.pytorch.org/whl/cpu
    " >> "$LOG_FILE" 2>&1 || fail "Failed to install PyTorch on $worker"
    ok "  $worker ready"
  fi
done

# ---------------------------------------------------------------------------
# Discover node IPs
# ---------------------------------------------------------------------------
info "Discovering node topology..."
WORKER0="${WORKERS[0]}"
WORKER1="${WORKERS[1]}"
MASTER_IP=$(kubectl get node "$WORKER0" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
ok "Node 0: $WORKER0 (IP: $MASTER_IP) — rank 0 / master"
ok "Node 1: $WORKER1 — rank 1"

# ---------------------------------------------------------------------------
# Deploy ConfigMap with training script
# ---------------------------------------------------------------------------
info "Creating ConfigMap with training script..."
kubectl apply -f "$SCRIPT_DIR/configmap.yaml" >> "$LOG_FILE" 2>&1
ok "ConfigMap pytorch-ddp-train created."

# ---------------------------------------------------------------------------
# Create ReaperPod CRDs (one per rank)
# ---------------------------------------------------------------------------
echo ""
printf "${C_BOLD}=== Distributed Training Demo ===${C_RESET}\n"
echo ""

info "Creating ReaperPod CRDs..."

# Clean up any previous run
kubectl delete reaperpod pytorch-rank-0 pytorch-rank-1 --ignore-not-found >> "$LOG_FILE" 2>&1
sleep 2

for rank in 0 1; do
  if [[ $rank -eq 0 ]]; then
    NODE="$WORKER0"
  else
    NODE="$WORKER1"
  fi

  kubectl apply -f - <<YAML
apiVersion: reaper.io/v1alpha1
kind: ReaperPod
metadata:
  name: pytorch-rank-${rank}
  labels:
    app: pytorch-ddp
    rank: "${rank}"
spec:
  command: ["python3", "/opt/train/train.py"]
  nodeSelector:
    kubernetes.io/hostname: ${NODE}
  env:
    - name: MASTER_ADDR
      value: "${MASTER_IP}"
    - name: MASTER_PORT
      value: "29500"
    - name: RANK
      value: "${rank}"
    - name: WORLD_SIZE
      value: "2"
    - name: LOCAL_RANK
      value: "0"
    - name: PYTHONUNBUFFERED
      value: "1"
  volumes:
    - name: training-script
      mountPath: /opt/train
      readOnly: true
      configMap: pytorch-ddp-train
YAML
  ok "Rank $rank ReaperPod created → $NODE"
done

# ---------------------------------------------------------------------------
# Wait for both pods to complete
# ---------------------------------------------------------------------------
echo ""
info "Waiting for training to complete..."

MAX_WAIT=120
POLL_INTERVAL=3
elapsed=0

while [[ $elapsed -lt $MAX_WAIT ]]; do
  PHASE0=$(kubectl get reaperpod pytorch-rank-0 -o jsonpath='{.status.phase}' 2>/dev/null || echo "Pending")
  PHASE1=$(kubectl get reaperpod pytorch-rank-1 -o jsonpath='{.status.phase}' 2>/dev/null || echo "Pending")

  printf "\r  rank 0: %-12s  rank 1: %-12s  (%ds)" "$PHASE0" "$PHASE1" "$elapsed"

  if [[ "$PHASE0" == "Failed" || "$PHASE1" == "Failed" ]]; then
    echo ""
    warn "Training failed! Logs below."
    echo ""
    POD0=$(kubectl get reaperpod pytorch-rank-0 -o jsonpath='{.status.podName}' 2>/dev/null)
    POD1=$(kubectl get reaperpod pytorch-rank-1 -o jsonpath='{.status.podName}' 2>/dev/null)
    printf "${C_BOLD}=== Rank 0 Logs ===${C_RESET}\n"
    kubectl logs "$POD0" 2>/dev/null || echo "(no logs)"
    echo ""
    printf "${C_BOLD}=== Rank 1 Logs ===${C_RESET}\n"
    kubectl logs "$POD1" 2>/dev/null || echo "(no logs)"
    exit 1
  fi

  if [[ "$PHASE0" == "Succeeded" && "$PHASE1" == "Succeeded" ]]; then
    echo ""
    ok "Both ranks completed successfully!"
    break
  fi

  sleep $POLL_INTERVAL
  elapsed=$((elapsed + POLL_INTERVAL))
done

if [[ $elapsed -ge $MAX_WAIT ]]; then
  echo ""
  fail "Timeout after ${MAX_WAIT}s. Jobs did not complete."
fi

# ---------------------------------------------------------------------------
# Print output from both ranks
# ---------------------------------------------------------------------------
POD0=$(kubectl get reaperpod pytorch-rank-0 -o jsonpath='{.status.podName}')
POD1=$(kubectl get reaperpod pytorch-rank-1 -o jsonpath='{.status.podName}')

echo ""
printf "${C_BOLD}=== Rank 0 Output ===${C_RESET}\n"
kubectl logs "$POD0"

echo ""
printf "${C_BOLD}=== Rank 1 Output ===${C_RESET}\n"
kubectl logs "$POD1"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
printf "${C_BOLD}=== Demo Complete ===${C_RESET}\n"
echo ""
echo "What happened:"
echo "  1. Kind cluster with 2 worker nodes"
echo "  2. Reaper runtime installed (runtimeClassName: reaper-v2)"
echo "  3. ReaperPod CRD + controller installed"
echo "  4. Python3 + PyTorch (CPU) installed on each node"
echo "  5. Training script delivered as ConfigMap volume"
echo "  6. 2 ReaperPod CRDs created — controller made real Pods"
echo "  7. Reaper ran them directly on bare metal"
echo "  8. PyTorch Gloo backend synced gradients across nodes"
echo ""
echo "No custom HTTP API — just ReaperPod CRDs + ConfigMaps."
echo ""
echo "With the Wren controller, this is a single command:"
echo "  kubectl apply -f configmap.yaml"
echo "  kubectl apply -f user.yaml"
echo "  wren submit job.yaml"
echo ""
echo "Cleanup: $0 --cleanup"
