"""The only module that talks to GitHub: two GraphQL calls per poll, rate-limit aware."""
from __future__ import annotations

import json
import subprocess
import urllib.error
import urllib.request

from .refs import PRRef

API = "https://api.github.com/graphql"

PR_FIELDS = """
  number title url state isDraft mergeable mergeStateStatus reviewDecision
  headRefName baseRefName headRefOid updatedAt mergedAt closedAt
  author { login }
  repository { nameWithOwner }
  commits(last: 1) { nodes { commit { oid committedDate statusCheckRollup { state }
    checkSuites(first: 50) { pageInfo { hasNextPage } nodes { status workflowRun { event } } } } } }
  latestReviews(first: 10) { nodes { author { login } state submittedAt commit { oid } } }
  comments { totalCount }
"""

FAILING_FIELDS = """
  commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 60) { nodes {
    __typename
    ... on CheckRun { name status conclusion detailsUrl }
    ... on StatusContext { context state targetUrl }
  } } } } } }
"""


class GitHubError(RuntimeError):
    """A failed call. `transient` means retrying later should work: a 5xx, a network error, GitHub's own query
    timeout, or a slow `gh`. Anything else (bad credentials, a malformed query) needs a person."""

    def __init__(self, msg: str, rate_limited: bool = False, reset_at: str | None = None, transient: bool = False):
        super().__init__(msg)
        self.rate_limited = rate_limited
        self.reset_at = reset_at
        self.transient = transient


def gh_token(timeout: int = 30) -> str:
    """The token `gh` is logged in with."""
    try:
        out = subprocess.run(["gh", "auth", "token"], capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise GitHubError(f"`gh auth token` took longer than {timeout}s", transient=True) from None
    except OSError as e:
        raise GitHubError(f"cannot run gh: {e}") from None
    tok = out.stdout.strip()
    if not tok:
        raise GitHubError("no GitHub token: run `gh auth login`")
    return tok


def graphql(query: str, variables: dict | None = None, token: str | None = None, timeout: int = 30) -> dict:
    body = json.dumps({"query": query, "variables": variables or {}}).encode()
    req = urllib.request.Request(API, data=body, method="POST", headers={
        "Authorization": f"bearer {token or gh_token()}",
        "Content-Type": "application/json",
        "User-Agent": "prhub",
    })
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            payload = json.loads(resp.read())
    except urllib.error.HTTPError as e:
        text = e.read().decode("utf-8", "replace")[:300]
        limited = e.code in (403, 429) and "rate limit" in text.lower()
        raise GitHubError(f"HTTP {e.code}: {text}", rate_limited=limited,
                          reset_at=e.headers.get("x-ratelimit-reset"), transient=e.code >= 500) from None
    except (urllib.error.URLError, TimeoutError, OSError) as e:
        raise GitHubError(f"network: {e}", transient=True) from None
    errors = payload.get("errors") or []
    if errors and not payload.get("data"):
        msg = "; ".join(err.get("message", "?") for err in errors)
        limited = any(err.get("type") == "RATE_LIMITED" for err in errors) or "rate limit" in msg.lower()
        # "Something went wrong while executing your query" is GitHub timing out on a heavy query.
        raise GitHubError(f"GraphQL: {msg}", rate_limited=limited,
                          transient="something went wrong" in msg.lower() or "timeout" in msg.lower())
    return payload.get("data") or {}


def discover_open(search_query: str, token: str) -> tuple[list[PRRef], dict]:
    """Every open PR matching `search_query` (paged, 100 per call). Returns refs and the last rateLimit."""
    q = """query($q: String!, $after: String) {
      rateLimit { cost remaining resetAt }
      search(query: $q, type: ISSUE, first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { ... on PullRequest { number repository { nameWithOwner } } }
      }
    }"""
    refs, after, rate = [], None, {}
    for _ in range(10):  # 1,000 PRs is plenty for one person's open work
        d = graphql(q, {"q": search_query, "after": after}, token)
        rate = d.get("rateLimit") or rate
        s = d.get("search") or {}
        for n in s.get("nodes") or []:
            if n and n.get("repository"):
                owner, repo = n["repository"]["nameWithOwner"].split("/", 1)
                refs.append(PRRef(owner, repo, n["number"]))
        if not (s.get("pageInfo") or {}).get("hasNextPage"):
            break
        after = s["pageInfo"]["endCursor"]
    return refs, rate


def _alias_query(refs: list[PRRef], fields: str) -> str:
    parts = [f'p{i}: repository(owner: {json.dumps(r.owner)}, name: {json.dumps(r.repo)}) '
             f'{{ pullRequest(number: {r.number}) {{ {fields} }} }}' for i, r in enumerate(refs)]
    return "query { rateLimit { cost remaining resetAt }\n" + "\n".join(parts) + "\n}"


MAX_TRANSIENT_FAILURES = 4  # consecutive failures in one read: past this it is an outage, and splitting adds load


def _batched(refs: list, build_query, read, token: str, chunk: int) -> tuple[dict, dict]:
    """Read `refs` `chunk` per GraphQL call. When GitHub fails a call transiently (a 502/504, or its "Something went
    wrong" timeout on a heavy query), split that batch in half and retry, down to single items. An item that still
    fails is left out of the result, so one heavy PR or commit cannot stop all the others from being read; its
    previous record is kept. Split halves go to the back of the queue, and any success resets the count, so more than
    MAX_TRANSIENT_FAILURES consecutive failures mean an outage and raise, while one bad item among many is isolated."""
    out: dict = {}
    rate: dict = {}
    failures = 0
    pending = [refs[i:i + chunk] for i in range(0, len(refs), chunk)]
    while pending:
        part = pending.pop(0)
        try:
            d = graphql(build_query(part), token=token)
        except GitHubError as e:
            if not e.transient:
                raise
            failures += 1
            if failures > MAX_TRANSIENT_FAILURES:
                raise
            if len(part) > 1:
                pending += [part[:len(part) // 2], part[len(part) // 2:]]
            continue
        failures = 0
        rate = d.get("rateLimit") or rate
        for j, r in enumerate(part):
            out[r] = read(d, j)
    return out, rate


def fetch_prs(refs: list[PRRef], token: str, chunk: int = 20) -> tuple[dict[PRRef, dict | None], dict]:
    """Full state of each PR, batched `chunk` per GraphQL call. A PR that no longer resolves maps to None; a PR
    GitHub could not return this time is missing from the result."""
    return _batched(refs, lambda part: _alias_query(part, PR_FIELDS),
                    lambda d, j: (d.get(f"p{j}") or {}).get("pullRequest"), token, chunk)


def fetch_failing(refs: list[PRRef], token: str) -> dict[PRRef, list[dict]]:
    """Names and links of the failing checks, only for PRs whose CI just went red (kept out of the hot query)."""
    if not refs:
        return {}
    d = graphql(_alias_query(refs, FAILING_FIELDS), token=token)
    out: dict[PRRef, list[dict]] = {}
    for j, r in enumerate(refs):
        pr = (d.get(f"p{j}") or {}).get("pullRequest") or {}
        nodes = (((((pr.get("commits") or {}).get("nodes") or [{}])[0].get("commit") or {})
                  .get("statusCheckRollup") or {}).get("contexts") or {}).get("nodes") or []
        failing = []
        for n in nodes:
            if n.get("__typename") == "CheckRun" and n.get("conclusion") in ("FAILURE", "TIMED_OUT", "CANCELLED",
                                                                             "ACTION_REQUIRED", "STARTUP_FAILURE"):
                failing.append({"name": n["name"], "conclusion": n["conclusion"], "url": n.get("detailsUrl")})
            elif n.get("__typename") == "StatusContext" and n.get("state") in ("FAILURE", "ERROR"):
                failing.append({"name": n["context"], "conclusion": n["state"], "url": n.get("targetUrl")})
        out[r] = failing
    return out


def fetch_commits(refs, token: str, chunk: int = 5) -> tuple[dict, dict]:
    """Raw repository nodes (commit + its repo's recent deployments) per CommitRef, batched `chunk` per call (each
    one is heavy: up to 50 check suites and 30 deployments). A commit GitHub could not return is missing."""
    from .commits import alias_query
    return _batched(refs, alias_query, lambda d, j: d.get(f"c{j}"), token, chunk)


def contains(owner: str, repo: str, sha: str, newer: str, token: str) -> bool:
    """Whether commit `newer` contains `sha` (REST compare: `newer` is ahead of or identical to `sha`)."""
    req = urllib.request.Request(f"https://api.github.com/repos/{owner}/{repo}/compare/{sha}...{newer}",
                                 headers={"Authorization": f"bearer {token}", "User-Agent": "prhub",
                                          "Accept": "application/vnd.github+json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read()).get("status") in ("ahead", "identical")
    except urllib.error.HTTPError as e:
        text = e.read().decode("utf-8", "replace")[:200]
        raise GitHubError(f"compare HTTP {e.code}: {text}",
                          rate_limited=e.code in (403, 429) and "rate limit" in text.lower()) from None
    except (urllib.error.URLError, TimeoutError, OSError) as e:
        raise GitHubError(f"network: {e}") from None


def run_state(owner: str, repo: str, run: str, token: str) -> tuple[str, str | None]:
    """(status, conclusion) of one workflow run, upper-cased like GraphQL's (REST GET actions/runs/{id})."""
    req = urllib.request.Request(f"https://api.github.com/repos/{owner}/{repo}/actions/runs/{run}",
                                 headers={"Authorization": f"bearer {token}", "User-Agent": "prhub",
                                          "Accept": "application/vnd.github+json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            d = json.loads(resp.read())
    except urllib.error.HTTPError as e:
        text = e.read().decode("utf-8", "replace")[:200]
        raise GitHubError(f"run HTTP {e.code}: {text}",
                          rate_limited=e.code in (403, 429) and "rate limit" in text.lower()) from None
    except (urllib.error.URLError, TimeoutError, OSError) as e:
        raise GitHubError(f"network: {e}") from None
    return (d.get("status") or "").upper(), (d.get("conclusion") or "").upper() or None
