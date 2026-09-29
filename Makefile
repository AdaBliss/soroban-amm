# Common developer tasks. This should track the checks CI actually runs in
# .github/workflows/ci.yml — that workflow is the source of truth; keep this
# file in sync with it rather than the other way around.

WASM_DIR := target/wasm32v1-none/release

SHELL := bash

.PHONY: all help build release-build optimize test test-all fmt lint lint-js check-node-versions check check-docs check-deploy-scripts \
        size size-check doc audit bench deploy e2e clean fuzz-cl

# Bare `make` explains itself instead of building.
.DEFAULT_GOAL := help

help: ## Show this help
	@echo "Available targets:"
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

all: build ## Build release WASM (default target if invoked explicitly)

# Exclusions live in scripts/build_workspace.sh, shared with ci.yml and
# release.yml so all three stay in sync.
build: ## Build every deployable contract for wasm32v1-none
	bash scripts/build_workspace.sh

release-build: build ## Alias for the release build (same as `make build`)

optimize: build ## Optimize every WASM artifact produced by `make build`
	@shopt -s nullglob; \
	wasms=($(WASM_DIR)/*.wasm); \
	if [ $${#wasms[@]} -eq 0 ]; then \
		echo "error: no WASM artifacts found in $(WASM_DIR) — run 'make build' first" >&2; \
		exit 1; \
	fi; \
	for f in "$${wasms[@]}"; do \
		echo "optimizing $$f"; \
		stellar contract optimize --wasm "$$f"; \
	done

test: build ## Build, then run the full workspace test suite
	cargo test --workspace

test-all: ## Run tests for the whole workspace
	cargo test --workspace

fuzz-cl: ## Build the wasm deps amm_fuzz imports, then run the full amm_fuzz suite incl. CL stateful properties
	cargo build --release --target wasm32v1-none -p concentrated_liquidity -p amm -p token
	cargo test -p amm_fuzz --features cl

fmt: ## cargo fmt --all
	cargo fmt --all

lint: ## cargo clippy --all -- -D warnings
	cargo clippy --all -- -D warnings

lint-js: ## ESLint every JS/TS workspace (run `npm ci` at the root and in each workspace first)
	npm run lint

check-node-versions: ## Verify every JS package's @types/node and engines.node match .nvmrc
	node scripts/check_node_versions.mjs

check-docs: ## Verify docs/error-codes.md matches #[contracterror] enums
	bash scripts/check_error_docs.sh

check-deploy-scripts: ## Fail if a deployable contract has no scripts/deploy/ module wired into deploy.sh
	bash scripts/check_deploy_scripts.sh

size: ## Print a WASM size report for all built contracts
	bash scripts/size_report.sh

size-check: ## Fail if any contract WASM exceeds the size limit
	bash scripts/size_report.sh --fail-on-limit

doc: ## Build workspace docs with warnings denied
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace

audit: ## Run a security audit of dependencies (cargo install cargo-audit if missing)
	@command -v cargo-audit >/dev/null 2>&1 || { \
		echo "cargo-audit not found; install it with: cargo install cargo-audit" >&2; \
		exit 1; \
	}
	cargo audit

check: fmt lint test check-docs check-deploy-scripts size-check doc ## Run the checks CI enforces before pushing

bench: ## Run hot-path benchmarks
	cargo run -p benches -- --check

deploy: ## Deploy contracts via scripts/deploy.sh
	bash scripts/deploy.sh

e2e: ## Run the end-to-end test suite
	bash scripts/e2e.sh

clean: ## cargo clean
	cargo clean
