.PHONY: check fmt check-secrets

# rustfmt, clippy and the offline test suite; what CI's test and lint jobs run
check:
	cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked

fmt:
	cargo fmt

# gitleaks over the full history; what CI's gitleaks job runs
check-secrets:
	gitleaks detect --source . --redact --log-opts="--all" --config .gitleaks.toml
