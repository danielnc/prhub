import json
import os
import time

import pytest
from fakes import commit_node, pr_node

from prhub import daemon, store
from prhub.commits import CommitRef
from prhub.daemon import KEEP_COMMIT_WATCH, RUN_RECHECK, Daemon, seconds_until
from prhub.github import GitHubError
from prhub.refs import PRRef

API = PRRef("acme", "api", 1)
OID = "a" * 40
LATER = "b" * 40


class FakeGitHub:
    """What the daemon sees of GitHub; each attribute is the next answer, and calls are counted."""

    def __init__(self):
        self.open = [API]
        self.prs = {API: pr_node()}
        self.failing = []
        self.commits = {}
        self.contains_answer = True
        self.run_answer = ("COMPLETED", "SUCCESS")
        self.calls = {"discover": 0, "prs": 0, "failing": 0, "commits": 0, "contains": 0, "run": 0}
        self.rate = {"remaining": 4000, "resetAt": "2026-10-11T20:00:00Z"}

    def install(self, monkeypatch):
        monkeypatch.setattr(daemon, "gh_token", lambda: "tok")
        monkeypatch.setattr(daemon, "discover_open", self.discover_open)
        monkeypatch.setattr(daemon, "fetch_prs", self.fetch_prs)
        monkeypatch.setattr(daemon, "fetch_failing", self.fetch_failing)
        monkeypatch.setattr(daemon, "fetch_commits", self.fetch_commits)
        monkeypatch.setattr(daemon, "contains", self.contains)
        monkeypatch.setattr(daemon, "run_state", self.run_state)
        return self

    def discover_open(self, query, token):
        self.calls["discover"] += 1
        return list(self.open), self.rate

    def fetch_prs(self, refs, token):
        self.calls["prs"] += 1
        return {r: self.prs.get(r) for r in refs}, self.rate

    def fetch_failing(self, refs, token):
        self.calls["failing"] += 1
        return {r: list(self.failing) for r in refs}

    def fetch_commits(self, refs, token):
        self.calls["commits"] += 1
        return {r: self.commits.get(r) for r in refs}, self.rate

    def run_state(self, owner, repo, run, token):
        self.calls["run"] += 1
        if isinstance(self.run_answer, Exception):
            raise self.run_answer
        return self.run_answer

    def contains(self, owner, repo, sha, newer, token):
        self.calls["contains"] += 1
        if isinstance(self.contains_answer, Exception):
            raise self.contains_answer
        return self.contains_answer


@pytest.fixture
def gh(monkeypatch):
    return FakeGitHub().install(monkeypatch)


def events():
    p = store.home() / "events.jsonl"
    return [json.loads(line) for line in p.read_text().splitlines()] if p.exists() else []


# ---- timing helpers ----

def test_seconds_until():
    assert seconds_until(None) == 300 and seconds_until("garbage") == 300
    assert seconds_until(str(int(time.time()) + 100)) in range(105, 112)
    assert seconds_until("2000-01-01T00:00:00Z") == 30  # already past: retry soon, never immediately


# ---- PRs ----

def test_poll_discovers_records_and_logs_changes(gh):
    d = Daemon()
    res = d.poll()
    assert res["tracked"] == 1 and res["changed"] == 1 and res["rate"]["remaining"] == 4000
    rec = store.read_pr(API)
    assert rec["summary"]["state"] == "OPEN" and rec["summary"]["approved_head"] is True
    assert events()[0]["ref"] == "acme/api#1" and events()[0]["from"] is None
    assert d.poll()["changed"] == 0 and gh.calls["discover"] == 1  # discovery runs every few minutes, not every poll


def test_a_pr_that_goes_red_gets_its_failing_checks_once(gh):
    gh.prs[API] = pr_node(checks="FAILURE")
    gh.failing = [{"name": "test", "conclusion": "FAILURE", "url": "u"}]
    d = Daemon()
    d.poll()
    assert store.read_pr(API)["failing"][0]["name"] == "test" and gh.calls["failing"] == 1
    d.poll()  # still red, same head: names are kept, not fetched again
    assert gh.calls["failing"] == 1 and store.read_pr(API)["failing"][0]["name"] == "test"


def test_closing_is_noticed_and_the_pr_keeps_being_read_until_then(gh):
    d = Daemon()
    d.poll()
    gh.open = []  # no longer found by the search, but we last saw it open
    gh.prs[API] = pr_node(state="MERGED")
    d.last_discover = 0
    d.poll()
    rec = store.read_pr(API)
    assert rec["summary"]["state"] == "MERGED" and rec["closed_seen_at"]
    first_seen = rec["closed_seen_at"]
    d.poll()
    assert store.read_pr(API) is None or store.read_pr(API)["closed_seen_at"] == first_seen


def test_a_watched_pr_that_does_not_resolve_is_skipped(gh):
    gh.open = []
    ghost = PRRef("acme", "api", 999)
    store.add_watch(ghost)
    assert Daemon().poll()["changed"] == 0 and store.read_pr(ghost) is None


# ---- commits ----

def test_commit_live_through_a_later_deploy_asks_compare_once(gh):
    ref = CommitRef("acme", "api", "aaaaaaa")
    store.add_commit_watch(ref)
    gh.commits[ref] = commit_node(OID, deployments=[(LATER, "production", "2026-10-12T01:00:00Z", "SUCCESS")])
    d = Daemon()
    assert d.poll()["commits"] == 1
    rec = store.read_commit(ref)
    assert rec["summary"]["envs"]["production"]["live"] is True and rec["contains"] == {LATER: True}
    d.poll()
    assert gh.calls["contains"] == 1  # remembered per deployment



def due_again(*refs):
    for ref in refs:  # a finished commit is otherwise re-read only every few minutes
        rec = store.read_commit(ref)
        rec["fetched_at"] -= 1000
        store.write_commit(ref, rec)


def runs_seen():
    return store.read_json(store.home() / "runs.json", {})


def age_runs(seconds):
    store.write_json(store.home() / "runs.json", {k: [v[0], v[1], v[2] - seconds] for k, v in runs_seen().items()})


def test_a_later_deploy_is_live_only_once_its_run_has_finished(gh):
    ref = CommitRef("acme", "api", "aaaaaaa")
    store.add_commit_watch(ref)
    gh.commits[ref] = commit_node(OID, deployments=[(LATER, "production", "2026-10-12T01:00:00Z", "SUCCESS",
                                                     "https://github.com/acme/api/actions/runs/9/job/90")])

    def live():
        return store.read_commit(ref)["summary"]["envs"]["production"]["live"]

    gh.run_answer = ("IN_PROGRESS", None)
    d = Daemon()
    d.poll()
    assert live() is False and gh.calls["contains"] == 0 and gh.calls["run"] == 1
    gh.run_answer = GitHubError("run HTTP 500")
    due_again(ref)
    d.poll()  # a run still in progress is asked again next poll; an ordinary error leaves it unknown: not live
    assert live() is False and gh.calls["run"] == 2
    gh.run_answer = ("COMPLETED", "FAILURE")
    due_again(ref)
    d.poll()
    assert live() is False and gh.calls["run"] == 3
    gh.run_answer = ("COMPLETED", "SUCCESS")
    due_again(ref)
    d.poll()  # a run that ended badly is not asked again right away...
    assert live() is False and gh.calls["run"] == 3
    age_runs(RUN_RECHECK + 1)
    due_again(ref)
    d.poll()  # ...but after RUN_RECHECK, since a re-run keeps its id and may succeed
    assert live() is True and gh.calls["run"] == 4 and runs_seen()["acme/api#9"][:2] == ["COMPLETED", "SUCCESS"]
    age_runs(RUN_RECHECK + 1)
    due_again(ref)
    d.poll()
    assert gh.calls["run"] == 4  # a successful run is remembered
    age_runs(KEEP_COMMIT_WATCH + 1)
    due_again(ref)
    d.poll()
    assert runs_seen() == {}  # and forgotten with the watches that could need it
    gh.run_answer = GitHubError("limit", rate_limited=True)
    gh.commits[ref] = commit_node(OID, deployments=[(LATER, "production", "2026-10-12T02:00:00Z", "SUCCESS",
                                                     "https://github.com/acme/api/actions/runs/10/job/100")])
    due_again(ref)
    with pytest.raises(GitHubError):
        d.poll()


def test_commits_contained_by_one_deploy_run_ask_it_once_per_poll(gh):
    refs = [CommitRef("acme", "api", c * 7) for c in "ac"]
    for ref, oid in zip(refs, ("a" * 40, "c" * 40)):
        store.add_commit_watch(ref)
        gh.commits[ref] = commit_node(oid, deployments=[(LATER, "production", "2026-10-12T01:00:00Z", "SUCCESS",
                                                         "https://github.com/acme/api/actions/runs/9/job/90")])
    gh.run_answer = ("IN_PROGRESS", None)
    d = Daemon()
    d.poll()
    assert gh.calls["run"] == 1
    due_again(*refs)
    d.poll()
    assert gh.calls["run"] == 2  # still running: asked again, once


def test_commit_with_an_older_deploy_is_not_live_and_needs_no_compare(gh):
    ref = CommitRef("acme", "api", "aaaaaaa")
    store.add_commit_watch(ref)
    gh.commits[ref] = commit_node(OID, deployments=[(LATER, "production", "2026-10-01T01:00:00Z", "SUCCESS")])
    Daemon().poll()
    assert store.read_commit(ref)["summary"]["envs"]["production"]["live"] is False and gh.calls["contains"] == 0


def test_compare_errors(gh):
    ref = CommitRef("acme", "api", "aaaaaaa")
    store.add_commit_watch(ref)
    gh.commits[ref] = commit_node(OID, deployments=[(LATER, "production", "2026-10-12T01:00:00Z", "SUCCESS")])
    gh.contains_answer = GitHubError("compare HTTP 500")
    Daemon().poll()  # an ordinary error leaves the answer unknown and asks again next poll
    assert store.read_commit(ref)["contains"] == {}
    gh.contains_answer = GitHubError("limit", rate_limited=True)
    rec = store.read_commit(ref)
    rec["fetched_at"] -= 1000  # due again: a finished commit is otherwise re-read only every few minutes
    store.write_commit(ref, rec)
    with pytest.raises(GitHubError):
        Daemon().poll()


def test_a_commit_that_does_not_exist_is_recorded_as_missing(gh):
    ref = CommitRef("acme", "api", "deadbee")
    store.add_commit_watch(ref)
    Daemon().poll()
    assert store.read_commit(ref)["missing"] is True


# ---- pruning ----

def test_prune_drops_old_closed_watches_files_and_idle_commits(gh):
    now = time.time()
    closed = PRRef("acme", "api", 7)
    store.add_watch(closed)
    store.write_pr(closed, {"ref": str(closed), "summary": {"state": "MERGED"},
                            "closed_seen_at": int(now - 8 * 86400)})
    store.add_watch(API)
    old = CommitRef("acme", "api", "aaaaaaa")
    store.add_commit_watch(old)
    w = store.commit_watches()
    w[str(old)]["last_asked"] = int(now - 7 * 3600)
    store.save_commit_watches(w)
    stale = store.commit_path(CommitRef("acme", "api", "bbbbbbb"))
    stale.write_text("{}")
    os.utime(stale, (now - 3 * 86400, now - 3 * 86400))
    Daemon().prune(now)
    assert list(store.watches()) == ["acme/api#1"]
    assert store.read_pr(closed) is None and store.commit_watches() == {} and not stale.exists()


# ---- the loop ----

def test_run_once_records_health(gh):
    Daemon().run(once=True)
    d = store.daemon_state()
    assert d["last_poll_ok"] and d["tracked"] == 1 and d["interval"] == 60 and d["rate_limited_until"] is None


def test_run_once_waits_for_the_reset_when_rate_limited(gh, monkeypatch):
    def limited(*a):
        raise GitHubError("GraphQL: API rate limit already exceeded", rate_limited=True)
    monkeypatch.setattr(daemon, "discover_open", limited)
    d = Daemon()
    d.state["rate"] = {"resetAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + 900))}
    d.run(once=True)
    st = store.daemon_state()
    assert 880 <= st["interval"] <= 920 and st["rate_limited_until"] > time.time() and store.daemon_healthy()


def test_run_once_backs_off_on_other_errors(gh, monkeypatch):
    def broken(*a):
        raise GitHubError("network: down")
    monkeypatch.setattr(daemon, "discover_open", broken)
    Daemon().run(once=True)
    st = store.daemon_state()
    assert st["interval"] == 120 and st["rate_limited_until"] is None and "network" in st["last_error"]


def test_a_nudge_ends_the_sleep_early(monkeypatch):
    monkeypatch.setattr(daemon, "NUDGE_MIN_GAP", 0)
    monkeypatch.setattr(daemon.time, "sleep", lambda s: None)
    started = time.time() - 1
    store.nudge()
    t0 = time.time()
    Daemon().sleep(3600, started)
    assert time.time() - t0 < 5 and not (store.home() / "nudge").exists()


# ---- resilience ----

def test_the_token_is_reused_and_a_slow_gh_keeps_the_last_one(gh, monkeypatch):
    calls = []

    def token():
        calls.append(1)
        if len(calls) > 1:
            raise GitHubError("`gh auth token` took longer than 30s", transient=True)
        return "tok"
    monkeypatch.setattr(daemon, "gh_token", token)
    d = Daemon()
    assert d.get_token() == "tok" and d.get_token() == "tok" and len(calls) == 1  # cached
    d.token_at = 0  # expired: the refresh fails transiently, so the old token is kept
    assert d.get_token() == "tok" and len(calls) == 2


def test_without_any_token_a_slow_gh_is_an_error(gh, monkeypatch):
    def slow():
        raise GitHubError("slow", transient=True)
    monkeypatch.setattr(daemon, "gh_token", slow)
    with pytest.raises(GitHubError):
        Daemon().get_token()


def test_github_errors_make_the_daemon_degraded_not_down(gh, monkeypatch):
    def gateway(*a):
        raise GitHubError("HTTP 504: gateway timeout", transient=True)
    monkeypatch.setattr(daemon, "discover_open", gateway)
    d = Daemon()
    d.run(once=True)
    st = store.daemon_state()
    since = st["degraded_since"]
    assert since and st["interval"] == 120 and store.degraded_since() == since and store.daemon_healthy()
    d.run(once=True)  # still failing: the start of the trouble is kept
    assert store.daemon_state()["degraded_since"] == since
    monkeypatch.setattr(daemon, "discover_open", gh.discover_open)
    d.run(once=True)  # recovered
    assert store.daemon_state()["degraded_since"] is None and store.degraded_since() is None


def test_permanent_errors_are_not_degraded(gh, monkeypatch):
    def denied(*a):
        raise GitHubError("HTTP 401: Bad credentials")
    monkeypatch.setattr(daemon, "discover_open", denied)
    Daemon().run(once=True)
    assert store.degraded_since() is None and not store.daemon_healthy()


def test_a_surprise_exception_is_logged_not_fatal(gh, monkeypatch):
    def boom(*a):
        raise KeyError("pullRequest")
    monkeypatch.setattr(daemon, "discover_open", boom)
    Daemon().run(once=True)
    assert "KeyError" in store.daemon_state()["last_error"]


def test_skipped_items_are_counted(gh, monkeypatch):
    other = PRRef("acme", "api", 2)
    gh.open = [API, other]
    monkeypatch.setattr(daemon, "fetch_prs", lambda refs, token: ({API: pr_node()}, gh.rate))  # GitHub dropped #2
    Daemon().run(once=True)
    st = store.daemon_state()
    assert st["skipped"] == 1 and st["degraded_since"] and store.degraded_since() and "previous state" in st["last_error"]
    monkeypatch.setattr(daemon, "fetch_prs", gh.fetch_prs)
    Daemon().run(once=True)  # nothing skipped: degraded clears
    assert store.daemon_state()["degraded_since"] is None



# ---- cadence and long polls ----

def test_a_merged_pr_is_reread_only_every_few_minutes_unless_asked(gh):
    gh.open = []
    store.add_watch(API)
    gh.prs[API] = pr_node(state="MERGED")
    d = Daemon()
    d.poll()
    assert gh.calls["prs"] == 1
    d.poll()  # settled and just read: skipped
    assert gh.calls["prs"] == 1
    rec = store.read_pr(API)
    rec["fetched_at"] -= 5
    store.write_pr(API, rec)
    store.add_watch(API)  # someone waits on it: read on the next poll
    d.poll()
    assert gh.calls["prs"] == 2
    rec = store.read_pr(API)
    rec["fetched_at"] -= daemon.CLOSED_RECHECK
    store.write_pr(API, rec)
    d.poll()
    assert gh.calls["prs"] == 3


def test_a_finished_commit_is_reread_only_every_few_minutes_unless_asked(gh):
    ref = CommitRef("acme", "api", "aaaaaaa")
    store.add_commit_watch(ref)
    gh.commits[ref] = commit_node(OID)
    d = Daemon()
    d.poll()
    d.poll()
    assert gh.calls["commits"] == 1
    rec = store.read_commit(ref)
    rec["fetched_at"] -= 5
    store.write_commit(ref, rec)
    store.add_commit_watch(ref)
    d.poll()
    assert gh.calls["commits"] == 2


def test_a_running_commit_is_read_every_poll(gh):
    ref = CommitRef("acme", "api", "aaaaaaa")
    store.add_commit_watch(ref)
    gh.commits[ref] = commit_node(OID, runs=[("CI", "IN_PROGRESS", None)])
    d = Daemon()
    d.poll()
    d.poll()
    assert gh.calls["commits"] == 2


def test_a_long_poll_is_not_an_outage():
    now = int(time.time())
    store.write_json(store.home() / "daemon.json", {"pid": os.getpid(), "last_poll_ok": now - 900,
                                                    "polling_since": now - 400})
    assert store.polling_since() == now - 400 and store.daemon_healthy()
    store.write_json(store.home() / "daemon.json", {"pid": os.getpid(), "last_poll_ok": now - 2000,
                                                    "polling_since": now - 1000})
    assert store.polling_since() is None and not store.daemon_healthy()  # stuck, not slow


def test_run_records_when_the_poll_ended(gh):
    Daemon().run(once=True)
    st = store.daemon_state()
    assert st["polling_since"] is None and st["last_poll_done"] >= st["last_poll"] and "poll_seconds" in st
