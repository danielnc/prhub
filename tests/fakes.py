"""Hand-built GitHub responses and a fake clock, shared by the offline tests."""
import io
import json
import os
import time
import urllib.error

from prhub import store

HEAD = "abc1234567"
COMMITTED = "2026-10-11T08:00:00Z"


def pr_node(state="OPEN", draft=False, checks="SUCCESS", review="APPROVED", merge="CLEAN", head=HEAD,
            approved_head=True, number=1, repo="acme/api"):
    """One pullRequest node in the shape the daemon's GraphQL query returns."""
    reviews = []
    if review == "APPROVED":
        reviews = [{"author": {"login": "bot"}, "state": "APPROVED", "submittedAt": "2026-10-11T09:00:00Z",
                    "commit": {"oid": head if approved_head else "old0000000"}}]
    return {"number": number, "title": "a title", "url": f"https://github.com/{repo}/pull/{number}", "state": state,
            "isDraft": draft, "mergeable": "MERGEABLE", "mergeStateStatus": merge, "reviewDecision": review,
            "headRefName": "feat", "baseRefName": "main", "headRefOid": head,
            "commits": {"nodes": [{"commit": {"oid": head, "committedDate": COMMITTED,
                                              "statusCheckRollup": {"state": checks} if checks else None}}]},
            "latestReviews": {"nodes": reviews}, "repository": {"nameWithOwner": repo}}


def commit_node(oid="a" * 40, runs=(("CI", "COMPLETED", "SUCCESS"),), deployments=(), committed=COMMITTED):
    """One aliased repository node (commit + recent deployments) as fetch_commits returns it."""
    suites = [{"app": {"slug": "github-actions"}, "status": st, "conclusion": concl, "checkRuns": {"totalCount": 1},
               "workflowRun": {"event": "push", "url": "u", "workflow": {"name": name}}} for name, st, concl in runs]
    return {"object": {"oid": oid, "committedDate": committed, "status": {"contexts": []},
                       "checkSuites": {"nodes": suites}},
            "deployments": {"nodes": [{"commitOid": d[0], "environment": d[1], "createdAt": d[2],
                                       "latestStatus": {"state": d[3], "logUrl": d[4] if len(d) > 4 else None}}
                                      for d in deployments]}}


def healthy_daemon(**extra):
    store.write_json(store.home() / "daemon.json", {"pid": os.getpid(), "last_poll": int(time.time()),
                                                    "last_poll_ok": int(time.time()), "interval": 60, **extra})


class FakeResponse:
    def __init__(self, payload):
        self.body = json.dumps(payload).encode()

    def read(self):
        return self.body

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


def http_error(code, text, headers=None):
    return urllib.error.HTTPError("https://api.github.com/graphql", code, "error", headers or {},
                                  io.BytesIO(text.encode()))


class FakeClock:
    """Stands in for the `time` module inside prhub.cli: sleeping advances the clock instead of waiting."""

    def __init__(self, start=None):
        self.now = start if start is not None else time.time()

    def time(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds

    def strftime(self, fmt, t=None):
        return time.strftime(fmt, t if t is not None else time.localtime(self.now))

    def localtime(self, t=None):
        return time.localtime(self.now if t is None else t)
