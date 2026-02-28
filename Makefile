.PHONY: build build-release fmt clippy check-linux test test-unit coverage ci integration integration-quick clean clean-all help

## Build

build: ## Build all crates (debug)
	cargo build --workspace

build-release: ## Build all crates (release)
	cargo build --workspace --release

## Quality

fmt: ## Check formatting
	cargo fmt --all -- --check

clippy: ## Run clippy lints
	cargo clippy --workspace --all-targets -- -D warnings

check-linux: ## Cross-check for x86_64 Linux (requires target installed)
	cargo check --workspace --target x86_64-unknown-linux-gnu

## Test

test: ## Run all tests
	cargo test --workspace

test-unit: ## Run unit tests only (no integration)
	cargo test --workspace --lib

## Coverage

coverage: ## Run coverage via tarpaulin in Docker (mirrors CI)
	docker run --rm \
		-v "$(CURDIR):/volume" \
		-w /volume \
		--security-opt seccomp=unconfined \
		xd009642/tarpaulin:latest \
		cargo tarpaulin --config tarpaulin.toml

## CI

ci: fmt clippy check-linux test coverage ## Run full CI-equivalent locally

## Integration

integration: ## Run integration tests (requires kind cluster)
	./scripts/run-integration-tests.sh

integration-quick: ## Run integration tests (skip cluster creation)
	SKIP_CLUSTER_CREATE=1 ./scripts/run-integration-tests.sh

## Clean

clean: ## Remove build artifacts
	cargo clean

clean-all: clean ## Remove build artifacts and Docker build cache
	docker rmi bubo-dev 2>/dev/null || true

## Help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

.DEFAULT_GOAL := help
