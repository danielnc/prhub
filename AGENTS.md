# AGENTS.md

Instructions for coding agents (Claude Code, Codex, and others) working **on** prhub. If you only want to *use*
prhub while waiting on GitHub, read [docs/agents.md](docs/agents.md) instead.

## What this repo is

A standard-library-only Python package (`prhub/`) with one daemon and a CLI that reads the daemon's files.

## Commands

```bash
uv sync                    # dev tools only (pytest, pytest-cov, ruff); the runtime needs nothing
uv run pytest -q           # offline test suite; must pass before every commit
uv run ruff check .        # lint; must pass before every commit
PRHUB_HOME=/tmp/prhub-dev uv run python -m prhub daemon --once   # one real poll, isolated from an installed daemon
```

## Rules

- **Runtime is stdlib only.** No new dependencies in `prhub/`.
- **Tests are offline.** Build GraphQL responses by hand in GitHub's shape (see `node()` and `pr()` in the tests).
  Write a failing test first for any behaviour change.
- **Only the daemon calls GitHub.** `state`, `wait`, `list` and the hook read local files. Keep it that way: an extra
  API call in a command that agents run in a loop is exactly the problem prhub exists to remove.
- **Fail closed on readiness.** A condition such as `done` or `deployed` must never be met on missing or ambiguous
  data. Prefer a positive check (an approval *of this head*) over a negation.
- **The hook fails open.** If the daemon is not healthy, `prhub hook claude` must allow everything.
- **No personal data in the repo.** No real names, emails, hostnames, home paths, tokens, or private repository or
  organization names in code, tests, docs or commit messages. Use `acme/api`.
- **Measured numbers are labelled** with whose machine, how many PRs, and when.
- **Git:** work on a branch, small commits with conventional prefixes (`feat:`, `fix:`, `docs:`, `test:`), and never
  force-push shared branches.

## Layout

| Path | What |
|---|---|
| `prhub/daemon.py` | the poll loop: discovery, batched reads, backoff, pruning |
| `prhub/github.py` | GraphQL and REST calls (the only module that talks to GitHub) |
| `prhub/conditions.py` | PR summary and the PR conditions (`done`, `green`, …) |
| `prhub/commits.py` | commit refs, CI rollup, deployments and the commit conditions |
| `prhub/refs.py` | turning URLs, `owner/repo#N` and nicknames into PR refs |
| `prhub/store.py` | config, state files, watches, daemon health |
| `prhub/cli.py` | the commands, the Claude Code hook and the service installers |
