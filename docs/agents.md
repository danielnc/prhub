# Using prhub from coding agents

prhub only helps if the agents actually use it instead of their own polling loops. Two things make that happen:
telling them (instructions they read every session) and enforcing it (a hook that refuses a polling loop and says
what to run instead).

## Instructions to give your agents

Paste this into the file your agents read every session (`CLAUDE.md`, `AGENTS.md`, a Cursor rule, a system
prompt). It is written for the agent, not for you.

```markdown
## Waiting on GitHub: use prhub, never poll

Every agent on this machine shares one GitHub token (5,000 requests an hour). Polling loops empty it for everyone.
One daemon (prhub) reads all PRs and commits once a minute; these commands read its local files and cost nothing:

| Need | Command |
|------|---------|
| Block until a PR reaches a state | `prhub wait <pr> --until done|green|checks|merged|closed|review|change --timeout 3600` |
| Block until a commit's CI or deploy finishes | `prhub wait <owner/repo@sha> --until checks|green|deployed [--env production] [--workflow deploy]` |
| Current state of a PR or commit | `prhub state <pr or owner/repo@sha>` (`--json` for scripts) |
| My PRs that are ready to merge | `prhub list --ready` |
| Is the daemon up? | `prhub status` |

`<pr>` is a URL, `owner/repo#N`, a nickname like `api #42`, or a bare number inside a clone.
Exit codes: 0 met, 1 timeout, 3 can never happen (PR closed, run or deploy failed), 4 daemon down.

- Run long waits in the background if you have other work, and act on the result when it returns.
- Never loop over `gh pr checks`, `gh pr view`, `gh run watch`, `gh run view` or `gh api` with `sleep`, and never
  use `gh pr checks --watch` or `gh run watch`. One-off reads and all writes (`gh pr create`, `gh pr merge`) are fine.
- `PAUSED by the GitHub rate limit` in `prhub status` is not an outage: keep waiting, it resumes by itself at the
  reset, and polling by hand would only hit the same exhausted limit. Only if it says `NOT polling`, check by hand,
  no more often than every 3 minutes.
- `done` already requires an approval of the current head. Still run your own pre-merge checks if your merge
  rules need more than GitHub's review decision, CI and merge state.
```

## Enforcing it with a hook (Claude Code)

`prhub hook claude` reads a Claude Code `PreToolUse` event on stdin and denies a Bash command that polls GitHub:

- `gh pr checks … --watch`, `gh run watch`, and `watch … gh pr|run|api …`;
- a `while`, `until` or `for` loop that runs `sleep` and also calls `gh pr checks|view|status|list`,
  `gh run view|list` or `gh api` (also through an `rtk` / `rtk proxy` wrapper).

The denial message lists the `prhub` commands to use instead. Quoted mentions (a `grep` pattern, a heredoc of notes)
do not count, one-off reads and writes pass through, and the hook allows everything when the daemon is not healthy,
so a broken prhub never blocks your agents. Put `PRHUB_ALLOW=1` in a command to let one loop through on purpose.

```json
{ "hooks": { "PreToolUse": [ { "matcher": "Bash",
    "hooks": [ { "type": "command", "command": "prhub hook claude", "timeout": 5 } ] } ] } }
```

Other agents (Codex, Cursor, …) have no equivalent hook in prhub yet. The instructions above still work there, and
a pull request adding a hook for another agent is welcome.

## Telling waits apart

Each watch records who asked: `PRHUB_CALLER` if set, otherwise a tmux (`TMUX_PANE`) or Herdr (`HERDR_PANE_ID`)
pane id, otherwise `cli`. It is visible in `~/.local/state/prhub/watch.json` and helps when you want to know which
agent is waiting on what. Set `PRHUB_CALLER` in an agent's environment to give it a readable name.

## Reading the state from your own tools

The files under `~/.local/state/prhub` are the API: one JSON record per PR (`prs/`) and per commit (`commits/`),
plus `events.jsonl`, which gets one line per state change. A merge queue, a dashboard or a status line can read
them instead of calling GitHub. `prhub list --json` and `prhub state --json` give the same data in one call.
