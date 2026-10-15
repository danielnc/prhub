//! One poller per machine. Every `interval` seconds: one batched GraphQL read of every tracked PR and of the commits
//! agents are waiting on; every `discover_every` seconds also one search for new open PRs. Agents read the files this
//! writes; they never call GitHub themselves.

use crate::commits::{self, CommitRef, RunStates};
use crate::conditions::{fingerprint, summary};
use crate::github::{Api, GhError, GitHub};
use crate::refs::{self, PrRef};
use crate::store::{self, CommitRecord, Config, PrRecord};
use crate::util::{local_datetime, now, now_i, parse_iso, truthy};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

/// An agent's `wait` can pull a poll forward, but not more often than this.
pub const NUDGE_MIN_GAP: f64 = 15.0;
/// A watched PR stays tracked this long after it closes.
pub const KEEP_CLOSED_WATCH: f64 = 3600.0;
pub const KEEP_CLOSED_FILE: f64 = 7.0 * 86400.0;
/// Deploy-run answers are kept this long; so are commit watches written before keep_until.
pub const KEEP_COMMIT_WATCH: i64 = 3600;
/// A commit GitHub keeps failing on is skipped this long before trying again.
pub const COMMIT_RETRY: f64 = 300.0;
/// Most commits read in one poll (most urgent first), so a poll stays short however many are watched; the rest are
/// read on the next polls.
pub const COMMITS_PER_POLL: usize = 50;
pub const KEEP_COMMIT_FILE: f64 = 2.0 * 86400.0;
/// A merged/closed PR still being watched is re-read this often (it can be reopened).
pub const CLOSED_RECHECK: f64 = 600.0;
/// So is a commit whose runs have all finished (its deploy can still land later).
pub const SETTLED_RECHECK: f64 = 300.0;
/// A deploy run that ended badly is asked again this often: a re-run keeps its id.
pub const RUN_RECHECK: f64 = 600.0;
/// Re-read `gh auth token` this often; a slow or failing `gh` keeps the last good token meanwhile.
pub const TOKEN_TTL: f64 = 600.0;

pub fn log(msg: &str) {
    println!("{} {msg}", local_datetime(now_i()));
}

/// Slow down when the shared GitHub budget runs low, instead of finishing it off for every other tool.
pub fn adaptive_interval(base: i64, rate: &Value) -> i64 {
    match rate.get("remaining").and_then(Value::as_i64) {
        Some(r) if r < 300 => base.max(600),
        Some(r) if r < 1000 => base.max(180),
        _ => base,
    }
}

pub fn seconds_until(reset_at: Option<&str>, default: i64) -> i64 {
    let Some(reset) = reset_at.filter(|r| !r.is_empty()) else { return default };
    let target = if reset.chars().all(|c| c.is_ascii_digit()) { reset.parse::<i64>().ok() } else { parse_iso(reset) };
    match target {
        Some(t) => 30.max(t - now_i() + 10),
        None => default,
    }
}

/// Why a poll did not finish: GitHub said no, or something else went wrong (a full disk, a bug).
pub enum PollError {
    GitHub(GhError),
    Other(String),
}

impl From<GhError> for PollError {
    fn from(e: GhError) -> Self {
        PollError::GitHub(e)
    }
}

impl From<std::io::Error> for PollError {
    fn from(e: std::io::Error) -> Self {
        PollError::Other(format!("OSError: {e}"))
    }
}

pub struct PollResult {
    pub tracked: usize,
    pub commits: usize,
    pub changed: usize,
    pub rate: Value,
    pub skipped: usize,
}

pub struct Daemon {
    pub api: Box<dyn Api>,
    pub cfg: Config,
    pub open_refs: HashSet<PrRef>,
    pub last_discover: f64,
    pub state: Map<String, Value>,
    pub token: Option<String>,
    pub token_at: f64,
    /// commit -> when to try again after GitHub kept failing on it
    pub commit_retry: HashMap<String, f64>,
}

fn pick(s: &crate::conditions::Summary) -> Value {
    json!({"state": s.state, "draft": s.draft, "checks": s.checks, "review": s.review, "merge_state": s.merge_state, "head": s.head})
}

/// Whether to read an item this poll. Unsettled items are read every poll; settled ones every `recheck` seconds,
/// unless someone asked about them since the last read (a new `wait` needs fresh data).
pub fn due(exists: bool, fetched: f64, last_asked: f64, settled: bool, recheck: f64, now: f64) -> bool {
    if !exists || !settled {
        return true;
    }
    now - fetched >= recheck || last_asked > fetched
}

impl Daemon {
    pub fn new(cfg: Config) -> Self {
        Daemon::with_api(cfg, Box::new(GitHub::default()))
    }

    pub fn with_api(cfg: Config, api: Box<dyn Api>) -> Self {
        let mut state = store::daemon_state();
        state.insert("pid".into(), json!(std::process::id()));
        state.insert("started".into(), json!(now_i()));
        Daemon {
            api,
            cfg,
            open_refs: HashSet::new(),
            last_discover: 0.0,
            state,
            token: None,
            token_at: 0.0,
            commit_retry: HashMap::new(),
        }
    }

    pub fn get_token(&mut self) -> Result<String, GhError> {
        if let Some(t) = &self.token {
            if now() - self.token_at < TOKEN_TTL {
                return Ok(t.clone());
            }
        }
        match self.api.token() {
            Ok(t) => {
                self.token = Some(t.clone());
                self.token_at = now();
                Ok(t)
            }
            Err(e) if e.transient && self.token.is_some() => {
                log(&format!("keeping the previous token: {e}"));
                Ok(self.token.clone().unwrap_or_default())
            }
            Err(e) => Err(e),
        }
    }

    pub fn tracked(&self) -> BTreeSet<PrRef> {
        let mut out: BTreeSet<PrRef> = self.open_refs.iter().cloned().collect();
        out.extend(store::watches().keys().filter_map(|k| refs::parse(k)));
        // anything we last saw open keeps being read until it closes
        out.extend(
            store::all_prs()
                .iter()
                .filter(|r| r.summary.state.as_deref() == Some("OPEN"))
                .filter_map(|r| refs::parse(&r.r)),
        );
        out
    }

    pub fn poll(&mut self) -> Result<PollResult, PollError> {
        let token = self.get_token()?;
        let now = now();
        let mut rate = Value::Null;
        if now - self.last_discover >= self.cfg.discover_every as f64 {
            let (found, r) = self.api.discover_open(&self.cfg.search, &token)?;
            self.open_refs = found.into_iter().collect();
            self.last_discover = now;
            rate = r;
        }
        let w = store::watches();
        let mut want = Vec::new();
        for r in self.tracked() {
            let rec = store::read_pr(&r);
            let closed = rec.as_ref().is_some_and(|x| matches!(x.summary.state.as_deref(), Some("MERGED" | "CLOSED")))
                && !self.open_refs.contains(&r);
            let asked = w.get(&r.to_string()).and_then(|x| x.last_asked).unwrap_or(0) as f64;
            let fetched = rec.as_ref().map(|x| x.fetched_at).unwrap_or(0) as f64;
            if due(rec.is_some(), fetched, asked, closed, CLOSED_RECHECK, now) {
                want.push(r);
            }
        }
        let (data, rate2) = if want.is_empty() { (vec![], Value::Null) } else { self.api.fetch_prs(&want, &token)? };
        if truthy(&rate2) {
            rate = rate2;
        }
        let mut went_red: Vec<PrRef> = Vec::new();
        let mut changed = 0;
        for (r, pr) in &data {
            let Some(pr) = pr else { continue }; // deleted repo, lost access, or a typo in a watch
            let prev = store::read_pr(r);
            let s = summary(pr);
            let red = matches!(s.checks.as_deref(), Some("FAILURE" | "ERROR"));
            let mut rec = PrRecord {
                r: r.to_string(),
                fetched_at: now as i64,
                summary: s.clone(),
                pr: pr.clone(),
                failing: if red { prev.as_ref().map(|p| p.failing.clone()).unwrap_or_default() } else { vec![] },
                closed_seen_at: None,
            };
            let old = prev.as_ref().filter(|p| !p.r.is_empty()).map(|p| p.summary.clone());
            if old.as_ref().is_none_or(|o| fingerprint(o) != fingerprint(&s)) {
                changed += 1;
                store::append_event(
                    &json!({"at": now as i64, "ref": r.to_string(), "from": old.as_ref().map(pick), "to": pick(&s)}),
                )?;
                if red && old.as_ref().is_none_or(|o| o.checks != s.checks || o.head != s.head) {
                    went_red.push(r.clone());
                }
            }
            if old.as_ref().is_some_and(|o| o.state.as_deref() == Some("OPEN")) && s.state.as_deref() != Some("OPEN") {
                rec.closed_seen_at = Some(now as i64);
            } else if let Some(c) = prev.as_ref().and_then(|p| p.closed_seen_at) {
                rec.closed_seen_at = Some(c);
            }
            store::write_pr(r, &rec)?;
        }
        if !went_red.is_empty() {
            went_red.truncate(20);
            for (r, failing) in self.api.fetch_failing(&went_red, &token)? {
                if let Some(mut rec) = store::read_pr(&r) {
                    rec.failing = failing;
                    store::write_pr(&r, &rec)?;
                }
            }
        }
        let skipped = want.len() - data.len();
        let (crate_rate, ncommits, cskipped) = self.poll_commits(&token, now)?;
        self.prune(now)?;
        Ok(PollResult {
            tracked: want.len(),
            commits: ncommits,
            changed,
            rate: if truthy(&crate_rate) { crate_rate } else { rate },
            skipped: skipped + cskipped,
        })
    }

    pub fn poll_commits(&mut self, token: &str, now: f64) -> Result<(Value, usize, usize), PollError> {
        let mut due_list: Vec<(u8, f64, CommitRef)> = Vec::new();
        for (k, watch) in store::commit_watches() {
            if store::commit_watch_expired(&watch, now, KEEP_COMMIT_WATCH) {
                continue; // nobody has asked for a while; prune() drops it
            }
            if self.commit_retry.get(&k).is_some_and(|t| *t > now) {
                continue; // GitHub kept failing on it: back off so it cannot crowd out the rest
            }
            let Some(r) = commits::parse(&k) else { continue };
            let rec = store::read_commit(&r);
            let settled = rec.as_ref().is_some_and(|x| {
                x.missing
                    || x.summary
                        .as_ref()
                        .is_some_and(|s| matches!(commits::rollup(s, None).0.as_deref(), Some("SUCCESS" | "FAILURE")))
            });
            let fetched = rec.as_ref().map(|x| x.fetched_at).unwrap_or(0) as f64;
            let asked = watch.last_asked.unwrap_or(0) as f64;
            if due(rec.is_some(), fetched, asked, settled, SETTLED_RECHECK, now) {
                // never read or just asked about first, then commits still running, then the stalest finished ones
                let rank = if rec.is_none() || asked > fetched {
                    0
                } else if !settled {
                    1
                } else {
                    2
                };
                due_list.push((rank, fetched, r));
            }
        }
        due_list.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        let want: Vec<CommitRef> = due_list.into_iter().take(COMMITS_PER_POLL).map(|(_, _, r)| r).collect();
        if want.is_empty() {
            return Ok((Value::Null, 0, 0));
        }
        let (data, rate) = self.api.fetch_commits(&want, token)?;
        // Deploy runs of later commits, shared by every commit they may contain: each is asked at most once per poll, a
        // successful one is remembered, and one that ended badly is asked again every RUN_RECHECK.
        let runs_file = store::home().join("runs.json");
        // a hand-edited or half-written file must not stop every poll: keep only well-formed entries
        let mut seen: Map<String, Value> = match store::read_json(&runs_file) {
            Some(Value::Object(m)) => {
                m.into_iter().filter(|(_, v)| v.as_array().is_some_and(|a| a.len() == 3 && a[2].is_number())).collect()
            }
            _ => Map::new(),
        };
        let known = |seen: &Map<String, Value>, key: &str| -> Option<(Option<String>, Option<String>)> {
            let v = seen.get(key)?.as_array()?;
            let (status, concl, at) =
                (v[0].as_str().map(String::from), v[1].as_str().map(String::from), v[2].as_f64()?);
            let finished_ok =
                status.as_deref() == Some("COMPLETED") && (commits::is_ok(concl.as_deref()) || now - at < RUN_RECHECK);
            (at >= now || finished_ok).then_some((status, concl))
        };
        let got: HashSet<&CommitRef> = data.iter().map(|(r, _)| r).collect();
        for r in &want {
            if got.contains(r) {
                self.commit_retry.remove(&r.to_string());
            } else {
                // still failing after the batch was split and retried
                self.commit_retry.insert(r.to_string(), now + COMMIT_RETRY);
            }
        }
        for (r, node) in &data {
            let prev = store::read_commit(r).unwrap_or_default();
            let Some(mut s) = commits::summarize(node) else {
                store::write_commit(
                    r,
                    &CommitRecord { r: r.to_string(), fetched_at: now as i64, missing: true, ..Default::default() },
                )?;
                continue;
            };
            // A deployment counts once the workflow run that made it has finished; this commit's runs came with it.
            let mut states: RunStates = commits::own_runs(&s);
            for run in commits::unknown_runs(&s, &states) {
                let key = format!("{}/{}#{run}", r.owner, r.repo);
                let st = match known(&seen, &key) {
                    Some(st) => st,
                    None => match self.api.run_state(&r.owner, &r.repo, &run, token) {
                        Ok(st) => {
                            seen.insert(key, json!([st.0, st.1, now]));
                            st
                        }
                        Err(e) if e.rate_limited => return Err(e.into()),
                        Err(_) => continue,
                    },
                };
                states.insert(run, st);
            }
            commits::gate_deploys(&mut s, &states);
            let mut cache = prev.contains.clone();
            for (env, sha) in commits::needs_containment(&s) {
                if !cache.contains_key(&sha) {
                    // one compare per new deployment, then remembered
                    match self.api.contains(&r.owner, &r.repo, &s.oid, &sha, token) {
                        Ok(v) => {
                            cache.insert(sha.clone(), v);
                        }
                        Err(e) if e.rate_limited => return Err(e.into()),
                        Err(_) => continue,
                    }
                }
                if let Some(e) = s.envs.get_mut(&env) {
                    e.live = cache.get(&sha).copied();
                }
            }
            for e in s.envs.values_mut() {
                // deployed there, but not this commit or a finished deploy containing it
                if e.live.is_none()
                    && e.this.as_deref() != Some("SUCCESS")
                    && (!e.successes.is_empty() || e.last_success.is_some())
                {
                    e.live = Some(false);
                }
            }
            let changed = prev.summary.as_ref().is_none_or(|old| commits::fingerprint(old) != commits::fingerprint(&s));
            if changed {
                store::append_event(
                    &json!({"at": now as i64, "ref": r.to_string(), "commit": commits::one_line(&r.to_string(), &s, None)}),
                )?;
            }
            store::write_commit(
                r,
                &CommitRecord {
                    r: r.to_string(),
                    fetched_at: now as i64,
                    missing: false,
                    summary: Some(s),
                    contains: cache,
                },
            )?;
        }
        let keep: Map<String, Value> =
            seen.into_iter().filter(|(_, v)| now - v[2].as_f64().unwrap_or(0.0) < KEEP_COMMIT_WATCH as f64).collect();
        store::write_json(&runs_file, &Value::Object(keep))?;
        Ok((rate, want.len(), want.len() - data.len()))
    }

    pub fn prune(&mut self, now: f64) -> Result<(), PollError> {
        let w = store::watches();
        let keep: store::Watches = w
            .iter()
            .filter(|(k, _)| match refs::parse(k) {
                None => false,
                Some(r) => {
                    let closed_at = store::read_pr(&r).and_then(|x| x.closed_seen_at);
                    !closed_at.is_some_and(|c| now - c as f64 > KEEP_CLOSED_WATCH)
                }
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if keep != w {
            store::save_watches(&keep)?;
        }
        store::with_commit_watches(|cw| cw.retain(|_, v| !store::commit_watch_expired(v, now, KEEP_COMMIT_WATCH)))?;
        if let Ok(rd) = std::fs::read_dir(store::home().join("commits")) {
            for f in rd.flatten() {
                let old = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .is_some_and(|age| age.as_secs_f64() > KEEP_COMMIT_FILE);
                if old {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        for rec in store::all_prs() {
            if rec.closed_seen_at.is_some_and(|c| now - c as f64 > KEEP_CLOSED_FILE) {
                if let Some(r) = refs::parse(&rec.r) {
                    let _ = std::fs::remove_file(store::pr_path(&r));
                }
            }
        }
        Ok(())
    }

    fn set(&mut self, key: &str, v: Value) {
        self.state.insert(key.into(), v);
    }

    fn save_state(&self) {
        let _ = store::write_json(&store::home().join("daemon.json"), &Value::Object(self.state.clone()));
    }

    /// One poll with its bookkeeping; returns the seconds until the next one.
    pub fn poll_once(&mut self, interval: i64) -> i64 {
        let started = now();
        let st = started as i64;
        self.set("polling_since", json!(st));
        self.save_state();
        let result = catch_unwind(AssertUnwindSafe(|| self.poll()));
        let degraded_since =
            |d: &Map<String, Value>| d.get("degraded_since").filter(|v| truthy(v)).cloned().unwrap_or(json!(st));
        let next = match result {
            Ok(Ok(res)) => {
                let interval = adaptive_interval(self.cfg.interval, &res.rate);
                // Items GitHub kept failing on were skipped and still show their previous state: that is degraded.
                let partial = res.skipped > 0;
                let since = if partial { degraded_since(&self.state) } else { Value::Null };
                let error = if partial {
                    json!(format!("GitHub failed to return {} PRs/commits; their previous state is kept", res.skipped))
                } else {
                    Value::Null
                };
                for (k, v) in [
                    ("last_poll_ok", json!(st)),
                    ("last_error", error),
                    ("tracked", json!(res.tracked)),
                    ("tracked_commits", json!(res.commits)),
                    ("skipped", json!(res.skipped)),
                    ("rate", res.rate.clone()),
                    ("interval", json!(interval)),
                    ("rate_limited_until", Value::Null),
                    ("last_error_transient", if partial { json!(true) } else { Value::Null }),
                    ("degraded_since", since),
                ] {
                    self.set(k, v);
                }
                let skipped =
                    if res.skipped > 0 { format!("{} skipped (GitHub error), ", res.skipped) } else { String::new() };
                let remaining = res.rate.get("remaining").map(|v| v.to_string()).unwrap_or_else(|| "None".into());
                log(&format!(
                    "poll ok in {}s: {} PRs read, {} commits read, {} changed, {skipped}rate {remaining} left, next in {interval}s",
                    (now() - started) as i64,
                    res.tracked,
                    res.commits,
                    res.changed
                ));
                interval
            }
            Ok(Err(PollError::GitHub(e))) => {
                // A GraphQL-level limit carries no reset time; the last rateLimit we read does.
                let last_reset =
                    self.state.get("rate").and_then(|r| r.get("resetAt")).and_then(Value::as_str).map(String::from);
                let reset = e.reset_at.clone().or(last_reset);
                let wait = if e.rate_limited {
                    seconds_until(reset.as_deref(), 300)
                } else {
                    (if e.transient { 300 } else { 600 }).min(60.max(interval * 2))
                };
                let since = if e.transient { degraded_since(&self.state) } else { Value::Null };
                for (k, v) in [
                    ("last_error", json!(crate::util::take(&e.msg, 300))),
                    ("last_error_at", json!(st)),
                    ("interval", json!(wait)),
                    ("rate_limited_until", if e.rate_limited { json!(st + wait) } else { Value::Null }),
                    ("last_error_transient", json!(e.transient)),
                    ("degraded_since", since),
                ] {
                    self.set(k, v);
                }
                let kind = if e.rate_limited {
                    "rate limited"
                } else if e.transient {
                    "GitHub error, retrying"
                } else {
                    "error"
                };
                log(&format!("poll failed ({kind}): {e}; next in {wait}s"));
                wait
            }
            Ok(Err(PollError::Other(msg))) => self.crashed(&msg, interval, st),
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".into());
                self.crashed(&format!("panic: {msg}"), interval, st)
            }
        };
        self.set("last_poll", json!(st));
        self.set("last_poll_done", json!(now_i()));
        self.set("poll_seconds", json!((now() - started) as i64));
        self.set("polling_since", Value::Null);
        self.save_state();
        next
    }

    // never die on a surprise: log it and keep polling
    fn crashed(&mut self, msg: &str, interval: i64, started: i64) -> i64 {
        let next = 600.min(60.max(interval * 2));
        for (k, v) in [
            ("last_error", json!(crate::util::take(msg, 300))),
            ("last_error_at", json!(started)),
            ("interval", json!(next)),
            ("last_error_transient", json!(false)),
            ("degraded_since", Value::Null),
        ] {
            self.set(k, v);
        }
        log(&format!("poll crashed: {msg}; next in {next}s"));
        next
    }

    pub fn run(&mut self, once: bool) {
        log(&format!("prhub daemon started (pid {}, home {})", std::process::id(), store::home().display()));
        let mut interval = self.cfg.interval;
        loop {
            let started = now();
            interval = self.poll_once(interval);
            if once {
                return;
            }
            sleep(interval, started);
        }
    }
}

/// Sleep until the next poll is due, or until an agent's `wait` nudges for an early one.
pub fn sleep(interval: i64, started: f64) {
    while now() - started < interval as f64 {
        std::thread::sleep(Duration::from_secs(2));
        if store::nudged_since(started) && now() - started >= NUDGE_MIN_GAP {
            store::clear_nudge();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commits::tests::{node_at, suite, LATER, OID};
    use crate::conditions::tests::{pr, Pr};
    use crate::github::{CommitBatch, Failing, PrBatch, RunState};
    use crate::testutil::Isolated;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// What the daemon sees of GitHub; each field is the next answer, and calls are counted.
    #[derive(Default)]
    struct Fake {
        open: Vec<PrRef>,
        prs: HashMap<PrRef, Value>,
        failing: Vec<Failing>,
        commits: HashMap<CommitRef, Value>,
        /// commits GitHub fails on every time (left out of fetch_commits' answer)
        commit_errors: HashSet<CommitRef>,
        contains: Option<Result<bool, GhError>>,
        run: Option<Result<RunState, GhError>>,
        token: Option<Result<String, GhError>>,
        discover_error: Option<GhError>,
        rate: Value,
        calls: HashMap<&'static str, usize>,
        commit_reads: Vec<Vec<String>>,
    }

    #[derive(Clone, Default)]
    struct Shared(Rc<RefCell<Fake>>);

    impl Shared {
        fn get(&self) -> std::cell::RefMut<'_, Fake> {
            self.0.borrow_mut()
        }
        fn calls(&self, k: &str) -> usize {
            self.0.borrow().calls.get(k).copied().unwrap_or(0)
        }
    }

    impl Api for Shared {
        fn token(&mut self) -> Result<String, GhError> {
            let mut f = self.get();
            *f.calls.entry("token").or_default() += 1;
            f.token.clone().unwrap_or(Ok("tok".into()))
        }
        fn discover_open(&mut self, _q: &str, _t: &str) -> Result<(Vec<PrRef>, Value), GhError> {
            let mut f = self.get();
            *f.calls.entry("discover").or_default() += 1;
            if let Some(e) = f.discover_error.clone() {
                return Err(e);
            }
            Ok((f.open.clone(), f.rate.clone()))
        }
        fn fetch_prs(&mut self, refs: &[PrRef], _t: &str) -> Result<PrBatch, GhError> {
            let mut f = self.get();
            *f.calls.entry("prs").or_default() += 1;
            Ok((refs.iter().map(|r| (r.clone(), f.prs.get(r).cloned())).collect(), f.rate.clone()))
        }
        fn fetch_failing(&mut self, refs: &[PrRef], _t: &str) -> Result<Vec<(PrRef, Vec<Failing>)>, GhError> {
            let mut f = self.get();
            *f.calls.entry("failing").or_default() += 1;
            Ok(refs.iter().map(|r| (r.clone(), f.failing.clone())).collect())
        }
        fn fetch_commits(&mut self, refs: &[CommitRef], _t: &str) -> Result<CommitBatch, GhError> {
            let mut f = self.get();
            *f.calls.entry("commits").or_default() += 1;
            f.commit_reads.push(refs.iter().map(|r| r.to_string()).collect());
            let out = refs
                .iter()
                .filter(|r| !f.commit_errors.contains(*r))
                .map(|r| {
                    (
                        r.clone(),
                        f.commits
                            .get(r)
                            .cloned()
                            .unwrap_or_else(|| node_at(OID, &[suite("CI")], &[], &[], "2026-10-11T08:00:00Z")),
                    )
                })
                .collect();
            Ok((out, f.rate.clone()))
        }
        fn contains(&mut self, _o: &str, _r: &str, _s: &str, _n: &str, _t: &str) -> Result<bool, GhError> {
            let mut f = self.get();
            *f.calls.entry("contains").or_default() += 1;
            f.contains.clone().unwrap_or(Ok(true))
        }
        fn run_state(&mut self, _o: &str, _r: &str, _run: &str, _t: &str) -> Result<RunState, GhError> {
            let mut f = self.get();
            *f.calls.entry("run").or_default() += 1;
            f.run.clone().unwrap_or(Ok((Some("COMPLETED".into()), Some("SUCCESS".into()))))
        }
    }

    fn cfg() -> Config {
        store::load_config().unwrap()
    }

    fn setup() -> (Isolated, Shared, Daemon) {
        let iso = Isolated::new();
        let gh = Shared::default();
        let api = PrRef::new("acme", "api", 1);
        gh.get().open = vec![api.clone()];
        gh.get().prs.insert(api, pr(Pr::default()));
        gh.get().rate = json!({"remaining": 4000, "resetAt": "2026-10-11T20:00:00Z"});
        let d = Daemon::with_api(cfg(), Box::new(gh.clone()));
        (iso, gh, d)
    }

    fn api_ref() -> PrRef {
        PrRef::new("acme", "api", 1)
    }

    fn ok(d: &mut Daemon) -> PollResult {
        match d.poll() {
            Ok(r) => r,
            Err(PollError::GitHub(e)) => panic!("github: {e}"),
            Err(PollError::Other(e)) => panic!("other: {e}"),
        }
    }

    fn due_again(r: &CommitRef) {
        let mut rec = store::read_commit(r).unwrap();
        rec.fetched_at -= 1000; // a finished commit is otherwise re-read only every few minutes
        store::write_commit(r, &rec).unwrap();
    }

    fn events() -> Vec<Value> {
        std::fs::read_to_string(store::home().join("events.jsonl"))
            .map(|t| t.lines().map(|l| serde_json::from_str(l).unwrap()).collect())
            .unwrap_or_default()
    }

    fn runs_seen() -> Map<String, Value> {
        match store::read_json(&store::home().join("runs.json")) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }

    fn age_runs(seconds: f64) {
        let m: Map<String, Value> =
            runs_seen().into_iter().map(|(k, v)| (k, json!([v[0], v[1], v[2].as_f64().unwrap() - seconds]))).collect();
        store::write_json(&store::home().join("runs.json"), &Value::Object(m)).unwrap();
    }

    #[test]
    fn pacing() {
        assert_eq!(adaptive_interval(60, &json!({"remaining": 4000})), 60);
        assert_eq!(adaptive_interval(60, &json!({"remaining": 900})), 180);
        assert_eq!(adaptive_interval(60, &json!({"remaining": 100})), 600);
        assert_eq!(adaptive_interval(60, &json!({})), 60);
        assert_eq!(seconds_until(None, 300), 300);
        assert_eq!(seconds_until(Some("garbage"), 300), 300);
        assert_eq!(seconds_until(Some("2000-01-01T00:00:00Z"), 300), 30); // already past: retry soon, never immediately
        let soon = (now_i() + 100).to_string();
        assert!((105..=111).contains(&seconds_until(Some(soon.as_str()), 300)));
        assert!(due(false, 0.0, 0.0, true, 300.0, 1000.0) && due(true, 900.0, 0.0, false, 300.0, 1000.0));
        assert!(!due(true, 900.0, 0.0, true, 300.0, 1000.0) && due(true, 900.0, 950.0, true, 300.0, 1000.0));
    }

    #[test]
    fn a_poll_records_prs_and_their_changes() {
        let (_iso, gh, mut d) = setup();
        let res = ok(&mut d);
        assert_eq!((res.tracked, res.changed), (1, 1));
        let rec = store::read_pr(&api_ref()).unwrap();
        assert_eq!(rec.summary.checks.as_deref(), Some("SUCCESS"));
        assert!(events()[0]["from"].is_null() && events()[0]["to"]["checks"] == "SUCCESS");
        assert_eq!(ok(&mut d).changed, 0);
        gh.get().prs.insert(api_ref(), pr(Pr { checks: Some("FAILURE"), ..Default::default() }));
        gh.get().failing = vec![Failing { name: "test".into(), conclusion: Some("FAILURE".into()), url: None }];
        ok(&mut d);
        let rec = store::read_pr(&api_ref()).unwrap();
        assert_eq!(rec.failing[0].name, "test");
        assert_eq!(gh.calls("failing"), 1);
        ok(&mut d); // still red at the same head: no second lookup
        assert_eq!(gh.calls("failing"), 1);
        assert_eq!(store::read_pr(&api_ref()).unwrap().failing[0].name, "test");
    }

    #[test]
    fn a_merged_pr_is_seen_closing_and_then_read_rarely() {
        let (_iso, gh, mut d) = setup();
        ok(&mut d);
        gh.get().prs.insert(api_ref(), pr(Pr { state: "MERGED", ..Default::default() }));
        ok(&mut d);
        let rec = store::read_pr(&api_ref()).unwrap();
        assert!(rec.summary.state.as_deref() == Some("MERGED") && rec.closed_seen_at.is_some());
        let first = rec.closed_seen_at;
        gh.get().open.clear();
        d.last_discover = 0.0;
        let before = gh.calls("prs");
        ok(&mut d);
        assert_eq!(gh.calls("prs"), before); // closed and not open any more: not due yet
        assert_eq!(store::read_pr(&api_ref()).unwrap().closed_seen_at, first);
    }

    #[test]
    fn a_watched_pr_that_does_not_resolve_is_skipped() {
        let (_iso, gh, mut d) = setup();
        gh.get().open.clear();
        let ghost = PrRef::new("acme", "api", 999);
        store::add_watch(&ghost, "cli").unwrap();
        assert_eq!(ok(&mut d).changed, 0);
        assert!(store::read_pr(&ghost).is_none());
    }

    #[test]
    fn commit_live_through_a_later_deploy_asks_compare_once() {
        let (_iso, gh, mut d) = setup();
        let r = CommitRef::new("acme", "api", "aaaaaaa");
        store::add_commit_watch(&r, "cli", store::STATE_KEEP).unwrap();
        gh.get().commits.insert(
            r.clone(),
            node_at(
                OID,
                &[suite("CI")],
                &[],
                &[(LATER, "production", "2026-10-12T01:00:00Z", "SUCCESS", None)],
                "2026-10-11T08:00:00Z",
            ),
        );
        assert_eq!(ok(&mut d).commits, 1);
        let rec = store::read_commit(&r).unwrap();
        assert_eq!(rec.summary.unwrap().envs["production"].live, Some(true));
        assert_eq!(rec.contains.get(LATER), Some(&true));
        due_again(&r);
        ok(&mut d);
        assert_eq!(gh.calls("contains"), 1); // remembered per deployment
    }

    #[test]
    fn commit_with_an_older_deploy_is_not_live_and_needs_no_compare() {
        let (_iso, gh, mut d) = setup();
        let r = CommitRef::new("acme", "api", "aaaaaaa");
        store::add_commit_watch(&r, "cli", store::STATE_KEEP).unwrap();
        gh.get().commits.insert(
            r.clone(),
            node_at(
                OID,
                &[suite("CI")],
                &[],
                &[(LATER, "production", "2026-10-01T01:00:00Z", "SUCCESS", None)],
                "2026-10-11T08:00:00Z",
            ),
        );
        ok(&mut d);
        assert_eq!(store::read_commit(&r).unwrap().summary.unwrap().envs["production"].live, Some(false));
        assert_eq!(gh.calls("contains"), 0);
    }

    #[test]
    fn compare_errors() {
        let (_iso, gh, mut d) = setup();
        let r = CommitRef::new("acme", "api", "aaaaaaa");
        store::add_commit_watch(&r, "cli", store::STATE_KEEP).unwrap();
        gh.get().commits.insert(
            r.clone(),
            node_at(
                OID,
                &[suite("CI")],
                &[],
                &[(LATER, "production", "2026-10-12T01:00:00Z", "SUCCESS", None)],
                "2026-10-11T08:00:00Z",
            ),
        );
        gh.get().contains = Some(Err(GhError::new("compare HTTP 500")));
        ok(&mut d); // an ordinary error leaves the answer unknown and asks again next poll
        assert!(store::read_commit(&r).unwrap().contains.is_empty());
        gh.get().contains = Some(Err(GhError { rate_limited: true, ..GhError::new("limit") }));
        due_again(&r);
        assert!(matches!(d.poll(), Err(PollError::GitHub(e)) if e.rate_limited));
    }

    #[test]
    fn a_later_deploy_is_live_only_once_its_run_has_finished() {
        let (_iso, gh, mut d) = setup();
        let r = CommitRef::new("acme", "api", "aaaaaaa");
        store::add_commit_watch(&r, "cli", store::STATE_KEEP).unwrap();
        let job = Some("https://github.com/acme/api/actions/runs/9/job/90");
        gh.get().commits.insert(
            r.clone(),
            node_at(
                OID,
                &[suite("CI")],
                &[],
                &[(LATER, "production", "2026-10-12T01:00:00Z", "SUCCESS", job)],
                "2026-10-11T08:00:00Z",
            ),
        );
        let live = |r: &CommitRef| store::read_commit(r).unwrap().summary.unwrap().envs["production"].live;
        gh.get().run = Some(Ok((Some("IN_PROGRESS".into()), None)));
        ok(&mut d);
        assert!(live(&r) == Some(false) && gh.calls("contains") == 0 && gh.calls("run") == 1);
        gh.get().run = Some(Err(GhError::new("run HTTP 500")));
        due_again(&r);
        ok(&mut d); // still in progress last time: asked again; an ordinary error leaves it unknown, not live
        assert!(live(&r) == Some(false) && gh.calls("run") == 2);
        gh.get().run = Some(Ok((Some("COMPLETED".into()), Some("FAILURE".into()))));
        due_again(&r);
        ok(&mut d);
        assert!(live(&r) == Some(false) && gh.calls("run") == 3);
        gh.get().run = Some(Ok((Some("COMPLETED".into()), Some("SUCCESS".into()))));
        due_again(&r);
        ok(&mut d); // a run that ended badly is not asked again right away...
        assert!(live(&r) == Some(false) && gh.calls("run") == 3);
        age_runs(RUN_RECHECK + 1.0);
        due_again(&r);
        ok(&mut d); // ...but after RUN_RECHECK, since a re-run keeps its id and may succeed
        assert!(live(&r) == Some(true) && gh.calls("run") == 4);
        assert_eq!(runs_seen()["acme/api#9"][0], "COMPLETED");
        age_runs(RUN_RECHECK + 1.0);
        due_again(&r);
        ok(&mut d);
        assert_eq!(gh.calls("run"), 4); // a successful run is remembered
        age_runs(KEEP_COMMIT_WATCH as f64 + 1.0);
        due_again(&r);
        ok(&mut d);
        assert!(runs_seen().is_empty()); // and forgotten with the watches that could need it
        gh.get().run = Some(Err(GhError { rate_limited: true, ..GhError::new("limit") }));
        gh.get().commits.insert(
            r.clone(),
            node_at(
                OID,
                &[suite("CI")],
                &[],
                &[(
                    LATER,
                    "production",
                    "2026-10-12T02:00:00Z",
                    "SUCCESS",
                    Some("https://github.com/acme/api/actions/runs/10/job/100"),
                )],
                "2026-10-11T08:00:00Z",
            ),
        );
        due_again(&r);
        assert!(matches!(d.poll(), Err(PollError::GitHub(e)) if e.rate_limited));
    }

    #[test]
    fn a_malformed_runs_file_is_ignored_and_rewritten() {
        for junk in [json!(["not", "a", "dict"]), json!({"acme/api#9": "x", "acme/api#8": [1, 2]})] {
            let (_iso, gh, mut d) = setup();
            let r = CommitRef::new("acme", "api", "aaaaaaa");
            store::add_commit_watch(&r, "cli", store::STATE_KEEP).unwrap();
            gh.get().commits.insert(
                r.clone(),
                node_at(
                    OID,
                    &[suite("CI")],
                    &[],
                    &[(
                        LATER,
                        "production",
                        "2026-10-12T01:00:00Z",
                        "SUCCESS",
                        Some("https://github.com/acme/api/actions/runs/9/job/90"),
                    )],
                    "2026-10-11T08:00:00Z",
                ),
            );
            store::write_json(&store::home().join("runs.json"), &junk).unwrap();
            ok(&mut d);
            assert_eq!(store::read_commit(&r).unwrap().summary.unwrap().envs["production"].live, Some(true));
            assert_eq!(runs_seen().keys().cloned().collect::<Vec<_>>(), vec!["acme/api#9"]);
        }
    }

    #[test]
    fn commits_contained_by_one_deploy_run_ask_it_once_per_poll() {
        let (_iso, gh, mut d) = setup();
        let refs = [CommitRef::new("acme", "api", "aaaaaaa"), CommitRef::new("acme", "api", "ccccccc")];
        for (r, oid) in refs.iter().zip(["a".repeat(40), "c".repeat(40)]) {
            store::add_commit_watch(r, "cli", store::STATE_KEEP).unwrap();
            gh.get().commits.insert(
                r.clone(),
                node_at(
                    &oid,
                    &[suite("CI")],
                    &[],
                    &[(
                        LATER,
                        "production",
                        "2026-10-12T01:00:00Z",
                        "SUCCESS",
                        Some("https://github.com/acme/api/actions/runs/9/job/90"),
                    )],
                    "2026-10-11T08:00:00Z",
                ),
            );
        }
        gh.get().run = Some(Ok((Some("IN_PROGRESS".into()), None)));
        ok(&mut d);
        assert_eq!(gh.calls("run"), 1);
        refs.iter().for_each(due_again);
        ok(&mut d);
        assert_eq!(gh.calls("run"), 2); // still running: asked again, once
    }

    #[test]
    fn a_poll_reads_at_most_its_share_of_commits_most_urgent_first() {
        let (_iso, gh, mut d) = setup();
        let many: Vec<CommitRef> = (0..COMMITS_PER_POLL + 5)
            .map(|i| CommitRef::new("acme", "api", &format!("{:07x}", 0x1000000 + i)))
            .collect();
        for r in &many {
            store::add_commit_watch(r, "cli", store::STATE_KEEP).unwrap();
        }
        ok(&mut d);
        assert_eq!(gh.get().commit_reads.last().unwrap().len(), COMMITS_PER_POLL);
        ok(&mut d); // the rest next poll; the ones just read are finished and not due yet
        let reads = gh.get().commit_reads.clone();
        assert_eq!(reads[1].len(), 5);
        assert!(reads[1].iter().all(|r| !reads[0].contains(r)));
        store::with_commit_watches(|w| {
            for r in &many {
                // all finished and due again, nobody asking
                w.get_mut(&r.to_string()).unwrap().last_asked = Some(now_i() - 2000);
            }
        })
        .unwrap();
        many.iter().for_each(due_again);
        store::add_commit_watch(many.last().unwrap(), "cli", store::STATE_KEEP).unwrap(); // just asked about: first
        ok(&mut d);
        let last = gh.get().commit_reads.last().unwrap().clone();
        assert_eq!(last[0], many.last().unwrap().to_string());
        assert_eq!(last.len(), COMMITS_PER_POLL);
    }

    #[test]
    fn a_commit_github_keeps_failing_on_backs_off() {
        let (_iso, gh, mut d) = setup();
        let bad = CommitRef::new("acme", "api", "aaaaaaa");
        let good = CommitRef::new("acme", "api", "bbbbbbb");
        store::add_commit_watch(&bad, "cli", store::STATE_KEEP).unwrap();
        store::add_commit_watch(&good, "cli", store::STATE_KEEP).unwrap();
        gh.get().commit_errors.insert(bad.clone());
        ok(&mut d);
        assert_eq!(gh.get().commit_reads.last().unwrap(), &vec![bad.to_string(), good.to_string()]);
        due_again(&good);
        ok(&mut d);
        assert_eq!(gh.get().commit_reads.last().unwrap(), &vec![good.to_string()]); // the failing one waits out COMMIT_RETRY
        d.commit_retry.insert(bad.to_string(), 0.0);
        due_again(&good);
        ok(&mut d);
        assert!(gh.get().commit_reads.last().unwrap().contains(&bad.to_string()));
    }

    #[test]
    fn an_expired_commit_watch_is_not_read() {
        let (_iso, gh, mut d) = setup();
        let r = CommitRef::new("acme", "api", "aaaaaaa");
        store::add_commit_watch(&r, "cli", store::STATE_KEEP).unwrap();
        store::with_commit_watches(|w| w.get_mut(&r.to_string()).unwrap().keep_until = Some(now_i() - 1)).unwrap();
        ok(&mut d);
        assert_eq!(gh.calls("commits"), 0);
        assert!(store::read_commit(&r).is_none() && store::commit_watches().is_empty());
    }

    #[test]
    fn a_missing_commit_is_recorded_as_missing() {
        let (_iso, gh, mut d) = setup();
        let r = CommitRef::new("acme", "api", "aaaaaaa");
        store::add_commit_watch(&r, "cli", store::STATE_KEEP).unwrap();
        gh.get().commits.insert(r.clone(), json!({"object": null}));
        ok(&mut d);
        assert!(store::read_commit(&r).unwrap().missing);
        assert!(events().iter().all(|e| e.get("commit").is_none()));
    }

    #[test]
    fn prune_drops_closed_watches_and_old_files() {
        let (_iso, _gh, mut d) = setup();
        let t = now();
        let closed = PrRef::new("acme", "api", 7);
        store::add_watch(&closed, "cli").unwrap();
        store::write_pr(
            &closed,
            &PrRecord { r: closed.to_string(), closed_seen_at: Some((t - 8.0 * 86400.0) as i64), ..Default::default() },
        )
        .unwrap();
        store::add_watch(&api_ref(), "cli").unwrap();
        let old = CommitRef::new("acme", "api", "aaaaaaa");
        store::add_commit_watch(&old, "cli", store::STATE_KEEP).unwrap();
        store::with_commit_watches(|w| w.get_mut(&old.to_string()).unwrap().keep_until = Some(t as i64 - 1)).unwrap();
        let stale = store::commit_path(&CommitRef::new("acme", "api", "bbbbbbb"));
        std::fs::write(&stale, "{}").unwrap();
        let f = std::fs::File::options().write(true).open(&stale).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(3 * 86400)).unwrap();
        d.prune(t).ok();
        assert_eq!(store::watches().keys().cloned().collect::<Vec<_>>(), vec!["acme/api#1"]);
        assert!(store::read_pr(&closed).is_none() && store::commit_watches().is_empty() && !stale.exists());
    }

    #[test]
    fn run_once_records_health() {
        let (_iso, _gh, mut d) = setup();
        d.run(true);
        let s = store::daemon_state();
        assert!(s["last_poll_ok"].is_number() && s["polling_since"].is_null() && s["tracked"] == 1);
        assert!(store::daemon_healthy(300.0));
    }

    #[test]
    fn a_rate_limit_pauses_until_the_reset() {
        let (_iso, gh, mut d) = setup();
        let reset = (now_i() + 900).to_string();
        gh.get().discover_error = Some(GhError {
            rate_limited: true,
            reset_at: Some(reset),
            ..GhError::new("HTTP 403: API rate limit exceeded")
        });
        let wait = d.poll_once(60);
        assert!((900..=915).contains(&wait));
        let s = store::daemon_state();
        assert!(s["rate_limited_until"].is_number() && store::daemon_healthy(300.0));
        assert!(store::rate_limited_until(&s).is_some());
    }

    #[test]
    fn transient_errors_degrade_instead_of_failing() {
        let (_iso, gh, mut d) = setup();
        gh.get().discover_error = Some(GhError::transient("HTTP 502: bad gateway"));
        assert_eq!(d.poll_once(60), 120);
        let first = store::daemon_state()["degraded_since"].clone();
        assert!(first.is_number() && store::degraded_since(&store::daemon_state()).is_some());
        d.poll_once(60);
        assert_eq!(store::daemon_state()["degraded_since"], first); // the start of the trouble is kept
        gh.get().discover_error = None;
        d.poll_once(60);
        assert!(store::daemon_state()["degraded_since"].is_null());
        gh.get().discover_error = Some(GhError::new("HTTP 401: Bad credentials"));
        d.last_discover = 0.0; // discovery runs every few minutes; make this poll search again
        assert_eq!(d.poll_once(60), 120);
        assert!(
            store::daemon_state()["degraded_since"].is_null()
                && !truthy(&store::daemon_state()["last_error_transient"])
        );
    }

    #[test]
    fn skipped_items_mean_degraded() {
        let (_iso, gh, mut d) = setup();
        let r = CommitRef::new("acme", "api", "aaaaaaa");
        store::add_commit_watch(&r, "cli", store::STATE_KEEP).unwrap();
        gh.get().commit_errors.insert(r);
        d.poll_once(60);
        let s = store::daemon_state();
        assert_eq!(s["skipped"], 1);
        assert!(s["last_error"].as_str().unwrap().contains("failed to return 1"));
        assert!(store::degraded_since(&s).is_some());
    }

    #[test]
    fn a_crash_is_logged_and_survived() {
        let (_iso, gh, mut d) = setup();
        gh.get().prs.insert(api_ref(), json!({"state": "OPEN"}));
        let prs = store::home().join("prs"); // home() recreates the folder, so take the path once
        std::fs::remove_dir_all(&prs).ok();
        std::fs::write(&prs, "not a folder").unwrap();
        let next = d.poll_once(60);
        assert_eq!(next, 120);
        let s = store::daemon_state();
        assert!(s["last_error"].as_str().unwrap().starts_with("OSError") && s["polling_since"].is_null());
    }

    #[test]
    fn tokens_are_cached_and_a_slow_gh_keeps_the_old_one() {
        let (_iso, gh, mut d) = setup();
        assert_eq!(d.get_token().unwrap(), "tok");
        d.get_token().unwrap();
        assert_eq!(gh.calls("token"), 1);
        d.token_at -= TOKEN_TTL + 1.0;
        gh.get().token = Some(Err(GhError::transient("`gh auth token` took longer than 30s")));
        assert_eq!(d.get_token().unwrap(), "tok");
        d.token = None;
        assert!(d.get_token().is_err());
    }
}
