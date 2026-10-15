# Changelog

All notable changes to prhub are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/),
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- prhub is now a single Rust binary instead of a Python package. Commands, options, conditions, exit codes, output
  lines and state files are the same, so agents, scripts and an existing `~/.local/state/prhub` keep working; the
  daemon picks up where the Python one left off. Measured on the maintainer's machine 15/10 (about 50 open PRs, 1,696
  PR records): the daemon holds 6.7 MB between polls instead of 36.6 MB, and the Claude Code hook, which runs before
  every Bash command, takes 4.4 ms instead of 58 ms. `uv tool install prhub` and
  `pipx install prhub` install a prebuilt binary wheel for macOS and Linux (x86_64 and aarch64), and every release
  also carries standalone binaries with SLSA provenance.
- `events.jsonl` no longer grows without bound: past 20 MB it moves to `events.jsonl.1`.


## [0.1.6] - 2026-10-15

### Fixed

- Polls no longer grow with every commit anyone ever asked about. A `state` or `wait` on a commit kept it tracked for
  6 hours, and each finished commit was re-read every 5 minutes, so watches piled up and came due together: seen on
  the maintainer's machine 15/10 with about 60 open PRs, 198 watched commits (170 not asked about for over an hour)
  made every fifth-minute poll read about 190 commits and take 7 to 13 minutes, and waits went that long without fresh
  data. Now a `state` keeps a commit tracked for 15 minutes, a running `wait` renews its claim every 5 minutes and
  lets it lapse 10 minutes after it ends, and a poll reads at most 50 commits: those never read or just asked about
  first, then those still running, then the stalest finished ones. Watches written by an older prhub expire an hour
  after they were last asked about, instead of six. Updates to `commit-watch.json` take a lock, and a commit GitHub
  keeps failing on is skipped for 5 minutes so it cannot crowd out the rest.

## [0.1.5] - 2026-10-14

### Fixed

- `deployed` matches an environment by its usual long and short forms when the repository does not use the exact name:
  `production` also matches `prod`/`prd`, `staging` matches `stage`/`stg`, `development` matches `dev`. A repository
  whose environment is `prod` made `--until deployed` (default `--env production`) wait until its timeout. When the
  repository deploys but never to the requested environment, `wait` now says which environments it saw.

## [0.1.4] - 2026-10-14

### Fixed

- Polls no longer slow down with many watched commits after a deploy. The run behind a later commit's deploy was asked
  once per watched commit it might contain, every poll (46 requests for 11 runs, polls of up to 200 s, seen on the
  maintainer's machine 14/10 with about 100 watched commits and 60 open PRs). It is now asked at most once per poll, shared through
  `runs.json`; a successful run is remembered, and one that ended badly is asked again every 10 minutes.

## [0.1.3] - 2026-10-14

### Fixed

- `green` and `done` are no longer met while a workflow on the head is still waiting for its jobs. GitHub creates a
  workflow's check suite at once but its check runs only when the jobs are created, which can take a minute (a
  runner queue, a concurrency group), and its rollup ignores a suite with no runs, so a fast workflow that already
  passed made the PR or commit read SUCCESS. An unfinished workflow run now keeps checks PENDING, for PRs and
  commits. A head with more than 50 check suites stays PENDING, since an unread suite may still be running.
- A workflow run cancelled before any of its jobs started (replaced in its concurrency group by a newer run while it
  waited) is not a failure. It shows as `NOT STARTED`; `green` reports it can never be met, since that commit's CI
  never ran there, unless another run of the same workflow ran on it. A dispatched run cancelled before it started
  is left out, like a cron run. Other cancelled runs are listed as `name (cancelled)`.
- `deployed` is no longer met while a deploy is still running. A workflow makes one Deployment per job that names
  the environment, and GitHub marks each one successful when its own job ends, so the build job's Deployment read
  successful while the migrate and deploy jobs had not run yet. A Deployment made by a workflow run now counts only
  once that run has finished successfully (`DEPLOYING` until then); for a later commit's deploy, the daemon reads that
  run's state once per poll until it has succeeded (a failed run is asked again, since a re-run keeps its id).
- An HTTP 429 from GitHub always counts as the rate limit (PAUSED), whatever its body says.
- A GitHub response cut short mid-read (`IncompleteRead`, a truncated JSON body) is a transient network error: the
  daemon reports DEGRADED and retries, instead of logging a crash.

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
