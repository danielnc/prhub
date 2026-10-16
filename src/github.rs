//! The only module that talks to GitHub: batched GraphQL reads plus two small REST calls, rate-limit aware.

use crate::commits::{CommitRef, alias_query as commit_alias_query};
use crate::refs::PrRef;
use crate::util::{RunError, dig, run_timeout, s, take, truthy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

pub const API: &str = "https://api.github.com/graphql";

pub const PR_FIELDS: &str = "
  number title url state isDraft mergeable mergeStateStatus reviewDecision
  headRefName baseRefName headRefOid updatedAt mergedAt closedAt
  author { login }
  repository { nameWithOwner }
  commits(last: 1) { nodes { commit { oid committedDate statusCheckRollup { state }
    checkSuites(first: 50) { pageInfo { hasNextPage } nodes { status workflowRun { event } } } } } }
  latestReviews(first: 10) { nodes { author { login } state submittedAt commit { oid } } }
  comments { totalCount }
";

const FAILING_FIELDS: &str = "
  commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 60) { nodes {
    __typename
    ... on CheckRun { name status conclusion detailsUrl }
    ... on StatusContext { context state targetUrl }
  } } } } } }
";

/// A failed call. `transient` means retrying later should work: a 5xx, a network error, GitHub's own query timeout,
/// or a slow `gh`. Anything else (bad credentials, a malformed query) needs a person.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GhError {
    pub msg: String,
    pub rate_limited: bool,
    pub reset_at: Option<String>,
    pub transient: bool,
}

impl GhError {
    pub fn new(msg: impl Into<String>) -> Self {
        GhError { msg: msg.into(), ..Default::default() }
    }

    pub fn transient(msg: impl Into<String>) -> Self {
        GhError { msg: msg.into(), transient: true, ..Default::default() }
    }
}

impl fmt::Display for GhError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

/// One failing check of a PR: its name, how it ended and where to read it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Failing {
    pub name: String,
    pub conclusion: Option<String>,
    pub url: Option<String>,
}

/// (status, conclusion) of a workflow run, upper-cased like GraphQL's.
pub type RunState = (Option<String>, Option<String>);

/// PRs read in one go (None: the PR no longer resolves), and the last rateLimit GitHub reported.
pub type PrBatch = (Vec<(PrRef, Option<Value>)>, Value);

/// Repository nodes of the commits read in one go, and the last rateLimit GitHub reported.
pub type CommitBatch = (Vec<(CommitRef, Value)>, Value);

/// What one HTTP exchange produced: Ok((status, x-ratelimit-reset, body)) or Err(why no response arrived).
pub type Reply = Result<(u16, Option<String>, String), String>;

/// 429 is always a rate limit; a 403 is one only when GitHub says so (otherwise it is a permission error).
pub fn rate_limited(code: u16, text: &str) -> bool {
    code == 429 || (code == 403 && text.to_lowercase().contains("rate limit"))
}

/// One HTTP exchange: Ok((status, x-ratelimit-reset, body)) for any response, Err(message) when none arrived (a
/// connection that failed, or a body cut short mid-read).
pub trait Transport: Sync {
    fn send(&self, method: &str, url: &str, token: &str, body: Option<&str>) -> Reply;
}

/// How many requests to GitHub run at once. A poll's batches and REST calls used to go one after another, so a poll
/// took as long as all of them together; GitHub's secondary rate limits penalise much more concurrency than this.
pub const CONCURRENCY: usize = 4;

/// `f` over every item, `workers` at a time, answers in the items' order.
pub fn parallel<T: Sync, R: Send>(items: &[T], workers: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if items.len() <= 1 || workers <= 1 {
        return items.iter().map(&f).collect();
    }
    let next = AtomicUsize::new(0);
    let out: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers.min(items.len()) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    *out[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                }
            });
        }
    });
    out.into_iter().filter_map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner())).collect()
}

/// When a rate-limited request may be retried, as epoch seconds: GitHub's `x-ratelimit-reset` for the primary limit,
/// else `Retry-After` (seconds from now) for a secondary limit.
pub fn reset_from(ratelimit_reset: Option<&str>, retry_after: Option<&str>, now: i64) -> Option<String> {
    if let Some(r) = ratelimit_reset.filter(|r| !r.is_empty()) {
        return Some(r.to_string());
    }
    retry_after.and_then(|r| r.trim().parse::<i64>().ok()).map(|secs| (now + secs).to_string())
}

pub struct Ureq {
    agent: ureq::Agent,
}

impl Default for Ureq {
    fn default() -> Self {
        Ureq { agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build() }
    }
}

impl Transport for Ureq {
    fn send(&self, method: &str, url: &str, token: &str, body: Option<&str>) -> Reply {
        let req = self
            .agent
            .request(method, url)
            .set("Authorization", &format!("bearer {token}"))
            .set("User-Agent", "prhub")
            .set("Accept", "application/vnd.github+json");
        let res = match body {
            Some(b) => req.set("Content-Type", "application/json").send_string(b),
            None => req.call(),
        };
        let resp = match res {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(e.to_string()),
        };
        let status = resp.status();
        let reset = reset_from(resp.header("x-ratelimit-reset"), resp.header("retry-after"), crate::util::now_i());
        let text = resp.into_string().map_err(|e| e.to_string())?;
        Ok((status, reset, text))
    }
}

/// The token `gh` is logged in with.
pub fn gh_token(timeout: u64) -> Result<String, GhError> {
    token_from(run_timeout("gh", &["auth", "token"], Duration::from_secs(timeout)), timeout)
}

/// What `gh auth token` answered, as a token or an error that says what to do.
pub fn token_from(answer: Result<(i32, String), RunError>, timeout: u64) -> Result<String, GhError> {
    match answer {
        Err(RunError::Timeout) => Err(GhError::transient(format!("`gh auth token` took longer than {timeout}s"))),
        Err(RunError::Spawn(e)) => Err(GhError::new(format!("cannot run gh: {e}"))),
        Ok((_, out)) if out.trim().is_empty() => Err(GhError::new("no GitHub token: run `gh auth login`")),
        Ok((_, out)) => Ok(out.trim().to_string()),
    }
}

pub fn graphql(t: &dyn Transport, query: &str, variables: Value, token: &str) -> Result<Value, GhError> {
    let body = json!({"query": query, "variables": variables}).to_string();
    let (code, reset, text) =
        t.send("POST", API, token, Some(&body)).map_err(|e| GhError::transient(format!("network: {e}")))?;
    if !(200..300).contains(&code) {
        let text = take(&text, 300);
        return Err(GhError {
            msg: format!("HTTP {code}: {text}"),
            rate_limited: rate_limited(code, &text),
            reset_at: reset,
            transient: code >= 500,
        });
    }
    let payload: Value = serde_json::from_str(&text).map_err(|e| GhError::transient(format!("network: {e}")))?;
    let errors = payload.get("errors").and_then(Value::as_array).cloned().unwrap_or_default();
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    if !errors.is_empty() && !truthy(&data) {
        let msg = errors
            .iter()
            .map(|e| e.get("message").and_then(Value::as_str).unwrap_or("?").to_string())
            .collect::<Vec<_>>()
            .join("; ");
        let lower = msg.to_lowercase();
        let limited = errors.iter().any(|e| e.get("type").and_then(Value::as_str) == Some("RATE_LIMITED"))
            || lower.contains("rate limit");
        // "Something went wrong while executing your query" is GitHub timing out on a heavy query.
        return Err(GhError {
            msg: format!("GraphQL: {msg}"),
            rate_limited: limited,
            reset_at: None,
            transient: lower.contains("something went wrong") || lower.contains("timeout"),
        });
    }
    Ok(if truthy(&data) { data } else { json!({}) })
}

fn rest_get(t: &dyn Transport, url: &str, token: &str, label: &str) -> Result<Value, GhError> {
    let (code, reset, text) = t.send("GET", url, token, None).map_err(|e| GhError::new(format!("network: {e}")))?;
    if !(200..300).contains(&code) {
        let text = take(&text, 200);
        return Err(GhError {
            msg: format!("{label} HTTP {code}: {text}"),
            rate_limited: rate_limited(code, &text),
            reset_at: reset,
            ..Default::default()
        });
    }
    serde_json::from_str(&text).map_err(|e| GhError::new(format!("network: {e}")))
}

fn pr_alias_query(refs: &[PrRef], fields: &str) -> String {
    let parts: Vec<String> = refs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            format!(
                "p{i}: repository(owner: {}, name: {}) {{ pullRequest(number: {}) {{ {fields} }} }}",
                json!(r.owner),
                json!(r.repo),
                r.number
            )
        })
        .collect();
    format!("query {{ rateLimit {{ cost remaining resetAt }}\n{}\n}}", parts.join("\n"))
}

/// Consecutive failures in one read: past this it is an outage, and splitting adds load.
pub const MAX_TRANSIENT_FAILURES: usize = 4;

/// Read `refs` `chunk` per GraphQL call. When GitHub fails a call transiently (a 502/504, or its "Something went
/// wrong" timeout on a heavy query), split that batch in half and retry, down to single items. An item that still
/// fails is left out of the result, so one heavy PR or commit cannot stop all the others from being read; its previous
/// record is kept. Split halves go to the back of the queue, and any success resets the count, so more than
/// MAX_TRANSIENT_FAILURES consecutive failures mean an outage and raise, while one bad item among many is isolated.
pub fn batched<R: Clone + Send + Sync>(
    t: &dyn Transport,
    refs: &[R],
    chunk: usize,
    workers: usize,
    build: impl Fn(&[R]) -> String + Sync,
    read: impl Fn(&Value, usize) -> Value + Sync,
    token: &str,
) -> Result<(Vec<(R, Value)>, Value), GhError> {
    struct Work<R> {
        pending: VecDeque<Vec<(usize, R)>>,
        busy: usize,
        failures: usize,
        error: Option<GhError>,
        out: Vec<(usize, R, Value)>,
        rate: Value,
    }
    let indexed: Vec<(usize, R)> = refs.iter().cloned().enumerate().collect();
    let work = Mutex::new(Work {
        pending: indexed.chunks(chunk.max(1)).map(|c| c.to_vec()).collect(),
        busy: 0,
        failures: 0,
        error: None,
        out: Vec::new(),
        rate: Value::Null,
    });
    let changed = Condvar::new();
    let worker = || loop {
        let part = {
            let mut w = work.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if w.error.is_some() {
                    return;
                }
                if let Some(p) = w.pending.pop_front() {
                    w.busy += 1;
                    break p;
                }
                if w.busy == 0 {
                    return; // nothing left, and nobody still running can split a batch into more work
                }
                w = changed.wait(w).unwrap_or_else(|e| e.into_inner());
            }
        };
        let only: Vec<R> = part.iter().map(|(_, r)| r.clone()).collect();
        let answer = graphql(t, &build(&only), json!({}), token);
        let mut w = work.lock().unwrap_or_else(|e| e.into_inner());
        w.busy -= 1;
        match answer {
            Ok(d) => {
                w.failures = 0;
                if truthy(&d["rateLimit"]) {
                    w.rate = d["rateLimit"].clone();
                }
                for (j, (i, r)) in part.into_iter().enumerate() {
                    w.out.push((i, r, read(&d, j)));
                }
            }
            Err(e) if !e.transient => w.error = Some(e),
            Err(e) => {
                w.failures += 1;
                if w.failures > MAX_TRANSIENT_FAILURES {
                    w.error = Some(e);
                } else if part.len() > 1 {
                    let half = part.len() / 2;
                    w.pending.push_back(part[..half].to_vec());
                    w.pending.push_back(part[half..].to_vec());
                }
            }
        }
        changed.notify_all();
    };
    std::thread::scope(|scope| {
        for _ in 1..workers.max(1) {
            scope.spawn(worker);
        }
        worker();
    });
    let w = work.into_inner().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = w.error {
        return Err(e);
    }
    let mut out = w.out;
    out.sort_by_key(|(i, _, _)| *i);
    Ok((out.into_iter().map(|(_, r, v)| (r, v)).collect(), w.rate))
}

/// What the daemon needs from GitHub. The real one is `GitHub`; tests use a fake.
pub trait Api: Sync {
    fn token(&self) -> Result<String, GhError>;
    fn discover_open(&self, query: &str, token: &str) -> Result<(Vec<PrRef>, Value), GhError>;
    /// Full state of each PR. A PR that no longer resolves maps to None; one GitHub could not return is missing.
    fn fetch_prs(&self, refs: &[PrRef], token: &str) -> Result<PrBatch, GhError>;
    /// Names and links of the failing checks, only for PRs whose CI just went red (kept out of the hot query).
    fn fetch_failing(&self, refs: &[PrRef], token: &str) -> Result<Vec<(PrRef, Vec<Failing>)>, GhError>;
    /// Raw repository nodes (commit + its repo's recent deployments). A commit GitHub could not return is missing.
    fn fetch_commits(&self, refs: &[CommitRef], token: &str) -> Result<CommitBatch, GhError>;
    /// Whether commit `newer` contains `sha`.
    fn contains(&self, owner: &str, repo: &str, sha: &str, newer: &str, token: &str) -> Result<bool, GhError>;
    fn run_state(&self, owner: &str, repo: &str, run: &str, token: &str) -> Result<RunState, GhError>;
}

pub struct GitHub<T: Transport = Ureq> {
    pub t: T,
}

impl Default for GitHub<Ureq> {
    fn default() -> Self {
        GitHub { t: Ureq::default() }
    }
}

pub fn parse_failing(pr: &Value) -> Vec<Failing> {
    let nodes = dig(pr, &["commits", "nodes", "0", "commit", "statusCheckRollup", "contexts", "nodes"]);
    let mut out = Vec::new();
    for n in nodes.as_array().into_iter().flatten() {
        let kind = s(n, &["__typename"]);
        if kind.as_deref() == Some("CheckRun") {
            let c = s(n, &["conclusion"]);
            if matches!(
                c.as_deref(),
                Some("FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE")
            ) {
                out.push(Failing {
                    name: s(n, &["name"]).unwrap_or_default(),
                    conclusion: c,
                    url: s(n, &["detailsUrl"]),
                });
            }
        } else if kind.as_deref() == Some("StatusContext") {
            let st = s(n, &["state"]);
            if matches!(st.as_deref(), Some("FAILURE" | "ERROR")) {
                out.push(Failing {
                    name: s(n, &["context"]).unwrap_or_default(),
                    conclusion: st,
                    url: s(n, &["targetUrl"]),
                });
            }
        }
    }
    out
}

impl<T: Transport> Api for GitHub<T> {
    fn token(&self) -> Result<String, GhError> {
        gh_token(30)
    }

    fn discover_open(&self, query: &str, token: &str) -> Result<(Vec<PrRef>, Value), GhError> {
        let q = "query($q: String!, $after: String) {
          rateLimit { cost remaining resetAt }
          search(query: $q, type: ISSUE, first: 100, after: $after) {
            pageInfo { hasNextPage endCursor }
            nodes { ... on PullRequest { number repository { nameWithOwner } } }
          }
        }";
        let (mut refs, mut after, mut rate) = (Vec::new(), Value::Null, Value::Null);
        for _ in 0..10 {
            // 1,000 PRs is plenty for one person's open work
            let d = graphql(&self.t, q, json!({"q": query, "after": after}), token)?;
            if truthy(&d["rateLimit"]) {
                rate = d["rateLimit"].clone();
            }
            let search = &d["search"];
            for n in search["nodes"].as_array().into_iter().flatten() {
                if let (Some(nwo), Some(num)) = (n["repository"]["nameWithOwner"].as_str(), n["number"].as_u64()) {
                    if let Some((owner, repo)) = nwo.split_once('/') {
                        refs.push(PrRef::new(owner, repo, num));
                    }
                }
            }
            if !truthy(&search["pageInfo"]["hasNextPage"]) {
                break;
            }
            after = search["pageInfo"]["endCursor"].clone();
        }
        Ok((refs, rate))
    }

    fn fetch_prs(&self, refs: &[PrRef], token: &str) -> Result<PrBatch, GhError> {
        let (out, rate) = batched(
            &self.t,
            refs,
            20,
            CONCURRENCY,
            |part| pr_alias_query(part, PR_FIELDS),
            |d, j| d[format!("p{j}")]["pullRequest"].clone(),
            token,
        )?;
        Ok((out.into_iter().map(|(r, v)| (r, (!v.is_null()).then_some(v))).collect(), rate))
    }

    fn fetch_failing(&self, refs: &[PrRef], token: &str) -> Result<Vec<(PrRef, Vec<Failing>)>, GhError> {
        if refs.is_empty() {
            return Ok(vec![]);
        }
        let d = graphql(&self.t, &pr_alias_query(refs, FAILING_FIELDS), json!({}), token)?;
        Ok(refs
            .iter()
            .enumerate()
            .map(|(j, r)| (r.clone(), parse_failing(&d[format!("p{j}")]["pullRequest"])))
            .collect())
    }

    fn fetch_commits(&self, refs: &[CommitRef], token: &str) -> Result<CommitBatch, GhError> {
        // each one is heavy: up to 50 check suites and 30 deployments
        batched(&self.t, refs, 5, CONCURRENCY, commit_alias_query, |d, j| d[format!("c{j}")].clone(), token)
    }

    fn contains(&self, owner: &str, repo: &str, sha: &str, newer: &str, token: &str) -> Result<bool, GhError> {
        let url = format!("https://api.github.com/repos/{owner}/{repo}/compare/{sha}...{newer}");
        let d = rest_get(&self.t, &url, token, "compare")?;
        Ok(matches!(d["status"].as_str(), Some("ahead" | "identical")))
    }

    fn run_state(&self, owner: &str, repo: &str, run: &str, token: &str) -> Result<RunState, GhError> {
        let url = format!("https://api.github.com/repos/{owner}/{repo}/actions/runs/{run}");
        let d = rest_get(&self.t, &url, token, "run")?;
        let status = d["status"].as_str().unwrap_or("").to_uppercase();
        let conclusion = d["conclusion"].as_str().unwrap_or("").to_uppercase();
        Ok((Some(status), (!conclusion.is_empty()).then_some(conclusion)))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Canned responses, in order: Ok((status, reset, body)) or Err(network message). Requests are recorded.
    #[derive(Default)]
    pub struct FakeTransport {
        pub queue: Mutex<VecDeque<Reply>>,
        pub calls: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl FakeTransport {
        pub fn ok(&self, body: Value) {
            self.queue.lock().unwrap().push_back(Ok((200, None, body.to_string())));
        }
        pub fn status(&self, code: u16, body: &str, reset: Option<&str>) {
            self.queue.lock().unwrap().push_back(Ok((code, reset.map(String::from), body.to_string())));
        }
        pub fn raw(&self, body: &str) {
            self.queue.lock().unwrap().push_back(Ok((200, None, body.to_string())));
        }
        pub fn down(&self, msg: &str) {
            self.queue.lock().unwrap().push_back(Err(msg.to_string()));
        }
    }

    impl Transport for FakeTransport {
        fn send(&self, method: &str, url: &str, _token: &str, body: Option<&str>) -> Reply {
            self.calls.lock().unwrap().push((method.into(), url.into(), body.map(String::from)));
            self.queue.lock().unwrap().pop_front().expect("an unexpected request")
        }
    }

    fn gh() -> GitHub<FakeTransport> {
        GitHub { t: FakeTransport::default() }
    }

    #[test]
    fn rate_limits_are_recognised() {
        let g = gh();
        g.t.status(403, "API rate limit exceeded for user", Some("1791750000"));
        let e = graphql(&g.t, "q", json!({}), "t").unwrap_err();
        assert!(e.rate_limited && !e.transient && e.reset_at.as_deref() == Some("1791750000"));
        g.t.status(502, "bad gateway", None);
        let e = graphql(&g.t, "q", json!({}), "t").unwrap_err();
        assert!(!e.rate_limited && e.transient && e.msg.starts_with("HTTP 502"));
        g.t.ok(json!({"errors": [{"type": "RATE_LIMITED", "message": "API rate limit already exceeded"}]}));
        assert!(graphql(&g.t, "q", json!({}), "t").unwrap_err().rate_limited);
        g.t.ok(json!({"errors": [{"message": "Something went wrong while executing your query"}]}));
        assert!(graphql(&g.t, "q", json!({}), "t").unwrap_err().transient);
        g.t.ok(json!({"errors": [{"message": "Bad credentials"}]}));
        let e = graphql(&g.t, "q", json!({}), "t").unwrap_err();
        assert!(!e.transient && e.msg == "GraphQL: Bad credentials");
        g.t.ok(json!({"errors": [{"message": "partial"}], "data": {"a": 1}}));
        assert_eq!(graphql(&g.t, "q", json!({}), "t").unwrap(), json!({"a": 1}));
        assert!(!rate_limited(403, "Resource not accessible"));
    }

    #[test]
    fn a_429_is_a_rate_limit_whatever_its_body_says() {
        let g = gh();
        g.t.status(429, "", None);
        assert!(g.run_state("acme", "api", "7", "t").unwrap_err().rate_limited);
        g.t.status(429, "", None);
        assert!(g.contains("acme", "api", "aaa", "bbb", "t").unwrap_err().rate_limited);
        g.t.status(429, "", None);
        assert!(graphql(&g.t, "q", json!({}), "t").unwrap_err().rate_limited);
    }

    #[test]
    fn a_response_cut_short_is_a_transient_network_error() {
        let g = gh();
        g.t.down("connection reset");
        let e = graphql(&g.t, "q", json!({}), "t").unwrap_err();
        assert!(e.transient && e.msg.starts_with("network"));
        g.t.raw("{\"data\": {\"rateLim");
        assert!(graphql(&g.t, "q", json!({}), "t").unwrap_err().transient);
        g.t.raw("{\"status\": ");
        assert!(g.run_state("acme", "api", "7", "t").unwrap_err().msg.starts_with("network"));
        g.t.down("timed out");
        assert!(g.contains("acme", "api", "a", "b", "t").unwrap_err().msg.starts_with("network"));
    }

    #[test]
    fn rest_calls() {
        let g = gh();
        for (status, want) in [("ahead", true), ("identical", true), ("behind", false), ("diverged", false)] {
            g.t.ok(json!({"status": status}));
            assert_eq!(g.contains("acme", "api", "aaa", "bbb", "t").unwrap(), want);
        }
        assert!(g.t.calls.lock().unwrap()[0].1.ends_with("/repos/acme/api/compare/aaa...bbb"));
        g.t.status(404, "Not Found", None);
        let e = g.contains("acme", "api", "aaa", "bbb", "t").unwrap_err();
        assert!(e.msg.starts_with("compare HTTP 404") && !e.rate_limited);
        g.t.ok(json!({"status": "completed", "conclusion": "success"}));
        assert_eq!(g.run_state("acme", "api", "7", "t").unwrap(), (Some("COMPLETED".into()), Some("SUCCESS".into())));
        g.t.ok(json!({"status": "in_progress", "conclusion": null}));
        assert_eq!(g.run_state("acme", "api", "7", "t").unwrap(), (Some("IN_PROGRESS".into()), None));
        assert!(g.t.calls.lock().unwrap().last().unwrap().1.ends_with("/repos/acme/api/actions/runs/7"));
        g.t.status(403, "API rate limit exceeded", None);
        assert!(g.run_state("acme", "api", "7", "t").unwrap_err().rate_limited);
        g.t.status(403, "You have exceeded a secondary rate limit", Some("1791750000"));
        let e = g.contains("acme", "api", "aaa", "bbb", "t").unwrap_err();
        assert!(e.rate_limited && e.reset_at.as_deref() == Some("1791750000")); // REST keeps the reset too
    }

    #[test]
    fn discovery_pages_through_the_search() {
        let g = gh();
        g.t.ok(json!({"data": {"rateLimit": {"remaining": 10}, "search": {
            "pageInfo": {"hasNextPage": true, "endCursor": "c1"},
            "nodes": [{"number": 1, "repository": {"nameWithOwner": "acme/api"}}, {}]}}}));
        g.t.ok(json!({"data": {"rateLimit": {"remaining": 9}, "search": {
            "pageInfo": {"hasNextPage": false}, "nodes": [{"number": 2, "repository": {"nameWithOwner": "acme/web"}}]}}}));
        let (refs, rate) = g.discover_open("is:pr", "t").unwrap();
        assert_eq!(refs, vec![PrRef::new("acme", "api", 1), PrRef::new("acme", "web", 2)]);
        assert_eq!(rate["remaining"], 9);
        assert!(g.t.calls.lock().unwrap()[1].2.as_ref().unwrap().contains("\"after\":\"c1\""));
    }

    #[test]
    fn prs_failing_checks_and_commits() {
        let g = gh();
        let a = PrRef::new("acme", "api", 1);
        let b = PrRef::new("acme", "api", 2);
        g.t.ok(json!({"data": {"rateLimit": {"remaining": 5}, "p0": {"pullRequest": {"number": 1}}, "p1": {"pullRequest": null}}}));
        let (out, rate) = g.fetch_prs(&[a.clone(), b.clone()], "t").unwrap();
        assert_eq!(out, vec![(a.clone(), Some(json!({"number": 1}))), (b.clone(), None)]);
        assert_eq!(rate["remaining"], 5);
        let q = g.t.calls.lock().unwrap()[0].2.clone().unwrap();
        assert!(q.contains("p0: repository(owner: \\\"acme\\\", name: \\\"api\\\") { pullRequest(number: 1)"));
        assert!(g.fetch_failing(&[], "t").unwrap().is_empty());
        g.t.ok(json!({"data": {"p0": {"pullRequest": {"commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [
            {"__typename": "CheckRun", "name": "test", "conclusion": "FAILURE", "detailsUrl": "https://example.com/1"},
            {"__typename": "CheckRun", "name": "lint", "conclusion": "SUCCESS"},
            {"__typename": "StatusContext", "context": "ci/ext", "state": "ERROR", "targetUrl": null},
            {"__typename": "StatusContext", "context": "ok", "state": "SUCCESS"}]}}}}]}}}}}));
        let f = g.fetch_failing(std::slice::from_ref(&a), "t").unwrap();
        assert_eq!(f[0].1.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["test", "ci/ext"]);
        let c = CommitRef::new("acme", "api", "abcdef1");
        g.t.ok(json!({"data": {"rateLimit": {"remaining": 3}, "c0": {"object": {"oid": "abcdef1000"}}}}));
        let (out, rate) = g.fetch_commits(std::slice::from_ref(&c), "t").unwrap();
        assert!(out[0].1["object"]["oid"].as_str().unwrap().starts_with("abcdef1") && rate["remaining"] == 3);
    }

    #[test]
    fn a_batch_github_times_out_on_is_split_and_one_bad_item_is_isolated() {
        let g = gh();
        let refs: Vec<PrRef> = (1..=4).map(|n| PrRef::new("acme", "api", n)).collect();
        let timeout = json!({"errors": [{"message": "Something went wrong while executing your query"}]});
        g.t.ok(timeout.clone()); // the batch of 4
        g.t.ok(json!({"data": {"p0": {"pullRequest": {"number": 1}}, "p1": {"pullRequest": {"number": 2}}}}));
        g.t.ok(timeout.clone()); // [3, 4]
        g.t.ok(timeout.clone()); // [3]
        g.t.ok(json!({"data": {"p0": {"pullRequest": {"number": 4}}}}));
        let (out, _) = batched(
            &g.t,
            &refs,
            4,
            1, // one worker: the order of the canned answers is the order of the requests
            |p| pr_alias_query(p, "number"),
            |d, j| d[format!("p{j}")]["pullRequest"].clone(),
            "t",
        )
        .unwrap();
        assert_eq!(out.iter().map(|(r, _)| r.number).collect::<Vec<_>>(), vec![1, 2, 4]);
        for _ in 0..3 * MAX_TRANSIENT_FAILURES {
            g.t.status(504, "gateway timeout", None); // enough for every worker still in flight at the cutoff
        }
        let e = g.fetch_prs(&refs, "t").unwrap_err();
        assert!(e.transient && e.msg.starts_with("HTTP 504"));
        g.t.queue.lock().unwrap().clear();
        g.t.status(401, "Bad credentials", None);
        assert!(!g.fetch_prs(&refs, "t").unwrap_err().transient);
    }

    #[test]
    fn parallel_work_keeps_the_input_order() {
        let items: Vec<u64> = (0..50).collect();
        let out = parallel(&items, CONCURRENCY, |n| {
            std::thread::sleep(Duration::from_millis(50 - n));
            n * 2
        });
        assert_eq!(out, items.iter().map(|n| n * 2).collect::<Vec<_>>());
        assert_eq!(parallel(&[7u64], 4, |n| n + 1), vec![8]);
        assert!(parallel(&Vec::<u64>::new(), 4, |n| n + 1).is_empty());
    }

    #[test]
    fn concurrent_batches_return_every_item_in_order() {
        let g = gh();
        let refs: Vec<PrRef> = (1..=9).map(|n| PrRef::new("acme", "api", n)).collect();
        for _ in 0..5 {
            // batches of two: whichever worker asks gets the next answer, so every answer names p0 and p1
            g.t.ok(json!({"data": {"rateLimit": {"remaining": 7}, "p0": {"pullRequest": {"n": 0}}, "p1": {"pullRequest": {"n": 1}}}}));
        }
        let (out, rate) = batched(
            &g.t,
            &refs,
            2,
            CONCURRENCY,
            |p| pr_alias_query(p, "number"),
            |d, j| d[format!("p{j}")]["pullRequest"].clone(),
            "t",
        )
        .unwrap();
        assert_eq!(out.iter().map(|(r, _)| r.number).collect::<Vec<_>>(), (1..=9).collect::<Vec<_>>());
        assert_eq!(rate["remaining"], 7);
        assert_eq!(g.t.calls.lock().unwrap().len(), 5);
    }

    #[test]
    fn a_secondary_limit_says_when_to_retry() {
        assert_eq!(reset_from(Some("1791750000"), Some("60"), 100).as_deref(), Some("1791750000"));
        assert_eq!(reset_from(None, Some("60"), 100).as_deref(), Some("160"));
        assert_eq!(reset_from(Some(""), Some(" 30 "), 100).as_deref(), Some("130"));
        assert_eq!(reset_from(None, Some("soon"), 100), None);
        assert_eq!(reset_from(None, None, 100), None);
        let g = gh();
        g.t.status(403, "You have exceeded a secondary rate limit", Some("160"));
        let e = graphql(&g.t, "q", json!({}), "t").unwrap_err();
        assert!(e.rate_limited && e.reset_at.as_deref() == Some("160"));
    }

    #[test]
    fn tokens_come_from_gh() {
        assert_eq!(token_from(Ok((0, "tok123\n".into())), 30).unwrap(), "tok123");
        assert!(token_from(Ok((1, "".into())), 30).unwrap_err().msg.contains("gh auth login"));
        let slow = token_from(Err(RunError::Timeout), 30).unwrap_err();
        assert!(slow.transient && slow.msg.contains("longer than 30s"));
        let missing = token_from(Err(RunError::Spawn("No such file".into())), 30).unwrap_err();
        assert!(!missing.transient && missing.msg.starts_with("cannot run gh"));
        assert_eq!(GhError::new("x").to_string(), "x");
    }
}
