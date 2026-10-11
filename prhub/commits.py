"""Watch one commit's CI and deployments, in any repo: `owner/repo@sha`.

Generic on purpose. Workflow names, default branches (main, develop…) and deploy styles differ per repo, so:
- CI is every check suite that actually ran something on the commit, minus cron runs (a scheduled workflow attaches
  to whatever the branch head is, so it says nothing about this commit) and minus apps that never start a run
  (cursor, classroom, cloudflare… leave a suite QUEUED forever with zero runs).
- "deployed to <env>" comes from GitHub Deployments, which both deploy-on-merge and nightly-release repos record,
  plus commit statuses named like `deployed/<env>`. A deploy of a LATER commit counts when that commit contains this
  one (the daemon asks GitHub's compare API once per new deployment), because a cancelled or batched deploy means
  your own commit may never be deployed by itself.
"""
from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass

from .refs import RefResolver, remote_of

URL_RE = re.compile(r"github\.com/([\w.-]+)/([\w.-]+)/commit/([0-9a-fA-F]{7,40})")
FULL_RE = re.compile(r"^([\w.-]+)/([\w.-]+)@([0-9a-fA-F]{7,40})$")
NICK_RE = re.compile(r"^([\w .-]+?)\s*@\s*([0-9a-fA-F]{7,40})$")
HERE_RE = re.compile(r"^@([0-9a-fA-F]{7,40})$")

OK = {"SUCCESS", "SKIPPED", "NEUTRAL"}
CONDITIONS = ("checks", "green", "deployed", "change")


@dataclass(frozen=True, order=True)
class CommitRef:
    owner: str
    repo: str
    sha: str  # as given (may be abbreviated); records also carry the full oid

    def __str__(self) -> str:
        return f"{self.owner}/{self.repo}@{self.sha}"

    @property
    def slug(self) -> str:
        return f"{self.owner}__{self.repo}__{self.sha}"


def is_commit_text(text: str) -> bool:
    t = text.strip()
    return bool(URL_RE.search(t) or FULL_RE.match(t) or NICK_RE.match(t) or HERE_RE.match(t))


def resolve(text: str, resolver: RefResolver | None = None, cwd: str | None = None) -> CommitRef:
    t = text.strip()
    if m := URL_RE.search(t):
        return CommitRef(m.group(1), m.group(2), m.group(3).lower())
    if m := FULL_RE.match(t):
        return CommitRef(m.group(1), m.group(2), m.group(3).lower())
    if m := HERE_RE.match(t):
        r = remote_of(cwd or os.getcwd())
        if not r:
            raise ValueError(f"'{t}' names a commit in this checkout, but the current directory is not a GitHub clone")
        return CommitRef(r[0], r[1], m.group(1).lower())
    if m := NICK_RE.match(t):
        repo = (resolver or RefResolver())._repo_for(m.group(1))
        if repo:
            owner, name = repo.split("/", 1)
            return CommitRef(owner, name, m.group(2).lower())
        raise ValueError(f"unknown repo nickname '{m.group(1)}': add it to [aliases] in the prhub config")
    raise ValueError(f"cannot read '{t}' as a commit (try owner/repo@sha or a commit URL)")


COMMIT_FIELDS = """
  object(expression: %s) { ... on Commit { oid committedDate
    status { contexts { context state targetUrl } }
    checkSuites(first: 50) { nodes { app { slug } status conclusion checkRuns(first: 0) { totalCount }
      workflowRun { event url workflow { name } } } } } }
  deployments(last: 30) { nodes { commitOid environment createdAt latestStatus { state } } }
"""


def alias_query(refs: list[CommitRef]) -> str:
    parts = [f"c{i}: repository(owner: {json.dumps(r.owner)}, name: {json.dumps(r.repo)}) {{"
             + COMMIT_FIELDS % json.dumps(r.sha) + "}" for i, r in enumerate(refs)]
    return "query { rateLimit { cost remaining resetAt }\n" + "\n".join(parts) + "\n}"


def summarize(repo_node: dict) -> dict | None:
    """Flatten one aliased repository node. None when the commit does not exist (typo, or not pushed yet)."""
    obj = (repo_node or {}).get("object") or {}
    if not obj.get("oid"):
        return None
    oid = obj["oid"]
    runs = []
    for s in ((obj.get("checkSuites") or {}).get("nodes") or []):
        if not s or not ((s.get("checkRuns") or {}).get("totalCount") or 0):
            continue  # an app that never started a run on this commit
        wr = s.get("workflowRun") or {}
        if wr.get("event") == "schedule":
            continue  # cron runs land on the branch head; they are not about this commit
        runs.append({"name": ((wr.get("workflow") or {}).get("name")) or (s.get("app") or {}).get("slug") or "?",
                     "event": wr.get("event"), "status": s.get("status"), "conclusion": s.get("conclusion"),
                     "url": wr.get("url")})
    statuses = [{"name": c.get("context"), "state": c.get("state"), "url": c.get("targetUrl")}
                for c in (((obj.get("status") or {}).get("contexts")) or []) if c]
    envs: dict[str, dict] = {}
    for d in ((repo_node.get("deployments") or {}).get("nodes") or []):  # oldest first
        if not d:
            continue
        st = (d.get("latestStatus") or {}).get("state") or "PENDING"
        e = envs.setdefault(d.get("environment") or "?", {"this": None, "last_success": None})
        if d.get("commitOid") == oid:
            e["this"] = st
        if st == "SUCCESS":
            e["last_success"] = {"sha": d.get("commitOid"), "at": d.get("createdAt")}
    return {"oid": oid, "committed": obj.get("committedDate"), "runs": runs, "statuses": statuses, "envs": envs}


def needs_containment(s: dict) -> list[tuple[str, str]]:
    """(env, deployed sha) pairs where a successful deploy of ANOTHER commit happened after this one was made,
    so whether it contains this commit has to be asked."""
    out = []
    for env, e in (s.get("envs") or {}).items():
        ls = e.get("last_success")
        if ls and ls["sha"] != s["oid"] and (ls.get("at") or "") >= (s.get("committed") or ""):
            out.append((env, ls["sha"]))
    return out


def rollup(s: dict, workflow: str | None = None) -> tuple[str | None, list[str]]:
    """(PENDING | SUCCESS | FAILURE | None when nothing ran yet, names of failed runs/statuses). `workflow` keeps only
    runs and statuses whose name contains it (case-insensitive)."""
    w = (workflow or "").lower()
    runs = [r for r in s["runs"] if w in (r["name"] or "").lower()]
    statuses = [x for x in s["statuses"] if w in (x["name"] or "").lower()]
    if not runs and not statuses:
        return None, []
    # A run CANCELLED because a newer run of the same workflow replaced it (re-run, concurrency group) is not a
    # failure when a sibling of that name passed; with no passing sibling it still is.
    passed = {r["name"] for r in runs if r["status"] == "COMPLETED" and (r["conclusion"] or "") in OK}
    failed = ([r["name"] for r in runs if r["status"] == "COMPLETED" and (r["conclusion"] or "") not in OK
               and not (r["conclusion"] == "CANCELLED" and r["name"] in passed)]
              + [x["name"] for x in statuses if x["state"] in ("FAILURE", "ERROR")])
    pending = (any(r["status"] != "COMPLETED" for r in runs)
               or any(x["state"] in ("PENDING", "EXPECTED") for x in statuses))
    return ("PENDING" if pending else "FAILURE" if failed else "SUCCESS"), failed


def deploy_state(s: dict, env: str) -> tuple[bool, str | None]:
    """(live in `env`, failure text or None). `env` "any" accepts every environment."""
    names = [n for n in (s.get("envs") or {}) if env == "any" or n.lower() == env.lower()]
    failure = None
    for n in names:
        e = s["envs"][n]
        if e.get("this") == "SUCCESS" or e.get("live"):
            return True, None
        if e.get("this") in ("FAILURE", "ERROR"):
            failure = f"the {n} deployment of this commit {e['this']}"
    for x in s.get("statuses") or []:  # e.g. a `deployed/production` commit status
        name = (x["name"] or "").lower()
        if "deploy" in name and (env == "any" or env.lower() in name):
            if x["state"] == "SUCCESS":
                return True, None
            if x["state"] in ("FAILURE", "ERROR"):
                failure = f"status {x['name']}={x['state']}"
    return False, failure


def fingerprint(s: dict) -> tuple:
    return (tuple((r["name"], r["status"], r["conclusion"]) for r in s["runs"]),
            tuple((x["name"], x["state"]) for x in s["statuses"]),
            tuple(sorted((k, v.get("this"), bool(v.get("live")), (v.get("last_success") or {}).get("sha"))
                         for k, v in (s.get("envs") or {}).items())))


def evaluate(cond: str, s: dict, baseline: dict | None, env: str = "production",
             workflow: str | None = None) -> tuple[bool, str | None]:
    """(met, reason it can no longer be met or None)."""
    if cond == "change":
        return baseline is not None and fingerprint(s) != fingerprint(baseline), None
    state, failed = rollup(s, workflow)
    if cond == "checks":
        return state in ("SUCCESS", "FAILURE"), None
    if cond == "green":
        return state == "SUCCESS", (f"failed: {', '.join(failed)}" if state == "FAILURE" else None)
    if cond == "deployed":
        live, failure = deploy_state(s, env)
        return live, (None if live else failure)
    raise ValueError(f"unknown condition '{cond}' for a commit (use: {', '.join(CONDITIONS)})")


def one_line(ref: str, s: dict, workflow: str | None = None) -> str:
    state, failed = rollup(s, workflow)
    bits = [f"{ref} ({s['oid'][:10]})", f"checks={state or 'none yet'}"]
    shown = [r for r in s["runs"] if (workflow or "").lower() in (r["name"] or "").lower()]
    if shown:
        bits.append("runs: " + ", ".join(f"{r['name']}={r['conclusion'] or r['status']}" for r in shown[:8])
                    + (f" (+{len(shown) - 8})" if len(shown) > 8 else ""))
    if failed:
        bits.append("failing: " + ", ".join(failed[:6]))
    deps = []
    for env, e in sorted((s.get("envs") or {}).items()):
        if e.get("this") == "SUCCESS" or e.get("live"):
            via = "" if e.get("this") == "SUCCESS" else f" via {(e.get('last_success') or {}).get('sha', '')[:8]}"
            deps.append(f"{env}=live{via}")
        elif e.get("this"):
            deps.append(f"{env}={e['this']}")
        elif e.get("live") is False:
            deps.append(f"{env}=not yet")
    bits.append("deploys: " + (", ".join(deps) if deps else "none recorded for this commit"))
    return "  ".join(bits)
