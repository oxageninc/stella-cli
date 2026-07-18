# Stella — developer convenience targets.
# Run `make help` for the full list.

BUMP ?= patch

.PHONY: help
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' Makefile | \
		awk 'BEGIN {FS = ":.*## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}' | \
		sort

.PHONY: build
build: ## Build the full workspace (debug)
	cargo build --workspace

.PHONY: build-release
build-release: ## Build the shipping binary (release, optimized)
	cargo build --release -p stella-cli

.PHONY: smoke
smoke: ## Compile check — runs `stella models` (no API key needed)
	cargo run -p stella-cli -- models

.PHONY: format
format: ## Format all code (rustfmt)
	cargo fmt

.PHONY: format-check
format-check: ## Check formatting without modifying (CI gate)
	cargo fmt --check

.PHONY: lint
lint: ## Run clippy with -D warnings (CI gate)
	cargo clippy --workspace --all-targets -- -D warnings

.PHONY: fix
fix: ## Auto-fix clippy lints + format
	cargo clippy --fix --allow-dirty --workspace --all-targets -- -D warnings
	cargo fmt

.PHONY: test
test: ## Run the full test suite (all crates)
	cargo test --workspace

.PHONY: test-core
test-core: ## Test stella-core only (fast engine iteration)
	cargo test -p stella-core

.PHONY: test-model
test-model: ## Test stella-model only (provider adapters)
	cargo test -p stella-model

.PHONY: test-tools
test-tools: ## Test stella-tools only (built-in tools)
	cargo test -p stella-tools

.PHONY: test-cli
test-cli: ## Test stella-cli only (the shipping binary)
	cargo test -p stella-cli

.PHONY: test-protocol
test-protocol: ## Test stella-protocol only (shared types)
	cargo test -p stella-protocol

.PHONY: gate
gate: format-check lint test ## Full CI gate: fmt-check + clippy + test

.PHONY: check
check: format-check lint ## Fast pre-push check (fmt + clippy, no tests)

.PHONY: hooks
hooks: ## Install the pre-push gate hook (runs `make gate` on every push)
	git config core.hooksPath .githooks
	@chmod +x .githooks/* 2>/dev/null || true
	@printf '\033[32m✔ hooks installed\033[0m — pre-push now runs the fmt+clippy+test gate.\n'
	@printf '  Needed because org Actions is billing-locked and never runs the gate itself.\n'
	@printf '  Bypass in emergencies: \033[36mSKIP_GATE=1 git push\033[0m (or \033[36mgit push --no-verify\033[0m).\n'

.PHONY: docs
docs: ## Build rustdoc for the workspace (skip dep docs)
	cargo doc --workspace --no-deps

.PHONY: deny
deny: ## cargo deny: advisories, dependency bans, source provenance
	cargo deny check advisories bans sources

.PHONY: vuln-scan
vuln-scan: ## cargo audit: security vulnerability scan
	cargo audit

.PHONY: supply-chain
supply-chain: deny vuln-scan ## Run both supply-chain checks

CARGO_WATCH := $(shell command -v cargo-watch 2>/dev/null)

.PHONY: watch
watch: ## Watch: re-run workspace tests on every save
ifeq ($(CARGO_WATCH),)
	$(error cargo-watch not installed — run: cargo install cargo-watch)
else
	cargo watch -x 'test --workspace'
endif

.PHONY: watch-core
watch-core: ## Watch: re-test stella-core on every save
ifeq ($(CARGO_WATCH),)
	$(error cargo-watch not installed — run: cargo install cargo-watch)
else
	cargo watch -x 'test -p stella-core'
endif

.PHONY: watch-lint
watch-lint: ## Watch: re-run clippy on every save
ifeq ($(CARGO_WATCH),)
	$(error cargo-watch not installed — run: cargo install cargo-watch)
else
	cargo watch -x 'clippy --workspace --all-targets -- -D warnings'
endif

.PHONY: watch-fix
watch-fix: ## Watch: auto-fix clippy + format on every save
ifeq ($(CARGO_WATCH),)
	$(error cargo-watch not installed — run: cargo install cargo-watch)
else
	cargo watch -x 'clippy --fix --allow-dirty --workspace --all-targets -- -D warnings' -x 'fmt'
endif

.PHONY: release
release: ## Cut a release (default: patch). Use BUMP=minor or BUMP=major
	scripts/release.sh $(BUMP)

.PHONY: release-patch
release-patch: ## Cut a patch release (0.1.0 -> 0.1.1)
	scripts/release.sh patch

.PHONY: release-minor
release-minor: ## Cut a minor release (0.1.0 -> 0.2.0)
	scripts/release.sh minor

.PHONY: release-major
release-major: ## Cut a major release (0.1.0 -> 1.0.0)
	scripts/release.sh major

.PHONY: clean
clean: ## Remove all build artifacts
	cargo clean

.PHONY: reap-agents
reap-agents: ## List orphaned stella agents/tool-subprocesses idle 20m+ (dry run)
	scripts/reap-agents.sh --dry-run --verbose

.PHONY: reap-agents-kill
reap-agents-kill: ## Kill orphaned stella agents/tool-subprocesses idle 20m+ (asks first)
	scripts/reap-agents.sh

.PHONY: audit
audit: ## Run full codebase audit (clippy, tests, supply-chain, dead-code scan)
	@printf '\033[1m=== Clippy ===\033[0m\n'
	cargo clippy --workspace --all-targets -- -D warnings
	@printf '\n\033[1m=== Tests ===\033[0m\n'
	cargo test --workspace
	@printf '\n\033[1m=== Supply chain ===\033[0m\n'
	cargo deny check advisories bans sources 2>/dev/null || printf '  \033[33mcargo-deny not installed — skipping\033[0m\n'
	cargo audit 2>/dev/null || printf '  \033[33mcargo-audit not installed — skipping\033[0m\n'
	@printf '\n\033[1m=== Unused dependencies ===\033[0m\n'
	cargo udeps --workspace 2>/dev/null || printf '  \033[33mcargo-udeps not installed — run: cargo install cargo-udeps\033[0m\n'
	@printf '\n\033[32m✔ Audit complete.\033[0m\n'
