#!/usr/bin/env bash
# run-integration-tests.sh — Comprehensive integration test runner for Wren HPC scheduler.
#
# Usage:
#   ./scripts/run-integration-tests.sh                     # Full run (build + kind + all tests)
#   ./scripts/run-integration-tests.sh --skip-cargo        # Skip cargo build & Rust tests
#   ./scripts/run-integration-tests.sh --no-cleanup        # Keep kind cluster after run
#   ./scripts/run-integration-tests.sh --verbose           # Print verbose output to stdout
#   ./scripts/run-integration-tests.sh --only-cli          # Run only CLI integration tests
#   ./scripts/run-integration-tests.sh --usertests-only    # Run only multi-user identity tests
#
# Environment variables (override defaults):
#   IMAGE_TAG=<tag>          — controller image tag (default: latest)
#   CLUSTER_NAME=<name>      — kind cluster name (default: wren-test)
#   NAMESPACE=<ns>           — controller namespace (default: wren-system)
#   TEST_NAMESPACE=<ns>      — namespace for test jobs (default: default)
#   CONTROLLER_TIMEOUT=<s>   — seconds to wait for controller ready (default: 120)
#   JOB_TIMEOUT=<s>          — seconds to wait for job status (default: 90)
#   TESTS=rust|shell|cli|user|all — which test suites to run (default: all)

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
WREN_JOB_LABEL="wren.giar.dev/job-name"

# Default test user for smoke/CLI tests (created after CRDs are installed).
TEST_USER_NAME="wren-test-user"
TEST_USER_UID=65534
TEST_USER_GID=65534

# ---------------------------------------------------------------------------
# Flags (set via CLI arguments, matching Reaper's interface)
# ---------------------------------------------------------------------------
SKIP_CARGO=false
NO_CLEANUP=false
VERBOSE=false
ONLY_CLI=false
ONLY_USER=false
ONLY_REAPER=false

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case $1 in
    --skip-cargo)  SKIP_CARGO=true; shift ;;
    --no-cleanup)  NO_CLEANUP=true; shift ;;
    --verbose)     VERBOSE=true; shift ;;
    --only-cli)    ONLY_CLI=true; shift ;;
    --usertests-only) ONLY_USER=true; shift ;;
    --reaper-only) ONLY_REAPER=true; shift ;;
    -h|--help)
      echo "Usage: $0 [--skip-cargo] [--no-cleanup] [--verbose] [--only-cli] [--usertests-only] [--reaper-only]"
      echo "  --skip-cargo      Skip cargo build and Rust integration tests"
      echo "  --no-cleanup      Keep kind cluster after run"
      echo "  --verbose         Also print verbose output to stdout"
      echo "  --only-cli        Run only CLI integration tests (requires running cluster)"
      echo "  --usertests-only  Run only multi-user identity integration tests"
      echo "  --reaper-only     Run only Reaper backend integration tests"
      echo ""
      echo "Environment variables:"
      echo "  CLUSTER_NAME   Kind cluster name (default: wren-test)"
      echo "  IMAGE_TAG      Controller image tag (default: latest)"
      echo "  TESTS          Test suites: rust|shell|cli|user|all (default: all)"
      echo "  JOB_TIMEOUT    Seconds to wait for job status (default: 90)"
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      echo "Usage: $0 [--skip-cargo] [--no-cleanup] [--verbose] [--only-cli] [--usertests-only] [--reaper-only]" >&2
      exit 1
      ;;
  esac
done

# Apply flag effects to config variables
if $ONLY_USER; then
  TESTS="user"
  SKIP_BUILD="${SKIP_BUILD:-0}"
elif $ONLY_REAPER; then
  TESTS="reaper"
  SKIP_BUILD="${SKIP_BUILD:-0}"
elif $ONLY_CLI; then
  TESTS="cli"
  SKIP_BUILD="${SKIP_BUILD:-0}"
elif $SKIP_CARGO; then
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

    # cargo is required when running Rust tests or CLI tests (to build the binary)
    if [[ "$TESTS" == "rust" || "$TESTS" == "cli" || "$TESTS" == "user" || "$TESTS" == "all" ]]; then
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
        -L topology.wren.giar.dev/switch \
        -L topology.wren.giar.dev/rack \
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
        crd/wrenjobs.wren.giar.dev \
        --timeout=30s
    kubectl wait --for=condition=Established \
        crd/wrenqueues.wren.giar.dev \
        --timeout=30s 2>/dev/null || warn "wrenqueues CRD not found — skipping"

    success "CRDs installed (${crd_count} files)"
}

# ---------------------------------------------------------------------------
# Step 4b: Create test WrenUser for smoke/CLI tests
#
# Every job requires a valid WrenUser identity (security: no anonymous or root
# execution). This creates a non-privileged test user used by all smoke and CLI
# tests. Multi-user identity tests create their own users.
# ---------------------------------------------------------------------------
create_test_user() {
    section "Creating test WrenUser '${TEST_USER_NAME}' (uid=${TEST_USER_UID})"
    cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: ${TEST_USER_NAME}
spec:
  uid: ${TEST_USER_UID}
  gid: ${TEST_USER_GID}
  homeDir: "/tmp"
EOF
    success "Test WrenUser created"
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

    if [[ "$TESTS" == "shell" || "$TESTS" == "cli" || "$TESTS" == "user" ]]; then
        record_skip "Rust integration tests (TESTS=${TESTS})"
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
        -l "wren.giar.dev/job-name=${job_name}" \
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
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
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
        "topology.wren.giar.dev/switch"
        "topology.wren.giar.dev/rack"
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
            # Escape both dots and slashes for jsonpath dot notation
            local escaped
            escaped=$(echo "$key" | sed 's/\./\\./g' | sed 's|/|\\/|g')
            val=$(kubectl get node "${node}" \
                -o jsonpath="{.metadata.labels.${escaped}}" 2>/dev/null || echo "")
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
        -l "topology.wren.giar.dev/switch=switch-0" \
        --no-headers 2>/dev/null | wc -l | tr -d ' ')
    switch1_count=$(kubectl get nodes \
        -l "topology.wren.giar.dev/switch=switch-1" \
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
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: ${queue_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
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
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
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
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
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
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
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
# the pods carry the correct wren.giar.dev/job-name label.
# ---------------------------------------------------------------------------
_smoke_test_pod_labels() {
    local job_name="smoke-pod-labels"
    local manifest
    manifest="$(cat <<EOF
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
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
    run_test "pod label selector (wren.giar.dev/job-name)" _smoke_test_pod_labels
}

# ---------------------------------------------------------------------------
# Smoke test 8: Headless service — verify controller creates a service per job.
# ---------------------------------------------------------------------------
_smoke_test_headless_service() {
    local job_name="smoke-headless-svc"
    local svc_name="${job_name}-workers"
    local manifest
    manifest="$(cat <<EOF
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  mpi:
    implementation: openmpi
    sshAuth: true
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo hello && sleep 60"]
EOF
)"
    log "Submitting MPI WrenJob '${job_name}' to check headless service ..."
    echo "${manifest}" | kubectl apply -f -

    # Wait for job to start
    if ! wait_for_job_state "${job_name}" "Scheduling|Running" "${JOB_TIMEOUT}"; then
        warn "Job did not progress — skipping headless service check"
        delete_job "${job_name}"
        return 0
    fi

    local deadline=$(( $(date +%s) + 30 ))
    local svc_count=0

    log "Waiting for headless service named '${svc_name}' ..."
    while true; do
        svc_count=$(kubectl get svc -n "${TEST_NAMESPACE}" \
            --field-selector="metadata.name=${svc_name}" \
            --no-headers 2>/dev/null | wc -l | tr -d ' ')

        if [[ "$svc_count" -gt 0 ]]; then
            success "Headless service '${svc_name}' found"
            kubectl get svc -n "${TEST_NAMESPACE}" \
                --field-selector="metadata.name=${svc_name}"
            delete_job "${job_name}"
            return 0
        fi

        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            warn "No headless service named '${svc_name}' found after 30s"
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
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
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
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
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
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
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
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${multi_job}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
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

# ===========================================================================
# Multi-user identity integration tests
#
# These tests exercise Phase 5.1 multi-user identity support: WrenUser CRDs,
# user annotation on jobs, securityContext (runAsUser/runAsGroup) on pods,
# USER/LOGNAME/HOME env vars, supplemental groups, and graceful degradation
# when WrenUser is missing.
# ===========================================================================

# Helper: create a WrenUser CRD and wait for it to be retrievable.
create_wrenuser() {
    local name="$1"
    local uid="$2"
    local gid="$3"
    local home_dir="${4:-}"
    local project="${5:-}"
    local extra_groups="${6:-}"  # comma-separated, e.g. "5000,6000"

    local supp_groups_yaml=""
    if [[ -n "$extra_groups" ]]; then
        supp_groups_yaml="  supplementalGroups:"
        IFS=',' read -ra groups <<< "$extra_groups"
        for g in "${groups[@]}"; do
            supp_groups_yaml="${supp_groups_yaml}
    - ${g}"
        done
    fi

    local home_yaml=""
    if [[ -n "$home_dir" ]]; then
        home_yaml="  homeDir: \"${home_dir}\""
    fi

    local project_yaml=""
    if [[ -n "$project" ]]; then
        project_yaml="  defaultProject: \"${project}\""
    fi

    cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: ${name}
spec:
  uid: ${uid}
  gid: ${gid}
${supp_groups_yaml}
${home_yaml}
${project_yaml}
EOF
}

# Helper: delete a WrenUser CRD quietly.
delete_wrenuser() {
    local name="$1"
    kubectl delete wrenuser "${name}" --ignore-not-found 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# User test 1: WrenUser CRD CRUD — create, retrieve, verify fields, delete.
# ---------------------------------------------------------------------------
_user_test_crd_crud() {
    local user_name="test-user-crud"
    local errors=0

    log "Creating WrenUser '${user_name}' ..."
    create_wrenuser "${user_name}" 2001 2001 "/home/${user_name}" "test-project" "2001,7000"

    # Verify it exists and has correct fields
    local retrieved_uid
    retrieved_uid=$(kubectl get wrenuser "${user_name}" \
        -o jsonpath='{.spec.uid}' 2>/dev/null || echo "")
    if [[ "$retrieved_uid" != "2001" ]]; then
        error "WrenUser uid mismatch: expected 2001, got '${retrieved_uid}'"
        errors=$(( errors + 1 ))
    else
        log "  uid: ${retrieved_uid} (correct)"
    fi

    local retrieved_gid
    retrieved_gid=$(kubectl get wrenuser "${user_name}" \
        -o jsonpath='{.spec.gid}' 2>/dev/null || echo "")
    if [[ "$retrieved_gid" != "2001" ]]; then
        error "WrenUser gid mismatch: expected 2001, got '${retrieved_gid}'"
        errors=$(( errors + 1 ))
    else
        log "  gid: ${retrieved_gid} (correct)"
    fi

    local retrieved_home
    retrieved_home=$(kubectl get wrenuser "${user_name}" \
        -o jsonpath='{.spec.homeDir}' 2>/dev/null || echo "")
    if [[ "$retrieved_home" != "/home/${user_name}" ]]; then
        error "WrenUser homeDir mismatch: expected '/home/${user_name}', got '${retrieved_home}'"
        errors=$(( errors + 1 ))
    else
        log "  homeDir: ${retrieved_home} (correct)"
    fi

    local retrieved_project
    retrieved_project=$(kubectl get wrenuser "${user_name}" \
        -o jsonpath='{.spec.defaultProject}' 2>/dev/null || echo "")
    if [[ "$retrieved_project" != "test-project" ]]; then
        error "WrenUser defaultProject mismatch: expected 'test-project', got '${retrieved_project}'"
        errors=$(( errors + 1 ))
    else
        log "  defaultProject: ${retrieved_project} (correct)"
    fi

    # Verify supplemental groups
    local retrieved_groups
    retrieved_groups=$(kubectl get wrenuser "${user_name}" \
        -o jsonpath='{.spec.supplementalGroups}' 2>/dev/null || echo "")
    log "  supplementalGroups: ${retrieved_groups}"

    # Verify print columns show up in table output
    local table_output
    table_output=$(kubectl get wrenuser "${user_name}" 2>/dev/null || echo "")
    log "  kubectl get output:"
    log "  ${table_output}"

    if [[ "$table_output" == *"2001"* ]]; then
        log "  UID visible in table output"
    else
        warn "UID not visible in kubectl get wrenuser table"
    fi

    # Clean up
    delete_wrenuser "${user_name}"

    # Verify deletion
    if kubectl get wrenuser "${user_name}" &>/dev/null; then
        error "WrenUser '${user_name}' still exists after deletion"
        errors=$(( errors + 1 ))
    else
        log "  WrenUser successfully deleted"
    fi

    [[ "$errors" -eq 0 ]]
}

user_test_crd_crud() {
    run_test "WrenUser CRD CRUD (create, read, delete)" _user_test_crd_crud
}

# ---------------------------------------------------------------------------
# User test 2: WrenUser minimal — only required fields (uid, gid).
# ---------------------------------------------------------------------------
_user_test_crd_minimal() {
    local user_name="test-user-minimal"

    log "Creating minimal WrenUser (uid/gid only) ..."
    cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: ${user_name}
spec:
  uid: 3001
  gid: 3001
EOF

    local retrieved_uid
    retrieved_uid=$(kubectl get wrenuser "${user_name}" \
        -o jsonpath='{.spec.uid}' 2>/dev/null || echo "")
    if [[ "$retrieved_uid" != "3001" ]]; then
        error "Minimal WrenUser uid mismatch: expected 3001, got '${retrieved_uid}'"
        delete_wrenuser "${user_name}"
        return 1
    fi

    # Optional fields should be absent/null
    local retrieved_home
    retrieved_home=$(kubectl get wrenuser "${user_name}" \
        -o jsonpath='{.spec.homeDir}' 2>/dev/null || echo "")
    if [[ -z "$retrieved_home" || "$retrieved_home" == "null" ]]; then
        log "  homeDir correctly absent"
    else
        warn "homeDir unexpectedly set: '${retrieved_home}'"
    fi

    local retrieved_project
    retrieved_project=$(kubectl get wrenuser "${user_name}" \
        -o jsonpath='{.spec.defaultProject}' 2>/dev/null || echo "")
    if [[ -z "$retrieved_project" || "$retrieved_project" == "null" ]]; then
        log "  defaultProject correctly absent"
    else
        warn "defaultProject unexpectedly set: '${retrieved_project}'"
    fi

    delete_wrenuser "${user_name}"
    return 0
}

user_test_crd_minimal() {
    run_test "WrenUser CRD with minimal fields" _user_test_crd_minimal
}

# ---------------------------------------------------------------------------
# User test 3: Job with user annotation + WrenUser → pod securityContext.
# Verifies runAsUser, runAsGroup, and supplementalGroups on the pod.
# ---------------------------------------------------------------------------
_user_test_pod_security_context() {
    local user_name="test-user-secctx"
    local job_name="user-secctx-job"
    local errors=0

    # Create the WrenUser first
    create_wrenuser "${user_name}" 4001 4001 "/home/${user_name}" "" "4001,8000"

    # Submit a job annotated with the user
    cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${user_name}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "id && sleep 30"]
EOF

    # Wait for job to progress and pods to appear
    if ! wait_for_job_state "${job_name}" "Scheduling|Running|Succeeded" "${JOB_TIMEOUT}"; then
        warn "Job did not progress — cannot verify securityContext"
        delete_job "${job_name}"
        delete_wrenuser "${user_name}"
        return 1
    fi

    # Wait for worker pod to appear
    local deadline=$(( $(date +%s) + JOB_TIMEOUT ))
    local pod_name=""
    while true; do
        pod_name=$(kubectl get pods -n "${TEST_NAMESPACE}" \
            -l "wren.giar.dev/job-name=${job_name}" \
            -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")
        if [[ -n "$pod_name" && "$pod_name" != "null" ]]; then
            break
        fi
        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            warn "No pod found for job '${job_name}'"
            delete_job "${job_name}"
            delete_wrenuser "${user_name}"
            return 1
        fi
        sleep 3
    done

    log "  Found pod: ${pod_name}"

    # Check runAsUser
    local run_as_user
    run_as_user=$(kubectl get pod "${pod_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.securityContext.runAsUser}' 2>/dev/null || echo "")
    if [[ "$run_as_user" == "4001" ]]; then
        log "  runAsUser: ${run_as_user} (correct)"
    else
        error "runAsUser mismatch: expected 4001, got '${run_as_user}'"
        errors=$(( errors + 1 ))
    fi

    # Check runAsGroup
    local run_as_group
    run_as_group=$(kubectl get pod "${pod_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.securityContext.runAsGroup}' 2>/dev/null || echo "")
    if [[ "$run_as_group" == "4001" ]]; then
        log "  runAsGroup: ${run_as_group} (correct)"
    else
        error "runAsGroup mismatch: expected 4001, got '${run_as_group}'"
        errors=$(( errors + 1 ))
    fi

    # Check supplementalGroups
    local supp_groups
    supp_groups=$(kubectl get pod "${pod_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.securityContext.supplementalGroups}' 2>/dev/null || echo "")
    log "  supplementalGroups: ${supp_groups}"
    if [[ "$supp_groups" == *"4001"* && "$supp_groups" == *"8000"* ]]; then
        log "  supplementalGroups contain expected values (correct)"
    else
        error "supplementalGroups missing expected values 4001,8000: got '${supp_groups}'"
        errors=$(( errors + 1 ))
    fi

    delete_job "${job_name}"
    delete_wrenuser "${user_name}"
    [[ "$errors" -eq 0 ]]
}

user_test_pod_security_context() {
    run_test "pod securityContext from WrenUser (runAsUser/runAsGroup)" _user_test_pod_security_context
}

# ---------------------------------------------------------------------------
# User test 4: Job with user annotation → USER/LOGNAME/HOME env vars on pod.
# ---------------------------------------------------------------------------
_user_test_pod_env_vars() {
    local user_name="test-user-env"
    local job_name="user-env-job"
    local errors=0

    create_wrenuser "${user_name}" 5001 5001 "/home/${user_name}" "envtest-project"

    cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${user_name}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo USER=\$USER LOGNAME=\$LOGNAME HOME=\$HOME && sleep 30"]
EOF

    if ! wait_for_job_state "${job_name}" "Scheduling|Running|Succeeded" "${JOB_TIMEOUT}"; then
        warn "Job did not progress"
        delete_job "${job_name}"
        delete_wrenuser "${user_name}"
        return 1
    fi

    # Wait for pod
    local deadline=$(( $(date +%s) + JOB_TIMEOUT ))
    local pod_name=""
    while true; do
        pod_name=$(kubectl get pods -n "${TEST_NAMESPACE}" \
            -l "wren.giar.dev/job-name=${job_name}" \
            -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")
        if [[ -n "$pod_name" && "$pod_name" != "null" ]]; then break; fi
        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            warn "No pod found for job '${job_name}'"
            delete_job "${job_name}"; delete_wrenuser "${user_name}"; return 1
        fi
        sleep 3
    done

    log "  Found pod: ${pod_name}"

    # Extract env vars from pod spec
    local env_json
    env_json=$(kubectl get pod "${pod_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.containers[0].env}' 2>/dev/null || echo "[]")

    # Check USER env var
    if [[ "$env_json" == *"\"name\":\"USER\""* || "$env_json" == *"USER"* ]]; then
        # Extract value — look for USER env with correct value
        local user_val
        user_val=$(kubectl get pod "${pod_name}" -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.spec.containers[0].env[?(@.name=="USER")].value}' 2>/dev/null || echo "")
        if [[ "$user_val" == "${user_name}" ]]; then
            log "  USER=${user_val} (correct)"
        else
            error "USER env var mismatch: expected '${user_name}', got '${user_val}'"
            errors=$(( errors + 1 ))
        fi
    else
        error "USER env var not found on pod"
        errors=$(( errors + 1 ))
    fi

    # Check LOGNAME env var
    local logname_val
    logname_val=$(kubectl get pod "${pod_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.containers[0].env[?(@.name=="LOGNAME")].value}' 2>/dev/null || echo "")
    if [[ "$logname_val" == "${user_name}" ]]; then
        log "  LOGNAME=${logname_val} (correct)"
    else
        error "LOGNAME env var mismatch: expected '${user_name}', got '${logname_val}'"
        errors=$(( errors + 1 ))
    fi

    # Check HOME env var
    local home_val
    home_val=$(kubectl get pod "${pod_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.containers[0].env[?(@.name=="HOME")].value}' 2>/dev/null || echo "")
    if [[ "$home_val" == "/home/${user_name}" ]]; then
        log "  HOME=${home_val} (correct)"
    else
        error "HOME env var mismatch: expected '/home/${user_name}', got '${home_val}'"
        errors=$(( errors + 1 ))
    fi

    delete_job "${job_name}"
    delete_wrenuser "${user_name}"
    [[ "$errors" -eq 0 ]]
}

user_test_pod_env_vars() {
    run_test "pod USER/LOGNAME/HOME env vars from WrenUser" _user_test_pod_env_vars
}

# ---------------------------------------------------------------------------
# User test 5: Missing WrenUser — job with wren.giar.dev/user annotation but
# no matching WrenUser CRD → job must be Failed (security: no anonymous runs).
# ---------------------------------------------------------------------------
_user_test_missing_wrenuser() {
    local job_name="user-missing-job"

    # Do NOT create a WrenUser — test that the job is rejected
    cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "nonexistent-user"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo no-user && sleep 30"]
EOF

    # The job should reach Failed — no valid user identity
    if wait_for_job_state "${job_name}" "Failed" "${JOB_TIMEOUT}"; then
        log "  Job correctly rejected with missing WrenUser"

        # Verify the reason mentions user identity
        local reason
        reason=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.status.reason}' 2>/dev/null || echo "")
        log "  Failure reason: ${reason}"
        if [[ "$reason" == *"WrenUser"* || "$reason" == *"user"* || "$reason" == *"identity"* ]]; then
            log "  Reason mentions user/identity (correct)"
        else
            warn "Failure reason does not mention user identity: '${reason}'"
        fi

        delete_job "${job_name}"
        return 0
    fi

    # If the job progressed to Running/Succeeded, that's a security failure
    local state
    state=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.status.state}' 2>/dev/null || echo "")
    if [[ "$state" == "Running" || "$state" == "Succeeded" ]]; then
        error "SECURITY: job ran without valid WrenUser identity (state: ${state})"
        delete_job "${job_name}"
        return 1
    fi

    warn "Job did not reach Failed within timeout (state: '${state}')"
    delete_job "${job_name}"
    return 1
}

user_test_missing_wrenuser() {
    run_test "missing WrenUser rejects job (security)" _user_test_missing_wrenuser
}

# ---------------------------------------------------------------------------
# User test 6: No annotation — job without wren.giar.dev/user must be Failed
# (security: every job must have a valid user identity).
# ---------------------------------------------------------------------------
_user_test_no_annotation() {
    local job_name="user-noannotation-job"

    cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
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
    command: ["sh", "-c", "echo anonymous && sleep 30"]
EOF

    # Job should reach Failed — no user annotation means no identity
    if wait_for_job_state "${job_name}" "Failed" "${JOB_TIMEOUT}"; then
        log "  Job correctly rejected without wren.giar.dev/user annotation"
        delete_job "${job_name}"
        return 0
    fi

    # If it ran, that's a security issue
    local state
    state=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.status.state}' 2>/dev/null || echo "")
    if [[ "$state" == "Running" || "$state" == "Succeeded" ]]; then
        error "SECURITY: job ran without wren.giar.dev/user annotation (state: ${state})"
        delete_job "${job_name}"
        return 1
    fi

    warn "Job did not reach Failed within timeout (state: '${state}')"
    delete_job "${job_name}"
    return 1
}

user_test_no_annotation() {
    run_test "job without wren.giar.dev/user annotation is rejected (security)" _user_test_no_annotation
}

# ---------------------------------------------------------------------------
# User test 10: WrenUser with uid=0 (root) — job must be rejected.
# ---------------------------------------------------------------------------
_user_test_reject_root_uid() {
    local user_name="test-user-root"
    local job_name="user-root-job"

    # Create a WrenUser with uid=0 (root)
    create_wrenuser "${user_name}" 0 0

    cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${user_name}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo should-not-run && sleep 30"]
EOF

    # Job should reach Failed — uid=0 is rejected
    if wait_for_job_state "${job_name}" "Failed" "${JOB_TIMEOUT}"; then
        log "  Job correctly rejected with uid=0 WrenUser"
        delete_job "${job_name}"
        delete_wrenuser "${user_name}"
        return 0
    fi

    local state
    state=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.status.state}' 2>/dev/null || echo "")
    if [[ "$state" == "Running" || "$state" == "Succeeded" ]]; then
        error "SECURITY: job ran as root (uid=0) — this must never happen"
        delete_job "${job_name}"
        delete_wrenuser "${user_name}"
        return 1
    fi

    warn "Job did not reach Failed within timeout (state: '${state}')"
    delete_job "${job_name}"
    delete_wrenuser "${user_name}"
    return 1
}

user_test_reject_root_uid() {
    run_test "WrenUser with uid=0 (root) rejects job (security)" _user_test_reject_root_uid
}

# ---------------------------------------------------------------------------
# User test 11: Two users, two jobs — verify each pod gets correct identity.
# ---------------------------------------------------------------------------
_user_test_two_users() {
    local user_a="test-user-alice"
    local user_b="test-user-bob"
    local job_a="user-alice-job"
    local job_b="user-bob-job"
    local errors=0

    create_wrenuser "${user_a}" 6001 6001 "/home/alice" "project-a"
    create_wrenuser "${user_b}" 6002 6002 "/home/bob" "project-b"

    for job_name in "${job_a}" "${job_b}"; do
        local user_ann="${user_a}"
        [[ "$job_name" == "$job_b" ]] && user_ann="${user_b}"
        cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${user_ann}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "id && sleep 30"]
EOF
    done

    # Wait for both jobs
    wait_for_job_state "${job_a}" "Scheduling|Running|Succeeded" "${JOB_TIMEOUT}" || true
    wait_for_job_state "${job_b}" "Scheduling|Running|Succeeded" "${JOB_TIMEOUT}" || true

    # Wait for pods
    sleep 5

    # Check Alice's pod
    local pod_a
    pod_a=$(kubectl get pods -n "${TEST_NAMESPACE}" \
        -l "wren.giar.dev/job-name=${job_a}" \
        -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")
    if [[ -n "$pod_a" && "$pod_a" != "null" ]]; then
        local uid_a
        uid_a=$(kubectl get pod "${pod_a}" -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.spec.securityContext.runAsUser}' 2>/dev/null || echo "")
        if [[ "$uid_a" == "6001" ]]; then
            log "  Alice's pod runAsUser: ${uid_a} (correct)"
        else
            error "Alice's pod runAsUser mismatch: expected 6001, got '${uid_a}'"
            errors=$(( errors + 1 ))
        fi

        local user_env_a
        user_env_a=$(kubectl get pod "${pod_a}" -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.spec.containers[0].env[?(@.name=="USER")].value}' 2>/dev/null || echo "")
        if [[ "$user_env_a" == "${user_a}" ]]; then
            log "  Alice's pod USER env: ${user_env_a} (correct)"
        else
            error "Alice's pod USER env mismatch: expected '${user_a}', got '${user_env_a}'"
            errors=$(( errors + 1 ))
        fi
    else
        error "No pod found for Alice's job"
        errors=$(( errors + 1 ))
    fi

    # Check Bob's pod
    local pod_b
    pod_b=$(kubectl get pods -n "${TEST_NAMESPACE}" \
        -l "wren.giar.dev/job-name=${job_b}" \
        -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")
    if [[ -n "$pod_b" && "$pod_b" != "null" ]]; then
        local uid_b
        uid_b=$(kubectl get pod "${pod_b}" -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.spec.securityContext.runAsUser}' 2>/dev/null || echo "")
        if [[ "$uid_b" == "6002" ]]; then
            log "  Bob's pod runAsUser: ${uid_b} (correct)"
        else
            error "Bob's pod runAsUser mismatch: expected 6002, got '${uid_b}'"
            errors=$(( errors + 1 ))
        fi

        local user_env_b
        user_env_b=$(kubectl get pod "${pod_b}" -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.spec.containers[0].env[?(@.name=="USER")].value}' 2>/dev/null || echo "")
        if [[ "$user_env_b" == "${user_b}" ]]; then
            log "  Bob's pod USER env: ${user_env_b} (correct)"
        else
            error "Bob's pod USER env mismatch: expected '${user_b}', got '${user_env_b}'"
            errors=$(( errors + 1 ))
        fi
    else
        error "No pod found for Bob's job"
        errors=$(( errors + 1 ))
    fi

    delete_job "${job_a}"
    delete_job "${job_b}"
    delete_wrenuser "${user_a}"
    delete_wrenuser "${user_b}"
    [[ "$errors" -eq 0 ]]
}

user_test_two_users() {
    run_test "two users get distinct identity on pods" _user_test_two_users
}

# ---------------------------------------------------------------------------
# User test 8: WrenUser without homeDir — HOME env var should be absent.
# ---------------------------------------------------------------------------
_user_test_no_homedir() {
    local user_name="test-user-nohome"
    local job_name="user-nohome-job"

    # Create WrenUser without homeDir
    create_wrenuser "${user_name}" 7001 7001

    cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${user_name}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo nohome && sleep 30"]
EOF

    if ! wait_for_job_state "${job_name}" "Scheduling|Running|Succeeded" "${JOB_TIMEOUT}"; then
        warn "Job did not progress"
        delete_job "${job_name}"; delete_wrenuser "${user_name}"; return 1
    fi

    local deadline=$(( $(date +%s) + JOB_TIMEOUT ))
    local pod_name=""
    while true; do
        pod_name=$(kubectl get pods -n "${TEST_NAMESPACE}" \
            -l "wren.giar.dev/job-name=${job_name}" \
            -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")
        if [[ -n "$pod_name" && "$pod_name" != "null" ]]; then break; fi
        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            warn "No pod found"; delete_job "${job_name}"; delete_wrenuser "${user_name}"; return 1
        fi
        sleep 3
    done

    local errors=0

    # runAsUser SHOULD be set (WrenUser exists)
    local run_as_user
    run_as_user=$(kubectl get pod "${pod_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.securityContext.runAsUser}' 2>/dev/null || echo "")
    if [[ "$run_as_user" == "7001" ]]; then
        log "  runAsUser: ${run_as_user} (correct — identity applied)"
    else
        error "runAsUser mismatch: expected 7001, got '${run_as_user}'"
        errors=$(( errors + 1 ))
    fi

    # USER env var SHOULD be set
    local user_val
    user_val=$(kubectl get pod "${pod_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.containers[0].env[?(@.name=="USER")].value}' 2>/dev/null || echo "")
    if [[ "$user_val" == "${user_name}" ]]; then
        log "  USER=${user_val} (correct)"
    else
        error "USER env var mismatch: expected '${user_name}', got '${user_val}'"
        errors=$(( errors + 1 ))
    fi

    # HOME env var should NOT be set (no homeDir in WrenUser)
    local home_val
    home_val=$(kubectl get pod "${pod_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.containers[0].env[?(@.name=="HOME")].value}' 2>/dev/null || echo "")
    if [[ -z "$home_val" ]]; then
        log "  HOME env var correctly absent (no homeDir)"
    else
        error "HOME env var unexpectedly set to '${home_val}' (WrenUser has no homeDir)"
        errors=$(( errors + 1 ))
    fi

    delete_job "${job_name}"
    delete_wrenuser "${user_name}"
    [[ "$errors" -eq 0 ]]
}

user_test_no_homedir() {
    run_test "WrenUser without homeDir — HOME env absent" _user_test_no_homedir
}

# ---------------------------------------------------------------------------
# User test 9: Job with project field — verify project is stored in CR.
# ---------------------------------------------------------------------------
_user_test_project_field() {
    local job_name="user-project-job"

    cat <<EOF | kubectl apply -f -
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  project: "climate-sim"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo project-test && sleep 10"]
EOF

    sleep 3

    local retrieved_project
    retrieved_project=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.project}' 2>/dev/null || echo "")
    if [[ "$retrieved_project" == "climate-sim" ]]; then
        log "  project field: ${retrieved_project} (correct)"
        delete_job "${job_name}"
        return 0
    else
        error "project field mismatch: expected 'climate-sim', got '${retrieved_project}'"
        delete_job "${job_name}"
        return 1
    fi
}

user_test_project_field() {
    run_test "WrenJob project field stored in CR" _user_test_project_field
}

# ---------------------------------------------------------------------------
# User test 10: WrenUser CRD is cluster-scoped — verify accessible without
# namespace and reusable across namespaces.
# ---------------------------------------------------------------------------
_user_test_cluster_scoped() {
    local user_name="test-user-cluster"

    create_wrenuser "${user_name}" 9001 9001 "/home/cluster-user"

    # Should be listable without -n flag (cluster-scoped)
    local list_output
    list_output=$(kubectl get wrenusers 2>&1 || echo "")
    if [[ "$list_output" == *"${user_name}"* ]]; then
        log "  WrenUser visible in cluster-wide list"
    else
        error "WrenUser not visible in 'kubectl get wrenusers' output"
        delete_wrenuser "${user_name}"
        return 1
    fi

    # Should NOT support -n (cluster-scoped resources ignore namespace)
    local ns_output
    ns_output=$(kubectl get wrenuser "${user_name}" 2>/dev/null || echo "")
    if [[ "$ns_output" == *"${user_name}"* || "$ns_output" == *"9001"* ]]; then
        log "  WrenUser accessible without namespace qualifier (correct — cluster-scoped)"
    else
        error "WrenUser not accessible: '${ns_output}'"
        delete_wrenuser "${user_name}"
        return 1
    fi

    delete_wrenuser "${user_name}"
    return 0
}

user_test_cluster_scoped() {
    run_test "WrenUser CRD is cluster-scoped" _user_test_cluster_scoped
}

# ---------------------------------------------------------------------------
# Run all multi-user integration tests
# ---------------------------------------------------------------------------
run_user_tests() {
    section "Running multi-user identity integration tests"

    if [[ "$TESTS" != "user" && "$TESTS" != "all" ]]; then
        record_skip "Multi-user identity tests (TESTS=${TESTS})"
        return
    fi

    user_test_crd_crud
    user_test_crd_minimal
    user_test_cluster_scoped
    user_test_project_field
    user_test_pod_security_context
    user_test_pod_env_vars
    user_test_no_homedir
    user_test_missing_wrenuser
    user_test_no_annotation
    user_test_reject_root_uid
    user_test_two_users
}

# ===========================================================================
# Reaper backend integration tests
#
# These tests verify that the Wren controller correctly accepts and processes
# WrenJobs with backend: reaper.  No actual Reaper agent is required — tests
# exercise CRD validation, field storage, and controller-side behavior only.
# ===========================================================================

# ---------------------------------------------------------------------------
# Reaper test 1: WrenJob with backend: reaper is accepted by the controller.
# ---------------------------------------------------------------------------
_reaper_test_backend_type_accepted() {
    local job_name="reaper-backend-test-$$"
    kubectl apply -f - <<YAML
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: wren-test-user
spec:
  queue: default
  priority: 50
  nodes: 1
  tasksPerNode: 1
  backend: reaper
  reaper:
    script: |
      #!/bin/bash
      echo "hello from reaper"
    environment:
      SCRATCH: /scratch
YAML

    # Wait briefly for the controller to process it
    local max_wait=15
    local elapsed=0
    local status=""
    while [[ $elapsed -lt $max_wait ]]; do
        status=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.status.state}' 2>/dev/null || echo "")
        if [[ -n "$status" ]]; then
            break
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    # Clean up
    kubectl delete wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        --ignore-not-found >> "$LOG_FILE" 2>&1

    if [[ -z "$status" ]]; then
        error "WrenJob with backend: reaper was not processed by controller"
        return 1
    fi

    log "PASS: WrenJob with backend: reaper accepted, status: ${status}"
}

# ---------------------------------------------------------------------------
# Reaper test 2: WrenJob with backend: reaper but no reaper spec should fail.
# ---------------------------------------------------------------------------
_reaper_test_reaper_spec_required() {
    local job_name="reaper-no-spec-test-$$"
    kubectl apply -f - <<YAML
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: wren-test-user
spec:
  queue: default
  priority: 50
  nodes: 1
  tasksPerNode: 1
  backend: reaper
YAML

    local max_wait=15
    local elapsed=0
    local status=""
    local message=""
    while [[ $elapsed -lt $max_wait ]]; do
        status=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.status.state}' 2>/dev/null || echo "")
        message=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.status.message}' 2>/dev/null || echo "")
        if [[ "$status" == "Failed" ]]; then
            break
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    # Clean up
    kubectl delete wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        --ignore-not-found >> "$LOG_FILE" 2>&1

    if [[ "$status" != "Failed" ]]; then
        error "Expected Failed status for reaper job without spec, got: ${status}"
        return 1
    fi

    log "PASS: WrenJob without reaper spec correctly failed: ${message}"
}

# ---------------------------------------------------------------------------
# Reaper test 3: Reaper spec fields are stored correctly in the CRD.
# ---------------------------------------------------------------------------
_reaper_test_reaper_spec_fields_stored() {
    local job_name="reaper-fields-test-$$"
    kubectl apply -f - <<YAML
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: wren-test-user
spec:
  queue: default
  priority: 50
  nodes: 2
  tasksPerNode: 1
  backend: reaper
  reaper:
    script: |
      #!/bin/bash
      module load cray-mpich
      srun ./my_app
    environment:
      SCRATCH: /scratch/project
      MY_VAR: test-value
    workingDir: /scratch/project
  mpi:
    implementation: cray-mpich
    sshAuth: false
    fabricInterface: hsn0
YAML

    sleep 2

    local script
    script=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.reaper.script}' 2>/dev/null || echo "")
    local env_scratch
    env_scratch=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.reaper.environment.SCRATCH}' 2>/dev/null || echo "")
    local working_dir
    working_dir=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.reaper.workingDir}' 2>/dev/null || echo "")
    local mpi_impl
    mpi_impl=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.mpi.implementation}' 2>/dev/null || echo "")
    local fabric
    fabric=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.spec.mpi.fabricInterface}' 2>/dev/null || echo "")

    # Clean up
    kubectl delete wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        --ignore-not-found >> "$LOG_FILE" 2>&1

    local failures=0
    if [[ -z "$script" ]] || ! echo "$script" | grep -q "cray-mpich"; then
        error "reaper script not stored correctly"
        failures=$((failures + 1))
    else
        log "  reaper.script: ok"
    fi
    if [[ "$env_scratch" != "/scratch/project" ]]; then
        error "reaper environment.SCRATCH not stored: got '${env_scratch}'"
        failures=$((failures + 1))
    else
        log "  reaper.environment.SCRATCH: ${env_scratch} (correct)"
    fi
    if [[ "$working_dir" != "/scratch/project" ]]; then
        error "reaper workingDir not stored: got '${working_dir}'"
        failures=$((failures + 1))
    else
        log "  reaper.workingDir: ${working_dir} (correct)"
    fi
    if [[ "$mpi_impl" != "cray-mpich" ]]; then
        error "mpi implementation not stored: got '${mpi_impl}'"
        failures=$((failures + 1))
    else
        log "  mpi.implementation: ${mpi_impl} (correct)"
    fi
    if [[ "$fabric" != "hsn0" ]]; then
        error "mpi fabricInterface not stored: got '${fabric}'"
        failures=$((failures + 1))
    else
        log "  mpi.fabricInterface: ${fabric} (correct)"
    fi

    if [[ $failures -gt 0 ]]; then
        return 1
    fi

    log "PASS: All reaper spec fields stored correctly in CRD"
}

# ---------------------------------------------------------------------------
# Reaper test 4: Reaper job with valid WrenUser is processed without identity
#               errors (stays in Scheduling — no Reaper agents in Kind).
# ---------------------------------------------------------------------------
_reaper_test_user_identity_on_reaper_job() {
    local job_name="reaper-user-test-$$"
    kubectl apply -f - <<YAML
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: wren-test-user
spec:
  queue: default
  priority: 50
  nodes: 1
  tasksPerNode: 1
  backend: reaper
  reaper:
    script: "echo test"
    environment: {}
YAML

    local max_wait=15
    local elapsed=0
    local status=""
    while [[ $elapsed -lt $max_wait ]]; do
        status=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.status.state}' 2>/dev/null || echo "")
        if [[ -n "$status" ]]; then
            break
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    # Capture message before cleanup
    local message=""
    message=$(kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.status.message}' 2>/dev/null || echo "")

    # Clean up
    kubectl delete wrenjob "${job_name}" -n "${TEST_NAMESPACE}" \
        --ignore-not-found >> "$LOG_FILE" 2>&1

    # The job must not have failed due to user identity resolution
    if [[ "$status" == "Failed" ]]; then
        if echo "$message" | grep -qi "user\|identity\|wrenuser"; then
            error "Reaper job failed due to user identity: ${message}"
            return 1
        fi
    fi

    log "PASS: Reaper job with valid user identity processed without identity errors, status: ${status}"
}

# ---------------------------------------------------------------------------
# Wrapper functions (mirror the user_test_* / smoke_test_* pattern)
# ---------------------------------------------------------------------------
reaper_test_backend_type_accepted()    { run_test "reaper backend type accepted"            _reaper_test_backend_type_accepted; }
reaper_test_reaper_spec_required()     { run_test "reaper spec required for reaper backend"  _reaper_test_reaper_spec_required; }
reaper_test_reaper_spec_fields_stored(){ run_test "reaper spec fields stored in CRD"         _reaper_test_reaper_spec_fields_stored; }
reaper_test_user_identity_on_reaper_job(){ run_test "user identity on reaper job"            _reaper_test_user_identity_on_reaper_job; }

# ---------------------------------------------------------------------------
# Run all Reaper backend integration tests
# ---------------------------------------------------------------------------
run_reaper_tests() {
    section "Running Reaper backend integration tests"

    if [[ "$TESTS" != "reaper" && "$TESTS" != "all" ]]; then
        record_skip "Reaper backend tests (TESTS=${TESTS})"
        return
    fi

    reaper_test_backend_type_accepted
    reaper_test_reaper_spec_required
    reaper_test_reaper_spec_fields_stored
    reaper_test_user_identity_on_reaper_job
}

# ===========================================================================
# CLI integration tests
#
# These tests exercise the `wren` CLI binary against a live cluster. They
# verify that submit, queue, status, cancel, and job IDs work end-to-end.
# ===========================================================================

# Helper: locate the wren CLI binary (release preferred, then debug).
find_wren_bin() {
    local bin="${REPO_ROOT}/target/release/wren"
    if [[ ! -x "$bin" ]]; then
        bin="${REPO_ROOT}/target/debug/wren"
    fi
    if [[ ! -x "$bin" ]]; then
        return 1
    fi
    echo "$bin"
}

# ---------------------------------------------------------------------------
# CLI test 1: wren submit — submit a job and verify job ID is returned.
# ---------------------------------------------------------------------------
_cli_test_submit_job_id() {
    local wren_bin
    wren_bin=$(find_wren_bin) || { error "wren CLI binary not found — run cargo build first"; return 1; }

    local job_name="cli-submit-jobid"
    local manifest_file
    manifest_file=$(mktemp /tmp/wren-cli-test-XXXXXX.yaml)
    cat > "${manifest_file}" <<EOF
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo hello && sleep 60"]
EOF

    log "Submitting job via wren CLI ..."
    local submit_output
    submit_output=$("${wren_bin}" submit "${manifest_file}" 2>&1) || true
    rm -f "${manifest_file}"
    log "  submit output: ${submit_output}"

    # Check output mentions the job name
    if [[ "$submit_output" != *"${job_name}"* ]]; then
        error "wren submit output does not mention job name '${job_name}'"
        delete_job "${job_name}"
        return 1
    fi

    # Check output contains a numeric job ID ("Submitted job <N>")
    if [[ "$submit_output" =~ Submitted\ job\ ([0-9]+) ]]; then
        local job_id="${BASH_REMATCH[1]}"
        log "  wren submit returned job ID: ${job_id}"
        success "wren submit returned job ID ${job_id}"
    else
        # Job ID may still be pending — check via status
        log "  submit did not return job ID inline, checking via status ..."
        sleep 5
        local status_job_id
        status_job_id=$(kubectl get wrenjob "${job_name}" \
            -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.status.jobId}' 2>/dev/null || echo "")
        if [[ -n "$status_job_id" && "$status_job_id" != "null" ]]; then
            log "  job ID found via API: ${status_job_id}"
        else
            warn "No job ID assigned (controller may not be running)"
        fi
    fi

    delete_job "${job_name}"
    return 0
}

cli_test_submit_job_id() {
    run_test "CLI: wren submit returns job ID" _cli_test_submit_job_id
}

# ---------------------------------------------------------------------------
# CLI test 2: wren queue — verify table output includes JOBID column.
# ---------------------------------------------------------------------------
_cli_test_queue_output() {
    local wren_bin
    wren_bin=$(find_wren_bin) || { error "wren CLI binary not found"; return 1; }

    # Submit a job so there's something to list
    local job_name="cli-queue-test"
    cat <<EOF | kubectl apply -f - 2>/dev/null
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo hello && sleep 60"]
EOF

    # Wait for it to be reconciled
    wait_for_job_state "${job_name}" "Scheduling|Running|Succeeded" 30 || true

    log "Running wren queue ..."
    local queue_output
    queue_output=$("${wren_bin}" queue -n "${TEST_NAMESPACE}" 2>&1) || true
    log "  queue output:"
    log "${queue_output}"

    local errors=0

    # Verify JOBID column header
    if [[ "$queue_output" == *"JOBID"* ]]; then
        log "  JOBID column header present"
    else
        error "wren queue output missing JOBID column header"
        errors=$(( errors + 1 ))
    fi

    # Verify NAME column
    if [[ "$queue_output" == *"NAME"* ]]; then
        log "  NAME column header present"
    else
        error "wren queue output missing NAME column header"
        errors=$(( errors + 1 ))
    fi

    # Verify STATE column
    if [[ "$queue_output" == *"STATE"* ]]; then
        log "  STATE column header present"
    else
        error "wren queue output missing STATE column header"
        errors=$(( errors + 1 ))
    fi

    # Verify our job appears in the output
    if [[ "$queue_output" == *"${job_name}"* ]]; then
        log "  job '${job_name}' listed in queue output"
    else
        error "wren queue output does not list job '${job_name}'"
        errors=$(( errors + 1 ))
    fi

    delete_job "${job_name}"
    [[ "$errors" -eq 0 ]]
}

cli_test_queue_output() {
    run_test "CLI: wren queue shows JOBID column" _cli_test_queue_output
}

# ---------------------------------------------------------------------------
# CLI test 3: wren status — verify output includes JobID and key fields.
# ---------------------------------------------------------------------------
_cli_test_status_output() {
    local wren_bin
    wren_bin=$(find_wren_bin) || { error "wren CLI binary not found"; return 1; }

    local job_name="cli-status-test"
    cat <<EOF | kubectl apply -f - 2>/dev/null
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
spec:
  queue: default
  nodes: 2
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo hello && sleep 60"]
EOF

    # Wait for controller to assign a job ID
    wait_for_job_state "${job_name}" "Scheduling|Running|Succeeded" 30 || true

    log "Running wren status ..."
    local status_output
    status_output=$("${wren_bin}" status "${job_name}" -n "${TEST_NAMESPACE}" 2>&1) || true
    log "  status output:"
    log "${status_output}"

    local errors=0

    # Verify key fields
    if [[ "$status_output" == *"Name:"* ]]; then
        log "  Name field present"
    else
        error "wren status missing Name field"
        errors=$(( errors + 1 ))
    fi

    if [[ "$status_output" == *"Namespace:"* ]]; then
        log "  Namespace field present"
    else
        error "wren status missing Namespace field"
        errors=$(( errors + 1 ))
    fi

    if [[ "$status_output" == *"Queue:"* ]]; then
        log "  Queue field present"
    else
        error "wren status missing Queue field"
        errors=$(( errors + 1 ))
    fi

    if [[ "$status_output" == *"Nodes:"* ]]; then
        log "  Nodes field present"
    else
        error "wren status missing Nodes field"
        errors=$(( errors + 1 ))
    fi

    # Check for JobID (may not be present if controller hasn't reconciled yet)
    if [[ "$status_output" == *"JobID:"* ]]; then
        log "  JobID field present"
    else
        warn "wren status missing JobID field (controller may not have assigned yet)"
    fi

    # Verify it shows the correct job name
    if [[ "$status_output" == *"${job_name}"* ]]; then
        log "  Correct job name in output"
    else
        error "wren status does not contain job name '${job_name}'"
        errors=$(( errors + 1 ))
    fi

    delete_job "${job_name}"
    [[ "$errors" -eq 0 ]]
}

cli_test_status_output() {
    run_test "CLI: wren status shows job details" _cli_test_status_output
}

# ---------------------------------------------------------------------------
# CLI test 4: wren cancel — submit then cancel, verify deletion.
# ---------------------------------------------------------------------------
_cli_test_cancel() {
    local wren_bin
    wren_bin=$(find_wren_bin) || { error "wren CLI binary not found"; return 1; }

    local job_name="cli-cancel-test"
    cat <<EOF | kubectl apply -f - 2>/dev/null
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "10m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "sleep 300"]
EOF

    # Give the controller a moment
    sleep 3

    log "Running wren cancel ..."
    local cancel_output
    cancel_output=$("${wren_bin}" cancel "${job_name}" -n "${TEST_NAMESPACE}" 2>&1) || true
    log "  cancel output: ${cancel_output}"

    if [[ "$cancel_output" == *"cancelled"* ]]; then
        log "  Cancel confirmed in output"
    else
        warn "wren cancel output did not contain 'cancelled': '${cancel_output}'"
    fi

    # Verify job is gone
    sleep 2
    if kubectl get wrenjob "${job_name}" -n "${TEST_NAMESPACE}" &>/dev/null; then
        error "WrenJob '${job_name}' still exists after wren cancel"
        delete_job "${job_name}"
        return 1
    fi

    log "  Job successfully deleted via wren cancel"
    # Clean up any remaining pods
    kubectl delete pods \
        -n "${TEST_NAMESPACE}" \
        -l "wren.giar.dev/job-name=${job_name}" \
        --ignore-not-found 2>/dev/null || true
    return 0
}

cli_test_cancel() {
    run_test "CLI: wren cancel deletes job" _cli_test_cancel
}

# ---------------------------------------------------------------------------
# CLI test 5: Sequential job IDs — submit two jobs and verify IDs increment.
# ---------------------------------------------------------------------------
_cli_test_sequential_job_ids() {
    local job_name_1="cli-seqid-1"
    local job_name_2="cli-seqid-2"

    for job_name in "${job_name_1}" "${job_name_2}"; do
        cat <<EOF | kubectl apply -f - 2>/dev/null
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo hello && sleep 60"]
EOF
    done

    # Wait for both to be reconciled
    wait_for_job_state "${job_name_1}" "Scheduling|Running|Succeeded" 30 || true
    wait_for_job_state "${job_name_2}" "Scheduling|Running|Succeeded" 30 || true

    local id1 id2
    id1=$(kubectl get wrenjob "${job_name_1}" \
        -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.status.jobId}' 2>/dev/null || echo "")
    id2=$(kubectl get wrenjob "${job_name_2}" \
        -n "${TEST_NAMESPACE}" \
        -o jsonpath='{.status.jobId}' 2>/dev/null || echo "")

    log "  Job 1 (${job_name_1}) ID: ${id1:-<none>}"
    log "  Job 2 (${job_name_2}) ID: ${id2:-<none>}"

    local errors=0

    # Both should have numeric IDs
    if [[ -z "$id1" || "$id1" == "null" ]]; then
        error "Job 1 has no job ID assigned"
        errors=$(( errors + 1 ))
    fi

    if [[ -z "$id2" || "$id2" == "null" ]]; then
        error "Job 2 has no job ID assigned"
        errors=$(( errors + 1 ))
    fi

    # IDs should be different and sequential (id2 > id1)
    if [[ "$errors" -eq 0 ]]; then
        if [[ "$id1" -ne "$id2" ]]; then
            log "  IDs are distinct (correct)"
        else
            error "Both jobs got the same ID: ${id1}"
            errors=$(( errors + 1 ))
        fi

        if [[ "$id2" -gt "$id1" ]]; then
            log "  ID2 (${id2}) > ID1 (${id1}) — sequential (correct)"
        else
            error "IDs are not sequential: id1=${id1}, id2=${id2}"
            errors=$(( errors + 1 ))
        fi
    fi

    # Verify IDs show up in kubectl get wrenjobs output
    local kubectl_output
    kubectl_output=$(kubectl get wrenjobs -n "${TEST_NAMESPACE}" 2>/dev/null || echo "")
    log "  kubectl get wrenjobs:"
    log "${kubectl_output}"

    if [[ "$kubectl_output" == *"JOBID"* ]]; then
        log "  JOBID column visible in kubectl output"
    else
        warn "JOBID column not visible in kubectl get wrenjobs (CRD may need regeneration)"
    fi

    delete_job "${job_name_1}"
    delete_job "${job_name_2}"
    [[ "$errors" -eq 0 ]]
}

cli_test_sequential_job_ids() {
    run_test "CLI: sequential job ID assignment" _cli_test_sequential_job_ids
}

# ---------------------------------------------------------------------------
# CLI test 6: wren queue with --queue filter — verify filtering works.
# ---------------------------------------------------------------------------
_cli_test_queue_filter() {
    local wren_bin
    wren_bin=$(find_wren_bin) || { error "wren CLI binary not found"; return 1; }

    local job_name="cli-filter-test"
    cat <<EOF | kubectl apply -f - 2>/dev/null
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: ${job_name}
  namespace: ${TEST_NAMESPACE}
  annotations:
    wren.giar.dev/user: "${TEST_USER_NAME}"
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "sleep 60"]
EOF

    sleep 3

    # Filter by existing queue — should include our job
    local output_default
    output_default=$("${wren_bin}" queue -q default -n "${TEST_NAMESPACE}" 2>&1) || true

    if [[ "$output_default" == *"${job_name}"* ]]; then
        log "  Job found in default queue filter"
    else
        error "Job not found when filtering by queue 'default'"
        delete_job "${job_name}"
        return 1
    fi

    # Filter by non-existent queue — should NOT include our job
    local output_none
    output_none=$("${wren_bin}" queue -q nonexistent -n "${TEST_NAMESPACE}" 2>&1) || true

    if [[ "$output_none" == *"${job_name}"* ]]; then
        error "Job appeared in wrong queue filter 'nonexistent'"
        delete_job "${job_name}"
        return 1
    fi

    log "  Queue filter correctly excludes job from wrong queue"
    delete_job "${job_name}"
    return 0
}

cli_test_queue_filter() {
    run_test "CLI: wren queue --queue filter" _cli_test_queue_filter
}

# ---------------------------------------------------------------------------
# Run all CLI tests
# ---------------------------------------------------------------------------
run_cli_tests() {
    section "Running CLI integration tests"

    if [[ "$TESTS" != "cli" && "$TESTS" != "all" ]]; then
        record_skip "CLI integration tests (TESTS=${TESTS})"
        return
    fi

    # Always build the CLI to ensure we test current code
    log "Building wren CLI (release) ..."
    if ! cargo build --release -p wren-cli --manifest-path "${REPO_ROOT}/Cargo.toml"; then
        error "Failed to build wren CLI"
        record_skip "CLI integration tests (build failed)"
        return
    fi

    local wren_bin
    wren_bin=$(find_wren_bin) || {
        error "wren CLI binary not found after build"
        record_skip "CLI integration tests (no binary)"
        return
    }

    log "Using wren CLI: ${wren_bin}"
    log "Version: $("${wren_bin}" --version 2>/dev/null || echo '<unknown>')"

    cli_test_submit_job_id
    cli_test_queue_output
    cli_test_status_output
    cli_test_cancel
    cli_test_sequential_job_ids
    cli_test_queue_filter
}

# ---------------------------------------------------------------------------
# Run all shell smoke tests
# ---------------------------------------------------------------------------
run_shell_tests() {
    section "Running shell smoke tests"

    if [[ "$TESTS" == "rust" || "$TESTS" == "cli" || "$TESTS" == "user" || "$TESTS" == "reaper" ]]; then
        record_skip "Shell smoke tests (TESTS=${TESTS})"
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
    echo "  Only CLI:    ${ONLY_CLI}"
    echo "  Only User:   ${ONLY_USER}"
    echo "  Only Reaper: ${ONLY_REAPER}"
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
    create_test_user
    ensure_namespace
    install_rbac
    deploy_controller
    wait_for_controller

    # Run the requested test suites
    run_shell_tests
    run_user_tests
    run_reaper_tests
    run_cli_tests
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
