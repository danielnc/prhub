//! Local state shared by the daemon (writer) and every CLI call (readers). Plain JSON files, atomic writes.

use crate::commits::{CommitRef, CommitSummary};
use crate::conditions::Summary;
use crate::github::Failing;
use crate::refs::PrRef;
use crate::util::{de_i64, de_opt_i64, expand_user, now, now_i, num, pid_alive};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

thread_local! {
    static OVERRIDES: RefCell<(Option<PathBuf>, Option<PathBuf>)> = const { RefCell::new((None, None)) };
}

/// Point this thread at another home and config (tests and fuzzing; the CLI uses PRHUB_HOME and PRHUB_CONFIG).
pub fn set_overrides(home: Option<PathBuf>, config: Option<PathBuf>) {
    OVERRIDES.with(|o| *o.borrow_mut() = (home, config));
}

fn env_path(var: &str, default: &str) -> PathBuf {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => expand_user(default),
    }
}

pub fn home() -> PathBuf {
    let p = OVERRIDES.with(|o| o.borrow().0.clone()).unwrap_or_else(|| env_path("PRHUB_HOME", "~/.local/state/prhub"));
    let _ = fs::create_dir_all(p.join("prs"));
    p
}

pub fn config_path() -> PathBuf {
    OVERRIDES.with(|o| o.borrow().1.clone()).unwrap_or_else(|| env_path("PRHUB_CONFIG", "~/.config/prhub/config.toml"))
}

#[derive(Clone, Debug)]
pub struct Config {
    /// What the daemon discovers on its own.
    pub search: String,
    /// Seconds between polls.
    pub interval: i64,
    /// Seconds between discovery searches.
    pub discover_every: i64,
    /// Folders whose git clones become repo nicknames, e.g. ["~/code"].
    pub clone_roots: Vec<String>,
    /// nickname -> repo name or owner/repo.
    pub aliases: BTreeMap<String, String>,
    /// mergeStateStatus values that count as ready for `done`.
    pub merge_states: Vec<String>,
}

fn default_config() -> Value {
    serde_json::json!({
        "search": "is:pr is:open author:@me",
        "interval": 60,
        "discover_every": 180,
        "clone_roots": [],
        "aliases": {},
        "done": {"merge_states": ["CLEAN", "HAS_HOOKS"]},
    })
}

pub fn load_config() -> Result<Config, String> {
    let mut cfg = default_config();
    let p = config_path();
    if p.exists() {
        let text = fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        let user: toml::Value = toml::from_str(&text).map_err(|e| format!("bad config {}: {e}", p.display()))?;
        let user = serde_json::to_value(user).map_err(|e| e.to_string())?;
        if let (Some(c), Value::Object(u)) = (cfg.as_object_mut(), user) {
            for (k, v) in u {
                match (c.get_mut(&k), v) {
                    (Some(Value::Object(dst)), Value::Object(src)) => dst.extend(src),
                    (_, v) => {
                        c.insert(k, v);
                    }
                }
            }
        }
    }
    let strs = |v: &Value| -> Vec<String> {
        v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
    };
    Ok(Config {
        search: cfg["search"].as_str().unwrap_or("is:pr is:open author:@me").to_string(),
        interval: cfg["interval"].as_i64().unwrap_or(60),
        discover_every: cfg["discover_every"].as_i64().unwrap_or(180),
        clone_roots: strs(&cfg["clone_roots"]),
        aliases: cfg["aliases"]
            .as_object()
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default(),
        merge_states: strs(&cfg["done"]["merge_states"]),
    })
}

pub fn write_json<T: Serialize + ?Sized>(path: &Path, data: &T) -> io::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or(""),
        std::process::id()
    ));
    let text = serde_json::to_string_pretty(data).map_err(io::Error::other)?;
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

pub fn read_json(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn read_as<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

// ---- pull requests ----

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct PrRecord {
    #[serde(rename = "ref")]
    pub r: String,
    #[serde(deserialize_with = "de_i64")]
    pub fetched_at: i64,
    pub summary: Summary,
    pub pr: Value,
    pub failing: Vec<Failing>,
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "de_opt_i64")]
    pub closed_seen_at: Option<i64>,
}

pub fn pr_path(r: &PrRef) -> PathBuf {
    home().join("prs").join(format!("{}.json", r.slug()))
}

pub fn read_pr(r: &PrRef) -> Option<PrRecord> {
    read_as(&pr_path(r))
}

pub fn write_pr(r: &PrRef, rec: &PrRecord) -> io::Result<()> {
    write_json(&pr_path(r), rec)
}

pub fn all_prs() -> Vec<PrRecord> {
    let mut paths: Vec<PathBuf> = fs::read_dir(home().join("prs"))
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect())
        .unwrap_or_default();
    paths.sort();
    paths
        .iter()
        .filter_map(|p| read_as::<PrListed>(p))
        .filter(|r| !r.r.is_empty())
        .map(|r| PrRecord {
            r: r.r,
            fetched_at: r.fetched_at,
            summary: r.summary,
            pr: Value::Null,
            failing: r.failing,
            closed_seen_at: r.closed_seen_at,
        })
        .collect()
}

/// A PR record without the raw GraphQL fields: reading every record at once (listing, discovery, pruning) then skips
/// the bulk of each file instead of building it in memory.
#[derive(Deserialize, Default)]
#[serde(default)]
struct PrListed {
    #[serde(rename = "ref")]
    r: String,
    #[serde(deserialize_with = "de_i64")]
    fetched_at: i64,
    summary: Summary,
    failing: Vec<Failing>,
    #[serde(deserialize_with = "de_opt_i64")]
    closed_seen_at: Option<i64>,
}

/// Something an agent asked about: who, when, and (commits) how long to keep reading it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Watch {
    #[serde(deserialize_with = "de_i64")]
    pub added_at: i64,
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "de_opt_i64")]
    pub last_asked: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "de_opt_i64")]
    pub keep_until: Option<i64>,
}

pub type Watches = BTreeMap<String, Watch>;

fn read_watches(path: &Path) -> Watches {
    read_as(path).unwrap_or_default()
}

pub fn watches() -> Watches {
    read_watches(&home().join("watch.json"))
}

pub fn add_watch(r: &PrRef, by: &str) -> io::Result<()> {
    let mut w = watches();
    let now = now_i();
    let e = w.entry(r.to_string()).or_insert(Watch { added_at: now, by: Some(by.into()), ..Default::default() });
    e.last_asked = Some(now); // the daemon reads anything asked about since its last read
    save_watches(&w)
}

pub fn save_watches(w: &Watches) -> io::Result<()> {
    write_json(&home().join("watch.json"), w)
}

// ---- commits ----
// Commits (`owner/repo@sha`) have their own watch file and records: a commit is only interesting for a while (until
// its CI and deploy finish), so these expire instead of following a PR's open/closed life.

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct CommitRecord {
    #[serde(rename = "ref")]
    pub r: String,
    #[serde(deserialize_with = "de_i64")]
    pub fetched_at: i64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<CommitSummary>,
    pub contains: BTreeMap<String, bool>,
}

pub fn commit_path(r: &CommitRef) -> PathBuf {
    let p = home().join("commits");
    let _ = fs::create_dir_all(&p);
    p.join(format!("{}.json", r.slug()))
}

pub fn read_commit(r: &CommitRef) -> Option<CommitRecord> {
    read_as(&commit_path(r))
}

pub fn write_commit(r: &CommitRef, rec: &CommitRecord) -> io::Result<()> {
    write_json(&commit_path(r), rec)
}

pub fn commit_watches() -> Watches {
    read_watches(&home().join("commit-watch.json"))
}

// How long asking about a commit keeps the daemon reading it. A `state` read is a glance; a running `wait` renews its
// claim every WAIT_RENEW, and when it ends the commit stays tracked RELEASE_KEEP more (for a follow-up `state`).
pub const STATE_KEEP: i64 = 900;
pub const WAIT_KEEP: i64 = 900;
pub const WAIT_RENEW: i64 = 300;
pub const RELEASE_KEEP: i64 = 600;

/// Read, change and save commit-watch.json under an inter-process lock, so concurrent waits, state reads and the
/// daemon's pruning never write over each other's renewals.
pub fn with_commit_watches<R>(f: impl FnOnce(&mut Watches) -> R) -> io::Result<R> {
    let lock = OpenOptions::new().create(true).append(true).open(home().join("commit-watch.lock"))?;
    // SAFETY: flock on a descriptor we own; the lock is released when `lock` closes even if we panic.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let path = home().join("commit-watch.json");
    let mut w = read_watches(&path);
    let out = f(&mut w);
    let saved = write_json(&path, &w);
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    saved.map(|_| out)
}

pub fn add_commit_watch(r: &CommitRef, by: &str, keep: i64) -> io::Result<()> {
    with_commit_watches(|w| {
        let now = now_i();
        let e = w.entry(r.to_string()).or_insert(Watch { added_at: now, by: Some(by.into()), ..Default::default() });
        e.last_asked = Some(now);
        e.keep_until = Some(e.keep_until.unwrap_or(0).max(now + keep));
    })
}

/// A `wait` on this commit ended: stop reading it soon, unless another `state` or `wait` renews it.
pub fn release_commit_watch(r: &CommitRef) -> io::Result<()> {
    with_commit_watches(|w| {
        if let Some(e) = w.get_mut(&r.to_string()) {
            e.keep_until = Some(e.keep_until.unwrap_or(0).min(now_i() + RELEASE_KEEP));
        }
    })
}

/// Watches written before `keep_until` existed expire `legacy_keep` after they were last asked about.
pub fn commit_watch_expired(e: &Watch, now: f64, legacy_keep: i64) -> bool {
    let until = match e.keep_until {
        Some(u) if u != 0 => u,
        _ => e.last_asked.unwrap_or(e.added_at) + legacy_keep,
    };
    now >= until as f64
}

// ---- events, nudges, daemon health ----

/// events.jsonl past this size is moved to events.jsonl.1 (replacing the previous one), so it never grows unbounded.
pub const EVENTS_MAX_BYTES: u64 = 20 * 1024 * 1024;

pub fn append_event(event: &Value) -> io::Result<()> {
    let path = home().join("events.jsonl");
    if fs::metadata(&path).map(|m| m.len() > EVENTS_MAX_BYTES).unwrap_or(false) {
        fs::rename(&path, home().join("events.jsonl.1"))?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(event).map_err(io::Error::other)?)
}

/// Ask the daemon for an early poll (it rate-limits these itself).
pub fn nudge() {
    let path = home().join("nudge");
    if let Ok(f) = OpenOptions::new().create(true).write(true).truncate(false).open(&path) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

pub fn nudged_since(t: f64) -> bool {
    fs::metadata(home().join("nudge"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() > t)
        .unwrap_or(false)
}

pub fn clear_nudge() {
    let _ = fs::remove_file(home().join("nudge"));
}

pub type DaemonState = Map<String, Value>;

pub fn daemon_state() -> DaemonState {
    match read_json(&home().join("daemon.json")) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn alive(d: &DaemonState) -> bool {
    num(d.get("pid")).map(|p| pid_alive(p as i64)).unwrap_or(false)
}

fn positive(d: &DaemonState, key: &str) -> Option<f64> {
    num(d.get(key)).filter(|v| *v != 0.0)
}

/// When the daemon is waiting out a GitHub rate limit: the epoch it resumes at, else None.
pub fn rate_limited_until(d: &DaemonState) -> Option<i64> {
    let until = positive(d, "rate_limited_until")?;
    (until > now() - 120.0 && alive(d)).then_some(until as i64)
}

/// When GitHub keeps failing the daemon's requests transiently (5xx, its query timeouts, network errors) while the
/// daemon is alive and still retrying: the epoch the trouble started, else None.
pub fn degraded_since(d: &DaemonState) -> Option<i64> {
    let since = positive(d, "degraded_since")?;
    let interval = positive(d, "interval").unwrap_or(60.0);
    let retrying = now() - num(d.get("last_poll")).unwrap_or(0.0) < 300f64.max(3.0 * interval);
    let transient = d.get("last_error_transient").map(crate::util::truthy).unwrap_or(false);
    (transient && retrying && alive(d)).then_some(since as i64)
}

/// A poll still running after this long is stuck, not slow.
pub const MAX_POLL_SECONDS: f64 = 900.0;

/// When the alive daemon started the poll it is in the middle of, else None. A poll can take minutes when GitHub or
/// the machine is slow, and that is not an outage.
pub fn polling_since(d: &DaemonState) -> Option<i64> {
    let since = positive(d, "polling_since")?;
    (now() - since < MAX_POLL_SECONDS && alive(d)).then_some(since as i64)
}

/// Doing its job: finished a good poll recently, is in the middle of one, or is alive and deliberately waiting out a
/// rate limit or a run of GitHub errors. None of those is an outage of prhub: agents polling by hand then would only
/// hit the same limit or errors.
pub fn daemon_healthy(max_age: f64) -> bool {
    let d = daemon_state();
    let ok = positive(&d, "last_poll_ok");
    let done = positive(&d, "last_poll_done").or(ok);
    if let (Some(_), Some(done)) = (ok, done) {
        if now() - done < max_age {
            return true;
        }
    }
    polling_since(&d).is_some() || rate_limited_until(&d).is_some() || degraded_since(&d).is_some()
}

/// Keep a file handle around just to hold a lock in tests of the locking itself.
pub fn open_lock() -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(home().join("commit-watch.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Isolated;
    use serde_json::json;

    #[test]
    fn config_defaults_and_overrides() {
        let iso = Isolated::new();
        let c = load_config().unwrap();
        assert_eq!(c.interval, 60);
        assert_eq!(c.merge_states, vec!["CLEAN", "HAS_HOOKS"]);
        fs::write(
            iso.dir.path().join("config.toml"),
            "interval = 30\nclone_roots = [\"~/code\"]\n[aliases]\nbackend = \"acme/api\"\n[done]\nmerge_states = [\"CLEAN\"]\n",
        )
        .unwrap();
        let c = load_config().unwrap();
        assert_eq!((c.interval, c.discover_every), (30, 180));
        assert_eq!(c.aliases.get("backend").map(String::as_str), Some("acme/api"));
        assert_eq!(c.merge_states, vec!["CLEAN"]);
        assert_eq!(c.clone_roots, vec!["~/code"]);
        fs::write(iso.dir.path().join("config.toml"), "interval = [").unwrap();
        assert!(load_config().unwrap_err().contains("bad config"));
    }

    #[test]
    fn read_json_tolerates_missing_and_broken_files() {
        let iso = Isolated::new();
        let p = iso.dir.path().join("x.json");
        assert!(read_json(&p).is_none());
        fs::write(&p, "{not json").unwrap();
        assert!(read_json(&p).is_none());
        write_json(&p, &json!({"a": 1})).unwrap();
        assert_eq!(read_json(&p), Some(json!({"a": 1})));
        fs::write(home().join("watch.json"), "[1, 2]").unwrap();
        assert!(watches().is_empty());
    }

    #[test]
    fn watches_record_who_and_when() {
        let _iso = Isolated::new();
        let r = PrRef::new("acme", "api", 1);
        add_watch(&r, "pane-1").unwrap();
        let w = watches();
        assert_eq!(w[&r.to_string()].by.as_deref(), Some("pane-1"));
        assert!(w[&r.to_string()].last_asked.is_some());
    }

    #[test]
    fn commit_watch_lifetimes() {
        let _iso = Isolated::new();
        let r = CommitRef::new("acme", "api", "aaaaaaa");
        let t = now_i();
        add_commit_watch(&r, "cli", STATE_KEEP).unwrap(); // a state read is a glance
        let e = commit_watches()[&r.to_string()].clone();
        let until = e.keep_until.unwrap();
        assert!((t + STATE_KEEP - 2..=t + STATE_KEEP + 2).contains(&until));
        assert!(!commit_watch_expired(&e, t as f64, 3600) && commit_watch_expired(&e, until as f64, 3600));
        add_commit_watch(&r, "cli", 5000).unwrap(); // a longer claim wins over a shorter one
        add_commit_watch(&r, "cli", STATE_KEEP).unwrap();
        assert!(commit_watches()[&r.to_string()].keep_until.unwrap() >= t + 4990);
        release_commit_watch(&r).unwrap(); // a wait ended: tracked a little longer, then dropped
        assert!(commit_watches()[&r.to_string()].keep_until.unwrap() <= now_i() + RELEASE_KEEP + 1);
        release_commit_watch(&CommitRef::new("acme", "api", "bbbbbbb")).unwrap(); // nothing to release
        assert_eq!(commit_watches().len(), 1);
        let legacy = Watch { added_at: 100, last_asked: Some(1000), ..Default::default() }; // before keep_until
        assert!(!commit_watch_expired(&legacy, 4599.0, 3600) && commit_watch_expired(&legacy, 4600.0, 3600));
        let _held = open_lock().unwrap();
    }

    #[test]
    fn events_rotate_past_their_cap() {
        let _iso = Isolated::new();
        append_event(&json!({"a": 1})).unwrap();
        let p = home().join("events.jsonl");
        let f = OpenOptions::new().append(true).open(&p).unwrap();
        f.set_len(EVENTS_MAX_BYTES + 1).unwrap();
        append_event(&json!({"b": 2})).unwrap();
        assert!(home().join("events.jsonl.1").exists());
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"b\":2}\n");
    }

    #[test]
    fn nudges() {
        let _iso = Isolated::new();
        let t = now() - 5.0;
        assert!(!nudged_since(t));
        nudge();
        assert!(nudged_since(t));
        clear_nudge();
        assert!(!nudged_since(t));
    }

    fn state(extra: Value) {
        let mut d = json!({"pid": std::process::id(), "last_poll": now_i()});
        d.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        write_json(&home().join("daemon.json"), &d).unwrap();
    }

    #[test]
    fn health_a_rate_limit_is_not_an_outage() {
        let _iso = Isolated::new();
        assert!(!daemon_healthy(300.0));
        state(json!({"last_poll_ok": now_i() - 30}));
        assert!(daemon_healthy(300.0));
        state(json!({"last_poll_ok": now_i() - 2000, "rate_limited_until": now_i() + 900}));
        assert!(daemon_healthy(300.0) && rate_limited_until(&daemon_state()).is_some());
        state(json!({"pid": 999999, "last_poll_ok": now_i() - 2000, "rate_limited_until": now_i() + 900}));
        assert!(!daemon_healthy(300.0));
        state(json!({"last_poll_ok": now_i() - 2000, "last_error": "HTTP 401"}));
        assert!(!daemon_healthy(300.0));
        state(json!({"last_poll_ok": now_i() - 2000, "degraded_since": now_i() - 600, "last_error_transient": true}));
        assert!(daemon_healthy(300.0) && degraded_since(&daemon_state()).is_some());
        state(json!({"last_poll_ok": now_i() - 2000, "polling_since": now_i() - 400}));
        assert!(daemon_healthy(300.0) && polling_since(&daemon_state()).is_some());
        state(json!({"last_poll_ok": now_i() - 2000, "polling_since": now_i() - 1000}));
        assert!(!daemon_healthy(300.0));
        state(json!({"last_poll_ok": now_i() - 2000, "last_poll_done": now_i() - 10}));
        assert!(daemon_healthy(300.0));
    }

    #[test]
    fn records_round_trip_and_tolerate_old_shapes() {
        let _iso = Isolated::new();
        let r = PrRef::new("acme", "api", 3);
        fs::write(pr_path(&r), r#"{"ref": "acme/api#3", "fetched_at": 12.0, "summary": {"state": "OPEN"}}"#).unwrap();
        let rec = read_pr(&r).unwrap();
        assert_eq!((rec.fetched_at, rec.summary.state.as_deref()), (12, Some("OPEN")));
        write_pr(&r, &PrRecord { pr: json!({"number": 3}), ..rec }).unwrap();
        assert!(read_pr(&r).unwrap().pr["number"] == 3);
        let listed = all_prs();
        assert!(listed.len() == 1 && listed[0].pr.is_null() && listed[0].summary.state.as_deref() == Some("OPEN"));
        let c = CommitRef::new("acme", "api", "abcdef1");
        write_commit(&c, &CommitRecord { r: c.to_string(), fetched_at: 5, missing: true, ..Default::default() })
            .unwrap();
        let back = read_commit(&c).unwrap();
        assert!(back.missing && back.summary.is_none());
    }
}
