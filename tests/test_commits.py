import pytest

from prhub import commits
from prhub.commits import CommitRef, deploy_state, evaluate, is_commit_text, needs_containment, resolve, rollup, summarize
from prhub.refs import RefResolver

OID = "a" * 40
LATER = "b" * 40


def suite(name, status="COMPLETED", conclusion="SUCCESS", runs=1, event="push", app="github-actions"):
    return {"app": {"slug": app}, "status": status, "conclusion": conclusion, "checkRuns": {"totalCount": runs},
            "workflowRun": {"event": event, "url": "u", "workflow": {"name": name}} if event else None}


def node(suites=(), statuses=(), deployments=(), committed="2026-10-10T10:00:00Z"):
    return {"object": {"oid": OID, "committedDate": committed,
                       "status": {"contexts": [{"context": c, "state": s, "targetUrl": None} for c, s in statuses]},
                       "checkSuites": {"nodes": list(suites)}},
            "deployments": {"nodes": [{"commitOid": sha, "environment": env, "createdAt": at,
                                       "latestStatus": {"state": st}} for sha, env, at, st in deployments]}}


# ---- naming a commit ----

@pytest.mark.parametrize("text,expected", [
    ("https://github.com/acme/api/commit/abcdef1234", CommitRef("acme", "api", "abcdef1234")),
    ("acme/api@ABCDEF1", CommitRef("acme", "api", "abcdef1")),
    ("backend@abcdef1", CommitRef("acme", "api", "abcdef1")),
])
def test_resolve_commit(text, expected):
    assert resolve(text, RefResolver(aliases={"backend": "acme/api"})) == expected


def test_pr_texts_are_not_commits():
    for t in ("acme/api#12", "backend #12", "12", "https://github.com/acme/api/pull/12"):
        assert not is_commit_text(t)
    assert is_commit_text("acme/api@abcdef1") and is_commit_text("@abcdef1")


# ---- what counts as CI on a commit ----

def test_ignores_apps_that_never_ran_and_cron_runs():
    s = summarize(node([suite("CI"), suite("cursor", status="QUEUED", conclusion=None, runs=0, event=None, app="cursor"),
                        suite("Nightly", status="IN_PROGRESS", conclusion=None, event="schedule")]))
    assert [r["name"] for r in s["runs"]] == ["CI"] and rollup(s) == ("SUCCESS", [])


def test_a_workflow_whose_jobs_do_not_exist_yet_keeps_the_commit_pending():
    # acme/api, as seen live: both workflows of the push got a check suite in the same second, but CI's jobs (its
    # check runs) were created 45 s later. Secret scan finished in between, and green must not be read from it alone.
    s = summarize(node([suite("Secret scan"), suite("CI", status="QUEUED", conclusion=None, runs=0)]))
    assert rollup(s) == ("PENDING", []) and evaluate("green", s, None) == (False, None)
    assert "CI=QUEUED" in commits.one_line("acme/api@aaaaaaa", s)


def test_a_workflow_run_that_ended_before_its_jobs_started_still_counts():
    # e.g. cancelled while waiting on a concurrency group: this commit's CI never ran
    s = summarize(node([suite("Secret scan"), suite("CI", conclusion="CANCELLED", runs=0)]))
    assert rollup(s) == ("FAILURE", ["CI"])


def test_pending_failure_and_nothing_yet():
    assert rollup(summarize(node([suite("CI", status="IN_PROGRESS", conclusion=None)])))[0] == "PENDING"
    assert rollup(summarize(node([suite("CI", conclusion="FAILURE")]))) == ("FAILURE", ["CI"])
    assert rollup(summarize(node())) == (None, [])
    assert rollup(summarize(node(statuses=[("ci/ext", "PENDING")])))[0] == "PENDING"


def test_cancelled_run_counts_only_without_a_passing_sibling():
    replaced = summarize(node([suite("CI", conclusion="CANCELLED"), suite("CI")]))
    assert rollup(replaced) == ("SUCCESS", [])
    alone = summarize(node([suite("CI", conclusion="CANCELLED")]))
    assert rollup(alone) == ("FAILURE", ["CI"])


def test_workflow_filter_narrows_checks():
    s = summarize(node([suite("Deploy (Auto)"), suite("Lint", conclusion="FAILURE")]))
    assert rollup(s)[0] == "FAILURE" and rollup(s, "deploy") == ("SUCCESS", [])
    assert evaluate("green", s, None, workflow="deploy") == (True, None)
    met, dead = evaluate("green", s, None)
    assert not met and "Lint" in dead


def test_missing_commit_is_none():
    assert summarize({"object": None, "deployments": {"nodes": []}}) is None


# ---- deployed ----

def test_deployed_when_this_commit_was_deployed():
    s = summarize(node(deployments=[(OID, "production", "2026-10-10T11:00:00Z", "SUCCESS")]))
    assert deploy_state(s, "production") == (True, None) and not needs_containment(s)


def test_a_later_deploy_needs_a_containment_answer():
    s = summarize(node(deployments=[(LATER, "production", "2026-10-11T01:00:00Z", "SUCCESS")]))
    assert needs_containment(s) == [("production", LATER)]
    assert deploy_state(s, "production")[0] is False  # unknown until the daemon asks
    s["envs"]["production"]["live"] = True  # what the daemon writes after compare says "ahead"
    assert deploy_state(s, "production") == (True, None)


def test_a_deploy_older_than_the_commit_is_not_asked_about():
    s = summarize(node(deployments=[(LATER, "production", "2026-10-09T01:00:00Z", "SUCCESS")]))
    assert needs_containment(s) == []


def test_failed_deploy_of_this_commit_ends_the_wait():
    s = summarize(node(deployments=[(OID, "production", "2026-10-10T11:00:00Z", "FAILURE")]))
    met, dead = evaluate("deployed", s, None, env="production")
    assert not met and "FAILURE" in dead


def test_deploy_status_context_counts_and_env_matters():
    s = summarize(node(statuses=[("deployed/production", "SUCCESS")]))
    assert deploy_state(s, "production") == (True, None)
    assert deploy_state(s, "staging")[0] is False
    assert deploy_state(s, "any")[0] is True


def test_change_compares_against_the_baseline():
    a = summarize(node([suite("CI", status="IN_PROGRESS", conclusion=None)]))
    b = summarize(node([suite("CI")]))
    assert evaluate("change", b, a)[0] and not evaluate("change", a, a)[0]


def test_unknown_condition_is_an_error():
    with pytest.raises(ValueError):
        evaluate("merged", summarize(node()), None)


def test_one_line_reports_deploys_and_failures():
    s = summarize(node([suite("CI", conclusion="FAILURE")],
                       deployments=[(LATER, "production", "2026-10-11T01:00:00Z", "SUCCESS")]))
    s["envs"]["production"]["live"] = False
    line = commits.one_line("acme/api@aaaaaaa", s)
    assert "failing: CI" in line and "production=not yet" in line
