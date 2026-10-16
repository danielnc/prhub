//! What `prhub wait --until X` means for a pull request, as pure functions over one PR record.

use crate::github::Failing;
use crate::util::{dig, s, truthy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

pub const CONDITIONS: [&str; 7] = ["done", "green", "checks", "merged", "closed", "review", "change"];

/// The fields that matter, flattened from the GraphQL shape.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Summary {
    pub state: Option<String>,
    pub draft: bool,
    pub checks: Option<String>,
    pub review: Option<String>,
    pub approved_head: bool,
    pub mergeable: Option<String>,
    pub merge_state: Option<String>,
    pub head: String,
    pub base: Option<String>,
    pub reviews: Vec<String>,
    pub title: Option<String>,
    pub url: Option<String>,
}

/// How the original Python prhub printed an optional field inside a review key: missing -> "None".
fn py_field(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn summary(pr: &Value) -> Summary {
    let commit = dig(pr, &["commits", "nodes", "0", "commit"]);
    let mut rollup = s(commit, &["statusCheckRollup", "state"]);
    // GitHub's rollup counts check runs only. A workflow run whose jobs are not created yet (queued for a runner,
    // waiting on a concurrency group, expanding a matrix) has a check suite but no runs, so the rollup can say SUCCESS
    // from the workflows that already finished. An unfinished GitHub Actions suite on the head keeps checks PENDING.
    // Suites with no workflow run are apps that may never start one; cron runs land on whatever the head is. More
    // suites than one page means an unread one may still be running, so that stays PENDING too (fails closed).
    let suites = dig(commit, &["checkSuites"]);
    if matches!(rollup.as_deref(), None | Some("SUCCESS")) {
        let more = truthy(dig(suites, &["pageInfo", "hasNextPage"]));
        let unfinished = dig(suites, &["nodes"]).as_array().into_iter().flatten().any(|su| {
            !su.is_null()
                && su.get("status").and_then(Value::as_str) != Some("COMPLETED")
                && matches!(s(su, &["workflowRun", "event"]).as_deref(), Some(e) if e != "schedule")
        });
        if more || unfinished {
            rollup = Some("PENDING".into());
        }
    }
    let reviews: Vec<&Value> =
        dig(pr, &["latestReviews", "nodes"]).as_array().into_iter().flatten().filter(|r| !r.is_null()).collect();
    let head = s(pr, &["headRefOid"]).unwrap_or_default();
    let committed = s(commit, &["committedDate"]).unwrap_or_default();
    // reviewDecision stays APPROVED after a new push unless the repo dismisses stale reviews, so "approved" must mean an
    // APPROVED review of THIS head. GitHub also relabels an old review onto a new head when a force-push leaves the
    // patch unchanged, so the review must be newer than the head commit too.
    let approved_head = reviews.iter().any(|r| {
        s(r, &["state"]).as_deref() == Some("APPROVED")
            && s(r, &["commit", "oid"]).as_deref() == Some(head.as_str())
            && s(r, &["submittedAt"]).unwrap_or_default() >= committed
    });
    let mut keys: Vec<String> = reviews
        .iter()
        .map(|r| {
            let login = match dig(r, &["author"]) {
                Value::Object(a) => a.get("login").map(py_field).unwrap_or_else(|| "?".into()),
                _ => "?".into(),
            };
            format!("{login}:{}@{}", py_field(dig(r, &["state"])), py_field(dig(r, &["submittedAt"])))
        })
        .collect();
    keys.sort();
    Summary {
        state: s(pr, &["state"]),
        draft: truthy(dig(pr, &["isDraft"])),
        checks: rollup,
        review: s(pr, &["reviewDecision"]),
        approved_head,
        mergeable: s(pr, &["mergeable"]),
        merge_state: s(pr, &["mergeStateStatus"]),
        head: head.chars().take(10).collect(),
        base: s(pr, &["baseRefName"]),
        reviews: keys,
        title: s(pr, &["title"]),
        url: s(pr, &["url"]),
    }
}

/// What counts as "something changed" for --until change.
pub type Fingerprint =
    (Option<String>, bool, Option<String>, Option<String>, bool, Option<String>, String, Vec<String>);

pub fn fingerprint(s: &Summary) -> Fingerprint {
    (
        s.state.clone(),
        s.draft,
        s.checks.clone(),
        s.review.clone(),
        s.approved_head,
        s.merge_state.clone(),
        s.head.clone(),
        s.reviews.clone(),
    )
}

pub fn is_closed(s: &Summary) -> bool {
    matches!(s.state.as_deref(), Some("MERGED") | Some("CLOSED"))
}

/// Ready to merge: open, not draft, approved AT THE CURRENT HEAD, CI green, mergeable in one of `merge_states`.
pub fn is_done(s: &Summary, merge_states: &[String]) -> bool {
    s.state.as_deref() == Some("OPEN")
        && !s.draft
        && s.review.as_deref() == Some("APPROVED")
        && s.approved_head
        && s.checks.as_deref() == Some("SUCCESS")
        && s.merge_state.as_ref().is_some_and(|m| merge_states.contains(m))
}

/// (met, impossible). `impossible` = the PR closed in a way that can never satisfy `cond`.
pub fn evaluate(
    cond: &str,
    s: &Summary,
    baseline: Option<&Summary>,
    merge_states: &[String],
) -> Result<(bool, bool), String> {
    Ok(match cond {
        "merged" => (s.state.as_deref() == Some("MERGED"), s.state.as_deref() == Some("CLOSED")),
        "closed" => (is_closed(s), false),
        "change" => (baseline.is_some_and(|b| fingerprint(s) != fingerprint(b)), false),
        "review" => {
            let new = baseline.is_some_and(|b| {
                let old: BTreeSet<&String> = b.reviews.iter().collect();
                s.reviews.iter().any(|r| !old.contains(r))
            });
            (new, is_closed(s))
        }
        "checks" => (!matches!(s.checks.as_deref(), None | Some("PENDING") | Some("EXPECTED")), is_closed(s)),
        "green" => (s.checks.as_deref() == Some("SUCCESS"), is_closed(s)),
        "done" => (is_done(s, merge_states), is_closed(s)),
        _ => return Err(format!("unknown condition '{cond}' (use one of: {})", CONDITIONS.join(", "))),
    })
}

pub fn one_line(r: &str, s: &Summary, failing: &[Failing]) -> String {
    let mut bits = vec![r.to_string(), s.state.clone().unwrap_or_else(|| "?".into())];
    if s.draft {
        bits.push("draft".into());
    }
    let mut review = s.review.clone().unwrap_or_else(|| "none".into());
    if review == "APPROVED" && !s.approved_head {
        review = "APPROVED(stale: no approval of this head)".into();
    }
    bits.push(format!("checks={}", s.checks.as_deref().unwrap_or("none")));
    bits.push(format!("review={review}"));
    bits.push(format!("merge={}", s.merge_state.as_deref().unwrap_or("?")));
    bits.push(format!("head={}", s.head));
    if !failing.is_empty() {
        bits.push(format!(
            "failing: {}",
            failing.iter().take(6).map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    bits.join(" ")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub const HEAD_COMMITTED: &str = "2026-10-11T08:00:00Z";

    /// (author, state, submittedAt, commit oid or None for the head)
    pub type Review = (&'static str, &'static str, &'static str, Option<&'static str>);

    pub struct Pr {
        pub state: &'static str,
        pub draft: bool,
        pub checks: Option<&'static str>,
        pub review: &'static str,
        pub merge: &'static str,
        pub head: &'static str,
        pub reviews: Option<Vec<Review>>,
        pub suites: Vec<(&'static str, Option<&'static str>)>,
    }

    impl Default for Pr {
        fn default() -> Self {
            Pr {
                state: "OPEN",
                draft: false,
                checks: Some("SUCCESS"),
                review: "APPROVED",
                merge: "CLEAN",
                head: "abc123",
                reviews: None,
                suites: vec![],
            }
        }
    }

    /// A PR node as GitHub returns it. By default an APPROVED review decision comes with one approval of the head,
    /// submitted after the head commit. `suites`: (status, workflow run event or None for an app that is not Actions).
    pub fn pr(p: Pr) -> Value {
        let reviews = p.reviews.clone().unwrap_or_else(|| {
            if p.review == "APPROVED" { vec![("approver", "APPROVED", "2026-10-11T09:00:00Z", None)] } else { vec![] }
        });
        json!({
            "state": p.state, "isDraft": p.draft, "reviewDecision": p.review, "mergeStateStatus": p.merge,
            "headRefOid": p.head, "mergeable": "MERGEABLE", "baseRefName": "main", "title": "t", "url": "u",
            "commits": {"nodes": [{"commit": {
                "oid": p.head, "committedDate": HEAD_COMMITTED,
                "statusCheckRollup": p.checks.map(|c| json!({"state": c})),
                "checkSuites": {"nodes": p.suites.iter().map(|(st, ev)| json!({
                    "status": st, "workflowRun": ev.map(|e| json!({"event": e}))})).collect::<Vec<_>>()}}}]},
            "latestReviews": {"nodes": reviews.iter().map(|(a, st, at, oid)| json!({
                "author": {"login": a}, "state": st, "submittedAt": at, "commit": {"oid": oid.unwrap_or(p.head)}}))
                .collect::<Vec<_>>()},
        })
    }

    fn ms() -> Vec<String> {
        vec!["CLEAN".into(), "HAS_HOOKS".into()]
    }

    fn ev(cond: &str, s: &Summary) -> (bool, bool) {
        evaluate(cond, s, None, &ms()).unwrap()
    }

    #[test]
    fn done_needs_everything() {
        assert_eq!(ev("done", &summary(&pr(Pr::default()))), (true, false));
        for bad in [
            Pr { draft: true, ..Default::default() },
            Pr { review: "REVIEW_REQUIRED", ..Default::default() },
            Pr { checks: Some("PENDING"), ..Default::default() },
            Pr { merge: "BLOCKED", ..Default::default() },
        ] {
            assert!(!ev("done", &summary(&pr(bad))).0);
        }
    }

    #[test]
    fn done_needs_an_approval_of_the_current_head() {
        // reviewDecision stays APPROVED after a push; the only approval is of the previous head
        let stale = summary(&pr(Pr {
            head: "new456",
            reviews: Some(vec![("bot", "APPROVED", "2026-10-11T09:00:00Z", Some("old123"))]),
            ..Default::default()
        }));
        assert!(stale.review.as_deref() == Some("APPROVED") && !stale.approved_head);
        assert_eq!(ev("done", &stale), (false, false));
        assert!(one_line("a/b#1", &stale, &[]).contains("stale"));
    }

    #[test]
    fn done_rejects_an_approval_relabelled_onto_the_head() {
        // a force-push with an unchanged patch moves an older review onto the new head: it predates the head commit
        let relabelled = summary(&pr(Pr {
            reviews: Some(vec![("bot", "APPROVED", "2026-10-11T07:00:00Z", None)]),
            ..Default::default()
        }));
        assert!(!relabelled.approved_head && !ev("done", &relabelled).0);
    }

    #[test]
    fn green_waits_for_a_workflow_whose_jobs_do_not_exist_yet() {
        let early = summary(&pr(Pr {
            suites: vec![("COMPLETED", Some("pull_request")), ("QUEUED", Some("pull_request"))],
            ..Default::default()
        }));
        assert_eq!(early.checks.as_deref(), Some("PENDING"));
        for cond in ["green", "done", "checks"] {
            assert_eq!(ev(cond, &early), (false, false));
        }
        let none = summary(&pr(Pr {
            checks: None,
            suites: vec![("IN_PROGRESS", Some("pull_request"))],
            ..Default::default()
        }));
        assert_eq!(none.checks.as_deref(), Some("PENDING"));
    }

    #[test]
    fn suites_that_say_nothing_about_the_head_do_not_hold_green() {
        let s = summary(&pr(Pr {
            suites: vec![("COMPLETED", Some("pull_request")), ("QUEUED", None), ("IN_PROGRESS", Some("schedule"))],
            ..Default::default()
        }));
        assert!(s.checks.as_deref() == Some("SUCCESS") && ev("green", &s) == (true, false));
        let red = summary(&pr(Pr {
            checks: Some("FAILURE"),
            suites: vec![("QUEUED", Some("pull_request"))],
            ..Default::default()
        }));
        assert_eq!(red.checks.as_deref(), Some("FAILURE"));
    }

    #[test]
    fn more_suites_than_one_page_keep_checks_pending() {
        let mut many = pr(Pr { suites: vec![("COMPLETED", Some("pull_request"))], ..Default::default() });
        many["commits"]["nodes"][0]["commit"]["checkSuites"]["pageInfo"] = json!({"hasNextPage": true});
        let s = summary(&many);
        assert!(s.checks.as_deref() == Some("PENDING") && ev("done", &s) == (false, false));
    }

    #[test]
    fn closed_and_merged() {
        assert_eq!(ev("done", &summary(&pr(Pr { state: "MERGED", ..Default::default() }))), (false, true));
        assert_eq!(ev("merged", &summary(&pr(Pr { state: "CLOSED", ..Default::default() }))), (false, true));
        assert_eq!(ev("closed", &summary(&pr(Pr { state: "CLOSED", ..Default::default() }))), (true, false));
    }

    #[test]
    fn checks_finished_includes_failure_but_not_pending() {
        assert!(ev("checks", &summary(&pr(Pr { checks: Some("FAILURE"), ..Default::default() }))).0);
        assert!(!ev("checks", &summary(&pr(Pr { checks: Some("PENDING"), ..Default::default() }))).0);
        assert!(!ev("checks", &summary(&pr(Pr { checks: None, ..Default::default() }))).0);
    }

    #[test]
    fn change_and_review_compare_against_baseline() {
        let base = summary(&pr(Pr { checks: Some("PENDING"), ..Default::default() }));
        let m = ms();
        assert!(!evaluate("change", &base, Some(&base), &m).unwrap().0);
        assert!(evaluate("change", &summary(&pr(Pr::default())), Some(&base), &m).unwrap().0);
        let newer = summary(&pr(Pr {
            checks: Some("PENDING"),
            reviews: Some(vec![("bot", "APPROVED", "2026-10-11T10:00:00Z", None)]),
            ..Default::default()
        }));
        assert!(evaluate("review", &newer, Some(&base), &m).unwrap().0);
        assert!(!evaluate("review", &base, Some(&base), &m).unwrap().0);
        assert!(evaluate("nope", &base, None, &m).unwrap_err().contains("unknown condition"));
    }

    #[test]
    fn fingerprint_ignores_title_and_reviews_are_python_shaped() {
        let a = summary(&pr(Pr::default()));
        let mut b = a.clone();
        b.title = Some("renamed".into());
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_eq!(a.reviews, vec!["approver:APPROVED@2026-10-11T09:00:00Z"]);
        let odd = summary(&json!({"latestReviews": {"nodes": [null, {"author": null, "state": "COMMENTED"}]}}));
        assert_eq!(odd.reviews, vec!["?:COMMENTED@None"]);
        assert_eq!(odd.head, "");
    }

    #[test]
    fn one_line_shape() {
        let s = summary(&pr(Pr { draft: true, checks: Some("FAILURE"), ..Default::default() }));
        let f = vec![Failing { name: "test".into(), conclusion: Some("FAILURE".into()), url: None }];
        assert_eq!(
            one_line("acme/api#1", &s, &f),
            "acme/api#1 OPEN draft checks=FAILURE review=APPROVED merge=CLEAN head=abc123 failing: test"
        );
        assert_eq!(one_line("x#1", &Summary::default(), &[]), "x#1 ? checks=none review=none merge=? head=");
    }
}
