# Contributing to Wren

Thank you for your interest in contributing to Wren! This guide covers setting up a development environment, understanding the project structure, and preparing your work for submission.

## Development Environment

### Prerequisites

Ensure you have the following installed:

- **Rust 1.70+** (stable) — Install via [rustup](https://rustup.rs/)
- **Cargo** — Included with Rust
- **Docker** — For building controller images
- **kubectl** — For Kubernetes interaction
- **kind** — For running integration tests locally
- **git** — For version control

### Initial Setup

1. Clone the repository:

```bash
git clone https://github.com/miguelgila/wren.git
cd wren
```

2. Verify Rust toolchain:

```bash
rustup update stable
rustup show
```

3. Install development tools:

```bash
# Code formatter
cargo install rustfmt

# Linter
cargo install clippy

# Test coverage (optional)
cargo install cargo-tarpaulin
```

## Project Structure

Wren is organized as a **Cargo workspace** with four crates:

```
wren/
├── crates/
│   ├── wren-core/           # Shared types, CRDs, traits (no K8s deps)
│   ├── wren-scheduler/      # Pure scheduling algorithms (testable without cluster)
│   ├── wren-controller/     # Kubernetes controller (main binary)
│   └── wren-cli/            # CLI tool (wren submit, queue, cancel, etc.)
├── charts/                  # Helm chart for deployment
├── manifests/               # Kubernetes manifests (CRDs, RBAC, examples)
├── docker/                  # Dockerfile for controller image
├── scripts/                 # Integration test runner and utilities
└── .github/workflows/       # CI/CD pipelines
```

### Crate Responsibilities

**wren-core** — Shared data types and traits
- `crd.rs` — CRD definitions (WrenJob, WrenQueue, WrenUser)
- `types.rs` — Common types (Placement, JobStatus, TopologyConstraint)
- `backend.rs` — ExecutionBackend trait for pluggable execution
- `error.rs` — Error types using `thiserror`
- No Kubernetes dependencies; pure Rust data structures

**wren-scheduler** — Scheduling algorithms
- `gang.rs` — Gang scheduling (all-or-nothing placement)
- `topology.rs` — Topology-aware node scoring
- `queue.rs` — Priority queue management
- `backfill.rs` — Backfill scheduler (Slurm-style)
- `fair_share.rs` — Fair-share priority adjustment
- `dependencies.rs` — Job dependencies and arrays
- No Kubernetes dependencies; testable as pure functions

**wren-controller** — Kubernetes controller
- `main.rs` — Entry point and controller setup
- `reconciler.rs` — WrenJob reconciliation loop
- `container.rs` — Container execution backend (Pods, Services)
- `reaper.rs` — Reaper execution backend
- `node_watcher.rs` — Node topology discovery
- `webhook.rs` — Admission webhook validation
- `metrics.rs` — Prometheus metrics endpoint

**wren-cli** — Command-line interface
- `main.rs` — CLI argument parsing
- `submit.rs` — `wren submit <job.yaml>`
- `queue.rs` — `wren queue` (list jobs)
- `cancel.rs` — `wren cancel <job-id>`
- `status.rs` — `wren status <job-id>`
- `logs.rs` — `wren logs <job-id> [--rank N]`

## Coding Conventions

### Error Handling

- Use `thiserror` for custom error types in libraries
- Use `anyhow` only in binaries (main.rs) for easier error chaining
- All error types must be `Clone + Debug`

Example:

```rust
use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum SchedulerError {
    #[error("Insufficient resources: need {needed} nodes, available {available}")]
    InsufficientResources { needed: usize, available: usize },

    #[error("Invalid topology constraint: {reason}")]
    TopologyError { reason: String },
}
```

### Logging

Use `tracing` for all logging — never use `println!` or `eprintln!`:

```rust
use tracing::{debug, info, warn, error, span, Level};

debug!("Scheduling job: {}", job.name);
info!(job_name = %job.name, nodes = job.spec.nodes, "Job scheduled");
warn!("Job exceeded walltime limit");
error!(error = ?err, "Failed to reconcile job");

// For async functions, use spans:
let span = span!(Level::DEBUG, "reconcile_job", job_name = %job.name);
async {
    info!("Reconciliation started");
}.instrument(span).await
```

### Serialization

All public API types must derive `Serialize`, `Deserialize`, `Clone`, and `Debug`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JobStatus {
    pub state: JobState,
    pub message: Option<String>,
}
```

CRD types additionally derive `JsonSchema` and `CustomResource`:

```rust
use kube::CustomResource;
use schemars::JsonSchema;

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(group = "wren.giar.dev", version = "v1alpha1", kind = "WrenJob", namespaced)]
pub struct WrenJobSpec {
    pub queue: String,
    pub nodes: usize,
    // ...
}
```

### Async Code

- Use `tokio` runtime (configured in main.rs)
- Use `async-trait` for async trait methods
- All futures should be `.await`-ed; avoid returning raw futures
- Prefer `spawn_blocking` for CPU-intensive work instead of blocking the runtime

```rust
use async_trait::async_trait;

#[async_trait]
pub trait ExecutionBackend {
    async fn launch(&self, job: &WrenJob) -> Result<()>;
    async fn status(&self, job: &WrenJob) -> Result<JobStatus>;
}
```

### Testing

- Unit tests live in each module with `#[cfg(test)]` sections
- Integration tests in `tests/` directories
- Use descriptive test names: `test_gang_schedule_fails_with_insufficient_nodes`
- Mock Kubernetes API responses using fixtures from `k8s-openapi`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gang_scheduler_all_or_nothing() {
        let nodes = vec![
            Node::new("node-0", 4),
            Node::new("node-1", 4),
        ];
        let job = WrenJob::new("job-1", 3); // Need 3 nodes, only 2 available

        let result = gang_schedule(&job, &nodes);
        assert!(result.is_err());
    }
}
```

## Pre-Push Checklist

Before pushing code or requesting review, verify the following locally:

### 1. Code Formatting

```bash
cargo fmt --all
```

This must pass without warnings.

### 2. Linting — Linux Target Required

**Important:** macOS clippy may miss errors that appear in Linux. Always test with the Linux target:

```bash
cargo clippy --target x86_64-unknown-linux-gnu --all-targets -- -D warnings
```

And also on your native platform (macOS):

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Both must pass with zero warnings. The `-D warnings` flag promotes warnings to errors.

### 3. Unit Tests

```bash
cargo test --workspace
```

All tests must pass. Run with `--nocapture` to see output:

```bash
cargo test --workspace -- --nocapture
```

### 4. Integration Tests

```bash
scripts/run-integration-tests.sh
```

This creates a local `kind` cluster and runs full integration tests (33 tests total).
Requires Docker and kind to be installed and running.

### 5. Helm Chart

```bash
helm lint charts/wren
```

The Helm chart must lint without errors.

## Commit Message Format

Wren follows **Conventional Commits** format. This enables automated changelog generation and versioning.

```
<type>(<scope>): <subject>

<body>

<footer>
```

### Types

- `feat` — A new feature
- `fix` — A bug fix
- `test` — Adding or updating tests
- `refactor` — Code restructuring without behavior change
- `docs` — Documentation updates
- `chore` — Build, CI, dependencies, or tooling changes
- `perf` — Performance improvements

### Scope (optional but encouraged)

Specify which component is affected: `scheduler`, `controller`, `cli`, `core`, `helm`, etc.

### Examples

```
feat(scheduler): add topology-aware node scoring

Implement switch-group and hop-distance calculation for multi-node job placement.
Scores improve for nodes in the same switch group and penalize inter-rack hops.

Closes #42
```

```
fix(controller): handle missing WrenUser CRD gracefully

Jobs with invalid user annotations now fail at scheduling instead of crashing
the reconciler.
```

```
test(core): add 15 unit tests for fair-share calculation

Coverage improved from 42% to 68% for fair_share.rs.
```

## Pull Request Process

1. **Create a feature branch** from `main`:

```bash
git checkout -b feat/topology-scoring
```

2. **Make your changes** and commit using Conventional Commits format.

3. **Run pre-push checks locally**:

```bash
# Format
cargo fmt --all

# Lint on both platforms
cargo clippy --target x86_64-unknown-linux-gnu --all-targets -- -D warnings
cargo clippy --workspace --all-targets -- -D warnings

# Test
cargo test --workspace

# Integration tests (if you modified scheduler or controller logic)
scripts/run-integration-tests.sh
```

4. **Push to your fork** and open a pull request against `main`.

5. **CI runs automatically**:
   - Format check (`cargo fmt`)
   - Lint on Linux target (`cargo clippy`)
   - Unit tests (`cargo test`)
   - Security audit (`cargo audit`)
   - Docker build
   - Helm lint

6. **Merge requirements**:
   - All CI checks pass
   - Code review approval
   - No merge conflicts

7. **Post-merge**:
   - Auto-release workflow triggers
   - Patch version bump (e.g., 0.1.2 → 0.1.3)
   - CHANGELOG updated with your commits
   - Release tag created
   - GitHub Release created with static binaries

## Code Review Guidelines

When reviewing PRs or requesting review:

- **Is the code change necessary?** Does it fix a real problem or add value?
- **Does it follow conventions?** Format, error handling, async patterns, naming?
- **Are there tests?** New features should have unit tests; bug fixes should have regression tests.
- **Does it impact performance?** Scheduling algorithms especially — measure if changed.
- **Is it documented?** Comments for non-obvious logic; CHANGELOG if user-facing.
- **Does it break anything?** Check for changes to public API types, CRD fields, etc.

## Reporting Issues

Found a bug? Please open a GitHub issue with:

- **Reproduction steps** — minimal example to trigger the bug
- **Expected behavior** — what should happen
- **Actual behavior** — what actually happens
- **Environment** — Kubernetes version, Wren version, cluster setup
- **Logs** — controller logs and job logs if relevant

## Questions?

- Check the [Architecture](../operator-guide/architecture.md) guide for design decisions
- Read the [CRD Reference](../reference/crds.md) for data structure details
- Review existing code in the crate you're working on — context is there
- Open a discussion issue if you have design questions

Thank you for contributing to Wren!
