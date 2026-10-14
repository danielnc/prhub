import pytest

from prhub import commits
from prhub.commits import CommitRef, deploy_state, evaluate, is_commit_text, needs_containment, resolve, rollup, summarize
from prhub.refs import RefResolver

OID = "a" * 40
LATER = "b" * 40


def suite(name, status="COMPLETED", conclusion="SUCCESS", runs=1, event="push", app="github-actions", url="u"):
    return {"app": {"slug": app}, "status": status, "conclusion": conclusion, "checkRuns": {"totalCount": runs},
            "workflowRun": {"event": event, "url": url, "workflow": {"name": name}} if event else None}


def node(suites=(), statuses=(), deployments=(), committed="2026-10-10T10:00:00Z"):
    return {"object": {"oid": OID, "committedDate": committed,
                       "status": {"contexts": [{"context": c, "state": s, "targetUrl": None} for c, s in statuses]},
                       "checkSuites": {"nodes": list(suites)}},
            "deployments": {"nodes": [{"commitOid": d[0], "environment": d[1], "createdAt": d[2],
                                       "latestStatus": {"state": d[3], "logUrl": d[4] if len(d) > 4 else None}}
                                      for d in deployments]}}


RUN = "https://github.com/acme/api/actions/runs/7"
JOB = RUN + "/job/70"


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
    s = summarize(node([suite("CI"), None,
                        suite("cursor", status="QUEUED", conclusion=None, runs=0, event=None, app="cursor"),
                        suite("Nightly", status="IN_PROGRESS", conclusion=None, event="schedule")]))
    assert [r["name"] for r in s["runs"]] == ["CI"] and rollup(s) == ("SUCCESS", [])


def test_a_workflow_whose_jobs_do_not_exist_yet_keeps_the_commit_pending():
    # acme/api, as seen live: both workflows of the push got a check suite in the same second, but CI's jobs (its
    # check runs) were created later. Secret scan finished in between, and green must not be read from it alone.
    s = summarize(node([suite("Secret scan"), suite("CI", status="QUEUED", conclusion=None, runs=0)]))
    assert rollup(s) == ("PENDING", []) and evaluate("green", s, None) == (False, None)
    assert "CI=QUEUED" in commits.one_line("acme/api@aaaaaaa", s)



def test_more_suites_than_one_page_keep_the_commit_pending():
    n = node([suite("CI")])
    n["object"]["checkSuites"]["pageInfo"] = {"hasNextPage": True}
    s = summarize(n)
    assert rollup(s)[0] == "PENDING" and evaluate("green", s, None) == (False, None)
    assert rollup(summarize(node([suite("CI")])))[0] == "SUCCESS"


def test_a_run_cancelled_before_it_started_is_not_a_failure_but_green_can_never_be_met():
    # replaced in its concurrency group by a newer run while it waited: this commit's CI never ran, so it is not red,
    # and green is impossible rather than met from the workflows that did run
    s = summarize(node([suite("Secret scan"), suite("CI", conclusion="CANCELLED", runs=0)]))
    assert rollup(s) == ("SUCCESS", [])
    met, why = evaluate("green", s, None)
    assert not met and "never ran on this commit" in why and "CI" in why
    assert evaluate("checks", s, None) == (True, None)
    assert "CI=NOT STARTED" in commits.one_line("acme/api@aaaaaaa", s) and "failing" not in commits.one_line(
        "acme/api@aaaaaaa", s)
    only = summarize(node([suite("CI", conclusion="CANCELLED", runs=0)]))
    assert rollup(only) == (None, []) and evaluate("checks", only, None)[1] and evaluate("green", only, None)[1]
    while_pending = summarize(node([suite("Build", status="IN_PROGRESS", conclusion=None),
                                    suite("CI", conclusion="CANCELLED", runs=0)]))
    assert evaluate("green", while_pending, None) == (False, None)


def test_a_run_cancelled_before_it_started_does_not_count_when_the_same_workflow_ran():
    s = summarize(node([suite("CI", conclusion="CANCELLED", runs=0), suite("CI")]))
    assert rollup(s) == ("SUCCESS", []) and evaluate("green", s, None) == (True, None)


def test_a_cancelled_dispatched_run_that_never_started_says_nothing_about_the_commit():
    # the merge commit of a deploy-on-merge repo: a post-merge job dispatched on the branch head waited in its
    # concurrency group and was replaced by the next merge's run; every check run on the commit passed
    s = summarize(node([suite("Wiki trigger"), suite("Publish image"), suite("Deploy", event="workflow_dispatch"),
                        suite("Wiki ingest", conclusion="CANCELLED", runs=0, event="workflow_dispatch")]))
    assert rollup(s) == ("SUCCESS", []) and evaluate("green", s, None) == (True, None)
    assert "Wiki ingest" not in commits.one_line("acme/api@aaaaaaa", s)
    started = summarize(node([suite("Deploy", conclusion="CANCELLED", event="workflow_dispatch")]))
    assert rollup(started) == ("FAILURE", ["Deploy (cancelled)"])


def test_pending_failure_and_nothing_yet():
    assert rollup(summarize(node([suite("CI", status="IN_PROGRESS", conclusion=None)])))[0] == "PENDING"
    assert rollup(summarize(node([suite("CI", conclusion="FAILURE")]))) == ("FAILURE", ["CI"])
    assert rollup(summarize(node())) == (None, [])
    assert rollup(summarize(node(statuses=[("ci/ext", "PENDING")])))[0] == "PENDING"


def test_cancelled_run_counts_only_without_a_passing_sibling():
    replaced = summarize(node([suite("CI", conclusion="CANCELLED"), suite("CI")]))
    assert rollup(replaced) == ("SUCCESS", [])
    alone = summarize(node([suite("CI", conclusion="CANCELLED")]))
    assert rollup(alone) == ("FAILURE", ["CI (cancelled)"])


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



def test_a_deployment_counts_only_once_the_run_that_made_it_has_finished():
    # one deploy run makes a Deployment per job that names the environment, and GitHub marks each SUCCESS when ITS
    # job ends: the build job's deployment reads SUCCESS while migrate and deploy have not run
    build = (OID, "production", "2026-10-10T11:00:00Z", "SUCCESS", JOB)
    s = summarize(node([suite("Deploy", status="IN_PROGRESS", conclusion=None, url=RUN)], deployments=[build]))
    assert s["envs"]["production"]["this"] == "DEPLOYING" and deploy_state(s, "production") == (False, None)
    assert "production=DEPLOYING" in commits.one_line("acme/api@aaaaaaa", s)
    assert evaluate("deployed", s, None, env="production") == (False, None)
    nxt = (OID, "production", "2026-10-10T11:05:00Z", "IN_PROGRESS", RUN + "/job/71")
    s = summarize(node([suite("Deploy", status="IN_PROGRESS", conclusion=None, url=RUN)], deployments=[build, nxt]))
    assert deploy_state(s, "production")[0] is False
    done = summarize(node([suite("Deploy", url=RUN)], deployments=[build]))
    assert deploy_state(done, "production") == (True, None)
    failed = summarize(node([suite("Deploy", conclusion="FAILURE", url=RUN)], deployments=[build]))
    live, why = deploy_state(failed, "production")
    assert not live and "FAILURE" in why


def test_a_finished_deploy_of_this_commit_stays_live_while_a_redeploy_runs():
    first = (OID, "production", "2026-10-10T11:00:00Z", "SUCCESS", JOB)
    again = (OID, "production", "2026-10-10T12:00:00Z", "SUCCESS", "https://github.com/acme/api/actions/runs/8/job/80")
    s = summarize(node([suite("Deploy", url=RUN)], deployments=[first, again]))
    assert s["envs"]["production"]["this"] == "SUCCESS"


def test_a_later_deploy_counts_once_its_run_has_finished():
    later = (LATER, "production", "2026-10-11T01:00:00Z", "SUCCESS", JOB)
    s = summarize(node(deployments=[later]))
    assert needs_containment(s) == [] and commits.unknown_runs(s, commits.own_runs(s)) == ["7"]
    commits.gate_deploys(s, {"7": ("IN_PROGRESS", None)})
    assert needs_containment(s) == []
    commits.gate_deploys(s, {"7": ("COMPLETED", "SUCCESS")})
    assert needs_containment(s) == [("production", LATER)]
    assert commits.unknown_runs(s, {"7": ("COMPLETED", "SUCCESS")}) == []


def test_a_deployment_made_outside_actions_goes_by_its_own_status():
    s = summarize(node(deployments=[(OID, "production", "2026-10-10T11:00:00Z", "SUCCESS", "https://ci.example/1")]))
    assert deploy_state(s, "production") == (True, None) and commits.run_id("https://ci.example/1") is None


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
