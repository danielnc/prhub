# prhub

**One GitHub poller for many coding agents.**

[![CI](https://github.com/danielnc/prhub/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/danielnc/prhub/actions/workflows/ci.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/danielnc/prhub/badge)](https://scorecard.dev/viewer/?uri=github.com/danielnc/prhub)
[![PyPI](https://img.shields.io/pypi/v/prhub)](https://pypi.org/project/prhub/)
[![Python versions](https://img.shields.io/pypi/pyversions/prhub)](https://pypi.org/project/prhub/)
[![Codecov](https://codecov.io/gh/danielnc/prhub/graph/badge.svg)](https://codecov.io/gh/danielnc/prhub)
[![CodeRabbit reviews](https://img.shields.io/coderabbit/prs/github/danielnc/prhub?label=CodeRabbit%20reviews)](https://coderabbit.ai)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

Coding agents (Claude Code, Codex, Cursor, …) spend a lot of their time waiting: for CI on the PR they just pushed,
for a review, for a merge, for the deploy. Left alone, each one waits the same way, with its own polling loop:

```sh
gh pr checks 2814 --watch
until gh pr view 2814 --json state | grep -q MERGED; do sleep 30; done
gh run watch 123456
```

Every one of those loops spends the same budget: the **5,000 requests per hour** of the one GitHub token they all
share. With a dozen agents the budget is gone within the hour, and then *every* `gh` command fails for everyone,
you included, until it resets. On the author's machine, a single agent's monitoring loop once made about 190
GraphQL calls a minute and emptied the shared budget in under half an hour.

prhub replaces all of those loops with **one daemon per machine**. Once a minute it reads every PR and commit
anyone is waiting on in a single batched GraphQL request, and writes the state to local files. Agents block on
those files with `prhub wait`, which never calls GitHub.

```sh
prhub wait api#2814 --until done                  # open, approved at the current head, CI green, mergeable
prhub wait api#2814 --until checks --timeout 1800 # CI finished, whatever the result
prhub wait acme/api@3a1e48e --until deployed      # this commit is live in production
prhub state https://github.com/acme/api/pull/42   # current state, instantly
prhub list --ready                                # your PRs that are ready to merge, right now
prhub status                                      # daemon health and the GitHub budget left
```

## How it works

```
                 ┌──────────────── one machine ────────────────┐
  GitHub API ◄── │ prhub daemon ──► ~/.local/state/prhub/*.json │ ◄── prhub wait / state / list
  (1 batched     │   every 60 s                                 │     (any number of agents,
   query/min)    └──────────────────────────────────────────────┘      no API calls)
```

- **Discovery.** Every few minutes the daemon runs one search for your open PRs (`is:pr is:open author:@me`, or
  your own query), so agents don't have to register anything.
- **Polling.** Every minute, batched GraphQL requests read every open tracked PR (20 per request) and every watched
  commit (5 per request), together with each repo's recent deployments. Settled items are read less often: a merged
  or closed PR every 10 minutes and a commit whose runs have finished every 5, unless someone asks about them. With 30 to 40 open PRs that costs about 1 point per poll on the
  author's machine, roughly 60 to 120 points an hour, however many agents are waiting.
- **Waiting.** `prhub wait` tracks whatever it is asked about, nudges the daemon for an early poll (at most one
  every 15 s), then reads local files until the condition holds. Only data fetched *after* the wait started is
  judged, so a stale file never ends a wait.
- **Backing off.** Below 1,000 points left the daemon slows to every 3 minutes, below 300 to every 10, and when
  GitHub rate-limits it, it waits for the reset. During a rate limit `prhub status` says `PAUSED`, and waits keep
  waiting rather than failing, so agents don't fall back to polling by hand against the same exhausted limit.
- **Riding out GitHub errors.** PRs are read 20 per request and commits 5. When GitHub times out on a request
  (a 502/504, or its "Something went wrong" on a heavy query), the daemon splits the batch and retries, and a PR or
  commit that still fails keeps its last state until the next poll. If GitHub keeps failing, `prhub status` says
  `DEGRADED` and waits keep waiting on the last good state, as during a rate limit.

## Install

Requires Python 3.11+ and the [GitHub CLI](https://cli.github.com), logged in (`gh auth login`). prhub reads the
token from `gh auth token` on every poll, never stores it, and has no other dependencies.

```sh
uv tool install prhub            # or: pipx install prhub
prhub install-launchd            # macOS: run the daemon at login
prhub install-systemd            # Linux: run it as a systemd user service
prhub status                     # "daemon: healthy" within a minute
```

To try it without installing a service, run `prhub daemon` in a terminal. Uninstall: on macOS,
`launchctl bootout gui/$(id -u)/dev.prhub.daemon` and delete `~/Library/LaunchAgents/dev.prhub.daemon.plist`; on
Linux, `systemctl --user disable --now prhub` and delete `~/.config/systemd/user/prhub.service`.

## Pull requests

`prhub wait <pr> --until <condition>[,<condition>…]` returns as soon as any one condition holds.

| condition | met when |
|---|---|
| `done` | open, not a draft, CI SUCCESS, merge state CLEAN (configurable), and an APPROVED review **of the current head** that is newer than the head commit |
| `green` | the CI rollup is SUCCESS and no workflow run on the head is still waiting for its jobs |
| `checks` | CI finished, whatever the result |
| `merged` | the PR was merged |
| `closed` | the PR was merged or closed |
| `review` | a new review arrived after the wait started |
| `change` | anything relevant changed: state, draft, CI, review, merge state, head commit |

`done` deliberately does not trust GitHub's `reviewDecision` alone: it stays APPROVED after a new push unless the
repository dismisses stale reviews, and a force-push with an unchanged patch relabels an old review onto the new
head. `prhub state` marks such a PR `review=APPROVED(stale: no approval of this head)`.

When CI goes red, `state` and `wait` also print the names of the failing checks.

## Commits: CI and deploys

The same daemon watches commits, in any repository, so agents stop polling `gh run watch` and the Actions API.

```sh
prhub wait acme/api@3a1e48e --until green                      # every CI run on the commit passed
prhub wait api@3a1e48e --until green --workflow deploy         # only runs whose name contains "deploy"
prhub wait https://github.com/acme/api/commit/3a1e48e --until deployed --env production
prhub state @3a1e48e                                           # a commit of the clone you are in
```

| condition | met when |
|---|---|
| `checks` | every run on the commit finished, whatever the result |
| `green` | every run passed (exits `3` as soon as one fails) |
| `deployed` | the commit is live in `--env` (default `production`, or `any`). Use the repository's environment name; `production` also matches `prod`/`prd`, `staging` matches `stage`/`stg`, `development` matches `dev`. If the repository never deploys to that name, `wait` says which environments it saw |
| `change` | anything about the commit's runs or deploys changed |

It works the same whatever a repository's workflow names, default branch or release style:

- **CI** is every workflow run on the commit, including one whose jobs GitHub has not created yet, and every other
  check suite that ran something. Suites from apps that never start a run (they sit QUEUED forever) and
  cron-triggered runs (they attach to whatever the branch head is) are left out. A run cancelled because a newer run
  of the same workflow replaced it is not a failure. A run cancelled before any of its jobs started (its
  concurrency group replaced it while it waited) is not a failure either: it shows as `NOT STARTED`. When no other run
  of that workflow started on the commit, `green` can never be met for it, because that workflow never ran there;
  wait on the newer commit or use `--until deployed`.
  A dispatched run (`workflow_dispatch`, `repository_dispatch`) cancelled before it started is left out, like cron.
  Other cancelled runs count as failures and are listed as `name (cancelled)`. A commit with more than 50 check suites stays PENDING, since
  prhub reads only the first 50 and an unread one may still be running.
- **Deployed** reads [GitHub Deployments](https://docs.github.com/en/rest/deployments), plus commit statuses named
  like `deployed/<env>`. A successful deploy of a *later* commit counts when that commit contains yours (asked once
  per new deployment through the compare API), so nightly releases, batched deploys and a cancelled deploy followed
  by a newer one all answer correctly. A workflow makes one Deployment per job that names the environment (build,
  migrate, deploy…), and GitHub marks each one successful when *its* job ends, so a Deployment made by a workflow run
  counts only once that whole run has finished successfully; until then the environment shows `DEPLOYING`. If a
  repository records no Deployments, use
  `--until green --workflow <name of its deploy workflow>`.

A commit is read while someone asks about it: 15 minutes after a `state`, and for as long as a `wait` runs plus 10
minutes after it ends. A poll reads at most 50 commits, those just asked about and those still running first, so a
poll stays short however many commits are watched.

## Naming things

| you type | means |
|---|---|
| `https://github.com/acme/api/pull/42`, `acme/api#42` | a PR |
| `42` | PR 42 of the clone in the current directory |
| `api #42`, `api#42` | a PR, by repo name or nickname (see below) |
| `https://github.com/acme/api/commit/3a1e48e`, `acme/api@3a1e48e` | a commit (7 to 40 hex characters) |
| `api@3a1e48e`, `@3a1e48e` | a commit, by nickname, or in the current clone |

Nicknames come from `[aliases]` in the config, and from the names of the git clones found under `clone_roots`.

## Exit codes

`0` the condition was met · `1` timed out · `2` bad arguments, or no such commit · `3` the condition can never be
met (the PR closed; for a commit, a run failed under `green` or its deploy failed) · `4` the daemon is not running.

## Agent setup

**Tell your agents.** Add something like this to your agents' instructions (`CLAUDE.md`, `AGENTS.md`, …); a
longer version is in [docs/agents.md](docs/agents.md):

```markdown
## Waiting on GitHub: use prhub, never poll
Every agent on this machine shares one GitHub token. To wait for a PR, CI or a deploy, run
`prhub wait <pr or owner/repo@sha> --until <condition>` (in the background if you need to keep working), and use
`prhub state <pr>` or `prhub list --ready` for the current state. These read local files and cost nothing.
Never loop over `gh pr checks`, `gh pr view`, `gh run watch` or `gh api` with `sleep`.
If `prhub status` says PAUSED, that is a GitHub rate limit: keep waiting, don't poll by hand.
```

**Enforce it (Claude Code).** `prhub hook claude` is a `PreToolUse` hook that refuses `gh pr checks --watch`,
`gh run watch`, `watch … gh …` and shell loops that `sleep` around `gh pr`, `gh run` or `gh api`, and tells the
agent which `prhub wait` to use instead. One-off reads and every write pass through untouched, and the hook stands
aside entirely when the daemon is not healthy. Add it to `~/.claude/settings.json`:

```json
{ "hooks": { "PreToolUse": [ { "matcher": "Bash",
    "hooks": [ { "type": "command", "command": "prhub hook claude", "timeout": 5 } ] } ] } }
```

Put `PRHUB_ALLOW=1` anywhere in a command to let one loop through on purpose.

## Configuration

`~/.config/prhub/config.toml` (override the path with `PRHUB_CONFIG`). Every key is optional:

```toml
search = "is:pr is:open author:@me org:acme"  # what the daemon discovers by itself
interval = 60                                 # seconds between polls
discover_every = 180                          # seconds between discovery searches
clone_roots = ["~/code"]                      # clones under here become repo nicknames

[aliases]
api = "acme/api"                              # nickname -> owner/repo, or the name of a local clone
web = "acme/web-app"

[done]
merge_states = ["CLEAN", "HAS_HOOKS"]         # merge states that count as mergeable for `done`
```

Any PR or commit an agent waits on is tracked automatically, even if the search would not find it (someone else's
PR, another organization). Closed PRs drop out an hour after they close.

## Files

Everything lives in `~/.local/state/prhub` (override with `PRHUB_HOME`):

| path | what |
|---|---|
| `prs/<owner>__<repo>__<n>.json` | one record per PR: a summary plus the raw GraphQL fields |
| `commits/<owner>__<repo>__<sha>.json` | one record per watched commit |
| `watch.json`, `commit-watch.json` | what agents asked to track, and who asked (`PRHUB_CALLER`, else a tmux or Herdr pane id) |
| `events.jsonl` | every state change, one JSON line each |
| `runs.json` | deploy runs of later commits already asked about, shared by every commit they may contain |
| `daemon.json`, `daemon.log` | daemon health, the GitHub budget left, and the log |

Other tools can read these too. A dashboard, a merge queue or a status bar can use them without calling GitHub.

## Limits

- github.com only for now; GitHub Enterprise Server is not supported.
- One daemon per machine and per GitHub token. Agents on other machines need their own daemon.
- The state is up to one poll interval old (60 s by default). `prhub state --fresh` asks for a poll first.
- `done` checks the review decision, CI and merge state that GitHub reports. If your merge rules need more (a
  specific reviewer, resolved threads), check those before merging.

## Contributing

Bug reports and pull requests are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues
privately, as described in [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
