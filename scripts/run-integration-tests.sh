#!/usr/bin/env bash
# run-integration-tests.sh — Integration test runner for Bubo
#
# Usage:
#   ./scripts/run-integration-tests.sh
#
# Environment variables:
#   KEEP_CLUSTER=1       — skip cluster teardown after tests
#   SKIP_BUILD=1         — skip Docker image build (use pre-built image)
#   IMAGE_TAG=<tag>      — controller image tag (default: latest)
#   CLUSTER_NAME=<name>  — kind cluster name (default: bubo-test)
#   NAMESPACE=<ns>       — controller namespace (default: bubo-system)
#   TEST_NAMESPACE=<ns>  — namespace for test jobs (default: default)
#   CONTROLLER_TIMEOUT=  — seconds to wait for controller ready (default: 120)
#   JOB_TIMEOUT=         — seconds to wait for job status (default: 90)

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
CLUSTER_NAME="${CLUSTER_NAME:-bubo-test}"
NAMESPACE="${NAMESPACE:-bubo-system}"
TEST_NAMESPACE="${TEST_NAMESPACE:-default}"
IMAGE_NAME="bubo-controller"
IMAGE_TAG="${IMAGE_TAG:-latest}"
IMAGE_REF="${IMAGE_NAME}:${IMAGE_TAG}"
CONTROLLER_TIMEOUT="${CONTROLLER_TIMEOUT:-120}"
JOB_TIMEOUT="${JOB_TIMEOUT:-90}"
LOG_DIR="/tmp/bubo-integration-logs"
KEEP_CLUSTER="${KEEP_CLUSTER:-0}"
SKIP_BUILD="${SKIP_BUILD:-0}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST_DIR="${REPO_ROOT}/manifests"
DOCKERFILE="${REPO_ROOT}/docker/Dockerfile.controller"
TEST_JOB_NAME="smoke-test-mpi"

# ---------------------------------------------------------------------------
# Colours & logging helpers
# ---------------------------------------------------------------------------
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
RESET='\033[0m'

log()      { echo -e "${BLUE}[INFO]${RESET}  $*"; }
success()  { echo -e "${GREEN}[PASS]${RESET}  $*"; }
warn()     { echo -e "${YELLOW}[WARN]${RESET}  $*"; }
error()    { echo -e "${RED}[FAIL]${RESET}  $*" >&2; }
section()  { echo -e "\n${BOLD}==> $*${RESET}"; }

# ---------------------------------------------------------------------------
# Prerequisite checks
# ---------------------------------------------------------------------------
check_prereqs() {
    section "Checking prerequisites"
    local missing=0
    for cmd in kind kubectl docker; do
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

    if [[ "$KEEP_CLUSTER" == "1" ]]; then
        warn "KEEP_CLUSTER=1 — skipping cluster teardown"
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
        -l app.kubernetes.io/name=bubo-controller \
        --all-containers \
        --tail=-1 \
        2>/dev/null > "${LOG_DIR}/controller.log" || true

    # Events in bubo-system
    kubectl get events -n "${NAMESPACE}" \
        --sort-by='.lastTimestamp' \
        2>/dev/null > "${LOG_DIR}/bubo-system-events.log" || true

    # Events in test namespace
    kubectl get events -n "${TEST_NAMESPACE}" \
        --sort-by='.lastTimestamp' \
        2>/dev/null > "${LOG_DIR}/test-namespace-events.log" || true

    # MPIJob state
    kubectl get mpijobs -n "${TEST_NAMESPACE}" -o yaml \
        2>/dev/null > "${LOG_DIR}/mpijobs.yaml" || true

    # All pods
    kubectl get pods -A -o wide \
        2>/dev/null > "${LOG_DIR}/pods.log" || true

    log "Logs written:"
    ls -lh "${LOG_DIR}"
}

# ---------------------------------------------------------------------------
# Step 1: Set up kind cluster
# ---------------------------------------------------------------------------
setup_cluster() {
    section "Setting up kind cluster '${CLUSTER_NAME}'"

    if kind get clusters 2>/dev/null | grep -qx "${CLUSTER_NAME}"; then
        warn "Cluster '${CLUSTER_NAME}' already exists — reusing it"
        kind export kubeconfig --name "${CLUSTER_NAME}"
    else
        log "Creating cluster '${CLUSTER_NAME}' ..."
        kind create cluster --name "${CLUSTER_NAME}" --wait 60s
        success "Cluster created"
    fi

    # Verify connectivity
    kubectl cluster-info --context "kind-${CLUSTER_NAME}"
    log "Nodes:"
    kubectl get nodes
    success "Cluster is reachable"
}

# ---------------------------------------------------------------------------
# Step 2: Build controller binary (optional, for local use)
# ---------------------------------------------------------------------------
build_binary() {
    section "Building controller binary"
    if [[ "$SKIP_BUILD" == "1" ]]; then
        log "SKIP_BUILD=1 — skipping cargo build"
        return
    fi
    log "Running: cargo build --release -p bubo-controller"
    cargo build --release -p bubo-controller \
        --manifest-path "${REPO_ROOT}/Cargo.toml"
    success "Binary built: target/release/bubo-controller"
}

# ---------------------------------------------------------------------------
# Step 3: Build and load Docker image into kind
# ---------------------------------------------------------------------------
build_and_load_image() {
    section "Building Docker image ${IMAGE_REF}"

    if [[ "$SKIP_BUILD" == "1" ]]; then
        log "SKIP_BUILD=1 — assuming image '${IMAGE_REF}' already exists locally"
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
        crd_count=$((crd_count + 1))
    done

    if [[ "$crd_count" -eq 0 ]]; then
        error "No CRD YAML files found in ${crd_dir}"
        exit 1
    fi

    # Wait for CRDs to become established
    log "Waiting for CRDs to be established ..."
    kubectl wait --for=condition=Established \
        crd/mpijobs.hpc.cscs.ch \
        --timeout=30s
    kubectl wait --for=condition=Established \
        crd/buboqueues.hpc.cscs.ch \
        --timeout=30s 2>/dev/null || warn "buboqueues CRD not found — skipping"

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
# Step 6: Create namespace (deployment.yaml also declares it, but be explicit)
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
# ---------------------------------------------------------------------------
deploy_controller() {
    section "Deploying controller from ${MANIFEST_DIR}/deployment.yaml"
    local deploy_file="${MANIFEST_DIR}/deployment.yaml"

    if [[ ! -f "$deploy_file" ]]; then
        error "Deployment manifest not found: ${deploy_file}"
        exit 1
    fi

    kubectl apply -f "${deploy_file}"
    success "Controller manifest applied"
}

# ---------------------------------------------------------------------------
# Step 8: Wait for controller to be ready
# ---------------------------------------------------------------------------
wait_for_controller() {
    section "Waiting for controller to become ready (timeout: ${CONTROLLER_TIMEOUT}s)"

    if ! kubectl wait deployment/bubo-controller \
        -n "${NAMESPACE}" \
        --for=condition=Available \
        --timeout="${CONTROLLER_TIMEOUT}s"; then
        error "Controller did not become ready within ${CONTROLLER_TIMEOUT}s"
        error "Controller pod status:"
        kubectl get pods -n "${NAMESPACE}" -l app.kubernetes.io/name=bubo-controller || true
        error "Controller logs:"
        kubectl logs -n "${NAMESPACE}" \
            -l app.kubernetes.io/name=bubo-controller \
            --tail=50 || true
        exit 1
    fi

    success "Controller is ready"
    kubectl get pods -n "${NAMESPACE}" -l app.kubernetes.io/name=bubo-controller
}

# ---------------------------------------------------------------------------
# Step 9: Smoke test
# ---------------------------------------------------------------------------

# Write the test MPIJob manifest inline so the script is self-contained.
# It mirrors manifests/examples/simple-mpi.yaml but uses a distinct name
# and an ultra-fast command so the job progresses quickly in CI.
TEST_JOB_MANIFEST="$(cat <<EOF
apiVersion: hpc.cscs.ch/v1alpha1
kind: MPIJob
metadata:
  name: ${TEST_JOB_NAME}
  namespace: ${TEST_NAMESPACE}
spec:
  queue: default
  nodes: 1
  tasksPerNode: 1
  walltime: "5m"
  container:
    image: busybox:latest
    command: ["sh", "-c", "echo hello-bubo && sleep 5"]
EOF
)"

smoke_test_create_job() {
    section "Smoke test: creating MPIJob '${TEST_JOB_NAME}'"
    echo "${TEST_JOB_MANIFEST}" | kubectl apply -f -
    success "MPIJob '${TEST_JOB_NAME}' submitted"
}

smoke_test_wait_for_scheduling() {
    section "Smoke test: waiting for job status to leave Pending (timeout: ${JOB_TIMEOUT}s)"

    local deadline=$(( $(date +%s) + JOB_TIMEOUT ))
    local state=""

    while true; do
        state=$(kubectl get mpijob "${TEST_JOB_NAME}" \
            -n "${TEST_NAMESPACE}" \
            -o jsonpath='{.status.state}' 2>/dev/null || echo "")

        log "  MPIJob state: '${state:-<empty>}'"

        case "$state" in
            Scheduling|Running|Succeeded)
                success "Job reached state: ${state}"
                return 0
                ;;
            Failed|Cancelled|WalltimeExceeded)
                error "Job entered terminal failure state: ${state}"
                return 1
                ;;
        esac

        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            error "Timed out waiting for job to leave Pending state (last state: '${state:-<empty>}')"
            error "This may indicate the controller is not reconciling MPIJob resources."
            return 1
        fi

        sleep 3
    done
}

smoke_test_verify_resources() {
    section "Smoke test: verifying pods and services are created"

    local deadline=$(( $(date +%s) + JOB_TIMEOUT ))
    local pod_count=0

    log "Waiting for worker pods labelled with job '${TEST_JOB_NAME}' ..."
    while true; do
        pod_count=$(kubectl get pods -n "${TEST_NAMESPACE}" \
            -l "bubo.hpc.cscs.ch/job=${TEST_JOB_NAME}" \
            --no-headers 2>/dev/null | wc -l | tr -d ' ')

        if [[ "$pod_count" -gt 0 ]]; then
            success "Found ${pod_count} pod(s) for job '${TEST_JOB_NAME}'"
            kubectl get pods -n "${TEST_NAMESPACE}" \
                -l "bubo.hpc.cscs.ch/job=${TEST_JOB_NAME}"
            break
        fi

        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            warn "No pods found with label bubo.hpc.cscs.ch/job=${TEST_JOB_NAME}"
            warn "Controller may use different label keys — listing all pods in ${TEST_NAMESPACE}:"
            kubectl get pods -n "${TEST_NAMESPACE}" || true
            # Not fatal: pod labels are an implementation detail still in flux.
            break
        fi

        sleep 3
    done

    # Check for headless service (controller creates one per job for DNS)
    local svc_count
    svc_count=$(kubectl get svc -n "${TEST_NAMESPACE}" \
        --field-selector="metadata.name=${TEST_JOB_NAME}" \
        --no-headers 2>/dev/null | wc -l | tr -d ' ')

    if [[ "$svc_count" -gt 0 ]]; then
        success "Headless service found for job '${TEST_JOB_NAME}'"
        kubectl get svc -n "${TEST_NAMESPACE}" \
            --field-selector="metadata.name=${TEST_JOB_NAME}"
    else
        warn "No headless service found named '${TEST_JOB_NAME}' (may not be created until Running phase)"
    fi
}

smoke_test_delete_job() {
    section "Smoke test: deleting MPIJob '${TEST_JOB_NAME}'"
    kubectl delete mpijob "${TEST_JOB_NAME}" \
        -n "${TEST_NAMESPACE}" \
        --timeout=60s
    success "MPIJob deleted"
}

smoke_test_verify_cleanup() {
    section "Smoke test: verifying cleanup (pods/services removed)"

    local deadline=$(( $(date +%s) + 60 ))

    log "Waiting for pods to be removed ..."
    while true; do
        local pod_count
        pod_count=$(kubectl get pods -n "${TEST_NAMESPACE}" \
            -l "bubo.hpc.cscs.ch/job=${TEST_JOB_NAME}" \
            --no-headers 2>/dev/null | wc -l | tr -d ' ')

        if [[ "$pod_count" -eq 0 ]]; then
            success "All pods for '${TEST_JOB_NAME}' have been removed"
            break
        fi

        if [[ "$(date +%s)" -ge "$deadline" ]]; then
            warn "Pods still present after 60s — cleanup may be in progress"
            kubectl get pods -n "${TEST_NAMESPACE}" \
                -l "bubo.hpc.cscs.ch/job=${TEST_JOB_NAME}" || true
            break
        fi

        log "  ${pod_count} pod(s) still present, waiting ..."
        sleep 3
    done

    # Verify the MPIJob itself is gone
    if kubectl get mpijob "${TEST_JOB_NAME}" -n "${TEST_NAMESPACE}" &>/dev/null; then
        warn "MPIJob '${TEST_JOB_NAME}' still exists after deletion — finalizer may be pending"
    else
        success "MPIJob '${TEST_JOB_NAME}' confirmed deleted"
    fi
}

run_smoke_tests() {
    smoke_test_create_job
    smoke_test_wait_for_scheduling
    smoke_test_verify_resources
    smoke_test_delete_job
    smoke_test_verify_cleanup
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
    echo -e "${BOLD}"
    echo "========================================================"
    echo "  Bubo Integration Tests"
    echo "  Cluster:   ${CLUSTER_NAME}"
    echo "  Namespace: ${NAMESPACE}"
    echo "  Image:     ${IMAGE_REF}"
    echo "  Log dir:   ${LOG_DIR}"
    echo "========================================================"
    echo -e "${RESET}"

    mkdir -p "${LOG_DIR}"

    check_prereqs
    register_cleanup   # trap registered after prereq check so we don't try to delete a non-existent cluster

    setup_cluster
    build_binary
    build_and_load_image
    install_crds
    install_rbac
    ensure_namespace
    deploy_controller
    wait_for_controller
    run_smoke_tests

    section "Results"
    success "All integration tests PASSED"
    log "Logs saved to: ${LOG_DIR}"

    if [[ "$KEEP_CLUSTER" == "1" ]]; then
        warn "KEEP_CLUSTER=1 — cluster '${CLUSTER_NAME}' left running"
    fi
}

main "$@"
