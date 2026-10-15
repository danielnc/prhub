"""prhub CLI. Everything except `daemon` reads local files only and never calls GitHub."""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time

from . import __version__, commits, store
from .conditions import CONDITIONS, evaluate, is_done, one_line
from .refs import RefResolver

EXIT_MET, EXIT_TIMEOUT, EXIT_IMPOSSIBLE, EXIT_NO_DAEMON = 0, 1, 3, 4


def caller() -> str:
    """Who is waiting, recorded with each watch: PRHUB_CALLER, else a terminal-multiplexer pane id, else "cli"."""
    for var in ("PRHUB_CALLER", "HERDR_PANE_ID", "TMUX_PANE"):
        if os.environ.get(var):
            return os.environ[var]
    return "cli"


def resolver(cfg: dict) -> RefResolver:
    from .refs import index_clones
    cache = store.home() / "repos.json"
    repos = store.read_json(cache)
    if not repos or time.time() - cache.stat().st_mtime > 6 * 3600:
        repos = index_clones(cfg["clone_roots"])
        store.write_json(cache, repos)
    return RefResolver(cfg.get("aliases") or {}, repos)


def _record(ref):
    rec = store.read_pr(ref)
    return rec, (rec or {}).get("summary")


def _commit_record(ref, fresh: bool):
    """The commit's record, asking the daemon for a poll (<= 90 s) when there is none yet or `fresh`."""
    store.add_commit_watch(ref, by=caller())
    rec = store.read_commit(ref)
    if rec is None or fresh:
        store.nudge()
        start = time.time()
        while time.time() < start + 90:
            rec = store.read_commit(ref)
            if rec and rec["fetched_at"] >= start - 1:
                break
            time.sleep(2)
    return rec


def cmd_state_commit(a, cfg) -> int:
    ref = commits.resolve(a.pr, resolver(cfg))
    rec = _commit_record(ref, a.fresh)
    if rec is None:
        print(f"{ref}: no data yet (is `prhub daemon` running? see `prhub status`)", file=sys.stderr)
        return EXIT_NO_DAEMON
    if rec.get("missing"):
        print(f"{ref}: GitHub has no such commit (typo, or not pushed yet)", file=sys.stderr)
        return 2
    s = rec["summary"]
    if a.json:
        state, failed = commits.rollup(s, a.workflow)
        print(json.dumps({"ref": rec["ref"], "fetched_at": rec["fetched_at"], "checks": state, "failing": failed, **s}))
    else:
        print(commits.one_line(rec["ref"], s, a.workflow) + f"  ({int(time.time() - rec['fetched_at'])}s old)")
    return 0


def cmd_wait_commit(a, cfg) -> int:
    ref = commits.resolve(a.pr, resolver(cfg))
    conds = [c.strip() for c in a.until.split(",")]
    for c in conds:
        if c not in commits.CONDITIONS:
            print(f"unknown condition '{c}' for a commit (use: {', '.join(commits.CONDITIONS)})", file=sys.stderr)
            return 2
    if not store.daemon_healthy():
        print("prhub daemon is not polling (see `prhub status`); start it with `prhub install-launchd` (macOS) or `prhub install-systemd` (Linux)",
              file=sys.stderr)
        return EXIT_NO_DAEMON
    store.add_commit_watch(ref, by=caller(), keep=store.WAIT_KEEP)
    store.nudge()
    try:
        return _wait_commit_loop(a, ref, conds)
    finally:
        store.release_commit_watch(ref)


def _wait_commit_loop(a, ref, conds) -> int:
    start = time.time()
    baseline = None
    last_line = None
    hint = None
    next_touch = start + store.WAIT_RENEW
    while time.time() < start + a.timeout:
        rec = store.read_commit(ref)
        if rec and rec["fetched_at"] >= start - 1:
            if rec.get("missing"):
                print(f"{ref}: GitHub has no such commit (typo, or not pushed yet)", file=sys.stderr)
                return 2
            s = rec["summary"]
            baseline = baseline or s
            if "deployed" in conds and hint is None:
                hint = commits.env_hint(s, a.env) or ""
                if hint:
                    print(f"note: {hint}", file=sys.stderr)
            line = commits.one_line(rec["ref"], s, a.workflow)
            for c in conds:
                met, dead = commits.evaluate(c, s, baseline, a.env, a.workflow)
                if met:
                    print(f"{c}: {line}")
                    return EXIT_MET
                if dead:
                    print(f"cannot reach {c} ({dead}): {line}")
                    return EXIT_IMPOSSIBLE
            last_line = line
        if time.time() > next_touch:  # a running wait keeps its commit watch from expiring
            store.add_commit_watch(ref, by=caller(), keep=store.WAIT_KEEP)
            next_touch = time.time() + store.WAIT_RENEW
        if not store.daemon_healthy(max_age=max(300, 3 * store.daemon_state().get("interval", 60))):
            print("prhub daemon stopped polling while waiting", file=sys.stderr)
            return EXIT_NO_DAEMON
        time.sleep(a.poll)
    print("timeout: " + (last_line or f"{ref}: no fresh data") + (f"  ({hint})" if hint else ""))
    return EXIT_TIMEOUT


def cmd_state(a, cfg) -> int:
    if commits.is_commit_text(a.pr):
        return cmd_state_commit(a, cfg)
    ref = resolver(cfg).resolve(a.pr)
    store.add_watch(ref)
    rec, s = _record(ref)
    if rec is None or a.fresh:
        store.nudge()
        deadline = time.time() + 90
        start = time.time()
        while time.time() < deadline:
            rec, s = _record(ref)
            if rec and rec["fetched_at"] >= start - 1:
                break
            time.sleep(2)
    if rec is None:
        print(f"{ref}: no data yet (is `prhub daemon` running? see `prhub status`)", file=sys.stderr)
        return EXIT_NO_DAEMON
    if a.json:
        print(json.dumps({"ref": rec["ref"], "fetched_at": rec["fetched_at"], **s, "failing": rec.get("failing", [])}))
    else:
        age = int(time.time() - rec["fetched_at"])
        print(one_line(rec["ref"], s, rec.get("failing")) + f"  ({age}s old)")
    return 0


def cmd_wait(a, cfg) -> int:
    if commits.is_commit_text(a.pr):
        return cmd_wait_commit(a, cfg)
    ref = resolver(cfg).resolve(a.pr)
    conds = [c.strip() for c in a.until.split(",")]
    for c in conds:
        if c not in CONDITIONS:
            print(f"unknown condition '{c}' (use: {', '.join(CONDITIONS)})", file=sys.stderr)
            return 2
    if not store.daemon_healthy():
        print("prhub daemon is not polling (see `prhub status`); start it with `prhub install-launchd` (macOS) or `prhub install-systemd` (Linux)",
              file=sys.stderr)
        return EXIT_NO_DAEMON
    store.add_watch(ref, by=caller())
    store.nudge()
    start = time.time()
    deadline = start + a.timeout
    merge_states = cfg["done"]["merge_states"]
    baseline = None
    last_line = None
    while time.time() < deadline:
        rec, s = _record(ref)
        if rec and rec["fetched_at"] >= start - 1:  # only judge data fetched after we started waiting
            if baseline is None:
                baseline = s
            for c in conds:
                met, impossible = evaluate(c, s, baseline, merge_states)
                if met:
                    print(f"{c}: " + one_line(rec["ref"], s, rec.get("failing")))
                    return EXIT_MET
                if impossible:
                    print(f"cannot reach {c}: " + one_line(rec["ref"], s, rec.get("failing")))
                    return EXIT_IMPOSSIBLE
            last_line = one_line(rec["ref"], s, rec.get("failing"))
        if not store.daemon_healthy(max_age=max(300, 3 * store.daemon_state().get("interval", 60))):
            print("prhub daemon stopped polling while waiting", file=sys.stderr)
            return EXIT_NO_DAEMON
        time.sleep(a.poll)
    print("timeout: " + (last_line or f"{ref}: no fresh data"))
    return EXIT_TIMEOUT


def cmd_watch(a, cfg) -> int:
    r = resolver(cfg)
    for p in a.pr:
        ref = r.resolve(p)
        store.add_watch(ref)
        print(f"watching {ref}")
    store.nudge()
    return 0


def cmd_unwatch(a, cfg) -> int:
    r = resolver(cfg)
    w = store.watches()
    for p in a.pr:
        w.pop(str(r.resolve(p)), None)
    store.save_watches(w)
    return 0


def cmd_list(a, cfg) -> int:
    merge_states = cfg["done"]["merge_states"]
    rows = []
    for rec in store.all_prs():
        s = rec.get("summary") or {}
        if a.ready and not is_done(s, merge_states):
            continue
        if not a.all and s.get("state") != "OPEN":
            continue
        rows.append(rec)
    if a.json:
        print(json.dumps([{"ref": r["ref"], **r["summary"], "failing": r.get("failing", [])} for r in rows]))
    else:
        for r in rows:
            print(one_line(r["ref"], r["summary"], r.get("failing")) + f"  {r['summary'].get('title', '')[:60]}")
    return 0


def cmd_status(a, cfg) -> int:
    d = store.daemon_state()
    healthy = store.daemon_healthy()
    print(f"prhub {__version__}  home {store.home()}")
    if not d:
        print("daemon: never ran")
        return EXIT_NO_DAEMON
    age = int(time.time() - d.get("last_poll", 0))
    rate = d.get("rate") or {}
    if until := store.rate_limited_until(d):
        print(f"daemon: PAUSED by the GitHub rate limit until {time.strftime('%H:%M', time.localtime(until))} "
              f"(pid {d.get('pid')}, alive). `prhub wait` keeps waiting and resumes by itself. Do not poll GitHub "
              f"by hand meanwhile: your calls hit the same exhausted limit.")
    if since := store.degraded_since(d):
        print(f"daemon: DEGRADED since {time.strftime('%H:%M', time.localtime(since))}: GitHub keeps failing its "
              f"requests (5xx or timeouts), and prhub keeps retrying (pid {d.get('pid')}, alive). `prhub wait` keeps "
              f"waiting on the last good state. Do not poll GitHub by hand meanwhile: it is failing for you too.")
    print(f"daemon: {'healthy' if healthy else 'NOT polling'} pid {d.get('pid')}  last poll {age}s ago  "
          f"interval {d.get('interval')}s  tracked {d.get('tracked')}  "
          + (f"polling now ({int(time.time() - since)}s)  " if (since := store.polling_since(d)) else "")
          + (f"last poll took {d['poll_seconds']}s  " if d.get("poll_seconds") else "")
          + f"github budget {rate.get('remaining', '?')} left (reset {rate.get('resetAt', '?')})")
    if d.get("last_error"):
        print(f"last error: {d['last_error']}")
    if d.get("skipped"):
        print(f"last poll skipped {d['skipped']} PRs/commits GitHub could not return; their previous state is kept")
    print(f"watches: {len(store.watches())}  PR files: {len(store.all_prs())}  "
          f"commits watched: {len(store.commit_watches())}")
    return 0 if healthy else EXIT_NO_DAEMON


def cmd_daemon(a, cfg) -> int:
    from .daemon import Daemon
    Daemon().run(once=a.once)
    return 0


# --- Claude Code PreToolUse hook: send agents to `prhub wait` instead of their own GitHub polling loops ---

# A command word starts the line or follows ; & | ( { or a shell keyword, so quoted mentions
# (a grep pattern, a heredoc of notes) do not count. An `rtk` / `rtk proxy` wrapper is seen through.
CMD = r"(?:^|[;&|({]|\b(?:then|do|while|until|if|xargs|time)\b|!)\s*(?:rtk\s+(?:proxy\s+)?)?"
# A loop only polls if it sleeps: `sleep` must be a command, so `time.sleep(…)` in a python -c, a `for`
# inside a script string or a file called watch.json do not count.
LOOP = re.compile(r"(?:^|[;&|({]|\bdo\b)\s*(while|until|for)\b[\s\S]*" + CMD + r"sleep\b|"
                  + CMD + r"watch\s+-n\b", re.M)
GH_READ = re.compile(CMD + r"gh\s+(pr\s+(checks|view|status|list)|run\s+(view|list)|api)\b", re.M)
GH_WATCH = re.compile(CMD + r"(gh\s+(pr\s+checks\b[^\n|;&]*--watch|run\s+watch)\b|"
                      r"watch\b[^\n|;&]*\bgh\s+(pr|run|api)\b)", re.M)


def hook_verdict(command: str) -> str | None:
    """A reason to refuse `command`, or None to let it run."""
    if "PRHUB_ALLOW=1" in command:
        return None
    if GH_WATCH.search(command) or (LOOP.search(command) and GH_READ.search(command)):
        return ("Don't poll GitHub in a loop: every agent shares one rate limit. prhub already watches PRs "
                "for you, at no API cost per agent:\n"
                "  prhub wait <pr> --until done|green|checks|merged|review|change [--timeout 3600]\n"
                "  prhub wait <owner/repo@sha> --until checks|green|deployed [--env production] [--workflow NAME]\n"
                "  prhub state <pr>      (current state, instant)\n"
                "  prhub list --ready    (your PRs ready to merge)\n"
                "<pr> can be a URL, owner/repo#N, a nickname like 'backend #2814', or a number in this repo. "
                "Run it in the background (or ScheduleWakeup) if you need to keep working. "
                "One-off reads like `gh pr view` without a loop are fine.")
    return None


def cmd_hook(a, cfg) -> int:
    try:
        data = json.load(sys.stdin)
    except ValueError:
        return 0
    if data.get("tool_name") != "Bash" or not store.daemon_healthy():  # fail open if prhub isn't running
        return 0
    reason = hook_verdict((data.get("tool_input") or {}).get("command") or "")
    if reason:
        print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse",
                                                 "permissionDecision": "deny",
                                                 "permissionDecisionReason": reason}}))
    return 0


PLIST = """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.prhub.daemon</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path}</string></dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"""

UNIT = """[Unit]
Description=prhub: one GitHub poller for many coding agents
After=network-online.target

[Service]
ExecStart={exe} daemon
Restart=always
RestartSec=30
Environment=PATH={path}
Nice=10

[Install]
WantedBy=default.target
"""


def _service_path(exe: str) -> str:
    """PATH for the service: wherever `gh` and prhub live, then the usual system directories."""
    dirs = [os.path.dirname(shutil.which("gh") or ""), os.path.dirname(exe),
            "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"]
    return ":".join(dict.fromkeys(d for d in dirs if d))


def _exe() -> str:
    return shutil.which("prhub") or os.path.abspath(sys.argv[0])


def cmd_install_launchd(a, cfg) -> int:
    exe = _exe()
    plist = os.path.expanduser("~/Library/LaunchAgents/dev.prhub.daemon.plist")
    log = str(store.home() / "daemon.log")
    os.makedirs(os.path.dirname(plist), exist_ok=True)
    with open(plist, "w") as f:
        f.write(PLIST.format(exe=exe, path=_service_path(exe), log=log))
    uid = os.getuid()
    subprocess.run(["launchctl", "bootout", f"gui/{uid}/dev.prhub.daemon"], capture_output=True)
    r = subprocess.run(["launchctl", "bootstrap", f"gui/{uid}", plist], capture_output=True, text=True)
    print(f"installed {plist} (log {log})" if r.returncode == 0 else f"launchctl: {r.stderr.strip()}")
    return r.returncode


def cmd_install_systemd(a, cfg) -> int:
    exe = _exe()
    unit = os.path.expanduser("~/.config/systemd/user/prhub.service")
    os.makedirs(os.path.dirname(unit), exist_ok=True)
    with open(unit, "w") as f:
        f.write(UNIT.format(exe=exe, path=_service_path(exe)))
    for cmd in (["systemctl", "--user", "daemon-reload"], ["systemctl", "--user", "enable", "--now", "prhub.service"]):
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode:
            print(f"{' '.join(cmd)}: {r.stderr.strip()}", file=sys.stderr)
            return r.returncode
    print(f"installed {unit}; logs: journalctl --user -u prhub. To keep it running while you are logged out: "
          "loginctl enable-linger $USER")
    return 0


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="prhub", description="One GitHub poller for many coding agents.")
    p.add_argument("--version", action="version", version=f"prhub {__version__}")
    sub = p.add_subparsers(dest="cmd", required=True)
    target = "a PR (URL, owner/repo#N, 'nick #N', N) or a commit (owner/repo@sha, 'nick@sha', @sha, commit URL)"

    s = sub.add_parser("state", help="current state of a PR or commit (local, instant)")
    s.add_argument("pr", help=target)
    s.add_argument("--json", action="store_true")
    s.add_argument("--fresh", action="store_true", help="ask the daemon for a poll first (<= 90 s)")
    s.add_argument("--workflow", help="commits: only runs/statuses whose name contains this")

    w = sub.add_parser("wait", help="block until a PR or commit reaches a condition")
    w.add_argument("pr", help=target)
    w.add_argument("--until", required=True, help="PR: " + ", ".join(CONDITIONS) + "; commit: "
                   + ", ".join(commits.CONDITIONS) + " (comma = any)")
    w.add_argument("--env", default="production", help="commits, deployed: environment name as the repo names it, or 'any' (production also matches prod/prd, "
                        "staging stage/stg, development dev)")
    w.add_argument("--workflow", help="commits, checks/green: only runs/statuses whose name contains this")
    w.add_argument("--timeout", type=int, default=3600, help="seconds (default 3600)")
    w.add_argument("--poll", type=float, default=3.0, help="seconds between local reads (default 3)")

    sw = sub.add_parser("watch", help="track PRs the discovery search does not find")
    sw.add_argument("pr", nargs="+")
    su = sub.add_parser("unwatch", help="stop tracking PRs")
    su.add_argument("pr", nargs="+")

    sl = sub.add_parser("list", help="tracked PRs")
    sl.add_argument("--ready", action="store_true", help="only PRs that meet `done`")
    sl.add_argument("--all", action="store_true", help="include merged/closed")
    sl.add_argument("--json", action="store_true")

    sub.add_parser("status", help="daemon health and GitHub budget")
    sd = sub.add_parser("daemon", help="run the poller (usually under launchd or systemd)")
    sd.add_argument("--once", action="store_true", help="poll once and exit")
    sh = sub.add_parser("hook", help="agent hook that refuses GitHub polling loops")
    sh.add_argument("agent", choices=["claude"])
    sub.add_parser("install-launchd", help="run the daemon at login (macOS)")
    sub.add_parser("install-systemd", help="run the daemon as a systemd user service (Linux)")
    return p


HANDLERS = {"state": cmd_state, "wait": cmd_wait, "watch": cmd_watch, "unwatch": cmd_unwatch, "list": cmd_list,
            "status": cmd_status, "daemon": cmd_daemon, "hook": cmd_hook,
            "install-launchd": cmd_install_launchd, "install-systemd": cmd_install_systemd}


def main(argv: list[str] | None = None) -> int:
    a = build_parser().parse_args(argv)
    cfg = store.load_config()
    try:
        return HANDLERS[a.cmd](a, cfg)
    except ValueError as e:
        print(f"prhub: {e}", file=sys.stderr)
        return 2
