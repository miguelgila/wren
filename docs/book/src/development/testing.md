# Testing in Wren

Wren has comprehensive test coverage across unit tests, integration tests, and end-to-end tests. This guide covers how to run tests locally and how to add new tests.

## Test Overview

| Layer | Location | Purpose | Running |
|-------|----------|---------|---------|
| **Unit tests** | `crates/*/src/**/*.rs` | Scheduling algorithms, types, error handling | `cargo test --workspace` |
| **Integration tests** | `tests/` and shell scripts | Controller reconciliation, job lifecycle, CLI | `scripts/run-integration-tests.sh` |
| **Example tests** | `manifests/examples/` | Real job execution on kind cluster | `scripts/test-examples.sh` |

Current test counts:
- **561 unit tests** (22 cli + 245 controller + 81 core + 213 scheduler) — all passing
- **33 integration tests** on kind cluster — all passing
- **4 Reaper backend tests** — all passing
- **11 multi-user identity tests** — all passing

## Running Unit Tests

Unit tests are the foundation. Run them with:

```bash
cargo test --workspace
```

Run tests for a specific crate:

```bash
cargo test --package wren-scheduler
cargo test --package wren-controller
cargo test --package wren-cli
cargo test --package wren-core
```

Run a specific test by name:

```bash
cargo test test_gang_schedule
```

Run tests with output (normally captured):

```bash
cargo test --workspace -- --nocapture
```

Run tests in single-threaded mode (useful for debugging):

```bash
cargo test --workspace -- --test-threads=1 --nocapture
```

Show test output even for passing tests:

```bash
cargo test --workspace -- --nocapture --show-output
```

## Running Integration Tests

Integration tests run against a real Kubernetes cluster (created locally using `kind`). They validate:
- CRD installation and schema
- Controller startup and readiness
- Job lifecycle (Pending → Scheduling → Running → Succeeded)
- Resource creation (Pods, Services, ConfigMaps)
- Job cancellation and cleanup
- Multi-user identity mapping
- Reaper backend execution

### Full Integration Test Suite

```bash
scripts/run-integration-tests.sh
```

This script:
1. Checks prerequisites (kind, kubectl, docker, cargo)
2. Creates a local kind cluster
3. Builds the controller Docker image
4. Applies CRDs and RBAC
5. Deploys the controller
6. Runs 33 integration tests
7. Collects logs and cleans up

Execution time: ~3-5 minutes.

### Integration Test Flags

Run only CLI integration tests (does not require kind cluster if already running):

```bash
scripts/run-integration-tests.sh --only-cli
```

Run only multi-user identity tests:

```bash
scripts/run-integration-tests.sh --usertests-only
```

Run only Reaper backend tests:

```bash
scripts/run-integration-tests.sh --reaper-only
```

Skip cargo build and Rust tests (only run shell-based integration tests):

```bash
scripts/run-integration-tests.sh --skip-cargo
```

Keep the kind cluster after tests (for debugging):

```bash
scripts/run-integration-tests.sh --no-cleanup
```

Print verbose output to stdout:

```bash
scripts/run-integration-tests.sh --verbose
```

### Integration Test Environment Variables

Override defaults with environment variables:

```bash
# Use a different cluster name
CLUSTER_NAME=wren-dev scripts/run-integration-tests.sh

# Use a different controller image tag
IMAGE_TAG=my-custom-tag scripts/run-integration-tests.sh

# Override test job timeout (default: 90 seconds)
JOB_TIMEOUT=180 scripts/run-integration-tests.sh

# Run only specific test suites: rust|shell|cli|user|reaper|all
TESTS=cli scripts/run-integration-tests.sh
```

## Test Organization

### Unit Tests

Unit tests are defined inline with the code they test:

```rust
// crates/wren-scheduler/src/gang.rs

pub fn gang_schedule(job: &WrenJob, nodes: &[Node]) -> Result<Vec<usize>> {
    // Implementation...
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gang_schedule_all_or_nothing() {
        let nodes = vec![
            Node::new("node-0", 4),
            Node::new("node-1", 4),
        ];
        let job = WrenJob::new("job-1", 3);

        let result = gang_schedule(&job, &nodes);
        assert!(result.is_err(), "Should fail when insufficient nodes");
    }

    #[test]
    fn test_gang_schedule_success() {
        let nodes = vec![
            Node::new("node-0", 4),
            Node::new("node-1", 4),
            Node::new("node-2", 4),
        ];
        let job = WrenJob::new("job-1", 3);

        let result = gang_schedule(&job, &nodes);
        assert!(result.is_ok(), "Should succeed with sufficient nodes");
    }
}
```

Tests use the same module structure as the code, making them easy to locate and maintain.

### Integration Tests

Integration tests are shell scripts in `scripts/run-integration-tests.sh` that:
1. Create or verify a kind cluster exists
2. Deploy Wren controller and CRDs
3. Submit jobs via `wren` CLI or kubectl
4. Poll job status
5. Verify pods, services, and configmaps are created
6. Verify cleanup

Example test (inside the shell script):

```bash
test_job_lifecycle() {
    local job_name="test-$(date +%s)"

    # Submit a job
    kubectl apply -f - <<EOF
    apiVersion: wren.giar.dev/v1alpha1
    kind: WrenJob
    metadata:
      name: $job_name
    spec:
      queue: default
      nodes: 2
      container:
        image: busybox
        command: ["sleep", "5"]
    EOF

    # Wait for job to complete
    kubectl wait --for=condition=Succeeded wrenjob/$job_name --timeout=60s

    # Verify pods were cleaned up
    local pods=$(kubectl get pods -l wren.giar.dev/job-name=$job_name --no-headers 2>/dev/null | wc -l)
    [[ $pods -eq 0 ]] && return 0 || return 1
}
```

## Adding New Tests

### Adding a Unit Test

1. Open the file you want to test: `crates/*/src/module.rs`

2. Add a `tests` module at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use some_helper_crate::*;

    #[test]
    fn test_feature_happy_path() {
        let input = setup_test_data();
        let result = function_under_test(input);
        assert_eq!(result.status, Status::Success);
    }

    #[test]
    fn test_feature_error_case() {
        let input = invalid_data();
        let result = function_under_test(input);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_async_function() {
        let result = async_function().await;
        assert!(result.is_ok());
    }
}
```

3. Run the test:

```bash
cargo test --package wren-scheduler test_feature_happy_path
```

### Adding an Integration Test

Integration tests validate behavior in a Kubernetes environment. Add tests to `scripts/run-integration-tests.sh`:

1. Define the test function:

```bash
test_my_new_feature() {
    section "Testing my new feature"

    # Submit a job
    kubectl apply -f - <<EOF
    apiVersion: wren.giar.dev/v1alpha1
    kind: WrenJob
    metadata:
      name: test-feature-$(date +%s)
    spec:
      queue: default
      nodes: 2
      container:
        image: busybox
        command: ["echo", "hello"]
    EOF

    # Verify expected state
    local job_name="test-feature-..."
    kubectl wait --for=condition=Succeeded wrenjob/$job_name --timeout=60s || return 1

    success "Feature test passed"
    return 0
}
```

2. Register the test in the main test runner:

```bash
if [[ "$TESTS" == "all" || "$TESTS" == "shell" ]]; then
    run_test "My New Feature" test_my_new_feature
fi
```

3. Run the test:

```bash
scripts/run-integration-tests.sh --no-cleanup
```

### Test Naming Conventions

Use descriptive test names that clearly state what is being tested:

**Good:**
- `test_gang_schedule_fails_with_insufficient_nodes`
- `test_topology_scorer_prefers_same_switch`
- `test_job_lifecycle_from_pending_to_succeeded`
- `test_reconciler_creates_headless_service`

**Avoid:**
- `test_it`
- `test_works`
- `test1`, `test2`

## Test Infrastructure

### Mock Kubernetes Objects

For unit tests of controller logic, use fixtures from `k8s-openapi`:

```rust
use k8s_openapi::api::core::v1::Node;
use kube_runtime::reflector::ObjectRef;

#[test]
fn test_node_tracking() {
    let node = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {
            "name": "node-0",
            "labels": {
                "kubernetes.io/hostname": "node-0",
                "topology.kubernetes.io/zone": "us-east-1a"
            }
        },
        "status": {
            "allocatable": {
                "cpu": "4",
                "memory": "16Gi"
            }
        }
    })).expect("parse node fixture");

    // Test node tracking logic
    assert_eq!(node.metadata.name, Some("node-0".to_string()));
}
```

### Test Isolation

Integration tests use:
- Isolated kubeconfig: `$KUBECONFIG` environment variable points to test cluster only
- Isolated namespace: test jobs run in `$TEST_NAMESPACE` (default: `default`)
- Isolated cluster: `kind` cluster named `wren-test` (configurable)

This prevents tests from interfering with the user's default Kubernetes context.

## Debugging Failed Tests

### Unit Tests

Use `--nocapture` to see print output and run with `--test-threads=1`:

```bash
cargo test --package wren-scheduler test_feature_happy_path -- --nocapture --test-threads=1
```

Add debug logging in tests:

```rust
#[test]
fn test_complex_logic() {
    let result = some_function();
    println!("Result: {:#?}", result);  // Will be shown with --nocapture
    assert!(result.is_ok());
}
```

### Integration Tests

Logs are saved to `/tmp/wren-integration-logs/`:

```bash
# View controller logs
cat /tmp/wren-integration-logs/controller.log

# View test script output
cat /tmp/wren-integration-logs/integration-test.log

# View specific job logs
kubectl logs -n wren-system -l app.kubernetes.io/name=wren-controller --all-containers
```

Keep the kind cluster for debugging:

```bash
scripts/run-integration-tests.sh --no-cleanup
```

Then manually inspect resources:

```bash
# List all WrenJobs
kubectl get wrenjobs -A

# Describe a specific job
kubectl describe wrenjob my-job

# View pod logs
kubectl logs pod/my-job-worker-0

# View all resources created by Wren
kubectl get all -l app.kubernetes.io/managed-by=wren
```

Delete the cluster when done:

```bash
kind delete cluster --name wren-test
```

## Coverage

Generate code coverage with tarpaulin:

```bash
cargo install cargo-tarpaulin
cargo tarpaulin --config tarpaulin.toml
```

View coverage report:

```bash
# Text report
cargo tarpaulin --config tarpaulin.toml --out Stdout

# HTML report (opens in browser)
cargo tarpaulin --config tarpaulin.toml --out Html
open tarpaulin-report.html
```

Current coverage target: **70%+ for controller and scheduler crates**.

## CI/CD Test Pipeline

All tests run automatically in CI:

1. **Format check** — `cargo fmt --all`
2. **Lint (Linux target)** — `cargo clippy --target x86_64-unknown-linux-gnu`
3. **Unit tests** — `cargo test --workspace`
4. **Coverage** — `cargo tarpaulin` (reported to Codecov)
5. **Integration tests** — Full suite on kind cluster
6. **Helm lint** — `helm lint charts/wren`
7. **Security audit** — `cargo audit`

Failures block PR merge. All checks must pass.

## Performance Testing

For changes to scheduling algorithms, measure performance:

```bash
# Benchmark scheduling with various job counts
cargo bench --package wren-scheduler

# Or manually time a large scheduling operation
time cargo run --release --package wren-scheduler -- --jobs 1000 --nodes 100
```

Include performance metrics in PR description if changes affect hot paths.

## Next Steps

- **[Contributing Guide](contributing.md)** — Development workflow and conventions
- **[Releasing Guide](releasing.md)** — Version bumping and release process
- **[Architecture](../operator-guide/architecture.md)** — System design and components
