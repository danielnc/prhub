"""What `prhub wait --until X` means, as pure functions over one PR record (so they are easy to test)."""
from __future__ import annotations

PENDING_ROLLUP = {None, "PENDING", "EXPECTED"}


def summary(pr: dict) -> dict:
    """The fields that matter, flattened from the GraphQL shape."""
    commit = ((((pr.get("commits") or {}).get("nodes") or [{}])[0]) or {}).get("commit") or {}
    rollup = (commit.get("statusCheckRollup") or {}).get("state")
    # GitHub's rollup counts check runs only. A workflow run whose jobs are not created yet (queued for a runner,
    # waiting on a concurrency group, expanding a matrix) has a check suite but no runs, so the rollup can say SUCCESS
    # from the workflows that already finished. An unfinished GitHub Actions suite on the head keeps checks PENDING.
    # Suites with no workflow run are apps that may never start one; cron runs land on whatever the head is.
    if rollup in (None, "SUCCESS") and any(
            su and su.get("status") != "COMPLETED" and (su.get("workflowRun") or {}).get("event") not in (None, "schedule")
            for su in ((commit.get("checkSuites") or {}).get("nodes") or [])):
        rollup = "PENDING"
    reviews = [r for r in ((pr.get("latestReviews") or {}).get("nodes") or []) if r]
    head = pr.get("headRefOid") or ""
    committed = commit.get("committedDate") or ""
    # reviewDecision stays APPROVED after a new push unless the repo dismisses stale reviews, so "approved" must
    # mean an APPROVED review of THIS head. GitHub also relabels an old review onto a new head when a force-push
    # leaves the patch unchanged, so the review must be newer than the head commit too.
    approved_head = any(r.get("state") == "APPROVED" and ((r.get("commit") or {}).get("oid") == head)
                        and (r.get("submittedAt") or "") >= committed for r in reviews)
    return {
        "state": pr.get("state"),
        "draft": bool(pr.get("isDraft")),
        "checks": rollup,
        "review": pr.get("reviewDecision"),
        "approved_head": approved_head,
        "mergeable": pr.get("mergeable"),
        "merge_state": pr.get("mergeStateStatus"),
        "head": (pr.get("headRefOid") or "")[:10],
        "base": pr.get("baseRefName"),
        "reviews": sorted(f"{(r.get('author') or {}).get('login', '?')}:{r.get('state')}@{r.get('submittedAt')}"
                          for r in reviews),
        "title": pr.get("title"),
        "url": pr.get("url"),
    }


def fingerprint(s: dict) -> tuple:
    """What counts as 'something changed' for --until change."""
    return (s["state"], s["draft"], s["checks"], s["review"], s.get("approved_head"), s["merge_state"], s["head"],
            tuple(s["reviews"]))


def is_closed(s: dict) -> bool:
    return s["state"] in ("MERGED", "CLOSED")


def is_done(s: dict, merge_states: list[str]) -> bool:
    """Ready to merge: open, not draft, approved AT THE CURRENT HEAD, CI green, mergeable in one of `merge_states`."""
    return (s["state"] == "OPEN" and not s["draft"] and s["review"] == "APPROVED" and s.get("approved_head") is True
            and s["checks"] == "SUCCESS" and s["merge_state"] in merge_states)


CONDITIONS = ("done", "green", "checks", "merged", "closed", "review", "change")


def evaluate(cond: str, s: dict, baseline: dict | None, merge_states: list[str]) -> tuple[bool, bool]:
    """(met, impossible). `impossible` = the PR closed in a way that can never satisfy `cond`."""
    if cond == "merged":
        return s["state"] == "MERGED", s["state"] == "CLOSED"
    if cond == "closed":
        return is_closed(s), False
    if cond == "change":
        return baseline is not None and fingerprint(s) != fingerprint(baseline), False
    if cond == "review":
        new = baseline is not None and set(s["reviews"]) - set(baseline["reviews"])
        return bool(new), is_closed(s)
    if cond == "checks":
        return s["checks"] not in PENDING_ROLLUP, is_closed(s)
    if cond == "green":
        return s["checks"] == "SUCCESS", is_closed(s)
    if cond == "done":
        return is_done(s, merge_states), is_closed(s)
    raise ValueError(f"unknown condition '{cond}' (use one of: {', '.join(CONDITIONS)})")


def one_line(ref: str, s: dict, failing: list[dict] | None = None) -> str:
    bits = [ref, s["state"] or "?"]
    if s["draft"]:
        bits.append("draft")
    review = s["review"] or "none"
    if review == "APPROVED" and not s.get("approved_head"):
        review = "APPROVED(stale: no approval of this head)"
    bits += [f"checks={s['checks'] or 'none'}", f"review={review}",
             f"merge={s['merge_state'] or '?'}", f"head={s['head']}"]
    if failing:
        bits.append("failing: " + ", ".join(f["name"] for f in failing[:6]))
    return " ".join(bits)
