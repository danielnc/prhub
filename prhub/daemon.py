"""One poller per machine. Every `interval` seconds: one batched GraphQL read of every tracked PR; every
`discover_every` seconds also one search for new open PRs. Agents read the files this writes; they never
call GitHub themselves."""
from __future__ import annotations

import os
import time
from datetime import datetime

from . import commits, store
from .conditions import fingerprint, summary
from .github import GitHubError, contains, discover_open, fetch_commits, fetch_failing, fetch_prs, gh_token
from .refs import PRRef, RefResolver

NUDGE_MIN_GAP = 15          # an agent's `wait` can pull a poll forward, but not more often than this
KEEP_CLOSED_WATCH = 3600    # a watched PR stays tracked this long after it closes
KEEP_CLOSED_FILE = 7 * 86400
KEEP_COMMIT_WATCH = 6 * 3600  # a commit stays tracked this long after the last state/wait that asked about it
KEEP_COMMIT_FILE = 2 * 86400


def log(msg: str) -> None:
    print(f"{datetime.now().strftime('%Y-%m-%d %H:%M:%S')} {msg}", flush=True)


def _parse(ref: str) -> PRRef | None:
    try:
        return RefResolver().resolve(ref)
    except ValueError:
        return None


def adaptive_interval(base: int, rate: dict) -> int:
    """Slow down when the shared GitHub budget runs low, instead of finishing it off for every other tool."""
    remaining = rate.get("remaining")
    if remaining is None:
        return base
    if remaining < 300:
        return max(base, 600)
    if remaining < 1000:
        return max(base, 180)
    return base


def seconds_until(reset_at: str | None, default: int = 300) -> int:
    if not reset_at:
        return default
    try:
        if reset_at.isdigit():
            target = int(reset_at)
        else:
            target = int(datetime.fromisoformat(reset_at.replace("Z", "+00:00")).timestamp())
        return max(30, target - int(time.time()) + 10)
    except ValueError:
        return default


class Daemon:
    def __init__(self):
        self.cfg = store.load_config()
        self.open_refs: set[PRRef] = set()
        self.last_discover = 0.0
        self.state = store.daemon_state()
        self.state.update({"pid": os.getpid(), "started": int(time.time())})

    def tracked(self) -> set[PRRef]:
        refs = set(self.open_refs)
        for k in store.watches():
            if r := _parse(k):
                refs.add(r)
        for rec in store.all_prs():  # anything we last saw open keeps being read until it closes
            if (rec.get("summary") or {}).get("state") == "OPEN" and (r := _parse(rec["ref"])):
                refs.add(r)
        return refs

    def poll(self) -> dict:
        token = gh_token()
        now = time.time()
        rate: dict = {}
        if now - self.last_discover >= self.cfg["discover_every"]:
            found, rate = discover_open(self.cfg["search"], token)
            self.open_refs = set(found)
            self.last_discover = now
        refs = sorted(self.tracked())
        data, rate2 = fetch_prs(refs, token) if refs else ({}, {})
        rate = rate2 or rate
        went_red: list[PRRef] = []
        changed = 0
        for ref, pr in data.items():
            prev = store.read_pr(ref)
            if pr is None:  # deleted repo, lost access, or a typo in a watch
                continue
            s = summary(pr)
            rec = {"ref": str(ref), "fetched_at": int(now), "summary": s, "pr": pr,
                   "failing": (prev or {}).get("failing", []) if s["checks"] in ("FAILURE", "ERROR") else []}
            old = (prev or {}).get("summary")
            if old is None or fingerprint(old) != fingerprint(s):
                changed += 1
                store.append_event({"at": int(now), "ref": str(ref), "from": old and {
                    k: old[k] for k in ("state", "draft", "checks", "review", "merge_state", "head")},
                    "to": {k: s[k] for k in ("state", "draft", "checks", "review", "merge_state", "head")}})
                if s["checks"] in ("FAILURE", "ERROR") and (old is None or old["checks"] != s["checks"]
                                                             or old["head"] != s["head"]):
                    went_red.append(ref)
            if old and old.get("state") == "OPEN" and s["state"] != "OPEN":
                rec["closed_seen_at"] = int(now)
            elif prev and prev.get("closed_seen_at"):
                rec["closed_seen_at"] = prev["closed_seen_at"]
            store.write_pr(ref, rec)
        if went_red:
            for ref, failing in fetch_failing(went_red[:20], token).items():
                rec = store.read_pr(ref) or {}
                rec["failing"] = failing
                store.write_pr(ref, rec)
        crate, ncommits = self.poll_commits(token, now)
        self.prune(now)
        return {"tracked": len(refs), "commits": ncommits, "changed": changed, "rate": crate or rate}

    def poll_commits(self, token: str, now: float) -> tuple[dict, int]:
        refs = []
        for k in store.commit_watches():
            try:
                refs.append(commits.resolve(k))
            except ValueError:
                pass
        if not refs:
            return {}, 0
        data, rate = fetch_commits(refs, token)
        for ref, node in data.items():
            prev = store.read_commit(ref) or {}
            s = commits.summarize(node or {})
            if s is None:
                store.write_commit(ref, {"ref": str(ref), "fetched_at": int(now), "missing": True})
                continue
            cache = dict(prev.get("contains") or {})
            for env, sha in commits.needs_containment(s):
                if sha not in cache:  # one compare per new deployment, then remembered
                    try:
                        cache[sha] = contains(ref.owner, ref.repo, s["oid"], sha, token)
                    except GitHubError as e:
                        if e.rate_limited:
                            raise
                        continue
                s["envs"][env]["live"] = cache.get(sha)
            for e in s["envs"].values():  # the last good deploy predates this commit: not live there yet
                if "live" not in e and e.get("this") != "SUCCESS" and e.get("last_success"):
                    e["live"] = False
            old = prev.get("summary")
            if old is None or commits.fingerprint(old) != commits.fingerprint(s):
                store.append_event({"at": int(now), "ref": str(ref), "commit": commits.one_line(str(ref), s)})
            store.write_commit(ref, {"ref": str(ref), "fetched_at": int(now), "summary": s, "contains": cache})
        return rate, len(refs)

    def prune(self, now: float) -> None:
        w = store.watches()
        keep = {}
        for k, v in w.items():
            r = _parse(k)
            rec = store.read_pr(r) if r else None
            closed_at = (rec or {}).get("closed_seen_at")
            if r and not (closed_at and now - closed_at > KEEP_CLOSED_WATCH):
                keep[k] = v
        if keep != w:
            store.save_watches(keep)
        cw = store.commit_watches()
        keep_c = {k: v for k, v in cw.items() if now - v.get("last_asked", v.get("added_at", 0)) < KEEP_COMMIT_WATCH}
        if keep_c != cw:
            store.save_commit_watches(keep_c)
        for f in (store.home() / "commits").glob("*.json") if (store.home() / "commits").exists() else []:
            if now - f.stat().st_mtime > KEEP_COMMIT_FILE:
                f.unlink(missing_ok=True)
        for rec in store.all_prs():
            closed_at = rec.get("closed_seen_at")
            if closed_at and now - closed_at > KEEP_CLOSED_FILE and (r := _parse(rec["ref"])):
                store.pr_path(r).unlink(missing_ok=True)

    def run(self, once: bool = False) -> None:
        log(f"prhub daemon started (pid {os.getpid()}, home {store.home()})")
        interval = self.cfg["interval"]
        while True:
            started = time.time()
            try:
                res = self.poll()
                interval = adaptive_interval(self.cfg["interval"], res["rate"])
                self.state.update({"last_poll_ok": int(started), "last_error": None, "tracked": res["tracked"],
                                   "tracked_commits": res["commits"],
                                   "rate": res["rate"], "interval": interval, "rate_limited_until": None})
                log(f"poll ok: {res['tracked']} tracked, {res['commits']} commits, {res['changed']} changed, "
                    f"rate {res['rate'].get('remaining')} left, next in {interval}s")
            except GitHubError as e:
                # A GraphQL-level limit carries no reset time; the last rateLimit we read does.
                reset = e.reset_at or (self.state.get("rate") or {}).get("resetAt")
                wait = seconds_until(reset) if e.rate_limited else min(600, max(60, interval * 2))
                interval = wait
                self.state.update({"last_error": str(e)[:300], "last_error_at": int(started), "interval": wait,
                                   "rate_limited_until": int(started) + wait if e.rate_limited else None})
                log(f"poll failed ({'rate limited' if e.rate_limited else 'error'}): {e}; next in {wait}s")
            self.state["last_poll"] = int(started)
            store.write_json(store.home() / "daemon.json", self.state)
            if once:
                return
            self.sleep(interval, started)

    def sleep(self, interval: int, started: float) -> None:
        nudge = store.home() / "nudge"
        while time.time() - started < interval:
            time.sleep(2)
            try:
                if nudge.stat().st_mtime > started and time.time() - started >= NUDGE_MIN_GAP:
                    nudge.unlink(missing_ok=True)
                    return
            except FileNotFoundError:
                pass
