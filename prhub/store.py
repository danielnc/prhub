"""Local state shared by the daemon (writer) and every CLI call (readers). Plain JSON files, atomic writes."""
from __future__ import annotations

import json
import os
import time
import tomllib
from pathlib import Path

from .refs import PRRef


def home() -> Path:
    p = Path(os.environ.get("PRHUB_HOME") or os.path.expanduser("~/.local/state/prhub"))
    (p / "prs").mkdir(parents=True, exist_ok=True)
    return p


def config_path() -> Path:
    return Path(os.environ.get("PRHUB_CONFIG") or os.path.expanduser("~/.config/prhub/config.toml"))


DEFAULT_CONFIG = {
    "search": "is:pr is:open author:@me",  # what the daemon discovers on its own
    "interval": 60,                          # seconds between polls
    "discover_every": 180,                   # seconds between discovery searches
    "clone_roots": [],                       # folders whose git clones become repo nicknames, e.g. ["~/code"]
    "aliases": {},                           # nickname -> repo name or owner/repo
    "done": {"merge_states": ["CLEAN", "HAS_HOOKS"]},
}


def load_config() -> dict:
    cfg = json.loads(json.dumps(DEFAULT_CONFIG))
    p = config_path()
    if p.exists():
        with open(p, "rb") as f:
            user = tomllib.load(f)
        for k, v in user.items():
            if isinstance(v, dict) and isinstance(cfg.get(k), dict):
                cfg[k].update(v)
            else:
                cfg[k] = v
    return cfg


def write_json(path: Path, data) -> None:
    tmp = path.with_suffix(path.suffix + f".{os.getpid()}.tmp")
    with open(tmp, "w") as f:
        json.dump(data, f, indent=1, sort_keys=True)
    os.replace(tmp, path)


def read_json(path: Path, default=None):
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


def pr_path(ref: PRRef) -> Path:
    return home() / "prs" / f"{ref.slug}.json"


def read_pr(ref: PRRef) -> dict | None:
    return read_json(pr_path(ref))


def write_pr(ref: PRRef, record: dict) -> None:
    write_json(pr_path(ref), record)


def all_prs() -> list[dict]:
    out = []
    for p in sorted((home() / "prs").glob("*.json")):
        rec = read_json(p)
        if rec:
            out.append(rec)
    return out


def watches() -> dict[str, dict]:
    return read_json(home() / "watch.json", {}) or {}


def add_watch(ref: PRRef, by: str = "cli") -> None:
    w = watches()
    w.setdefault(str(ref), {"added_at": int(time.time()), "by": by})
    w[str(ref)]["last_asked"] = int(time.time())  # the daemon reads anything asked about since its last read
    write_json(home() / "watch.json", w)


def save_watches(w: dict) -> None:
    write_json(home() / "watch.json", w)


# Commits (`owner/repo@sha`) have their own watch file and records: a commit is only interesting for a while (until
# its CI and deploy finish), so these expire instead of following a PR's open/closed life.

def commit_path(ref) -> Path:
    p = home() / "commits"
    p.mkdir(exist_ok=True)
    return p / f"{ref.slug}.json"


def read_commit(ref) -> dict | None:
    return read_json(commit_path(ref))


def write_commit(ref, record: dict) -> None:
    write_json(commit_path(ref), record)


def commit_watches() -> dict[str, dict]:
    return read_json(home() / "commit-watch.json", {}) or {}


def add_commit_watch(ref, by: str = "cli") -> None:
    w = commit_watches()
    w.setdefault(str(ref), {"added_at": int(time.time()), "by": by})
    w[str(ref)]["last_asked"] = int(time.time())
    write_json(home() / "commit-watch.json", w)


def save_commit_watches(w: dict) -> None:
    write_json(home() / "commit-watch.json", w)


def append_event(event: dict) -> None:
    with open(home() / "events.jsonl", "a") as f:
        f.write(json.dumps(event, sort_keys=True) + "\n")


def nudge() -> None:
    """Ask the daemon for an early poll (it rate-limits these itself)."""
    (home() / "nudge").touch()


def daemon_state() -> dict:
    return read_json(home() / "daemon.json", {}) or {}


def _pid_alive(pid) -> bool:
    try:
        os.kill(int(pid), 0)
        return True
    except PermissionError:
        return True
    except (ProcessLookupError, TypeError, ValueError):
        return False


def rate_limited_until(d: dict | None = None) -> int | None:
    """When the daemon is waiting out a GitHub rate limit: the epoch it resumes at, else None."""
    d = daemon_state() if d is None else d
    until = d.get("rate_limited_until")
    return until if until and until > time.time() - 120 and _pid_alive(d.get("pid")) else None


def degraded_since(d: dict | None = None) -> int | None:
    """When GitHub keeps failing the daemon's requests transiently (5xx, its query timeouts, network errors) while
    the daemon is alive and still retrying: the epoch the trouble started, else None."""
    d = daemon_state() if d is None else d
    since = d.get("degraded_since")
    retrying = time.time() - d.get("last_poll", 0) < max(300, 3 * (d.get("interval") or 60))
    return since if since and d.get("last_error_transient") and retrying and _pid_alive(d.get("pid")) else None


MAX_POLL_SECONDS = 900  # a poll still running after this long is stuck, not slow


def polling_since(d: dict | None = None) -> int | None:
    """When the alive daemon started the poll it is in the middle of, else None. A poll can take minutes when GitHub
    or the machine is slow, and that is not an outage."""
    d = daemon_state() if d is None else d
    since = d.get("polling_since")
    return since if since and time.time() - since < MAX_POLL_SECONDS and _pid_alive(d.get("pid")) else None


def daemon_healthy(max_age: int = 300) -> bool:
    """Doing its job: finished a good poll recently, is in the middle of one, or is alive and deliberately waiting
    out a rate limit or a run of GitHub errors. None of those is an outage of prhub: agents polling by hand then
    would only hit the same limit or errors."""
    d = daemon_state()
    done = d.get("last_poll_done") or d.get("last_poll_ok")
    if d.get("last_poll_ok") and done and time.time() - done < max_age:
        return True
    return polling_since(d) is not None or rate_limited_until(d) is not None or degraded_since(d) is not None
