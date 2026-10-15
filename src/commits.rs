//! Watch one commit's CI and deployments, in any repo: `owner/repo@sha`.
//!
//! Generic on purpose. Workflow names, default branches (main, develop…) and deploy styles differ per repo, so:
//! - CI is every workflow run on the commit, even one whose jobs do not exist yet, plus every other app's check suite
//!   that actually ran something, minus cron runs (a scheduled workflow attaches to whatever the branch head is, so it
//!   says nothing about this commit) and minus apps that never start a run (some leave a suite QUEUED forever with
//!   zero runs). Past the first 50 suites the commit stays PENDING, since an unread suite may still be running.
//! - "deployed to <env>" comes from GitHub Deployments, which both deploy-on-merge and nightly-release repos record,
//!   plus commit statuses named like `deployed/<env>`. A deploy of a LATER commit counts when that commit contains this
//!   one (the daemon asks GitHub's compare API once per new deployment), because a cancelled or batched deploy means
//!   your own commit may never be deployed by itself.

use crate::refs::{re, RefResolver};
use crate::util::{dig, s, take, truthy};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::OnceLock;

static URL_RE: OnceLock<Regex> = OnceLock::new();
static FULL_RE: OnceLock<Regex> = OnceLock::new();
static NICK_RE: OnceLock<Regex> = OnceLock::new();
static HERE_RE: OnceLock<Regex> = OnceLock::new();
static RUN_URL: OnceLock<Regex> = OnceLock::new();

const URL_P: &str = r"github\.com/([\w.-]+)/([\w.-]+)/commit/([0-9a-fA-F]{7,40})";
const FULL_P: &str = r"^([\w.-]+)/([\w.-]+)@([0-9a-fA-F]{7,40})$";
const NICK_P: &str = r"^([\w .-]+?)\s*@\s*([0-9a-fA-F]{7,40})$";
const HERE_P: &str = r"^@([0-9a-fA-F]{7,40})$";

pub const OK: [&str; 3] = ["SUCCESS", "SKIPPED", "NEUTRAL"];
/// Dispatched runs (a deploy button, a post-merge job) run on whatever the branch head is when they are started, like
/// cron runs, so one cancelled before it started says nothing about this commit.
const DISPATCH: [&str; 2] = ["workflow_dispatch", "repository_dispatch"];
/// Repositories name the same environment differently (production, prod, prd…). A name the repository does not use
/// stands for its usual long and short forms.
const ENV_ALIASES: [&[&str]; 3] =
    [&["production", "prod", "prd"], &["staging", "stage", "stg"], &["development", "dev"]];
pub const CONDITIONS: [&str; 4] = ["checks", "green", "deployed", "change"];

pub fn is_ok(conclusion: Option<&str>) -> bool {
    conclusion.is_some_and(|c| OK.contains(&c))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommitRef {
    pub owner: String,
    pub repo: String,
    /// As given (may be abbreviated); records also carry the full oid.
    pub sha: String,
}

impl fmt::Display for CommitRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}@{}", self.owner, self.repo, self.sha)
    }
}

impl CommitRef {
    pub fn new(owner: &str, repo: &str, sha: &str) -> Self {
        CommitRef { owner: owner.into(), repo: repo.into(), sha: sha.into() }
    }

    pub fn slug(&self) -> String {
        format!("{}__{}__{}", self.owner, self.repo, self.sha)
    }
}

pub fn is_commit_text(text: &str) -> bool {
    let t = text.trim();
    re(&URL_RE, URL_P).is_match(t)
        || re(&FULL_RE, FULL_P).is_match(t)
        || re(&NICK_RE, NICK_P).is_match(t)
        || re(&HERE_RE, HERE_P).is_match(t)
}

pub fn resolve(text: &str, resolver: &RefResolver) -> Result<CommitRef, String> {
    let t = text.trim();
    if let Some(m) = re(&URL_RE, URL_P).captures(t) {
        return Ok(CommitRef::new(&m[1], &m[2], &m[3].to_lowercase()));
    }
    if let Some(m) = re(&FULL_RE, FULL_P).captures(t) {
        return Ok(CommitRef::new(&m[1], &m[2], &m[3].to_lowercase()));
    }
    if let Some(m) = re(&HERE_RE, HERE_P).captures(t) {
        let (owner, repo) = resolver.here().ok_or_else(|| {
            format!("'{t}' names a commit in this checkout, but the current directory is not a GitHub clone")
        })?;
        return Ok(CommitRef::new(&owner, &repo, &m[1].to_lowercase()));
    }
    if let Some(m) = re(&NICK_RE, NICK_P).captures(t) {
        return match resolver.repo_for(&m[1]) {
            Some(repo) => {
                let (owner, name) = repo.split_once('/').unwrap_or((repo.as_str(), ""));
                Ok(CommitRef::new(owner, name, &m[2].to_lowercase()))
            }
            None => Err(format!("unknown repo nickname '{}': add it to [aliases] in the prhub config", &m[1])),
        };
    }
    Err(format!("cannot read '{t}' as a commit (try owner/repo@sha or a commit URL)"))
}

/// A canonical `owner/repo@sha` (what the watch file stores), without consulting git or the config.
pub fn parse(text: &str) -> Option<CommitRef> {
    resolve(text, &RefResolver { remote: |_| None, ..Default::default() }).ok()
}

const COMMIT_FIELDS: &str = "
  object(expression: SHA) { ... on Commit { oid committedDate
    status { contexts { context state targetUrl } }
    checkSuites(first: 50) { pageInfo { hasNextPage } nodes { app { slug } status conclusion checkRuns(first: 0) { totalCount }
      workflowRun { event url workflow { name } } } } } }
  deployments(last: 30) { nodes { commitOid environment createdAt latestStatus { state logUrl } } }
";

pub fn alias_query(refs: &[CommitRef]) -> String {
    let parts: Vec<String> = refs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            format!(
                "c{i}: repository(owner: {}, name: {}) {{{}}}",
                json!(r.owner),
                json!(r.repo),
                COMMIT_FIELDS.replace("SHA", &json!(r.sha).to_string())
            )
        })
        .collect();
    format!("query {{ rateLimit {{ cost remaining resetAt }}\n{}\n}}", parts.join("\n"))
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Run {
    pub name: String,
    pub event: Option<String>,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub url: Option<String>,
    pub not_started: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CommitStatus {
    pub name: Option<String>,
    pub state: Option<String>,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Success {
    pub sha: Option<String>,
    pub at: Option<String>,
    pub run: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct LastSuccess {
    pub sha: Option<String>,
    pub at: Option<String>,
}

/// One environment's deployments as seen from this commit.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Env {
    /// This commit's own deployments, oldest first: [latest status, run id].
    pub mine: Vec<(String, Option<String>)>,
    /// Every successful deployment to this environment (of any commit), oldest first.
    pub successes: Vec<Success>,
    /// This commit's deploy state (SUCCESS, DEPLOYING, FAILURE, IN_PROGRESS…), None when never deployed itself.
    pub this: Option<String>,
    pub last_success: Option<LastSuccess>,
    /// A deploy made after this commit whose run has not finished yet.
    pub deploying: bool,
    /// Whether a later commit's deploy contains this one (set by the daemon; absent until asked).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CommitSummary {
    pub oid: String,
    pub committed: Option<String>,
    pub runs: Vec<Run>,
    pub statuses: Vec<CommitStatus>,
    pub envs: BTreeMap<String, Env>,
    pub truncated: bool,
}

pub type RunStates = HashMap<String, (Option<String>, Option<String>)>;

/// Flatten one aliased repository node. None when the commit does not exist (typo, or not pushed yet).
pub fn summarize(repo_node: &Value) -> Option<CommitSummary> {
    let obj = dig(repo_node, &["object"]);
    let oid = s(obj, &["oid"]).filter(|o| !o.is_empty())?;
    let mut runs = Vec::new();
    for su in dig(obj, &["checkSuites", "nodes"]).as_array().into_iter().flatten() {
        if su.is_null() {
            continue;
        }
        let wr = dig(su, &["workflowRun"]);
        let has_wr = truthy(wr);
        let check_runs = dig(su, &["checkRuns", "totalCount"]).as_i64().unwrap_or(0);
        if !has_wr && check_runs == 0 {
            continue; // an app that never started a run on this commit
        }
        // A workflow run counts even with no check runs: its jobs may not be created yet, and green must wait for them
        let event = s(wr, &["event"]);
        if event.as_deref() == Some("schedule") {
            continue; // cron runs land on the branch head; they are not about this commit
        }
        let status = s(su, &["status"]);
        let conclusion = s(su, &["conclusion"]);
        // Cancelled before any job was created: a concurrency group replaced it with a newer run while it waited, or
        // someone cancelled it in the queue. Nothing ran, so it is neither a pass nor a failure of this commit.
        let not_started = has_wr
            && status.as_deref() == Some("COMPLETED")
            && conclusion.as_deref() == Some("CANCELLED")
            && check_runs == 0;
        if not_started && event.as_deref().is_some_and(|e| DISPATCH.contains(&e)) {
            continue;
        }
        let name = s(wr, &["workflow", "name"])
            .filter(|n| !n.is_empty())
            .or_else(|| s(su, &["app", "slug"]).filter(|n| !n.is_empty()))
            .unwrap_or_else(|| "?".into());
        runs.push(Run { name, event, status, conclusion, url: s(wr, &["url"]), not_started });
    }
    let statuses = dig(obj, &["status", "contexts"])
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| !c.is_null())
        .map(|c| CommitStatus { name: s(c, &["context"]), state: s(c, &["state"]), url: s(c, &["targetUrl"]) })
        .collect();
    let mut envs: BTreeMap<String, Env> = BTreeMap::new();
    for d in dig(repo_node, &["deployments", "nodes"]).as_array().into_iter().flatten() {
        // oldest first
        if d.is_null() {
            continue;
        }
        let st = s(d, &["latestStatus", "state"]).filter(|x| !x.is_empty()).unwrap_or_else(|| "PENDING".into());
        let run = run_id(s(d, &["latestStatus", "logUrl"]).as_deref());
        let env_name = s(d, &["environment"]).filter(|x| !x.is_empty()).unwrap_or_else(|| "?".into());
        let e = envs.entry(env_name).or_default();
        let sha = s(d, &["commitOid"]);
        if sha.as_deref() == Some(oid.as_str()) {
            e.mine.push((st.clone(), run.clone()));
        }
        if st == "SUCCESS" {
            e.successes.push(Success { sha, at: s(d, &["createdAt"]), run });
        }
    }
    let truncated = truthy(dig(obj, &["checkSuites", "pageInfo", "hasNextPage"]));
    let mut out = CommitSummary { oid, committed: s(obj, &["committedDate"]), runs, statuses, envs, truncated };
    let own = own_runs(&out);
    gate_deploys(&mut out, &own);
    Some(out)
}

/// The workflow run behind a deployment status (its log URL points at the job that deployed), or None.
pub fn run_id(url: Option<&str>) -> Option<String> {
    re(&RUN_URL, r"/actions/runs/(\d+)").captures(url?).map(|m| m[1].to_string())
}

/// {run id: (status, conclusion)} for the workflow runs read with this commit.
pub fn own_runs(s: &CommitSummary) -> RunStates {
    s.runs.iter().filter_map(|r| Some((run_id(r.url.as_deref())?, (r.status.clone(), r.conclusion.clone())))).collect()
}

/// Some(true) when the run that made a deployment finished well, Some(false) when it ended badly, None while it runs
/// or is unknown. A deployment made outside GitHub Actions (no run) has only its own status to go by.
fn run_finished(run: Option<&str>, states: &RunStates) -> Option<bool> {
    let Some(run) = run else { return Some(true) };
    let (status, conclusion) = states.get(run)?;
    if status.as_deref() != Some("COMPLETED") {
        return None;
    }
    Some(is_ok(conclusion.as_deref()))
}

/// Set each environment's `this` (this commit's deploy state) and `last_success` from its deployments.
///
/// A workflow makes one Deployment per job that names the environment (build, migrate, deploy…), and GitHub marks each
/// one SUCCESS when ITS job ends, so the first can read SUCCESS while the rest of the deploy has not run. A deployment
/// made by a workflow run counts only once that whole run has finished well; until then it is DEPLOYING.
pub fn gate_deploys(s: &mut CommitSummary, states: &RunStates) {
    let committed = s.committed.clone().unwrap_or_default();
    for e in s.envs.values_mut() {
        let mut this: Option<String> = None;
        for (state, run) in &e.mine {
            // oldest first: the newest decides, unless an earlier one finished
            let state = if state == "SUCCESS" {
                match run_finished(run.as_deref(), states) {
                    Some(true) => "SUCCESS".to_string(),
                    None => "DEPLOYING".to_string(),
                    Some(false) => "FAILURE".to_string(),
                }
            } else {
                state.clone()
            };
            if this.as_deref() != Some("SUCCESS") {
                this = Some(state);
            }
        }
        e.this = this;
        e.last_success = e
            .successes
            .iter()
            .rfind(|x| run_finished(x.run.as_deref(), states) == Some(true))
            .map(|x| LastSuccess { sha: x.sha.clone(), at: x.at.clone() });
        // a deploy made after this commit whose run has not finished yet: not live here yet, but it may be soon
        e.deploying = e
            .successes
            .iter()
            .any(|x| x.at.clone().unwrap_or_default() >= committed && run_finished(x.run.as_deref(), states).is_none());
    }
}

/// Runs behind this commit's deployments, or behind later ones that may contain it, whose end is not known yet.
pub fn unknown_runs(s: &CommitSummary, states: &RunStates) -> Vec<String> {
    let committed = s.committed.clone().unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for e in s.envs.values() {
        let mut cands: Vec<Option<String>> =
            e.mine.iter().filter(|(st, _)| st == "SUCCESS").map(|(_, r)| r.clone()).collect();
        cands.extend(
            e.successes.iter().filter(|x| x.at.clone().unwrap_or_default() >= committed).map(|x| x.run.clone()),
        );
        for r in cands.into_iter().flatten() {
            if !out.contains(&r) && !states.contains_key(&r) {
                out.push(r);
            }
        }
    }
    out
}

/// (env, deployed sha) pairs where a successful deploy of ANOTHER commit happened after this one was made, so whether
/// it contains this commit has to be asked.
pub fn needs_containment(s: &CommitSummary) -> Vec<(String, String)> {
    let committed = s.committed.clone().unwrap_or_default();
    s.envs
        .iter()
        .filter_map(|(env, e)| {
            let ls = e.last_success.as_ref()?;
            let sha = ls.sha.clone()?;
            (sha != s.oid && ls.at.clone().unwrap_or_default() >= committed).then(|| (env.clone(), sha))
        })
        .collect()
}

fn matches_workflow(name: &str, w: &str) -> bool {
    name.to_lowercase().contains(w)
}

/// (PENDING | SUCCESS | FAILURE | None when nothing ran yet, names of failed runs/statuses). `workflow` keeps only runs
/// and statuses whose name contains it (case-insensitive).
pub fn rollup(s: &CommitSummary, workflow: Option<&str>) -> (Option<String>, Vec<String>) {
    let w = workflow.unwrap_or("").to_lowercase();
    let runs: Vec<&Run> = s.runs.iter().filter(|r| matches_workflow(&r.name, &w) && !r.not_started).collect();
    let statuses: Vec<&CommitStatus> =
        s.statuses.iter().filter(|x| matches_workflow(x.name.as_deref().unwrap_or(""), &w)).collect();
    if runs.is_empty() && statuses.is_empty() {
        return (None, vec![]);
    }
    // A run CANCELLED because a newer run of the same workflow replaced it (re-run, concurrency group) is not a failure
    // when a sibling of that name passed; with no passing sibling it still is.
    let completed = |r: &&&Run| r.status.as_deref() == Some("COMPLETED");
    let passed: BTreeSet<&str> =
        runs.iter().filter(completed).filter(|r| is_ok(r.conclusion.as_deref())).map(|r| r.name.as_str()).collect();
    let mut failed: Vec<String> = runs
        .iter()
        .filter(completed)
        .filter(|r| !is_ok(r.conclusion.as_deref()))
        .filter(|r| !(r.conclusion.as_deref() == Some("CANCELLED") && passed.contains(r.name.as_str())))
        .map(|r| {
            if r.conclusion.as_deref() == Some("CANCELLED") {
                format!("{} (cancelled)", r.name)
            } else {
                r.name.clone()
            }
        })
        .collect();
    failed.extend(
        statuses
            .iter()
            .filter(|x| matches!(x.state.as_deref(), Some("FAILURE" | "ERROR")))
            .map(|x| x.name.clone().unwrap_or_else(|| "?".into())),
    );
    // more suites than one page: an unread one may still be running, so never call that SUCCESS (fails closed)
    let pending = s.truncated
        || runs.iter().any(|r| r.status.as_deref() != Some("COMPLETED"))
        || statuses.iter().any(|x| matches!(x.state.as_deref(), Some("PENDING" | "EXPECTED")));
    let state = if pending {
        "PENDING"
    } else if !failed.is_empty() {
        "FAILURE"
    } else {
        "SUCCESS"
    };
    (Some(state.into()), failed)
}

fn alias_forms(env: &str) -> Vec<String> {
    let low = env.to_lowercase();
    ENV_ALIASES
        .iter()
        .find(|g| g.contains(&low.as_str()))
        .map(|g| g.iter().map(|x| x.to_string()).collect())
        .unwrap_or_else(|| vec![low])
}

/// This commit's environments that `env` means: the exact name when the repository uses it, else its aliases
/// (`production` matches `prod`). "any" means every environment.
pub fn env_names(s: &CommitSummary, env: &str) -> Vec<String> {
    let names: Vec<String> = s.envs.keys().cloned().collect();
    if env == "any" {
        return names;
    }
    let exact: Vec<String> = names.iter().filter(|n| n.to_lowercase() == env.to_lowercase()).cloned().collect();
    if !exact.is_empty() {
        return exact;
    }
    let forms = alias_forms(env);
    names.into_iter().filter(|n| forms.contains(&n.to_lowercase())).collect()
}

/// Why `deployed` for `env` may never be met: the repository deploys, but never to an environment of that name.
pub fn env_hint(s: &CommitSummary, env: &str) -> Option<String> {
    if env == "any" || s.envs.is_empty() || !env_names(s, env).is_empty() {
        return None;
    }
    let names: Vec<&str> = s.envs.keys().map(String::as_str).collect();
    Some(format!(
        "no deployments to '{env}' recorded for this repository; environments seen: {} (use --env)",
        names.join(", ")
    ))
}

/// (live in `env`, failure text or None). `env` "any" accepts every environment.
pub fn deploy_state(s: &CommitSummary, env: &str) -> (bool, Option<String>) {
    let forms = alias_forms(env);
    let mut failure = None;
    for n in env_names(s, env) {
        let e = &s.envs[&n];
        if e.this.as_deref() == Some("SUCCESS") || e.live == Some(true) {
            return (true, None);
        }
        if let Some(t @ ("FAILURE" | "ERROR")) = e.this.as_deref() {
            failure = Some(format!("the {n} deployment of this commit {t}"));
        }
    }
    for x in &s.statuses {
        // e.g. a `deployed/production` commit status
        let name = x.name.clone().unwrap_or_default().to_lowercase();
        if name.contains("deploy") && (env == "any" || forms.iter().any(|f| name.contains(f.as_str()))) {
            match x.state.as_deref() {
                Some("SUCCESS") => return (true, None),
                Some(st @ ("FAILURE" | "ERROR")) => {
                    failure = Some(format!("status {}={st}", x.name.as_deref().unwrap_or("")))
                }
                _ => {}
            }
        }
    }
    (false, failure)
}

pub type CommitFingerprint = (
    Vec<(String, Option<String>, Option<String>)>,
    Vec<(Option<String>, Option<String>)>,
    Vec<(String, Option<String>, bool, Option<String>, bool)>,
);

pub fn fingerprint(s: &CommitSummary) -> CommitFingerprint {
    (
        s.runs.iter().map(|r| (r.name.clone(), r.status.clone(), r.conclusion.clone())).collect(),
        s.statuses.iter().map(|x| (x.name.clone(), x.state.clone())).collect(),
        s.envs
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.this.clone(),
                    v.live == Some(true),
                    v.last_success.as_ref().and_then(|l| l.sha.clone()),
                    v.deploying,
                )
            })
            .collect(),
    )
}

/// Workflows of this commit that were cancelled before any job started and never ran here at all (no run of the same
/// name started). Green can never be met for them: this commit's CI did not happen.
pub fn never_ran(s: &CommitSummary, workflow: Option<&str>) -> Vec<String> {
    let w = workflow.unwrap_or("").to_lowercase();
    let runs: Vec<&Run> = s.runs.iter().filter(|r| matches_workflow(&r.name, &w)).collect();
    let started: BTreeSet<&str> = runs.iter().filter(|r| !r.not_started).map(|r| r.name.as_str()).collect();
    let out: BTreeSet<String> =
        runs.iter().filter(|r| r.not_started && !started.contains(r.name.as_str())).map(|r| r.name.clone()).collect();
    out.into_iter().collect()
}

/// (met, reason it can no longer be met or None).
pub fn evaluate(
    cond: &str,
    s: &CommitSummary,
    baseline: Option<&CommitSummary>,
    env: &str,
    workflow: Option<&str>,
) -> Result<(bool, Option<String>), String> {
    if cond == "change" {
        return Ok((baseline.is_some_and(|b| fingerprint(s) != fingerprint(b)), None));
    }
    let (state, failed) = rollup(s, workflow);
    let unrun = never_ran(s, workflow);
    let unrun_reason = (!unrun.is_empty()).then(|| {
        format!(
            "never ran on this commit (cancelled before it started, usually replaced by a newer run): {}; wait on the newer commit, or use --until deployed",
            unrun.join(", ")
        )
    });
    match cond {
        "checks" => Ok((
            matches!(state.as_deref(), Some("SUCCESS" | "FAILURE")),
            if state.is_none() { unrun_reason } else { None },
        )),
        "green" => {
            if state.as_deref() == Some("FAILURE") {
                return Ok((false, Some(format!("failed: {}", failed.join(", ")))));
            }
            let met = unrun.is_empty() && state.as_deref() == Some("SUCCESS");
            Ok((met, if state.as_deref() != Some("PENDING") { unrun_reason } else { None }))
        }
        "deployed" => {
            let (live, failure) = deploy_state(s, env);
            Ok((live, if live { None } else { failure }))
        }
        _ => Err(format!("unknown condition '{cond}' for a commit (use: {})", CONDITIONS.join(", "))),
    }
}

pub fn one_line(r: &str, s: &CommitSummary, workflow: Option<&str>) -> String {
    let (state, failed) = rollup(s, workflow);
    let mut bits =
        vec![format!("{r} ({})", take(&s.oid, 10)), format!("checks={}", state.as_deref().unwrap_or("none yet"))];
    let w = workflow.unwrap_or("").to_lowercase();
    let shown: Vec<&Run> = s.runs.iter().filter(|r| matches_workflow(&r.name, &w)).collect();
    if !shown.is_empty() {
        let list: Vec<String> = shown
            .iter()
            .take(8)
            .map(|r| {
                let v = if r.not_started {
                    "NOT STARTED".to_string()
                } else {
                    r.conclusion
                        .clone()
                        .filter(|c| !c.is_empty())
                        .or_else(|| r.status.clone())
                        .unwrap_or_else(|| "None".into())
                };
                format!("{}={v}", r.name)
            })
            .collect();
        let more = if shown.len() > 8 { format!(" (+{})", shown.len() - 8) } else { String::new() };
        bits.push(format!("runs: {}{more}", list.join(", ")));
    }
    if !failed.is_empty() {
        bits.push(format!("failing: {}", failed.iter().take(6).cloned().collect::<Vec<_>>().join(", ")));
    }
    let unrun = never_ran(s, workflow);
    if !unrun.is_empty() {
        bits.push(format!("never ran here: {}", unrun.iter().take(6).cloned().collect::<Vec<_>>().join(", ")));
    }
    let mut deps = Vec::new();
    for (env, e) in &s.envs {
        if e.this.as_deref() == Some("SUCCESS") || e.live == Some(true) {
            let via = if e.this.as_deref() == Some("SUCCESS") {
                String::new()
            } else {
                format!(" via {}", take(e.last_success.as_ref().and_then(|l| l.sha.as_deref()).unwrap_or(""), 8))
            };
            deps.push(format!("{env}=live{via}"));
        } else if let Some(t) = e.this.as_deref() {
            deps.push(format!("{env}={t}"));
        } else if e.live == Some(false) {
            deps.push(format!("{env}=not yet"));
        }
    }
    bits.push(format!(
        "deploys: {}",
        if deps.is_empty() { "none recorded for this commit".to_string() } else { deps.join(", ") }
    ));
    bits.join("  ")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    pub const LATER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const RUN: &str = "https://github.com/acme/api/actions/runs/7";

    pub struct Suite {
        pub name: &'static str,
        pub status: &'static str,
        pub conclusion: Option<&'static str>,
        pub runs: i64,
        pub event: Option<&'static str>,
        pub app: &'static str,
        pub url: &'static str,
    }

    pub fn suite(name: &'static str) -> Suite {
        Suite {
            name,
            status: "COMPLETED",
            conclusion: Some("SUCCESS"),
            runs: 1,
            event: Some("push"),
            app: "github-actions",
            url: "u",
        }
    }

    impl Suite {
        pub fn st(mut self, status: &'static str, conclusion: Option<&'static str>) -> Self {
            self.status = status;
            self.conclusion = conclusion;
            self
        }
        pub fn runs(mut self, n: i64) -> Self {
            self.runs = n;
            self
        }
        pub fn event(mut self, e: Option<&'static str>) -> Self {
            self.event = e;
            self
        }
        pub fn app(mut self, a: &'static str) -> Self {
            self.app = a;
            self
        }
        pub fn url(mut self, u: &'static str) -> Self {
            self.url = u;
            self
        }
        pub fn json(&self) -> Value {
            json!({"app": {"slug": self.app}, "status": self.status, "conclusion": self.conclusion,
                   "checkRuns": {"totalCount": self.runs},
                   "workflowRun": self.event.map(|e| json!({"event": e, "url": self.url, "workflow": {"name": self.name}}))})
        }
    }

    /// (sha, env, createdAt, state[, logUrl])
    pub type Dep = (&'static str, &'static str, &'static str, &'static str, Option<&'static str>);

    pub fn node(suites: &[Suite], statuses: &[(&str, &str)], deployments: &[Dep]) -> Value {
        node_at(OID, suites, statuses, deployments, "2026-10-10T10:00:00Z")
    }

    pub fn node_at(
        oid: &str,
        suites: &[Suite],
        statuses: &[(&str, &str)],
        deployments: &[Dep],
        committed: &str,
    ) -> Value {
        json!({"object": {"oid": oid, "committedDate": committed,
                          "status": {"contexts": statuses.iter().map(|(c, st)| json!({"context": c, "state": st, "targetUrl": null})).collect::<Vec<_>>()},
                          "checkSuites": {"nodes": suites.iter().map(Suite::json).collect::<Vec<_>>()}},
               "deployments": {"nodes": deployments.iter().map(|(sha, env, at, st, log)| json!({
                   "commitOid": sha, "environment": env, "createdAt": at, "latestStatus": {"state": st, "logUrl": log}})).collect::<Vec<_>>()}})
    }

    fn sum(v: Value) -> CommitSummary {
        summarize(&v).unwrap()
    }

    fn ro(s: &CommitSummary) -> (Option<String>, Vec<String>) {
        rollup(s, None)
    }

    fn st(x: &str) -> Option<String> {
        Some(x.to_string())
    }

    #[test]
    fn naming_a_commit() {
        let aliases = BTreeMap::from([("backend".to_string(), "acme/api".to_string())]);
        let r = RefResolver {
            remote: |_| Some(("acme".into(), "api".into())),
            ..RefResolver::new(&aliases, &BTreeMap::new())
        };
        for (text, want) in [
            ("https://github.com/acme/api/commit/abcdef1234", CommitRef::new("acme", "api", "abcdef1234")),
            ("acme/api@ABCDEF1", CommitRef::new("acme", "api", "abcdef1")),
            ("backend@abcdef1", CommitRef::new("acme", "api", "abcdef1")),
            ("@abcdef1", CommitRef::new("acme", "api", "abcdef1")),
        ] {
            assert_eq!(resolve(text, &r).unwrap(), want);
            assert!(is_commit_text(text));
        }
        assert!(resolve("mystery@abcdef1", &RefResolver::default()).unwrap_err().contains("unknown repo nickname"));
        assert!(resolve("acme/api#1", &r).unwrap_err().contains("cannot read"));
        assert!(resolve("@abcdef1", &RefResolver { remote: |_| None, ..Default::default() })
            .unwrap_err()
            .contains("not a GitHub clone"));
        assert!(!is_commit_text("acme/api#12") && !is_commit_text("a@b"));
        assert_eq!(parse("acme/api@abcdef1").unwrap().slug(), "acme__api__abcdef1");
        assert_eq!(CommitRef::new("acme", "api", "abc").to_string(), "acme/api@abc");
        let q = alias_query(&[CommitRef::new("acme", "api", "abcdef1")]);
        assert!(
            q.contains("c0: repository(owner: \"acme\", name: \"api\")")
                && q.contains("object(expression: \"abcdef1\")")
        );
    }

    #[test]
    fn missing_commit() {
        assert!(summarize(&json!({"object": null, "deployments": {"nodes": []}})).is_none());
        assert!(summarize(&Value::Null).is_none());
    }

    #[test]
    fn ci_is_every_run_but_cron_and_idle_apps() {
        let s = sum(node(
            &[
                suite("CI"),
                suite("cursor").st("QUEUED", None).runs(0).event(None).app("cursor"),
                suite("Nightly").st("IN_PROGRESS", None).event(Some("schedule")),
            ],
            &[],
            &[],
        ));
        assert_eq!(s.runs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["CI"]);
        assert_eq!(ro(&s), (st("SUCCESS"), vec![]));
        let app = sum(node(&[suite("x").event(None).app("codecov")], &[], &[]));
        assert_eq!(app.runs[0].name, "codecov");
    }

    #[test]
    fn a_workflow_whose_jobs_do_not_exist_yet_keeps_the_commit_pending() {
        let s = sum(node(&[suite("Secret scan"), suite("CI").st("QUEUED", None).runs(0)], &[], &[]));
        assert_eq!(ro(&s), (st("PENDING"), vec![]));
        assert_eq!(evaluate("green", &s, None, "production", None).unwrap(), (false, None));
        assert!(one_line("acme/api@aaaaaaa", &s, None).contains("CI=QUEUED"));
    }

    #[test]
    fn more_suites_than_one_page_keep_the_commit_pending() {
        let mut n = node(&[suite("CI")], &[], &[]);
        n["object"]["checkSuites"]["pageInfo"] = json!({"hasNextPage": true});
        let s = sum(n);
        assert_eq!(ro(&s).0, st("PENDING"));
        assert_eq!(evaluate("green", &s, None, "production", None).unwrap(), (false, None));
        assert_eq!(ro(&sum(node(&[suite("CI")], &[], &[]))).0, st("SUCCESS"));
    }

    #[test]
    fn a_run_cancelled_before_it_started_is_not_a_failure_but_green_can_never_be_met() {
        let s = sum(node(&[suite("Secret scan"), suite("CI").st("COMPLETED", Some("CANCELLED")).runs(0)], &[], &[]));
        assert_eq!(ro(&s), (st("SUCCESS"), vec![]));
        let (met, why) = evaluate("green", &s, None, "production", None).unwrap();
        assert!(!met && why.unwrap().contains("never ran on this commit"));
        assert_eq!(evaluate("checks", &s, None, "production", None).unwrap(), (true, None));
        let line = one_line("acme/api@aaaaaaa", &s, None);
        assert!(line.contains("CI=NOT STARTED") && !line.contains("failing") && line.contains("never ran here: CI"));
        let only = sum(node(&[suite("CI").st("COMPLETED", Some("CANCELLED")).runs(0)], &[], &[]));
        assert_eq!(ro(&only), (None, vec![]));
        assert!(evaluate("checks", &only, None, "production", None).unwrap().1.is_some());
        assert!(evaluate("green", &only, None, "production", None).unwrap().1.is_some());
        let pending = sum(node(
            &[suite("Build").st("IN_PROGRESS", None), suite("CI").st("COMPLETED", Some("CANCELLED")).runs(0)],
            &[],
            &[],
        ));
        assert_eq!(evaluate("green", &pending, None, "production", None).unwrap(), (false, None));
    }

    #[test]
    fn a_run_cancelled_before_it_started_does_not_count_when_the_same_workflow_ran() {
        let s = sum(node(&[suite("CI").st("COMPLETED", Some("CANCELLED")).runs(0), suite("CI")], &[], &[]));
        assert_eq!(ro(&s), (st("SUCCESS"), vec![]));
        assert_eq!(evaluate("green", &s, None, "production", None).unwrap(), (true, None));
    }

    #[test]
    fn a_cancelled_dispatched_run_that_never_started_says_nothing_about_the_commit() {
        let s = sum(node(
            &[
                suite("Wiki trigger"),
                suite("Publish image"),
                suite("Deploy").event(Some("workflow_dispatch")),
                suite("Wiki ingest").st("COMPLETED", Some("CANCELLED")).runs(0).event(Some("workflow_dispatch")),
            ],
            &[],
            &[],
        ));
        assert_eq!(ro(&s), (st("SUCCESS"), vec![]));
        assert_eq!(evaluate("green", &s, None, "production", None).unwrap(), (true, None));
        assert!(!one_line("acme/api@aaaaaaa", &s, None).contains("Wiki ingest"));
        let started =
            sum(node(&[suite("Deploy").st("COMPLETED", Some("CANCELLED")).event(Some("workflow_dispatch"))], &[], &[]));
        assert_eq!(ro(&started), (st("FAILURE"), vec!["Deploy (cancelled)".to_string()]));
    }

    #[test]
    fn rollup_states() {
        assert_eq!(ro(&sum(node(&[suite("CI").st("IN_PROGRESS", None)], &[], &[]))).0, st("PENDING"));
        assert_eq!(
            ro(&sum(node(&[suite("CI").st("COMPLETED", Some("FAILURE"))], &[], &[]))),
            (st("FAILURE"), vec!["CI".to_string()])
        );
        assert_eq!(ro(&sum(node(&[], &[], &[]))), (None, vec![]));
        assert_eq!(ro(&sum(node(&[], &[("ci/ext", "PENDING")], &[]))).0, st("PENDING"));
        assert_eq!(ro(&sum(node(&[], &[("ci/ext", "ERROR")], &[]))), (st("FAILURE"), vec!["ci/ext".to_string()]));
    }

    #[test]
    fn cancelled_run_counts_only_without_a_passing_sibling() {
        let replaced = sum(node(&[suite("CI").st("COMPLETED", Some("CANCELLED")), suite("CI")], &[], &[]));
        assert_eq!(ro(&replaced), (st("SUCCESS"), vec![]));
        let alone = sum(node(&[suite("CI").st("COMPLETED", Some("CANCELLED"))], &[], &[]));
        assert_eq!(ro(&alone), (st("FAILURE"), vec!["CI (cancelled)".to_string()]));
    }

    #[test]
    fn workflow_filter() {
        let s = sum(node(&[suite("CI").st("COMPLETED", Some("FAILURE")), suite("Deploy")], &[], &[]));
        assert_eq!(ro(&s).0, st("FAILURE"));
        assert_eq!(rollup(&s, Some("deploy")), (st("SUCCESS"), vec![]));
        assert_eq!(evaluate("green", &s, None, "production", Some("deploy")).unwrap(), (true, None));
        assert!(one_line("x@1", &s, Some("deploy")).contains("runs: Deploy=SUCCESS"));
    }

    #[test]
    fn deployed_when_this_commit_was_deployed() {
        let s = sum(node(&[], &[], &[(OID, "production", "2026-10-10T11:00:00Z", "SUCCESS", None)]));
        assert_eq!(deploy_state(&s, "production"), (true, None));
        assert!(needs_containment(&s).is_empty());
    }

    #[test]
    fn a_later_deploy_needs_a_containment_answer() {
        let mut s = sum(node(&[], &[], &[(LATER, "production", "2026-10-11T01:00:00Z", "SUCCESS", None)]));
        assert_eq!(needs_containment(&s), vec![("production".to_string(), LATER.to_string())]);
        assert!(!deploy_state(&s, "production").0); // unknown until the daemon asks
        s.envs.get_mut("production").unwrap().live = Some(true);
        assert_eq!(deploy_state(&s, "production"), (true, None));
        assert!(one_line("x@1", &s, None).contains("production=live via bbbbbbbb"));
    }

    #[test]
    fn a_deploy_older_than_the_commit_is_not_asked_about() {
        let s = sum(node(&[], &[], &[(LATER, "production", "2026-10-09T01:00:00Z", "SUCCESS", None)]));
        assert!(needs_containment(&s).is_empty());
    }

    #[test]
    fn failed_deploy_of_this_commit_ends_the_wait() {
        let s = sum(node(&[], &[], &[(OID, "production", "2026-10-10T11:00:00Z", "FAILURE", None)]));
        let (met, dead) = evaluate("deployed", &s, None, "production", None).unwrap();
        assert!(!met && dead.unwrap().contains("FAILURE"));
        assert!(one_line("x@1", &s, None).contains("production=FAILURE"));
    }

    #[test]
    fn a_deployment_counts_only_once_the_run_that_made_it_has_finished() {
        let job = Some("https://github.com/acme/api/actions/runs/7/job/70");
        let build: Dep = (OID, "production", "2026-10-10T11:00:00Z", "SUCCESS", job);
        let s = sum(node(&[suite("Deploy").st("IN_PROGRESS", None).url(RUN)], &[], &[build]));
        assert!(
            s.envs["production"].this.as_deref() == Some("DEPLOYING")
                && deploy_state(&s, "production") == (false, None)
        );
        assert!(one_line("acme/api@aaaaaaa", &s, None).contains("production=DEPLOYING"));
        let next: Dep = (
            OID,
            "production",
            "2026-10-10T11:05:00Z",
            "IN_PROGRESS",
            Some("https://github.com/acme/api/actions/runs/7/job/71"),
        );
        let s = sum(node(&[suite("Deploy").st("IN_PROGRESS", None).url(RUN)], &[], &[build, next]));
        assert!(!deploy_state(&s, "production").0);
        assert_eq!(deploy_state(&sum(node(&[suite("Deploy").url(RUN)], &[], &[build])), "production"), (true, None));
        let failed = sum(node(&[suite("Deploy").st("COMPLETED", Some("FAILURE")).url(RUN)], &[], &[build]));
        let (live, why) = deploy_state(&failed, "production");
        assert!(!live && why.unwrap().contains("FAILURE"));
    }

    #[test]
    fn a_finished_deploy_of_this_commit_stays_live_while_a_redeploy_runs() {
        let first: Dep = (
            OID,
            "production",
            "2026-10-10T11:00:00Z",
            "SUCCESS",
            Some("https://github.com/acme/api/actions/runs/7/job/70"),
        );
        let again: Dep = (
            OID,
            "production",
            "2026-10-10T12:00:00Z",
            "SUCCESS",
            Some("https://github.com/acme/api/actions/runs/8/job/80"),
        );
        let s = sum(node(&[suite("Deploy").url(RUN)], &[], &[first, again]));
        assert_eq!(s.envs["production"].this.as_deref(), Some("SUCCESS"));
    }

    #[test]
    fn a_later_deploy_counts_once_its_run_has_finished() {
        let later: Dep = (
            LATER,
            "production",
            "2026-10-11T01:00:00Z",
            "SUCCESS",
            Some("https://github.com/acme/api/actions/runs/7/job/70"),
        );
        let mut s = sum(node(&[], &[], &[later]));
        assert!(needs_containment(&s).is_empty() && unknown_runs(&s, &own_runs(&s)) == vec!["7".to_string()]);
        let running: RunStates = HashMap::from([("7".to_string(), (st("IN_PROGRESS"), None))]);
        gate_deploys(&mut s, &running);
        assert!(needs_containment(&s).is_empty() && s.envs["production"].deploying);
        let done: RunStates = HashMap::from([("7".to_string(), (st("COMPLETED"), st("SUCCESS")))]);
        let before = s.clone();
        gate_deploys(&mut s, &done);
        assert_eq!(needs_containment(&s), vec![("production".to_string(), LATER.to_string())]);
        assert!(unknown_runs(&s, &done).is_empty());
        let mut failed = before.clone();
        gate_deploys(&mut failed, &HashMap::from([("7".to_string(), (st("COMPLETED"), st("FAILURE")))]));
        assert!(before.envs["production"].deploying && !failed.envs["production"].deploying);
        assert_ne!(fingerprint(&before), fingerprint(&failed));
    }

    #[test]
    fn a_deployment_made_outside_actions_goes_by_its_own_status() {
        let s = sum(node(
            &[],
            &[],
            &[(OID, "production", "2026-10-10T11:00:00Z", "SUCCESS", Some("https://ci.example.com/1"))],
        ));
        assert_eq!(deploy_state(&s, "production"), (true, None));
        assert!(run_id(Some("https://ci.example.com/1")).is_none() && run_id(None).is_none());
    }

    #[test]
    fn deploy_status_context_counts_and_env_matters() {
        let s = sum(node(&[], &[("deployed/production", "SUCCESS")], &[]));
        assert_eq!(deploy_state(&s, "production"), (true, None));
        assert!(!deploy_state(&s, "staging").0);
        assert!(deploy_state(&s, "any").0);
        let bad = sum(node(&[], &[("deployed/production", "FAILURE")], &[]));
        assert_eq!(deploy_state(&bad, "production").1.as_deref(), Some("status deployed/production=FAILURE"));
    }

    #[test]
    fn environment_names_match_their_usual_short_and_long_forms() {
        let s = sum(node(
            &[],
            &[],
            &[
                (OID, "prod", "2026-10-10T11:00:00Z", "SUCCESS", None),
                (OID, "stg", "2026-10-10T11:00:00Z", "SUCCESS", None),
            ],
        ));
        assert!(deploy_state(&s, "production").0 && deploy_state(&s, "prd").0);
        assert!(deploy_state(&s, "staging").0 && !deploy_state(&s, "development").0);
        let both = sum(node(
            &[],
            &[],
            &[
                (OID, "production", "2026-10-10T11:00:00Z", "SUCCESS", None),
                (OID, "prod", "2026-10-10T11:00:00Z", "FAILURE", None),
            ],
        ));
        assert_eq!(env_names(&both, "prod"), vec!["prod"]);
        assert_eq!(env_names(&both, "production"), vec!["production"]);
        assert!(env_names(&s, "qa").is_empty() && env_names(&s, "any") == vec!["prod", "stg"]);
        assert!(deploy_state(&sum(node(&[], &[("deployed/prod", "SUCCESS")], &[])), "production").0);
    }

    #[test]
    fn an_environment_the_repository_never_deploys_to_is_named() {
        let s = sum(node(
            &[],
            &[],
            &[
                (OID, "prod", "2026-10-10T11:00:00Z", "SUCCESS", None),
                (OID, "staging", "2026-10-10T11:00:00Z", "SUCCESS", None),
            ],
        ));
        let hint = env_hint(&s, "qa").unwrap();
        assert!(hint.contains("'qa'") && hint.contains("prod, staging"));
        assert!(env_hint(&s, "production").is_none() && env_hint(&s, "any").is_none());
        assert!(env_hint(&sum(node(&[], &[], &[])), "qa").is_none());
    }

    #[test]
    fn change_compares_against_the_baseline() {
        let a = sum(node(&[suite("CI").st("IN_PROGRESS", None)], &[], &[]));
        let b = sum(node(&[suite("CI")], &[], &[]));
        assert!(!evaluate("change", &a, Some(&a), "production", None).unwrap().0);
        assert!(evaluate("change", &b, Some(&a), "production", None).unwrap().0);
        assert!(evaluate("merged", &a, None, "production", None).unwrap_err().contains("unknown condition"));
    }

    #[test]
    fn one_line_lists_at_most_eight_runs() {
        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        let suites: Vec<Suite> = names.iter().map(|n| suite(n)).collect();
        let line = one_line("x@1", &sum(node(&suites, &[], &[])), None);
        assert!(line.contains("h=SUCCESS (+2)") && line.ends_with("deploys: none recorded for this commit"));
    }

    #[test]
    fn summaries_survive_a_round_trip() {
        let s = sum(node(
            &[suite("CI")],
            &[("deployed/production", "SUCCESS")],
            &[(LATER, "production", "2026-10-11T01:00:00Z", "SUCCESS", None)],
        ));
        let back: CommitSummary = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let v = serde_json::to_value(&s).unwrap();
        assert!(v["envs"]["production"].get("live").is_none() && v["envs"]["production"]["mine"].is_array());
    }
}
