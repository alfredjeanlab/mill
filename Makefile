.PHONY: check ci fmt install license coverage outdated smoke smoke-up smoke-down smoke-clean

# Quick checks
#
# Excluded:
#   SKIP `cargo audit`
#   SKIP `cargo deny`
#
check:
	cargo fmt --all
	cargo clippy --all -- -D warnings
	quench check --fix
	cargo build --all
	cargo test --workspace --exclude smoke

# Full pre-release checks
ci:
	cargo fmt --all --check
	cargo clippy --all -- -D warnings
	quench check --fix
	cargo build --all
	cargo test --all
	cargo audit
	cargo deny check licenses bans sources

# Format code
fmt:
	cargo fmt --all

# Build and install mill to ~/.local/bin
install:
	@scripts/install

# Generate coverage report
coverage:
	@scripts/coverage

# Check for outdated dependencies
outdated:
	cargo outdated

# Smoke tests (end-to-end via mill-harness)
smoke:
	@bash harness/mill-harness test

smoke-up:
	@bash harness/mill-harness up

smoke-down:
	@bash harness/mill-harness down

smoke-clean:
	@bash harness/mill-harness clean
