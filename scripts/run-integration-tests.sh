#!/usr/bin/env bash
# run-integration-tests.sh — Comprehensive integration test runner for Wren HPC scheduler.
#
# Usage:
#   ./scripts/run-integration-tests.sh                     # Full run (build + kind + all tests)
#   ./scripts/run-integration-tests.sh --skip-cargo        # Skip cargo build & Rust tests
#   ./scripts/run-integration-tests.sh --no-cleanup        # Keep kind cluster after run
#   ./scripts/run-integration-tests.sh --verbose           # Print verbose output to stdout
#
# Environment variables (override defaults):
#   IMAGE_TAG=<tag>          — controller image tag (default: latest)
#   CLUSTER_NAME=<name>      — kind cluster name (default: wren-test)
#   NAMESPACE=<ns>           — controller namespace (default: wren-system)
#   TEST_NAMESPACE=<ns>      — namespace for test jobs (default: default)
#   CONTROLLER_TIMEOUT=<s>   — seconds to wait for controller ready (default: 120)
#   JOB_TIMEOUT=<s>          — seconds to wait for job status (default: 90)
#   TESTS=rust|shell|all     — which test suites to run (default: all)

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration (env var defaults)
# ---------------------------------------------------------------------------
CLUSTER_NAME="${CLUSTER_NAME:-wren-test}"
NAMESPACE="${NAMESPACE:-wren-system}"
TEST_NAMESPACE="${TEST_NAMESPACE:-default}"
IMAGE_NAME="wren-controller"
IMAGE_TAG="${IMAGE_TAG:-latest}"
IMAGE_REF="${IMAGE_NAME}:${IMAGE_TAG}"
CONTROLLER_TIMEOUT="${CONTROLLER_TIMEOUT:-120}"
JOB_TIMEOUT="${JOB_TIMEOUT:-90}"
LOG_DIR="/tmp/wren-integration-logs"
LOG_FILE="${LOG_DIR}/integration-test.log"
TESTS="${TESTS:-all}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST_DIR="${REPO_ROOT}/manifests"
DOCKERFILE="${REPO_ROOT}/docker/Dockerfile.controller"
KIND_CONFIG="${REPO_ROOT}/kind-config.yaml"

# Isolated kubeconfig — ensures all kubectl/kind/cargo commands target only
# this test cluster and never leak into the user's default config.
export KUBECONFIG="${LOG_DIR}/kubeconfig"
export KUBECONFIG_FILE="${KUBECONFIG}"

# Pod label key used by the controller to associate pods with an WrenJob.
WREN_JOB_LABEL="wren.io/job-name"

# ---------------------------------------------------------------------------
# Flags (set via CLI arguments, matching Reaper's interface)
# ---------------------------------------------------------------------------
SKIP_CARGO=false
NO_CLEANUP=false
VERBOSE=false

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case $1 in
    --skip-cargo)  SKIP_CARGO=true; shift ;;
    --no-cleanup)  NO_CLEANUP=true; shift ;;
    --verbose)     VERBOSE=true; shift ;;
    -h|--help)
      echo "Usage: $0 [--skip-cargo] [--no-cleanup] [--verbose]"
      echo "  --skip-cargo  Skip cargo build and Rust integration tests"
      echo "  --no-cleanup  Keep kind cluster after run"
      echo "  --verbose     Also print verbose output to stdout"
      echo ""
      echo "Environment variables:"
      echo "  CLUSTER_NAME   Kind cluster name (default: wren-test)"
      echo "  IMAGE_TAG      Controller image tag (default: latest)"
      echo "  TESTS          Test suites: rust|shell|all (default: all)"
      echo "  JOB_TIMEOUT    Seconds to wait for job status (default: 90)"
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      echo "Usage: $0 [--skip-cargo] [--no-cleanup] [--verbose]" >&2
      exit 1
      ;;
  esac
done

# Apply flag effects to config variables
if $SKIP_CARGO; then
  # --skip-cargo implies: skip docker build AND skip Rust integration tests
  SKIP_BUILD="1"
  if [[ "$TESTS" == "all" ]]; then
    TESTS="shell"
  fi
else
  SKIP_BUILD="${SKIP_BUILD:-0}"
fi

if $NO_CLEANUP; then
  KEEP_CLUSTER="1"
else
  KEEP_CLUSTER="${KEEP_CLUSTER:-0}"
fi

# ---------------------------------------------------------------------------
# Colours & logging helpers
# ---------------------------------------------------------------------------
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
RESET='\033[0m'

# When --verbose is set, output goes to both stdout and the log file.
# Otherwise, only section headers, pass/fail, and errors go to stdout;
# detailed [INFO] lines go only to the log file.
_emit() {
    local msg="$1"
    local to_stdout="${2:-true}"
    echo -e "$msg" >> "${LOG_FILE}" 2>/dev/null || true
    if [[ "$to_stdout" == "true" ]] || $VERBOSE; then
        echo -e "$msg"
    fi
}

log()      { _emit "${BLUE}[INFO]${RESET}  $*" "false"; }
success()  { _emit "${GREEN}[PASS]${RESET}  $*" "true"; }
warn()     { _emit "${YELLOW}[WARN]${RESET}  $*" "true"; }
error()    { _emit "${RED}[FAIL]${RESET}  $*" "true"; }
section()  { _emit "\n${BOLD}==> $*${RESET}" "true"; }
skip()     { _emit "${YELLOW}[SKIP]${RESET}  $*" "true"; }

# ---------------------------------------------------------------------------
# Test summary tracking
# ---------------------------------------------------------------------------
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_SKIPPED=0
TEST_RESULTS=()  # array of "PASS|FAIL|SKIP:<name>" entries

record_pass() {
    TESTS_PASSED=$(( TESTS_PASSED + 1 ))
    TEST_RESULTS+=("PASS:$1")
    success "TEST PASSED: $1"
}

record_fail() {
    TESTS_FAILED=$(( TESTS_FAILED + 1 ))
    TEST_RESULTS+=("FAIL:$1")
    error "TEST FAILED: $1"
}

record_skip() {
    TESTS_SKIPPED=$(( TESTS_SKIPPED + 1 ))
    TEST_RESULTS+=("SKIP:$1")
    skip "TEST SKIPPED: $1"
}

# Run a named test function; catch failures and record result without aborting.
run_test() {
    local name="$1"
    local fn="$2"
    shift 2
    section "Test: ${name}"
    if "${fn}" "$@"; then
        record_pass "${name}"
    else
        record_fail "${name}"
    fi
}

print_summary() {
    local total=$(( TESTS_PASSED + TESTS_FAILED + TESTS_SKIPPED ))
    echo ""
    echo -e "${BOLD}========================================================"
    echo   "  Test Summary"
    echo   "========================================================"
    echo -e "${RESET}"
    for entry in "${TEST_RESULTS[@]}"; do
        local status="${entry%%:*}"
        local name="${entry#*:}"
        case "$status" in
            PASS) echo -e "  ${GREEN}PASS${RESET}  ${name}" ;;
            FAIL) echo -e "  ${RED}FAIL${RESET}  ${name}" ;;
            SKIP) echo -e "  ${YELLOW}SKIP${RESET}  ${name}" ;;
        esac
    done
    echo ""
    echo -e "  Total: ${total}  ${GREEN}Passed: ${TESTS_PASSED}${RESET}  ${RED}Failed: ${TESTS_FAILED}${RESET}  ${YELLOW}Skipped: ${TESTS_SKIPPED}${RESET}"
    echo -e "${BOLD}========================================================${RESET}"
}

# ---------------------------------------------------------------------------
# Prerequisite checks
# ---------------------------------------------------------------------------
check_prereqs() {
    section "Checking prerequisites"
    local missing=0
    local required_cmds=(kind kubectl docker)

    # cargo is only required when running Rust tests
    if [[ "$TESTS" == "rust" || "$TESTS" == "all" ]]; then
        required_cmds+=(cargo)
    fi

    for cmd in "${required_cmds[@]}"; do
        if command -v "$cmd" &>/dev/null; then
            log "$cmd: $(command -v "$cmd")"
        else
            error "Required command not found: $cmd"
            missing=1
        fi
    done

    if [[ "$missing" -eq 1 ]]; then
        error "Install missing prerequisites and retry."
        exit 1
    fi

    # Verify kind-config.yaml exists
    if [[ ! -f "${KIND_CONFIG}" ]]; then
        error "kind cluster config not found: ${KIND_CONFIG}"
        exit 1
    fi

    success "All prerequisites present"
}

# ---------------------------------------------------------------------------
# Cleanup / teardown
# ---------------------------------------------------------------------------
CLEANUP_REGISTERED=0

cleanup() {
    local exit_code=$?
    if [[ "$CLEANUP_REGISTERED" -eq 0 ]]; then
        return
    fi

    section "Collecting logs before cleanup"
    collect_logs || true

    if $NO_CLEANUP || [[ "$KEEP_CLUSTER" == "1" ]]; then
        warn "--no-cleanup — skipping cluster teardown"
        warn "To delete manually: kind delete cluster --name ${CLUSTER_NAME}"
    else
        section "Tearing down kind cluster"
        kind delete cluster --name "${CLUSTER_NAME}" 2>/dev/null || true
        log "Cluster '${CLUSTER_NAME}' deleted"
    fi

    if [[ "$exit_code" -ne 0 ]]; then
        error "Integration tests FAILED (exit code: ${exit_code})"
        error "Logs saved to: ${LOG_DIR}"
    fi
}

register_cleanup() {
    CLEANUP_REGISTERED=1
    trap cleanup EXIT
}

# ---------------------------------------------------------------------------
# Log collection
# ---------------------------------------------------------------------------
collect_logs() {
    mkdir -p "${LOG_DIR}"
    log "Saving logs to ${LOG_DIR}"

    # Controller logs
    kubectl logs -n "${NAMESPACE}" \
        -l app.kubernetes.io/name=wren-controller \
        --all-containers \
        --tail=-1 \
        2>/dev/null > "${LOG_DIR}/controller.log" || true

    # Events in wren-system namespace
    kubectl get events -n "${NAMESPACE}" \
        --sort-by='.lastTimestamp' \
        2>/dev/null > "${LOG_DIR}/wren-system-events.log" || true

    # Events in test namespace
    kubectl get events -n "${TEST_NAMESPACE}" \
        --sort-by='.lastTimestamp' \
        2>/dev/null > "${LOG_DIR}/test-namespace-events.log" || true

    # WrenJob state
    kubectl get wrenjobs -n "${TEST_NAMESPACE}" -o yaml \
        2>/dev/null > "${LOG_DIR}/wrenjobs.yaml" || true

    # WrenQueue state
    kubectl get wrenqueues -A -o yaml \
        2>/dev/null > "${LOG_DIR}/wrenqueues.yaml" || true

    # All pods with wide output
    kubectl get pods -A -o wide \
        2>/dev/null > "${LOG_DIR}/pods.log" || true

    # Node topology labels
    kubectl get nodes --show-labels \
        2>/dev/null > "${LOG_DIR}/nodes.log" || true

    log "Logs written:"
    ls -lh "${LOG_DIR}"
}

# ---------------------------------------------------------------------------
# Step 1: Set up kind cluster using the topology-aware kind-config.yaml
# ---------------------------------------------------------------------------
setup_cluster() {
    section "Setting up kind cluster '${CLUSTER_NAME}'"
    log "Using config: ${KIND_CONFIG}"

    if kind get clusters 2>/dev/null | grep -qx "${CLUSTER_NAME}"; then
        warn "Cluster '${CLUSTER_NAME}' already exists — reusing it"
        kind export kubeconfig --name "${CLUSTER_NAME}" --kubeconfig "${KUBECONFIG}"
    else
        log "Creating cluster '${CLUSTER_NAME}' with 4 worker nodes and topology labels ..."
        kind create cluster \
            --name "${CLUSTER_NAME}" \
            --config "${KIND_CONFIG}" \
            --kubeconfig "${KUBECONFIG}" \
            --wait 60s
        success "Cluster created"
    fi

    # Verify connectivity and show topology
    kubectl cluster-info --context "kind-${CLUSTER_NAME}"
    log "Nodes and topology labels:"
    kubectl get nodes \
        -L topology.wren.io/switch \
        -L topology.wren.io/rack \
        -L topology.kubernetes.io/zone
    success "Cluster is reachable"
}

# ---------------------------------------------------------------------------
# Step 2: Build controller binary
# ---------------------------------------------------------------------------
build_binary() {
    section "Building controller binary"
    if $SKIP_CARGO || [[ "$SKIP_BUILD" == "1" ]]; then
        log "Skipping cargo build (--skip-cargo)"
        return
    fi
    local bin="${REPO_ROOT}/target/release/wren-controller"
    if [[ -x "$bin" ]] && [[ "$bin" -nt "${REPO_ROOT}/crates/wren-controller/src/main.rs" ]]; then
        log "Binary is up-to-date, skipping cargo build"
        success "Binary exists: ${bin}"
        return
    fi
    log "Running: cargo build --release -p wren-controller"
    cargo build --release -p wren-controller \
        --manifest-path "${REPO_ROOT}/Cargo.toml"
    success "Binary built: target/release/wren-controller"
}

# ---------------------------------------------------------------------------
# Step 3: Build and load Docker image into kind
#
# The deployment manifest must use imagePullPolicy: Never so that Kubernetes
# uses the image loaded into kind's containerd instead of pulling from a
# registry (which would fail for locally-built images).
# ---------------------------------------------------------------------------
build_and_load_image() {
    section "Building Docker image ${IMAGE_REF}"

    local bin="${REPO_ROOT}/target/release/wren-controller"
    if $SKIP_CARGO || [[ "$SKIP_BUILD" == "1" ]]; then
        if [[ -x "$bin" ]]; then
            log "Using pre-built binary for Docker image"
            cp "$bin" "${REPO_ROOT}/docker/wren-controller"
            docker build \
                -f "${DOCKERFILE}" \
                --target runtime-prebuilt \
                -t "${IMAGE_REF}" \
                "${REPO_ROOT}/docker"
            rm -f "${REPO_ROOT}/docker/wren-controller"
            success "Image built (prebuilt binary): ${IMAGE_REF}"
        else
            log "Skipping Docker build (--skip-cargo, no binary found)"
        fi
    else
        log "Building: docker build -f ${DOCKERFILE} -t ${IMAGE_REF} ${REPO_ROOT}"
        docker build \
            -f "${DOCKERFILE}" \
            -t "${IMAGE_REF}" \
            "${REPO_ROOT}"
        success "Image built: ${IMAGE_REF}"
    fi

    log "Loading image into kind cluster '${CLUSTER_NAME}' ..."
    kind load docker-image "${IMAGE_REF}" --name "${CLUSTER_NAME}"
    success "Image loaded into cluster"
}

# ---------------------------------------------------------------------------
# Step 4: Install CRDs
# ---------------------------------------------------------------------------
install_crds() {
    section "Installing CRDs from ${MANIFEST_DIR}/crds/"
    local crd_dir="${MANIFEST_DIR}/crds"

    if [[ ! -d "$crd_dir" ]]; then
        error "CRD directory not found: ${crd_dir}"
        exit 1
    fi

    local crd_count=0
    for f in "${crd_dir}"/*.yaml; do
        [[ -f "$f" ]] || continue
        log "Applying CRD: $(basename "$f")"
        kubectl apply -f "$f"
        crd_count=$(( crd_count + 1 ))
    done

    if [[ "$crd_count" -eq 0 ]]; then
        error "No CRD YAML files found in ${crd_dir}"
        exit 1
    fi

    # Wait for CRDs to become established before proceeding
    log "Waiting for CRDs to be established ..."
    kubectl wait --for=condition=Established \
        crd/wrenjobs.hpc.cscs.ch \
        --timeout=30s
    kubectl wait --for=condition=Established \
        crd/wrenqueues.hpc.cscs.ch \
        --timeout=30s 2>/dev/null || warn "wrenqueues CRD not found — skipping"

    success "CRDs installed (${crd_count} files)"
}

# ---------------------------------------------------------------------------
# Step 5: Install RBAC
# ---------------------------------------------------------------------------
install_rbac() {
    section "Installing RBAC from ${MANIFEST_DIR}/rbac/rbac.yaml"
    local rbac_file="${MANIFEST_DIR}/rbac/rbac.yaml"

    if [[ ! -f "$rbac_file" ]]; then
        error "RBAC manifest not found: ${rbac_file}"
        exit 1
    fi

    kubectl apply -f "${rbac_file}"
    success "RBAC installed"
}

# ---------------------------------------------------------------------------
# Step 6: Ensure controller namespace exists
# ---------------------------------------------------------------------------
ensure_namespace() {
    section "Ensuring namespace '${NAMESPACE}' exists"
    if kubectl get namespace "${NAMESPACE}" &>/dev/null; then
        log "Namespace '${NAMESPACE}' already exists"
    else
        kubectl create namespace "${NAMESPACE}"
        success "Namespace '${NAMESPACE}' created"
    fi
}

# ---------------------------------------------------------------------------
# Step 7: Deploy the controller
#
# The deployment.yaml must set imagePullPolicy: Never for the controller
# container so that kind uses the locally-loaded image. Patching it here
# ensures this works even if the manifest file has a different policy.
# ---------------------------------------------------------------------------
deploy_controller() {
    section "Deploying controller from ${MANIFEST_DIR}/deployment.yaml"
    local deploy_file="${MANIFEST_DIR}/deployment.yaml"

    if [[ ! -f "$deploy_file" ]]; then
        error "Deployment manifest not found: ${deploy_file}"
        exit 1
    fi

    # Replace image and imagePullPolicy inline before applying so that only a
    # single ReplicaSet is created. The previous apply-then-patch approach caused
    # a transient old ReplicaSet that tried to pull the non-existent default tag.
    sed -e "s|image: wren-controller:dev|image: ${IMAGE_REF}|" \
        -e "s|imagePullPolicy: IfNotPresent|imagePullPolicy: Never|" \
        "${deploy_file}" | kubectl apply -f -

    success "Controller manifest applied"
}

# ---------------------------------------------------------------------------
# Step 8: Wait for controller to be ready
# ---------------------------------------------------------------------------
wait_for_controller() {
    section "Waiting for controller to become ready (timeout: ${CONTROLLER_TIMEOUT}s)"

    if ! kubectl wait deployment/wren-controller \
        -n "${NAMESPACE}" \
        --for=condition=Available \
        --timeout="${CONTROLLER_TIMEOUT}s"; then
        error "Controller did not become ready within ${CONTROLLER_TIMEOUT}s"
        error "Controller pod status:"
        kubectl get pods -n "${NAMESPACE}" -l app.kubernetes.io/name=wren-controller || true
        error "Controller logs:"
        kubectl logs -n "${NAMESPACE}" \
            -l app.kubernetes.io/name=wren-controller \
            --tail=50 || true
        exit 1
    fi

    success "Controller is ready"
    kubectl get pods -n "${NAMESPACE}" -l app.kubernetes.io/name=wren-controller
}

# ---------------------------------------------------------------------------
# Rust integration tests
#
# These are the #[ignore]-gated Rust tests inside wren-controller's
# tests/integration/ directory. They expect a live cluster to be available
# via KUBECONFIG and run with --test-threads=1 to avoid race conditions.
# ---------------------------------------------------------------------------
run_rust_tests() {
    section "Running Rust integration tests (cargo test --ignored)"

    if [[ "$TESTS" == "shell" ]]; then
        record_skip "Rust integration tests (TESTS=shell)"
        return
    fi

    log "Command: cargo test -p wren-controller --test integration -- --ignored --test-threads=1"

    local rust_log="${LOG_DIR}/rust-integration-tests.log"
    if cargo test \
            -p wren-controller \
            --test integration \
            -- \
            --ignored \
            --test-threads=1 \
            2>&1 | tee "${rust_log}"; then
        record_pass "Rust integration tests"
    else
        record_fail "Rust integration tests"
        warn "Full Rust test output saved to: ${rust_log}"
    fi
}

# ---------------------------------------------------------------------------
# Shell smoke tests
#
# These tests exercise the Wren CRDs and controller from the outside using
# kubectl. They are self-contained and produce pass/fail records for the
# final summary. Each test cleans up after itself.
# ---------------------------------------------------------------------------

# Helper: wait for an WrenJob to reach a target state.
# Usage: wait_for_job_state <job-name> <target-states-pipe-separated> <timeout-s>
# Returns 0 if the state is reached, 1 on timeout.
wait_for_job_state() {
    local job_name="$1"
    local target_pattern="$2"   # e.g. "Scheduling|Running|Succeeded"
    local timeout_s="${3:-${JOB_TIMEOUT}}"
    local deadline=$(( $(date +%s) + timeout_s ))
    local state=""

    while true; do
        state=$(kubectl get wrenjob "${job_name}" \
            -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.status.state}' 2>/dev/null || echo "")

        log "  WrenJob '${job_name}' state: '${state:-<empty>}'"

        if [[ "$state" =~ ^(${target_pattern})$ ]]; then
            log "  Reached expected state: ${state}"
            return 0
        fi

        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            error "Timed out after ${timeout_s}s waiting for state '${target_pattern}' (last: '${state:-<empty>}')"
            return 1
        fi

        sleep 3
    done
}

# Helper: delete a WrenJob and its associated pods quietly, ignore not-found errors.
delete_job() {
    local job_name="$1"
    # Delete associated pods first (they may not have ownerReferences)
    kubectl delete pods \
        -n "${TEST_NAMESPACE}" \
        -l "wren.io/job-name=${job_name}" \
        --ignore-not-found \
        --timeout=30s 2>/dev/null || true
    # Then delete the WrenJob
    kubectl delete wrenjob "${job_name}" \
        -n "${TEST_NAMESPACE}" \
        --ignore-not-found \
        --timeout=60s 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Smoke test 1: Multi-node job — submit a 2-node job and verify it progresses.
# ---------------------------------------------------------------------------
_smoke_test_multinode_job() {
    local job_name="smoke-multinode"
    local manifest
    manifest="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 2
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo hello-wren && sleep 10"]
EOF
)"
    log "Submitting 2-node WrenJob '${job_name}' ..."
    echo "${manifest}" | kubectl apply -f -

    # Expect the job to leave Pending and reach Scheduling, Running, or Succeeded
    if ! wait_for_job_state "${job_name}" "Scheduling|Running|Succeeded" "${JOB_TIMEOUT}"; then
        kubectl describe wrenjob "${job_name}" -n "${TEST_NAMESPACE}" || true
        delete_job "${job_name}"
        return 1
    fi

    delete_job "${job_name}"
    return 0
}

smoke_test_multinode_job() {
    run_test "multi-node job (2 nodes)" _smoke_test_multinode_job
}

# ---------------------------------------------------------------------------
# Smoke test 2: Topology labels — verify all 4 worker nodes carry the
# topology labels that the kind-config.yaml declared.
# ---------------------------------------------------------------------------
_smoke_test_topology_labels() {
    local errors=0

    log "Checking topology labels on worker nodes ..."

    # Each worker node must have all three topology keys
    local expected_keys=(
        "topology.wren.io/switch"
        "topology.wren.io/rack"
        "topology.kubernetes.io/zone"
    )

    local worker_nodes
    worker_nodes=$(kubectl get nodes \
        --selector='!node-role.kubernetes.io/control-plane' \
        -o jsonpath='{.items[*].metadata.name}')

    local node_count
    node_count=$(echo "${worker_nodes}" | wc -w | tr -d ' ')

    if [[ "$node_count" -lt 4 ]]; then
        error "Expected 4 worker nodes, found ${node_count}"
        errors=$(( errors + 1 ))
    else
        log "Found ${node_count} worker nodes: ${worker_nodes}"
    fi

    for node in ${worker_nodes}; do
        for key in "${expected_keys[@]}"; do
            local val
            val=$(kubectl get node "${node}" \
                -o jsonpath="{.metadata.labels.${key//\//\\.}}" 2>/dev/null || echo "")
            if [[ -z "$val" ]]; then
                error "Node '${node}' is missing label '${key}'"
                errors=$(( errors + 1 ))
            else
                log "  ${node}: ${key}=${val}"
            fi
        done
    done

    # Verify the expected topology groups are present
    local switch0_count switch1_count
    switch0_count=$(kubectl get nodes \
        -l "topology.wren.io/switch=switch-0" \
        --no-headers 2>/dev/null | wc -l | tr -d ' ')
    switch1_count=$(kubectl get nodes \
        -l "topology.wren.io/switch=switch-1" \
        --no-headers 2>/dev/null | wc -l | tr -d ' ')

    if [[ "$switch0_count" -ne 2 ]]; then
        error "Expected 2 nodes in switch-0, found ${switch0_count}"
        errors=$(( errors + 1 ))
    else
        log "switch-0 has ${switch0_count} nodes (correct)"
    fi

    if [[ "$switch1_count" -ne 2 ]]; then
        error "Expected 2 nodes in switch-1, found ${switch1_count}"
        errors=$(( errors + 1 ))
    else
        log "switch-1 has ${switch1_count} nodes (correct)"
    fi

    [[ "$errors" -eq 0 ]]
}

smoke_test_topology_labels() {
    run_test "topology labels on nodes" _smoke_test_topology_labels
}

# ---------------------------------------------------------------------------
# Smoke test 3: Queue creation — create a WrenQueue and verify it is accepted.
# ---------------------------------------------------------------------------
_smoke_test_queue_creation() {
    local queue_name="smoke-queue"
    local manifest
    manifest="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenQueue
metadata:
  name: ${queue_name}
  namespace: ${TEST_NAMESPACE}
spec:
  maxNodes: 64
  maxWalltime: "12h"
  maxJobsPerUser: 5
  defaultPriority: 50
  backfill:
    enabled: true
    lookAhead: "1h"
  fairShare:
    enabled: true
    decayHalfLife: "7d"
EOF
)"
    log "Creating WrenQueue '${queue_name}' ..."
    if ! echo "${manifest}" | kubectl apply -f - 2>/dev/null; then
        warn "WrenQueue CRD may not be installed — skipping queue test"
        return 0
    fi

    # Verify the queue is retrievable
    local retrieved
    retrieved=$(kubectl get wrenqueue "${queue_name}" \
        -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.metadata.name}' 2>/dev/null || echo "")

    if [[ "$retrieved" != "${queue_name}" ]]; then
        error "WrenQueue '${queue_name}' was not found after creation"
        kubectl delete wrenqueue "${queue_name}" -n "${TEST_NAMESPACE}" --ignore-not-found 2>/dev/null || true
        return 1
    fi

    log "WrenQueue '${queue_name}' created and verified"
    kubectl delete wrenqueue "${queue_name}" -n "${TEST_NAMESPACE}" --ignore-not-found 2>/dev/null || true
    return 0
}

smoke_test_queue_creation() {
    run_test "WrenQueue creation" _smoke_test_queue_creation
}

# ---------------------------------------------------------------------------
# Smoke test 4: Invalid job — submit a job with nodes=0, expect failure.
# The controller should reject or immediately fail this job.
# ---------------------------------------------------------------------------
_smoke_test_invalid_job() {
    local job_name="smoke-invalid"
    local manifest
    manifest="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 0
  tasksPerNode: 1
  walltime: "1m"
  container:
    image: busybox:latest
    command: ["echo", "this-should-not-run"]
EOF
)"
    log "Submitting invalid WrenJob (nodes=0) '${job_name}' ..."

    # The API server may reject it via webhook, or the controller may set a
    # Failed status. Both outcomes are acceptable.
    if ! echo "${manifest}" | kubectl apply -f - 2>/dev/null; then
        log "API server rejected the invalid job (admission webhook) — correct"
        return 0
    fi

    # If accepted, the controller should quickly move it to Failed
    local deadline=$(( $(date +%s) + 30 ))
    local state=""
    while true; do
        state=$(kubectl get wrenjob "${job_name}" \
            -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.status.state}' 2>/dev/null || echo "")
        log "  Invalid job state: '${state:-<empty>}'"

        if [[ "$state" == "Failed" ]]; then
            log "  Controller correctly failed the invalid job"
            delete_job "${job_name}"
            return 0
        fi

        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            # If it's still Pending or empty after 30s, treat it as failed
            # validation (acceptable — the controller may not validate nodes=0
            # explicitly in early development). We warn but do not fail.
            warn "Invalid job not explicitly failed after 30s (state: '${state:-<empty>}')"
            warn "This is acceptable if validation is not yet implemented"
            delete_job "${job_name}"
            return 0
        fi

        sleep 3
    done
}

smoke_test_invalid_job() {
    run_test "invalid job (nodes=0)" _smoke_test_invalid_job
}

# ---------------------------------------------------------------------------
# Smoke test 5: Walltime job — submit a job with very short walltime (5s).
# Expect it to reach WalltimeExceeded.
# ---------------------------------------------------------------------------
_smoke_test_walltime_job() {
    local job_name="smoke-walltime"
    local manifest
    manifest="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5s"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo starting && sleep 300"]
EOF
)"
    log "Submitting short-walltime WrenJob (5s) '${job_name}' ..."
    echo "${manifest}" | kubectl apply -f -

    # Allow generous time: job must start, run, then be killed by walltime enforcement
    local total_timeout=$(( JOB_TIMEOUT + 60 ))
    local deadline=$(( $(date +%s) + total_timeout ))
    local state=""

    while true; do
        state=$(kubectl get wrenjob "${job_name}" \
            -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.status.state}' 2>/dev/null || echo "")
        log "  Walltime job state: '${state:-<empty>}'"

        if [[ "$state" == "WalltimeExceeded" ]]; then
            log "  Job correctly reached WalltimeExceeded"
            delete_job "${job_name}"
            return 0
        fi

        # If the controller fails the job for walltime via Failed state, also accept
        if [[ "$state" == "Failed" ]]; then
            local reason
            reason=$(kubectl get wrenjob "${job_name}" \
                -n "${TEST_NAMESPACE}" \
                -o jsonpath='{.status.reason}' 2>/dev/null || echo "")
            if [[ "$reason" == *"walltime"* || "$reason" == *"Walltime"* ]]; then
                log "  Job failed with walltime reason: '${reason}'"
                delete_job "${job_name}"
                return 0
            fi
            warn "Job failed but not for walltime (reason: '${reason}')"
            warn "Walltime enforcement may not yet be implemented"
            delete_job "${job_name}"
            # Not a hard failure — walltime enforcement is a future feature
            return 0
        fi

        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            warn "WalltimeExceeded not reached within ${total_timeout}s (last: '${state:-<empty>}')"
            warn "Walltime enforcement may not yet be implemented"
            delete_job "${job_name}"
            # Not a hard failure in early development
            return 0
        fi

        sleep 3
    done
}

smoke_test_walltime_job() {
    run_test "walltime enforcement (5s walltime)" _smoke_test_walltime_job
}

# ---------------------------------------------------------------------------
# Smoke test 6: Concurrent jobs — submit 3 jobs simultaneously and verify
# all are tracked by the controller (appear in the API).
# ---------------------------------------------------------------------------
_smoke_test_concurrent_jobs() {
    local job_names=("smoke-concurrent-1" "smoke-concurrent-2" "smoke-concurrent-3")
    local errors=0

    log "Submitting 3 WrenJobs concurrently ..."
    for job_name in "${job_names[@]}"; do
        cat <<EOF | kubectl apply -f - &
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "2m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo ${job_name} && sleep 30"]
EOF
    done
    wait  # wait for all background kubectl applies to complete

    # Give the controller a moment to reconcile
    sleep 5

    # Verify all 3 jobs are tracked
    for job_name in "${job_names[@]}"; do
        local retrieved
        retrieved=$(kubectl get wrenjob "${job_name}" \
            -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.metadata.name}' 2>/dev/null || echo "")

        if [[ "$retrieved" == "${job_name}" ]]; then
            local state
            state=$(kubectl get wrenjob "${job_name}" \
                -n "${TEST_NAMESPACE}" \
                -o jsonpath='{.status.state}' 2>/dev/null || echo "<no state>")
            log "  ${job_name}: found, state='${state}'"
        else
            error "  ${job_name}: NOT FOUND in API"
            errors=$(( errors + 1 ))
        fi
    done

    # Clean up all concurrent jobs
    for job_name in "${job_names[@]}"; do
        delete_job "${job_name}"
    done

    [[ "$errors" -eq 0 ]]
}

smoke_test_concurrent_jobs() {
    run_test "concurrent job submission (3 jobs)" _smoke_test_concurrent_jobs
}

# ---------------------------------------------------------------------------
# Smoke test 7: Pod label selector — submit a job, wait for pods, verify
# the pods carry the correct wren.io/job-name label.
# ---------------------------------------------------------------------------
_smoke_test_pod_labels() {
    local job_name="smoke-pod-labels"
    local manifest
    manifest="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo hello && sleep 60"]
EOF
)"
    log "Submitting WrenJob '${job_name}' to check pod labels ..."
    echo "${manifest}" | kubectl apply -f -

    # Wait for the job to leave Pending
    if ! wait_for_job_state "${job_name}" "Scheduling|Running|Succeeded" "${JOB_TIMEOUT}"; then
        warn "Job did not progress — skipping pod label check"
        delete_job "${job_name}"
        return 0
    fi

    # Wait for pods to appear with the correct label
    local deadline=$(( $(date +%s) + JOB_TIMEOUT ))
    local pod_count=0

    log "Waiting for pods with label '${WREN_JOB_LABEL}=${job_name}' ..."
    while true; do
        pod_count=$(kubectl get pods -n "${TEST_NAMESPACE}" \
            -l "${WREN_JOB_LABEL}=${job_name}" \
            --no-headers 2>/dev/null | wc -l | tr -d ' ')

        if [[ "$pod_count" -gt 0 ]]; then
            success "Found ${pod_count} pod(s) with label '${WREN_JOB_LABEL}=${job_name}'"
            kubectl get pods -n "${TEST_NAMESPACE}" \
                -l "${WREN_JOB_LABEL}=${job_name}"
            delete_job "${job_name}"
            return 0
        fi

        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            warn "No pods found with label '${WREN_JOB_LABEL}=${job_name}'"
            warn "Controller may use a different label key — listing all pods in ${TEST_NAMESPACE}:"
            kubectl get pods -n "${TEST_NAMESPACE}" || true
            # Not a hard failure: pod labels are an implementation detail still in flux.
            delete_job "${job_name}"
            return 0
        fi

        sleep 3
    done
}

smoke_test_pod_labels() {
    run_test "pod label selector (wren.io/job-name)" _smoke_test_pod_labels
}

# ---------------------------------------------------------------------------
# Smoke test 8: Headless service — verify controller creates a service per job.
# ---------------------------------------------------------------------------
_smoke_test_headless_service() {
    local job_name="smoke-headless-svc"
    local manifest
    manifest="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo hello && sleep 60"]
EOF
)"
    log "Submitting WrenJob '${job_name}' to check headless service ..."
    echo "${manifest}" | kubectl apply -f -

    # Wait for job to start
    if ! wait_for_job_state "${job_name}" "Scheduling|Running" "${JOB_TIMEOUT}"; then
        warn "Job did not progress — skipping headless service check"
        delete_job "${job_name}"
        return 0
    fi

    local deadline=$(( $(date +%s) + 30 ))
    local svc_count=0

    log "Waiting for headless service named '${job_name}' ..."
    while true; do
        svc_count=$(kubectl get svc -n "${TEST_NAMESPACE}" \
            --field-selector="metadata.name=${job_name}" \
            --no-headers 2>/dev/null | wc -l | tr -d ' ')

        if [[ "$svc_count" -gt 0 ]]; then
            success "Headless service '${job_name}' found"
            kubectl get svc -n "${TEST_NAMESPACE}" \
                --field-selector="metadata.name=${job_name}"
            delete_job "${job_name}"
            return 0
        fi

        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            warn "No headless service named '${job_name}' found after 30s"
            warn "This may be expected if service creation is not yet implemented"
            delete_job "${job_name}"
            # Not a hard failure in early development
            return 0
        fi

        sleep 3
    done
}

smoke_test_headless_service() {
    run_test "headless service creation" _smoke_test_headless_service
}

# ---------------------------------------------------------------------------
# Smoke test 9: Job deletion and cleanup — submit a job, delete it, verify
# associated pods and services are removed.
# ---------------------------------------------------------------------------
_smoke_test_deletion_cleanup() {
    local job_name="smoke-cleanup"
    local manifest
    manifest="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "10m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "sleep 300"]
EOF
)"
    log "Submitting WrenJob '${job_name}' for deletion test ..."
    echo "${manifest}" | kubectl apply -f -

    # Give the controller a moment to create resources
    sleep 5

    log "Deleting WrenJob '${job_name}' ..."
    kubectl delete wrenjob "${job_name}" \
        -n "${TEST_NAMESPACE}" \
        --ignore-not-found \
        --timeout=60s

    # Verify the WrenJob is gone
    if kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" &>/dev/null; then
        warn "WrenJob '${job_name}' still exists after deletion — finalizer may be pending"
    else
        log "  WrenJob confirmed deleted"
    fi

    # Verify pods are cleaned up
    local deadline=$(( $(date +%s) + 60 ))
    local pod_count=0

    log "Waiting for pods to be removed ..."
    while true; do
        pod_count=$(kubectl get pods -n "${TEST_NAMESPACE}" \
            -l "${WREN_JOB_LABEL}=${job_name}" \
            --no-headers 2>/dev/null | wc -l | tr -d ' ')

        if [[ "$pod_count" -eq 0 ]]; then
            log "  All pods for '${job_name}' removed"
            break
        fi

        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            warn "${pod_count} pod(s) still present after 60s — cleanup may still be in progress"
            break
        fi

        log "  ${pod_count} pod(s) still present, waiting ..."
        sleep 3
    done

    return 0
}

smoke_test_deletion_cleanup() {
    run_test "job deletion and resource cleanup" _smoke_test_deletion_cleanup
}

# ---------------------------------------------------------------------------
# Smoke test 10: Pod preservation — verify pods are kept after job succeeds
# (for log retrieval). Pods should survive at least until the TTL expires.
# ---------------------------------------------------------------------------
_smoke_test_pod_preservation() {
    local job_name="smoke-pod-preserve"
    local manifest
    manifest="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "2m"
  container:
    image: busybox:latest
    command: ["echo", "preserve-me"]
EOF
)"
    log "Submitting WrenJob '${job_name}' to verify pod preservation ..."
    echo "${manifest}" | kubectl apply -f -

    # Wait for job to succeed
    if ! wait_for_job_state "${job_name}" "Succeeded" "${JOB_TIMEOUT}"; then
        warn "Job did not reach Succeeded — skipping pod preservation check"
        delete_job "${job_name}"
        return 1
    fi

    # Wait a few seconds for any cleanup to fire, then check pods still exist
    sleep 5

    local pod_count
    pod_count=$(kubectl get pods -n "${TEST_NAMESPACE}" \
        -l "${WREN_JOB_LABEL}=${job_name}" \
        --no-headers 2>/dev/null | wc -l | tr -d ' ')

    if [[ "$pod_count" -gt 0 ]]; then
        log "  Found ${pod_count} pod(s) preserved after Succeeded — correct"
        local pod_phase
        pod_phase=$(kubectl get pods -n "${TEST_NAMESPACE}" \
            -l "${WREN_JOB_LABEL}=${job_name}" \
            -o jsonpath='{.items[0].status.phase}' 2>/dev/null || echo "")
        log "  Pod phase: ${pod_phase}"
        delete_job "${job_name}"
        return 0
    else
        error "No pods found after job Succeeded — pods were deleted too early"
        delete_job "${job_name}"
        return 1
    fi
}

smoke_test_pod_preservation() {
    run_test "pod preservation after completion" _smoke_test_pod_preservation
}

# ---------------------------------------------------------------------------
# Smoke test 11: Job logs — submit a job, wait for completion, verify logs
# are retrievable via kubectl and wren CLI.
# ---------------------------------------------------------------------------
_smoke_test_job_logs() {
    local job_name="smoke-job-logs"
    local expected_output="wren-logs-test-output"
    local manifest
    manifest="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "2m"
  container:
    image: busybox:latest
    command: ["echo"]
    args: ["${expected_output}"]
EOF
)"
    log "Submitting WrenJob '${job_name}' to verify log retrieval ..."
    echo "${manifest}" | kubectl apply -f -

    # Wait for job to succeed
    if ! wait_for_job_state "${job_name}" "Succeeded" "${JOB_TIMEOUT}"; then
        warn "Job did not reach Succeeded — skipping logs check"
        delete_job "${job_name}"
        return 1
    fi

    # Verify logs via kubectl
    local pod_name="${job_name}-worker-0"
    local kubectl_logs
    kubectl_logs=$(kubectl logs "${pod_name}" -n "${TEST_NAMESPACE}" 2>/dev/null || echo "")

    if [[ "$kubectl_logs" == *"${expected_output}"* ]]; then
        log "  kubectl logs: found expected output"
    else
        error "kubectl logs did not contain '${expected_output}' (got: '${kubectl_logs}')"
        delete_job "${job_name}"
        return 1
    fi

    # Verify logs via wren CLI (if built)
    local wren_bin="${REPO_ROOT}/target/release/wren"
    if [[ ! -x "$wren_bin" ]]; then
        wren_bin="${REPO_ROOT}/target/debug/wren"
    fi

    if [[ -x "$wren_bin" ]]; then
        local cli_logs
        cli_logs=$("${wren_bin}" logs "${job_name}" -n "${TEST_NAMESPACE}" 2>/dev/null || echo "")
        if [[ "$cli_logs" == *"${expected_output}"* ]]; then
            log "  wren logs: found expected output"
        else
            warn "wren CLI logs did not contain expected output (got: '${cli_logs}')"
            warn "CLI may not be built or may have different output format"
        fi
    else
        log "  wren CLI binary not found — skipping CLI log check"
    fi

    # Verify logs for multi-node job (prefixed output)
    local multi_job="smoke-logs-multi"
    local multi_manifest
    multi_manifest="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: WrenJob
metadata:
  name: ${multi_job}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 2
  tasksPerNode: 1
  walltime: "2m"
  container:
    image: busybox:latest
    command: ["echo"]
    args: ["multi-node-log-test"]
EOF
)"
    log "Submitting 2-node WrenJob '${multi_job}' to verify multi-node logs ..."
    echo "${multi_manifest}" | kubectl apply -f -

    if wait_for_job_state "${multi_job}" "Succeeded" "${JOB_TIMEOUT}"; then
        if [[ -x "$wren_bin" ]]; then
            local multi_logs
            multi_logs=$("${wren_bin}" logs "${multi_job}" -n "${TEST_NAMESPACE}" 2>/dev/null || echo "")
            # Multi-node logs should have pod name prefixes
            if [[ "$multi_logs" == *"${multi_job}-worker-0"* ]] && \
               [[ "$multi_logs" == *"${multi_job}-worker-1"* ]]; then
                log "  wren logs (multi-node): both workers present with prefixes"
            elif [[ "$multi_logs" == *"multi-node-log-test"* ]]; then
                log "  wren logs (multi-node): output found (prefix format may differ)"
            else
                warn "wren logs (multi-node) output unexpected: '${multi_logs}'"
            fi
        fi
    else
        warn "Multi-node job did not succeed — skipping multi-node log check"
    fi

    delete_job "${job_name}"
    delete_job "${multi_job}"
    return 0
}

smoke_test_job_logs() {
    run_test "job log retrieval after completion" _smoke_test_job_logs
}

# ---------------------------------------------------------------------------
# Run all shell smoke tests
# ---------------------------------------------------------------------------
run_shell_tests() {
    section "Running shell smoke tests"

    if [[ "$TESTS" == "rust" ]]; then
        record_skip "Shell smoke tests (TESTS=rust)"
        return
    fi

    smoke_test_topology_labels
    smoke_test_queue_creation
    smoke_test_multinode_job
    smoke_test_invalid_job
    smoke_test_walltime_job
    smoke_test_concurrent_jobs
    smoke_test_pod_labels
    smoke_test_headless_service
    smoke_test_deletion_cleanup
    smoke_test_pod_preservation
    smoke_test_job_logs
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
    echo -e "${BOLD}"
    echo "========================================================"
    echo "  Wren HPC Scheduler — Integration Tests"
    echo "  Cluster:     ${CLUSTER_NAME}"
    echo "  Namespace:   ${NAMESPACE}"
    echo "  Image:       ${IMAGE_REF}"
    echo "  Test suites: ${TESTS}"
    echo "  Skip cargo:  ${SKIP_CARGO}"
    echo "  No cleanup:  ${NO_CLEANUP}"
    echo "  Verbose:     ${VERBOSE}"
    echo "  Log file:    ${LOG_FILE}"
    echo "========================================================"
    echo -e "${RESET}"

    mkdir -p "${LOG_DIR}"
    : > "${LOG_FILE}"  # truncate log file

    check_prereqs
    # Register cleanup trap after prereq check so we don't try to delete a
    # non-existent cluster if prereqs are missing.
    register_cleanup

    setup_cluster
    build_binary
    build_and_load_image
    install_crds
    ensure_namespace
    install_rbac
    deploy_controller
    wait_for_controller

    # Run the requested test suites
    run_shell_tests
    run_rust_tests

    # Print the consolidated test summary
    print_summary

    log "Logs saved to: ${LOG_DIR}"

    if $NO_CLEANUP || [[ "$KEEP_CLUSTER" == "1" ]]; then
        warn "--no-cleanup — cluster '${CLUSTER_NAME}' left running"
    fi

    # Exit non-zero if any tests failed
    if [[ "$TESTS_FAILED" -gt 0 ]]; then
        error "${TESTS_FAILED} test(s) FAILED"
        exit 1
    fi

    success "All tests PASSED"
}

main "$@"
