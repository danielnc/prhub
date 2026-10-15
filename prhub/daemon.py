"""One poller per machine. Every `interval` seconds: one batched GraphQL read of every tracked PR; every
`discover_every` seconds also one search for new open PRs. Agents read the files this writes; they never
call GitHub themselves."""
from __future__ import annotations

import os
import time
import traceback
from datetime import datetime

from . import commits, store
from .conditions import fingerprint, summary
from .github import (GitHubError, contains, discover_open, fetch_commits, fetch_failing, fetch_prs, gh_token,
                     run_state)
from .refs import PRRef, RefResolver

NUDGE_MIN_GAP = 15          # an agent's `wait` can pull a poll forward, but not more often than this
KEEP_CLOSED_WATCH = 3600    # a watched PR stays tracked this long after it closes
KEEP_CLOSED_FILE = 7 * 86400
KEEP_COMMIT_WATCH = 3600  # deploy-run answers are kept this long; so are commit watches written before keep_until
COMMITS_PER_POLL = 50  # most commits read in one poll (most urgent first), so a poll stays short however many are
#                        watched; the rest are read on the next polls
KEEP_COMMIT_FILE = 2 * 86400
CLOSED_RECHECK = 600   # a merged/closed PR still being watched is re-read this often (it can be reopened)
SETTLED_RECHECK = 300  # so is a commit whose runs have all finished (its deploy can still land later)
RUN_RECHECK = 600  # a deploy run that ended badly is asked again this often: a re-run keeps its id
TOKEN_TTL = 600  # re-read `gh auth token` this often; a slow or failing `gh` keeps the last good token meanwhile


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
        self.token: str | None = None
        self.token_at = 0.0

    def get_token(self) -> str:
        if self.token and time.time() - self.token_at < TOKEN_TTL:
            return self.token
        try:
            self.token, self.token_at = gh_token(), time.time()
        except GitHubError as e:
            if not (e.transient and self.token):
                raise
            log(f"keeping the previous token: {e}")
        return self.token

    def due(self, rec: dict | None, watch: dict | None, settled: bool, recheck: int, now: float) -> bool:
        """Whether to read an item this poll. Unsettled items are read every poll; settled ones every `recheck`
        seconds, unless someone asked about them since the last read (a new `wait` needs fresh data)."""
        if not rec or not settled:
            return True
        fetched = rec.get("fetched_at", 0)
        return now - fetched >= recheck or (watch or {}).get("last_asked", 0) > fetched

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
        token = self.get_token()
        now = time.time()
        rate: dict = {}
        if now - self.last_discover >= self.cfg["discover_every"]:
            found, rate = discover_open(self.cfg["search"], token)
            self.open_refs = set(found)
            self.last_discover = now
        w = store.watches()
        refs = []
        for ref in sorted(self.tracked()):
            rec = store.read_pr(ref)
            closed = (rec or {}).get("summary", {}).get("state") in ("MERGED", "CLOSED") and ref not in self.open_refs
            if self.due(rec, w.get(str(ref)), closed, CLOSED_RECHECK, now):
                refs.append(ref)
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
        skipped = len(refs) - len(data)
        crate, ncommits, cskipped = self.poll_commits(token, now)
        self.prune(now)
        return {"tracked": len(refs), "commits": ncommits, "changed": changed, "rate": crate or rate,
                "skipped": skipped + cskipped}

    def poll_commits(self, token: str, now: float) -> tuple[dict, int]:
        due = []
        for k, watch in store.commit_watches().items():
            if store.commit_watch_expired(watch, now, KEEP_COMMIT_WATCH):
                continue  # nobody has asked for a while; prune() drops it
            try:
                ref = commits.resolve(k)
            except ValueError:
                continue
            rec = store.read_commit(ref)
            s = (rec or {}).get("summary")
            settled = bool(rec and (rec.get("missing") or (s and commits.rollup(s)[0] in ("SUCCESS", "FAILURE"))))
            if self.due(rec, watch, settled, SETTLED_RECHECK, now):
                fetched = (rec or {}).get("fetched_at", 0)
                # never read or just asked about first, then commits still running, then the stalest finished ones
                rank = 0 if not rec or watch.get("last_asked", 0) > fetched else 1 if not settled else 2
                due.append((rank, fetched, ref))
        due.sort(key=lambda d: (d[0], d[1]))
        refs = [ref for _, _, ref in due[:COMMITS_PER_POLL]]
        if not refs:
            return {}, 0, 0
        data, rate = fetch_commits(refs, token)
        # Deploy runs of later commits, shared by every commit they may contain: each is asked at most once per poll,
        # a successful one is remembered, and one that ended badly is asked again every RUN_RECHECK.
        runs_file = store.home() / "runs.json"
        raw = store.read_json(runs_file, {})
        # a hand-edited or half-written file must not stop every poll: keep only well-formed entries
        seen = {k: v for k, v in raw.items() if isinstance(v, list) and len(v) == 3
                and isinstance(v[2], (int, float))} if isinstance(raw, dict) else {}

        def known(key: str) -> tuple | None:
            v = seen.get(key)
            if not v:
                return None
            status, concl, at = v
            if at >= now or (status == "COMPLETED" and ((concl or "") in commits.OK or now - at < RUN_RECHECK)):
                return status, concl
            return None

        for ref, node in data.items():
            prev = store.read_commit(ref) or {}
            s = commits.summarize(node or {})
            if s is None:
                store.write_commit(ref, {"ref": str(ref), "fetched_at": int(now), "missing": True})
                continue
            # A deployment counts once the workflow run that made it has finished; this commit's runs came with it.
            states = commits.own_runs(s)
            for run in commits.unknown_runs(s, states):
                key = f"{ref.owner}/{ref.repo}#{run}"
                st = known(key)
                if st is None:
                    try:
                        st = run_state(ref.owner, ref.repo, run, token)
                    except GitHubError as e:
                        if e.rate_limited:
                            raise
                        continue
                    seen[key] = [st[0], st[1], now]
                states[run] = st
            commits.gate_deploys(s, states)
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
            for e in s["envs"].values():  # deployed there, but not this commit or a finished deploy containing it
                if "live" not in e and e.get("this") != "SUCCESS" and (e.get("successes") or e.get("last_success")):
                    e["live"] = False
            old = prev.get("summary")
            if old is None or commits.fingerprint(old) != commits.fingerprint(s):
                store.append_event({"at": int(now), "ref": str(ref), "commit": commits.one_line(str(ref), s)})
            store.write_commit(ref, {"ref": str(ref), "fetched_at": int(now), "summary": s, "contains": cache})
        store.write_json(runs_file, {k: v for k, v in seen.items() if now - v[2] < KEEP_COMMIT_WATCH})
        return rate, len(refs), len(refs) - len(data)

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
        keep_c = {k: v for k, v in cw.items() if not store.commit_watch_expired(v, now, KEEP_COMMIT_WATCH)}
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
            self.state["polling_since"] = int(started)
            store.write_json(store.home() / "daemon.json", self.state)
            try:
                res = self.poll()
                interval = adaptive_interval(self.cfg["interval"], res["rate"])
                # Items GitHub kept failing on were skipped and still show their previous state: that is degraded.
                partial = res["skipped"] > 0
                self.state.update({"last_poll_ok": int(started),
                                   "last_error": f"GitHub failed to return {res['skipped']} PRs/commits; their "
                                                 "previous state is kept" if partial else None,
                                   "tracked": res["tracked"], "tracked_commits": res["commits"],
                                   "skipped": res["skipped"], "rate": res["rate"], "interval": interval,
                                   "rate_limited_until": None, "last_error_transient": partial or None,
                                   "degraded_since": (self.state.get("degraded_since") or int(started))
                                   if partial else None})
                log(f"poll ok in {int(time.time() - started)}s: {res['tracked']} PRs read, {res['commits']} commits read, "
                    f"{res['changed']} changed, "
                    + (f"{res['skipped']} skipped (GitHub error), " if res["skipped"] else "")
                    + f"rate {res['rate'].get('remaining')} left, next in {interval}s")
            except GitHubError as e:
                # A GraphQL-level limit carries no reset time; the last rateLimit we read does.
                reset = e.reset_at or (self.state.get("rate") or {}).get("resetAt")
                wait = seconds_until(reset) if e.rate_limited else min(300 if e.transient else 600,
                                                                       max(60, interval * 2))
                interval = wait
                self.state.update({"last_error": str(e)[:300], "last_error_at": int(started), "interval": wait,
                                   "rate_limited_until": int(started) + wait if e.rate_limited else None,
                                   "last_error_transient": e.transient,
                                   "degraded_since": (self.state.get("degraded_since") or int(started))
                                   if e.transient else None})
                kind = "rate limited" if e.rate_limited else "GitHub error, retrying" if e.transient else "error"
                log(f"poll failed ({kind}): {e}; next in {wait}s")
            except Exception as e:  # never die on a surprise: log it and keep polling
                interval = min(600, max(60, interval * 2))
                self.state.update({"last_error": f"{type(e).__name__}: {e}"[:300], "last_error_at": int(started),
                                   "interval": interval, "last_error_transient": False, "degraded_since": None})
                log(f"poll crashed: {traceback.format_exc()}next in {interval}s")
            self.state["last_poll"] = int(started)
            self.state["last_poll_done"] = int(time.time())
            self.state["poll_seconds"] = int(time.time() - started)
            self.state["polling_since"] = None
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
