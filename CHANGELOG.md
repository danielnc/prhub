# Changelog

All notable changes to prhub are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/),
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- `green` and `done` are no longer met while a workflow on the head is still waiting for its jobs. GitHub creates a
  workflow's check suite at once but its check runs only when the jobs are created, which can take a minute (a
  runner queue, a concurrency group); its rollup ignores a suite with no runs, so a fast workflow that already
  passed made the PR or commit read SUCCESS. An unfinished workflow run now keeps checks PENDING, for PRs and
  commits, and a commit's workflow run that ended before its jobs started counts as that workflow's result. A head
  with more than 50 check suites stays PENDING rather than SUCCESS, since an unread suite may still be running.

## [0.1.2] - 2026-10-12

### Fixed

- GitHub gateway timeouts (502/504, "Something went wrong while executing your query") no longer fail a whole
  poll: PRs are read 20 per request and commits 5, a batch GitHub times out on is split and retried, and a PR or
  commit that still fails keeps its last state until the next poll.
- A slow `gh auth token` no longer crashes the daemon: the token is reused for 10 minutes, and a slow refresh keeps
  the previous one. An unexpected error is logged and retried instead of stopping the daemon.
- While GitHub keeps failing, `prhub status` says `DEGRADED` and `prhub wait` keeps waiting instead of reporting the
  daemon as down, so agents no longer fall back to polling by hand during a GitHub incident.
- A poll that takes minutes (a slow GitHub or an overloaded machine) no longer reads as an outage: health counts a
  poll in progress, and `status` shows it and how long the last one took.

### Changed

- Settled items cost less: a merged or closed PR that is still watched is re-read every 10 minutes and a commit whose
  runs have all finished every 5, instead of every poll. Anything someone asks about since its last read is read on
  the next poll, so a new `wait` still gets fresh data.

## [0.1.1] - 2026-10-12

### Added

- Release provenance: each GitHub release carries the wheel, the sdist and their SLSA build provenance
  (`prhub.intoto.jsonl`). SECURITY.md shows how to verify them.
- Fuzzing with ClusterFuzzLite: the PR/commit name parsers and the hook's command matcher, on every pull request
  that touches them and weekly.

## [0.1.0] - 2026-10-11

First public release.

### Added

- `prhub daemon`: one poller per machine. One batched GraphQL request a minute reads every tracked PR and watched
  commit; a discovery search finds your open PRs every few minutes. It slows down when the GitHub budget runs low
  and waits for the reset when rate-limited.
- `prhub wait` for PRs with `done`, `green`, `checks`, `merged`, `closed`, `review` and `change`. `done` requires an
  APPROVED review of the current head that is newer than the head commit.
- `prhub wait` for commits (`owner/repo@sha`) with `checks`, `green`, `deployed` and `change`, plus `--env` and
  `--workflow`. `deployed` reads GitHub Deployments and `deployed/<env>` statuses, and counts a later deploy that
  contains the commit.
- `prhub state`, `prhub list` (`--ready`, `--all`, `--json`), `prhub watch` / `unwatch` and `prhub status`, which
  says `PAUSED` during a GitHub rate limit.
- `prhub hook claude`: a Claude Code `PreToolUse` hook that refuses GitHub polling loops and points to `prhub wait`.
- `prhub install-launchd` (macOS) and `prhub install-systemd` (Linux) to run the daemon as a service.
- Names for PRs and commits: URLs, `owner/repo#N`, `owner/repo@sha`, nicknames from the config and local clones,
  and bare numbers or `@sha` inside a clone.
