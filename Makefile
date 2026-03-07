# Wren — Development Makefile
#
# Primary entry point for all build, test, and CI workflows.
# Run `make help` to see available targets.

LINUX_TARGET := x86_64-unknown-linux-musl
DOCKER_IMAGE := wren-controller
CONTROLLER_BIN := wren-controller
CLI_BIN := wren

.PHONY: help build build-release cli cli-release cli-install controller controller-release \
        fmt clippy check-linux test test-unit test-cli test-scheduler test-core \
        coverage ci docker helm-lint helm-template integration integration-quick \
        quickstart crds clean clean-all

.DEFAULT_GOAL := help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

build: ## Build all crates (debug)
	cargo build --workspace

build-release: ## Build all crates (release)
	cargo build --workspace --release

cli: ## Build the wren CLI (debug)
	cargo build --bin $(CLI_BIN)

cli-release: ## Build the wren CLI (release)
	cargo build --release --bin $(CLI_BIN)

cli-install: cli-release ## Build and install the wren CLI to ~/.cargo/bin
	cp target/release/$(CLI_BIN) ~/.cargo/bin/wren

controller: ## Build the controller binary (debug)
	cargo build --bin $(CONTROLLER_BIN)

controller-release: ## Build the controller binary (release)
	cargo build --release --bin $(CONTROLLER_BIN)

# ---------------------------------------------------------------------------
# Quality checks
# ---------------------------------------------------------------------------

fmt: ## Check formatting (fails if unformatted)
	cargo fmt --all -- --check

clippy: ## Run clippy with warnings as errors
	cargo clippy --workspace --all-targets -- -D warnings

check-linux: ## Cross-check for Linux musl target (requires target installed)
	cargo check --workspace --target $(LINUX_TARGET)

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

test: ## Run all tests
	cargo test --workspace

test-unit: ## Run unit tests only (no integration)
	cargo test --workspace --lib

test-cli: ## Run only CLI tests
	cargo test -p wren-cli

test-scheduler: ## Run only scheduler tests
	cargo test -p wren-scheduler

test-core: ## Run only core tests
	cargo test -p wren-core

# ---------------------------------------------------------------------------
# Coverage
# ---------------------------------------------------------------------------

coverage: ## Run coverage via tarpaulin in Docker (mirrors CI)
	docker run --rm \
		-v "$(CURDIR):/volume" \
		-w /volume \
		--security-opt seccomp=unconfined \
		xd009642/tarpaulin:latest \
		cargo tarpaulin --config tarpaulin.toml

# ---------------------------------------------------------------------------
# CI — run everything GitHub Actions runs (except integration)
# ---------------------------------------------------------------------------

ci: fmt clippy check-linux test coverage ## Full CI-equivalent check
	@echo ""
	@echo "All CI checks passed."

# ---------------------------------------------------------------------------
# Docker
# ---------------------------------------------------------------------------

docker: ## Build controller Docker image (dev tag)
	docker build -f docker/Dockerfile.controller -t $(DOCKER_IMAGE):dev .

# ---------------------------------------------------------------------------
# Helm
# ---------------------------------------------------------------------------

helm-lint: ## Lint the Helm chart
	helm lint charts/wren

helm-template: ## Render Helm chart templates locally (dry-run)
	helm template wren charts/wren

# ---------------------------------------------------------------------------
# Integration tests (requires Docker + kind)
# ---------------------------------------------------------------------------

integration: ## Run K8s integration tests (kind cluster)
	./scripts/run-integration-tests.sh

integration-quick: ## Run integration tests (skip cluster creation)
	SKIP_CLUSTER_CREATE=1 ./scripts/run-integration-tests.sh

quickstart: ## Run quickstart script (spins up kind cluster + examples)
	./examples/quickstart.sh

# ---------------------------------------------------------------------------
# CRD generation
# ---------------------------------------------------------------------------

crds: ## Regenerate CRD manifests from Rust types
	cargo run -p wren-core --bin crd-gen -- all

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

clean: ## Remove build artifacts
	cargo clean

clean-all: clean ## Remove build artifacts and Docker images
	docker rmi $(DOCKER_IMAGE):dev 2>/dev/null || true

clean-cluster: ## Delete the kind test cluster
	./examples/quickstart.sh --cleanup
