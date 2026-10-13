import pytest

from prhub.cli import hook_verdict
from prhub.conditions import evaluate, fingerprint, one_line, summary
from prhub.daemon import adaptive_interval
from prhub.refs import PRRef, RefResolver

MERGE_STATES = ["CLEAN", "HAS_HOOKS"]


HEAD_COMMITTED = "2026-10-11T08:00:00Z"


def pr(state="OPEN", draft=False, checks="SUCCESS", review="APPROVED", merge="CLEAN", head="abc123", reviews=None,
       suites=()):
    """`reviews`: (author, state, submittedAt[, commit oid]); the oid defaults to `head`. By default an APPROVED
    review decision comes with one approval of the head, submitted after the head commit. `suites`: the head's check
    suites as (status, workflow run event or None for an app that is not GitHub Actions)."""
    if reviews is None:
        reviews = [("approver", "APPROVED", "2026-10-11T09:00:00Z")] if review == "APPROVED" else []
    return {"state": state, "isDraft": draft, "reviewDecision": review, "mergeStateStatus": merge,
            "headRefOid": head, "mergeable": "MERGEABLE", "baseRefName": "main", "title": "t", "url": "u",
            "commits": {"nodes": [{"commit": {"oid": head, "committedDate": HEAD_COMMITTED,
                                              "statusCheckRollup": {"state": checks} if checks else None,
                                              "checkSuites": {"nodes": [
                                                  {"status": st, "workflowRun": {"event": ev} if ev else None}
                                                  for st, ev in suites]}}}]},
            "latestReviews": {"nodes": [{"author": {"login": r[0]}, "state": r[1], "submittedAt": r[2],
                                         "commit": {"oid": r[3] if len(r) > 3 else head}} for r in reviews]}}


# ---- refs ----

@pytest.mark.parametrize("text,expected", [
    ("https://github.com/acme/api/pull/42", PRRef("acme", "api", 42)),
    ("https://github.com/acme/api/pull/42/files", PRRef("acme", "api", 42)),
    ("acme/api#7", PRRef("acme", "api", 7)),
    ("backend #2814", PRRef("acme", "api", 2814)),
    ("API#9", PRRef("acme", "api", 9)),
    ("web#3", PRRef("acme", "web-app", 3)),
])
def test_resolve(text, expected):
    r = RefResolver(aliases={"backend": "api", "web": "acme/web-app"}, repos={"api": "acme/api"})
    assert r.resolve(text) == expected


def test_resolve_unknown_nickname_is_an_error():
    with pytest.raises(ValueError, match="unknown repo nickname"):
        RefResolver().resolve("mystery #1")


def test_ref_slug_and_url():
    r = PRRef("acme", "api", 42)
    assert str(r) == "acme/api#42" and r.slug == "acme__api__42" and r.url.endswith("/acme/api/pull/42")


# ---- conditions ----

def test_done_needs_everything():
    assert evaluate("done", summary(pr()), None, MERGE_STATES) == (True, False)
    for bad in (dict(draft=True), dict(review="REVIEW_REQUIRED"), dict(checks="PENDING"), dict(merge="BLOCKED")):
        assert evaluate("done", summary(pr(**bad)), None, MERGE_STATES)[0] is False


def test_done_needs_an_approval_of_the_current_head():
    # reviewDecision stays APPROVED after a push; the only approval is of the previous head
    stale = summary(pr(head="new456", reviews=[("bot", "APPROVED", "2026-10-11T09:00:00Z", "old123")]))
    assert stale["review"] == "APPROVED" and not stale["approved_head"]
    assert evaluate("done", stale, None, MERGE_STATES) == (False, False)
    assert "stale" in one_line("a/b#1", stale)


def test_done_rejects_an_approval_relabelled_onto_the_head():
    # a force-push with an unchanged patch moves an older review onto the new head: it predates the head commit
    relabelled = summary(pr(reviews=[("bot", "APPROVED", "2026-10-11T07:00:00Z")]))
    assert not relabelled["approved_head"] and not evaluate("done", relabelled, None, MERGE_STATES)[0]


def test_green_waits_for_a_workflow_whose_jobs_do_not_exist_yet():
    # GitHub's rollup counts only check runs. CI's suite exists but its jobs are not created yet, while the other
    # workflow on the head already passed: the rollup says SUCCESS, and neither green nor done may be met.
    early = summary(pr(suites=[("COMPLETED", "pull_request"), ("QUEUED", "pull_request")]))
    assert early["checks"] == "PENDING"
    for cond in ("green", "done", "checks"):
        assert evaluate(cond, early, None, MERGE_STATES) == (False, False)
    assert summary(pr(checks=None, suites=[("IN_PROGRESS", "pull_request")]))["checks"] == "PENDING"


def test_suites_that_say_nothing_about_the_head_do_not_hold_green():
    # an app that never starts a run (no workflow run) and a cron run that landed on the head
    s = summary(pr(suites=[("COMPLETED", "pull_request"), ("QUEUED", None), ("IN_PROGRESS", "schedule")]))
    assert s["checks"] == "SUCCESS" and evaluate("green", s, None, MERGE_STATES) == (True, False)
    assert summary(pr(checks="FAILURE", suites=[("QUEUED", "pull_request")]))["checks"] == "FAILURE"


def test_done_is_impossible_once_closed():
    assert evaluate("done", summary(pr(state="MERGED")), None, MERGE_STATES) == (False, True)


def test_checks_finished_includes_failure_but_not_pending():
    assert evaluate("checks", summary(pr(checks="FAILURE")), None, MERGE_STATES)[0]
    assert not evaluate("checks", summary(pr(checks="PENDING")), None, MERGE_STATES)[0]
    assert not evaluate("checks", summary(pr(checks=None)), None, MERGE_STATES)[0]


def test_merged_vs_closed():
    assert evaluate("merged", summary(pr(state="CLOSED")), None, MERGE_STATES) == (False, True)
    assert evaluate("closed", summary(pr(state="CLOSED")), None, MERGE_STATES) == (True, False)


def test_change_and_review_compare_against_baseline():
    base = summary(pr(checks="PENDING"))
    assert not evaluate("change", base, base, MERGE_STATES)[0]
    assert evaluate("change", summary(pr(checks="SUCCESS")), base, MERGE_STATES)[0]
    newer = summary(pr(checks="PENDING", reviews=[("bot", "APPROVED", "2026-10-11T10:00:00Z")]))
    assert evaluate("review", newer, base, MERGE_STATES)[0]
    assert not evaluate("review", base, base, MERGE_STATES)[0]


def test_fingerprint_ignores_title():
    a, b = summary(pr()), summary(pr())
    b["title"] = "renamed"
    assert fingerprint(a) == fingerprint(b)


# ---- daemon pacing ----

def test_adaptive_interval_slows_when_budget_is_low():
    assert adaptive_interval(60, {"remaining": 4000}) == 60
    assert adaptive_interval(60, {"remaining": 900}) == 180
    assert adaptive_interval(60, {"remaining": 100}) == 600
    assert adaptive_interval(60, {}) == 60


# ---- hook ----

@pytest.mark.parametrize("cmd", [
    "gh pr checks 12 --watch",
    "gh run watch 123",
    "while true; do gh pr view 12 --json state; sleep 60; done",
    "until gh pr checks 12; do sleep 30; done",
    "for i in $(seq 20); do gh api repos/a/b/pulls/1 >/dev/null; sleep 90; done",
    "rtk gh pr checks 12 --watch",
    "while :; do rtk proxy gh pr view 12; sleep 60; done",
    "watch -n 30 gh pr checks 12",
])
def test_hook_refuses_polling_loops(cmd):
    assert hook_verdict(cmd) is not None


@pytest.mark.parametrize("cmd", [
    "gh pr view 12 --json state",
    "gh pr create --fill",
    "gh api repos/a/b/pulls -X POST -f base=main",
    "PRHUB_ALLOW=1 while true; do gh pr view 1; sleep 5; done",
    "for f in *.ts; do echo $f; done",
    "grep -rn 'gh pr checks --watch' docs/",
    "for o in a b; do gh api search/issues -f q=org:$o; done; cat ~/.local/state/prhub/watch.json",
    "python3 -c \"\nfor e in rows: print(e)\" ; gh api graphql -f query='{viewer{login}}'",
])
def test_hook_allows_one_off_reads_and_unrelated_loops(cmd):
    assert hook_verdict(cmd) is None


# ---- health: a rate limit is not an outage ----

def _state(tmp_path, monkeypatch, **d):
    import os
    import time
    from prhub import store
    monkeypatch.setenv("PRHUB_HOME", str(tmp_path))
    store.write_json(store.home() / "daemon.json", {"pid": os.getpid(), "last_poll": int(time.time()), **d})
    return store, time.time()


def test_recent_good_poll_is_healthy(tmp_path, monkeypatch):
    import time
    store, now = _state(tmp_path, monkeypatch, last_poll_ok=int(time.time()) - 30)
    assert store.daemon_healthy()


def test_rate_limited_but_alive_is_healthy(tmp_path, monkeypatch):
    import time
    store, now = _state(tmp_path, monkeypatch, last_poll_ok=int(time.time()) - 2000,
                        rate_limited_until=int(time.time()) + 900)
    assert store.daemon_healthy() and store.rate_limited_until() is not None


def test_rate_limited_but_dead_is_not_healthy(tmp_path, monkeypatch):
    import time
    store, now = _state(tmp_path, monkeypatch, pid=999999, last_poll_ok=int(time.time()) - 2000,
                        rate_limited_until=int(time.time()) + 900)
    assert not store.daemon_healthy()


def test_other_errors_are_not_healthy(tmp_path, monkeypatch):
    import time
    store, now = _state(tmp_path, monkeypatch, last_poll_ok=int(time.time()) - 2000, last_error="HTTP 401")
    assert not store.daemon_healthy()
