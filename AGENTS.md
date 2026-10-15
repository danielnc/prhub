# AGENTS.md

Instructions for coding agents (Claude Code, Codex, and others) working **on** prhub. If you only want to *use*
prhub while waiting on GitHub, read [docs/agents.md](docs/agents.md) instead.

## What this repo is

One Rust binary (`src/`) with a daemon and a CLI that reads the daemon's files. PyPI gets it as a binary wheel
(maturin, `pyproject.toml`), so `uv tool install prhub` works without Rust.

## Commands

```bash
make fmt                   # cargo fmt
make check                 # rustfmt, clippy -D warnings and the offline test suite; what CI runs
make check-secrets         # gitleaks over the full history
PRHUB_HOME=/tmp/prhub-dev cargo run -- daemon --once   # one real poll, isolated from an installed daemon
```

## Rules

- **Few dependencies.** The crates in `Cargo.toml` (clap, serde, regex, toml, ureq, libc) are the whole list; add one
  only with a reason in the pull request.
- **Tests are offline.** Build GraphQL responses by hand in GitHub's shape (see `node()` in `src/commits.rs` and `pr()` in
  `src/conditions.rs`); GitHub itself is a fake behind the `Api` and `Transport` traits.
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
| `src/daemon.rs` | the poll loop: discovery, batched reads, backoff, pruning |
| `src/github.rs` | GraphQL and REST calls (the only module that talks to GitHub) |
| `src/conditions.rs` | PR summary and the PR conditions (`done`, `green`, …) |
| `src/commits.rs` | commit refs, CI rollup, deployments and the commit conditions |
| `src/refs.rs` | turning URLs, `owner/repo#N` and nicknames into PR refs |
| `src/store.rs` | config, state files, watches, daemon health |
| `src/hook.rs` | the Claude Code hook's command matcher |
| `src/cli.rs` | the commands and the service installers |
| `fuzz/`, `.clusterfuzzlite/` | cargo-fuzz targets for the parsers and the hook matcher, and their ClusterFuzzLite build |
| `.github/workflows/publish.yml` | wheels, binaries, SLSA provenance, release assets and PyPI (`docs/releasing.md`) |
