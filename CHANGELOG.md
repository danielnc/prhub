# Changelog

All notable changes to prhub are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/),
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
