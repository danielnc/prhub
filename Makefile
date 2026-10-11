.PHONY: check check-secrets

# ruff and the offline test suite; what CI's test and lint jobs run
check:
	uv run ruff check . && uv run pytest -q

# gitleaks over the full history; what CI's gitleaks job runs
check-secrets:
	gitleaks detect --source . --redact --log-opts="--all" --config .gitleaks.toml
