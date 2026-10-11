import json
import subprocess
import urllib.error

import pytest
from fakes import FakeResponse, http_error

from prhub import github
from prhub.commits import CommitRef
from prhub.github import GitHubError
from prhub.refs import PRRef


@pytest.fixture
def urlopen(monkeypatch):
    """Replace urlopen with a queue of canned responses (a dict, or an exception to raise); records requests."""
    calls, queue = [], []

    def fake(req, timeout=None):
        calls.append(req)
        item = queue.pop(0)
        if isinstance(item, BaseException):
            raise item
        return FakeResponse(item)

    monkeypatch.setattr("urllib.request.urlopen", fake)
    return calls, queue


# ---- token ----

def test_token_comes_from_gh(monkeypatch):
    monkeypatch.setattr(github.subprocess, "run",
                        lambda *a, **k: subprocess.CompletedProcess(a, 0, stdout="tok123\n", stderr=""))
    assert github.gh_token() == "tok123"


def test_no_token_is_an_error(monkeypatch):
    monkeypatch.setattr(github.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 1, "", "no"))
    with pytest.raises(GitHubError, match="gh auth login"):
        github.gh_token()


# ---- graphql ----

def test_graphql_sends_the_token_only_in_the_header_and_returns_data(urlopen):
    calls, queue = urlopen
    queue.append({"data": {"viewer": {"login": "me"}}})
    assert github.graphql("query { viewer { login } }", {"x": 1}, token="tok") == {"viewer": {"login": "me"}}
    req = calls[0]
    assert req.full_url == "https://api.github.com/graphql"
    assert req.get_header("Authorization") == "bearer tok"
    assert json.loads(req.data) == {"query": "query { viewer { login } }", "variables": {"x": 1}}


def test_http_rate_limit_carries_the_reset(urlopen):
    _, queue = urlopen
    queue.append(http_error(403, "API rate limit exceeded for user", {"x-ratelimit-reset": "1791750000"}))
    with pytest.raises(GitHubError) as e:
        github.graphql("q", token="t")
    assert e.value.rate_limited and e.value.reset_at == "1791750000"


def test_other_http_errors_are_not_rate_limits(urlopen):
    _, queue = urlopen
    queue.append(http_error(502, "bad gateway"))
    with pytest.raises(GitHubError, match="HTTP 502") as e:
        github.graphql("q", token="t")
    assert not e.value.rate_limited


def test_network_errors(urlopen):
    _, queue = urlopen
    queue.append(urllib.error.URLError("no route"))
    with pytest.raises(GitHubError, match="network"):
        github.graphql("q", token="t")


def test_graphql_level_rate_limit(urlopen):
    _, queue = urlopen
    queue.append({"errors": [{"type": "RATE_LIMITED", "message": "API rate limit already exceeded"}]})
    with pytest.raises(GitHubError) as e:
        github.graphql("q", token="t")
    assert e.value.rate_limited and e.value.reset_at is None


def test_partial_data_with_errors_is_returned(urlopen):
    _, queue = urlopen
    queue.append({"data": {"p0": None}, "errors": [{"message": "Could not resolve to a PullRequest"}]})
    assert github.graphql("q", token="t") == {"p0": None}


# ---- discovery and batched reads ----

def test_discover_pages_through_the_search(urlopen):
    _, queue = urlopen
    page = {"pageInfo": {"hasNextPage": True, "endCursor": "c1"},
            "nodes": [{"number": 1, "repository": {"nameWithOwner": "acme/api"}}, {}]}
    last = {"pageInfo": {"hasNextPage": False}, "nodes": [{"number": 2, "repository": {"nameWithOwner": "acme/web"}}]}
    queue += [{"data": {"rateLimit": {"remaining": 10}, "search": page}},
              {"data": {"rateLimit": {"remaining": 9}, "search": last}}]
    refs, rate = github.discover_open("is:pr is:open author:@me", "t")
    assert refs == [PRRef("acme", "api", 1), PRRef("acme", "web", 2)] and rate == {"remaining": 9}


def test_fetch_prs_batches_and_maps_unresolved_to_none(urlopen):
    calls, queue = urlopen
    a, b = PRRef("acme", "api", 1), PRRef("acme", "api", 2)
    queue += [{"data": {"rateLimit": {"remaining": 5}, "p0": {"pullRequest": {"number": 1}}}},
              {"data": {"rateLimit": {"remaining": 4}, "p0": None}}]
    out, rate = github.fetch_prs([a, b], "t", chunk=1)
    assert out == {a: {"number": 1}, b: None} and rate == {"remaining": 4} and len(calls) == 2
    assert b"pullRequest(number: 1)" in calls[0].data and b"pullRequest(number: 2)" in calls[1].data


def test_fetch_failing_names_failed_runs_and_statuses(urlopen):
    _, queue = urlopen
    nodes = [{"__typename": "CheckRun", "name": "test", "conclusion": "FAILURE", "detailsUrl": "u1"},
             {"__typename": "CheckRun", "name": "lint", "conclusion": "SUCCESS", "detailsUrl": "u2"},
             {"__typename": "StatusContext", "context": "ext/ci", "state": "ERROR", "targetUrl": "u3"}]
    queue.append({"data": {"p0": {"pullRequest": {"commits": {"nodes": [
        {"commit": {"statusCheckRollup": {"contexts": {"nodes": nodes}}}}]}}}}})
    ref = PRRef("acme", "api", 1)
    assert github.fetch_failing([ref], "t") == {ref: [{"name": "test", "conclusion": "FAILURE", "url": "u1"},
                                                      {"name": "ext/ci", "conclusion": "ERROR", "url": "u3"}]}
    assert github.fetch_failing([], "t") == {}


def test_fetch_commits_returns_each_repository_node(urlopen):
    _, queue = urlopen
    ref = CommitRef("acme", "api", "abcdef1")
    queue.append({"data": {"rateLimit": {"remaining": 3}, "c0": {"object": {"oid": "abcdef1" + "0" * 33}}}})
    out, rate = github.fetch_commits([ref], "t")
    assert out[ref]["object"]["oid"].startswith("abcdef1") and rate == {"remaining": 3}


# ---- compare ----

@pytest.mark.parametrize("status,expected", [("ahead", True), ("identical", True), ("behind", False),
                                             ("diverged", False)])
def test_contains_reads_the_compare_status(urlopen, status, expected):
    calls, queue = urlopen
    queue.append({"status": status})
    assert github.contains("acme", "api", "aaa", "bbb", "t") is expected
    assert calls[0].full_url == "https://api.github.com/repos/acme/api/compare/aaa...bbb"


def test_contains_errors(urlopen):
    _, queue = urlopen
    queue.append(http_error(404, "Not Found"))
    with pytest.raises(GitHubError, match="compare HTTP 404") as e:
        github.contains("acme", "api", "aaa", "bbb", "t")
    assert not e.value.rate_limited
    queue.append(http_error(403, "API rate limit exceeded"))
    with pytest.raises(GitHubError) as e:
        github.contains("acme", "api", "aaa", "bbb", "t")
    assert e.value.rate_limited
    queue.append(TimeoutError("slow"))
    with pytest.raises(GitHubError, match="network"):
        github.contains("acme", "api", "aaa", "bbb", "t")
