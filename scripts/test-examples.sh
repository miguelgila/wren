#!/usr/bin/env bash
# test-examples.sh — Test all Wren examples against a Kind cluster.
#
# Usage:
#   ./scripts/test-examples.sh                  # Full run (create cluster + all tests)
#   ./scripts/test-examples.sh --skip-cluster   # Reuse existing Kind cluster
#   ./scripts/test-examples.sh --cleanup        # Delete Kind cluster and exit

set -euo pipefail

CLUSTER_NAME="wren-examples-test"
export KUBECONFIG="/tmp/wren-examples-test-kubeconfig"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

SKIP_CLUSTER=false
CLEANUP=false

while [[ $# -gt 0 ]]; do
  case $1 in
    --skip-cluster) SKIP_CLUSTER=true; shift ;;
    --cleanup)      CLEANUP=true; shift ;;
    -h|--help)
      echo "Usage: $0 [--skip-cluster] [--cleanup]"
      echo "  --skip-cluster   Reuse existing Kind cluster instead of creating one"
      echo "  --cleanup        Delete Kind cluster and exit"
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

# ---------------------------------------------------------------------------
# Color output (respect NO_COLOR)
# ---------------------------------------------------------------------------
if [[ -z "${NO_COLOR:-}" && -t 1 ]]; then
  RED='\033[0;31m'
  GREEN='\033[0;32m'
  YELLOW='\033[1;33m'
  BLUE='\033[0;34m'
  RESET='\033[0m'
else
  RED='' GREEN='' YELLOW='' BLUE='' RESET=''
fi

pass()  { echo -e "${GREEN}[PASS]${RESET} $*"; }
fail()  { echo -e "${RED}[FAIL]${RESET} $*"; }
skip()  { echo -e "${YELLOW}[SKIP]${RESET} $*"; }
info()  { echo -e "${BLUE}[INFO]${RESET} $*"; }

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------
if $CLEANUP; then
  info "Deleting Kind cluster: ${CLUSTER_NAME}"
  kind delete cluster --name "${CLUSTER_NAME}" 2>/dev/null || true
  rm -f "${KUBECONFIG}"
  info "Done."
  exit 0
fi

# ---------------------------------------------------------------------------
# Cluster setup
# ---------------------------------------------------------------------------
setup_cluster() {
  # Delete any existing cluster to ensure clean state (e.g. on CI retry)
  if kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
    info "Deleting existing Kind cluster: ${CLUSTER_NAME}"
    kind delete cluster --name "${CLUSTER_NAME}" 2>/dev/null || true
    rm -f "${KUBECONFIG}"
  fi

  info "Creating Kind cluster: ${CLUSTER_NAME}"

  local kind_config
  kind_config="$(mktemp /tmp/wren-examples-kind-config.XXXXXX.yaml)"
  cat > "${kind_config}" <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
  - role: worker
    labels:
      topology.wren.giar.dev/switch: "0"
      topology.wren.giar.dev/rack: "0"
      topology.kubernetes.io/zone: us-west-2a
  - role: worker
    labels:
      topology.wren.giar.dev/switch: "0"
      topology.wren.giar.dev/rack: "1"
      topology.kubernetes.io/zone: us-west-2a
  - role: worker
    labels:
      topology.wren.giar.dev/switch: "1"
      topology.wren.giar.dev/rack: "2"
      topology.kubernetes.io/zone: us-west-2b
  - role: worker
    labels:
      topology.wren.giar.dev/switch: "1"
      topology.wren.giar.dev/rack: "3"
      topology.kubernetes.io/zone: us-west-2b
EOF

  kind create cluster \
    --name "${CLUSTER_NAME}" \
    --config "${kind_config}" \
    --kubeconfig "${KUBECONFIG}"
  rm -f "${kind_config}"

  info "Installing Wren CRDs"
  kubectl apply -f "${REPO_ROOT}/manifests/crds/"

  REAPER_CRD_INSTALLED=false
  local subchart_crd="${REPO_ROOT}/charts/wren/charts/reaper/crds/reaperpods.reaper.giar.dev.yaml"
  local sibling_crd="${REPO_ROOT}/../reaper/deploy/kubernetes/crds/reaperpods.reaper.giar.dev.yaml"
  if [[ -f "${subchart_crd}" ]]; then
    info "Installing ReaperPod CRD from reaper subchart"
    kubectl apply -f "${subchart_crd}"
    REAPER_CRD_INSTALLED=true
  elif [[ -f "${sibling_crd}" ]]; then
    info "Installing ReaperPod CRD from sibling reaper project"
    kubectl apply -f "${sibling_crd}"
    REAPER_CRD_INSTALLED=true
  else
    echo -e "${YELLOW}[WARN]${RESET} ReaperPod CRD not found — skipping reaper backend examples"
  fi

  info "Creating wren-system namespace"
  kubectl create namespace wren-system 2>/dev/null || true

  info "Installing RBAC"
  kubectl apply -f "${REPO_ROOT}/manifests/rbac/rbac.yaml"

  info "Building and loading controller image"
  local prebuilt="${REPO_ROOT}/target/x86_64-unknown-linux-musl/release/wren-controller"
  if [[ -f "${prebuilt}" ]]; then
    cp "${prebuilt}" "${REPO_ROOT}/docker/"
    docker build \
      -f "${REPO_ROOT}/docker/Dockerfile.controller" \
      --target runtime-prebuilt \
      -t wren-controller:dev \
      "${REPO_ROOT}/docker/"
    rm -f "${REPO_ROOT}/docker/wren-controller"
  else
    docker build \
      -f "${REPO_ROOT}/docker/Dockerfile.controller" \
      -t wren-controller:dev \
      "${REPO_ROOT}"
  fi
  kind load docker-image wren-controller:dev --name "${CLUSTER_NAME}"

  info "Deploying controller"
  kubectl apply -f "${REPO_ROOT}/manifests/deployment.yaml"

  info "Waiting for controller to be ready (timeout 120s)"
  kubectl wait --for=condition=available deployment/wren-controller \
    --timeout=120s \
    -n wren-system 2>/dev/null \
  || kubectl wait --for=condition=available deployment/wren-controller \
    --timeout=120s 2>/dev/null
}

if ! $SKIP_CLUSTER; then
  setup_cluster
else
  info "Skipping cluster creation (--skip-cluster)"
fi

# Create shared demo-user WrenUser for all examples
info "Creating shared demo-user WrenUser"
kubectl apply -f "${REPO_ROOT}/examples/common/user.yaml" 2>/dev/null

# ---------------------------------------------------------------------------
# Helper: wait for a WrenJob to reach the expected state
# Returns 0 if state matched, 1 on timeout
# ---------------------------------------------------------------------------
wait_for_job_state() {
  local job_name="$1"
  local expected_state="$2"
  local timeout_s="$3"
  local namespace="${4:-default}"
  local elapsed=0

  while [[ $elapsed -lt $timeout_s ]]; do
    local state
    state="$(kubectl get wrenjob "${job_name}" \
      -n "${namespace}" \
      -o jsonpath='{.status.state}' 2>/dev/null || true)"
    if [[ "${state}" == "${expected_state}" ]]; then
      return 0
    fi
    sleep 3
    elapsed=$((elapsed + 3))
  done
  return 1
}

# ---------------------------------------------------------------------------
# Helper: wait for any of a list of WrenJobs to reach Succeeded
# Returns 0 if at least one succeeds within timeout
# ---------------------------------------------------------------------------
wait_for_any_succeeded() {
  local timeout_s="$1"
  shift
  local jobs=("$@")
  local elapsed=0

  while [[ $elapsed -lt $timeout_s ]]; do
    for job_name in "${jobs[@]}"; do
      local state
      state="$(kubectl get wrenjob "${job_name}" \
        -o jsonpath='{.status.state}' 2>/dev/null || true)"
      if [[ "${state}" == "Succeeded" ]]; then
        return 0
      fi
    done
    sleep 3
    elapsed=$((elapsed + 3))
  done
  return 1
}

# ---------------------------------------------------------------------------
# Helper: wait for all of a list of WrenJobs to reach Succeeded
# Returns 0 if all succeed within timeout
# ---------------------------------------------------------------------------
wait_for_all_succeeded() {
  local timeout_s="$1"
  shift
  local jobs=("$@")
  local elapsed=0

  while [[ $elapsed -lt $timeout_s ]]; do
    local all_done=true
    for job_name in "${jobs[@]}"; do
      local state
      state="$(kubectl get wrenjob "${job_name}" \
        -o jsonpath='{.status.state}' 2>/dev/null || true)"
      if [[ "${state}" != "Succeeded" ]]; then
        all_done=false
        break
      fi
    done
    if $all_done; then
      return 0
    fi
    sleep 3
    elapsed=$((elapsed + 3))
  done
  return 1
}

# ---------------------------------------------------------------------------
# Example test runner
#
# test_example <name> <timeout_s> <expected_state> <job_names_csv> [yaml_files...]
#
# expected_state:
#   Succeeded   — wait for the single job to succeed
#   Scheduling  — verify job reaches Scheduling (stays there, expected for reaper)
#   any         — handled by the caller (priority, pipeline)
# ---------------------------------------------------------------------------
test_example() {
  local name="$1"
  local timeout_s="$2"
  local expected_state="$3"
  local job_names_csv="$4"
  shift 4
  local yaml_files=("$@")

  info "Running: ${name}"

  for yaml in "${yaml_files[@]}"; do
    kubectl apply -f "${yaml}" 2>/dev/null
  done

  local result=0
  IFS=',' read -ra job_names <<< "${job_names_csv}"

  case "${expected_state}" in
    Succeeded)
      if wait_for_job_state "${job_names[0]}" "Succeeded" "${timeout_s}"; then
        pass "${name}"
        PASS_COUNT=$((PASS_COUNT + 1))
      else
        fail "${name} — job did not reach Succeeded within ${timeout_s}s (state: $(kubectl get wrenjob "${job_names[0]}" -o jsonpath='{.status.state}' 2>/dev/null || echo unknown))"
        FAIL_COUNT=$((FAIL_COUNT + 1))
        result=1
      fi
      ;;
    Scheduling)
      # For reaper backend jobs: expect Scheduling (no Reaper runtime in CI).
      # We wait up to timeout for the job to at least reach Scheduling.
      if wait_for_job_state "${job_names[0]}" "Scheduling" "${timeout_s}"; then
        pass "${name} (Scheduling — expected, no Reaper runtime)"
        PASS_COUNT=$((PASS_COUNT + 1))
      else
        local actual
        actual="$(kubectl get wrenjob "${job_names[0]}" -o jsonpath='{.status.state}' 2>/dev/null || echo unknown)"
        fail "${name} — job did not reach Scheduling within ${timeout_s}s (state: ${actual})"
        FAIL_COUNT=$((FAIL_COUNT + 1))
        result=1
      fi
      ;;
  esac

  # Cleanup: delete WrenJobs and associated resources
  for job_name in "${job_names[@]}"; do
    kubectl delete wrenjob "${job_name}" --ignore-not-found 2>/dev/null || true
  done

  return ${result}
}

# ---------------------------------------------------------------------------
# Example 01 — Hello World
# ---------------------------------------------------------------------------
test_example \
  "01-hello-world" \
  60 \
  "Succeeded" \
  "hello-wren" \
  "${REPO_ROOT}/examples/01-hello-world/job.yaml"

# ---------------------------------------------------------------------------
# Example 02 — Multi-node
# ---------------------------------------------------------------------------
test_example \
  "02-multi-node" \
  60 \
  "Succeeded" \
  "multi-node-demo" \
  "${REPO_ROOT}/examples/02-multi-node/job.yaml"

# ---------------------------------------------------------------------------
# Example 03 — Queues
# ---------------------------------------------------------------------------
info "Running: 03-queues"
kubectl apply -f "${REPO_ROOT}/examples/03-queues/queue.yaml" 2>/dev/null
kubectl apply -f "${REPO_ROOT}/examples/03-queues/job.yaml" 2>/dev/null

if wait_for_job_state "batch-job" "Succeeded" 60; then
  pass "03-queues"
  PASS_COUNT=$((PASS_COUNT + 1))
else
  actual="$(kubectl get wrenjob batch-job -o jsonpath='{.status.state}' 2>/dev/null || echo unknown)"
  fail "03-queues — batch-job did not reach Succeeded within 60s (state: ${actual})"
  FAIL_COUNT=$((FAIL_COUNT + 1))
fi

kubectl delete wrenjob batch-job --ignore-not-found 2>/dev/null || true
kubectl delete wrenqueue batch --ignore-not-found 2>/dev/null || true

# ---------------------------------------------------------------------------
# Example 04 — Topology
# ---------------------------------------------------------------------------
test_example \
  "04-topology" \
  60 \
  "Succeeded" \
  "topology-aware-job" \
  "${REPO_ROOT}/examples/04-topology/job.yaml"

# ---------------------------------------------------------------------------
# Example 05 — Priority
# ---------------------------------------------------------------------------
info "Running: 05-priority"
kubectl apply -f "${REPO_ROOT}/examples/05-priority/low-priority.yaml" 2>/dev/null
kubectl apply -f "${REPO_ROOT}/examples/05-priority/high-priority.yaml" 2>/dev/null

if wait_for_any_succeeded 90 "low-priority-job" "high-priority-job"; then
  pass "05-priority (at least one job succeeded)"
  PASS_COUNT=$((PASS_COUNT + 1))
else
  low_state="$(kubectl get wrenjob low-priority-job -o jsonpath='{.status.state}' 2>/dev/null || echo unknown)"
  high_state="$(kubectl get wrenjob high-priority-job -o jsonpath='{.status.state}' 2>/dev/null || echo unknown)"
  fail "05-priority — no job succeeded within 90s (low=${low_state}, high=${high_state})"
  FAIL_COUNT=$((FAIL_COUNT + 1))
fi

kubectl delete wrenjob low-priority-job high-priority-job --ignore-not-found 2>/dev/null || true

# ---------------------------------------------------------------------------
# Example 06 — Dependencies (pipeline: preprocess -> train -> evaluate)
# ---------------------------------------------------------------------------
info "Running: 06-dependencies"
kubectl apply -f "${REPO_ROOT}/examples/06-dependencies/pipeline.yaml" 2>/dev/null

if wait_for_all_succeeded 180 "preprocess" "train" "evaluate"; then
  pass "06-dependencies (all pipeline jobs succeeded)"
  PASS_COUNT=$((PASS_COUNT + 1))
else
  pre_state="$(kubectl get wrenjob preprocess -o jsonpath='{.status.state}' 2>/dev/null || echo unknown)"
  train_state="$(kubectl get wrenjob train -o jsonpath='{.status.state}' 2>/dev/null || echo unknown)"
  eval_state="$(kubectl get wrenjob evaluate -o jsonpath='{.status.state}' 2>/dev/null || echo unknown)"
  fail "06-dependencies — pipeline did not complete within 180s (preprocess=${pre_state}, train=${train_state}, evaluate=${eval_state})"
  FAIL_COUNT=$((FAIL_COUNT + 1))
fi

kubectl delete wrenjob preprocess train evaluate --ignore-not-found 2>/dev/null || true

# ---------------------------------------------------------------------------
# Example 07 — Reaper Backend
# ---------------------------------------------------------------------------
if [[ "${REAPER_CRD_INSTALLED}" == "true" ]]; then
  info "Running: 07-reaper-backend"
  kubectl apply -f "${REPO_ROOT}/examples/07-reaper-backend/user.yaml" 2>/dev/null
  kubectl apply -f "${REPO_ROOT}/examples/07-reaper-backend/job.yaml" 2>/dev/null

  if wait_for_job_state "reaper-hello" "Scheduling" 30; then
    pass "07-reaper-backend (Scheduling — expected, no Reaper runtime)"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    actual="$(kubectl get wrenjob reaper-hello -o jsonpath='{.status.state}' 2>/dev/null || echo unknown)"
    fail "07-reaper-backend — job did not reach Scheduling within 30s (state: ${actual})"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi

  kubectl delete wrenjob reaper-hello --ignore-not-found 2>/dev/null || true
else
  info "Skipping: 07-reaper-backend (ReaperPod CRD not installed)"
  SKIP_COUNT=$((SKIP_COUNT + 1))
fi

# ---------------------------------------------------------------------------
# Example 08 — Distributed Training
# ---------------------------------------------------------------------------
if [[ "${REAPER_CRD_INSTALLED}" == "true" ]]; then
  info "Running: 08-distributed-training"
  kubectl apply -f "${REPO_ROOT}/examples/08-distributed-training/configmap.yaml" 2>/dev/null
  kubectl apply -f "${REPO_ROOT}/examples/08-distributed-training/user.yaml" 2>/dev/null
  kubectl apply -f "${REPO_ROOT}/examples/08-distributed-training/job.yaml" 2>/dev/null

  if wait_for_job_state "pytorch-ddp-training" "Scheduling" 30; then
    pass "08-distributed-training (Scheduling — expected, no Reaper runtime)"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    actual="$(kubectl get wrenjob pytorch-ddp-training -o jsonpath='{.status.state}' 2>/dev/null || echo unknown)"
    fail "08-distributed-training — job did not reach Scheduling within 30s (state: ${actual})"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi

  kubectl delete wrenjob pytorch-ddp-training --ignore-not-found 2>/dev/null || true
else
  info "Skipping: 08-distributed-training (ReaperPod CRD not installed)"
  SKIP_COUNT=$((SKIP_COUNT + 1))
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== Example Test Results ==="
echo -e "${GREEN}Passed:${RESET}  ${PASS_COUNT}"
echo -e "${RED}Failed:${RESET}  ${FAIL_COUNT}"
echo -e "${YELLOW}Skipped:${RESET} ${SKIP_COUNT}"

if [[ ${FAIL_COUNT} -gt 0 ]]; then
  exit 1
fi
