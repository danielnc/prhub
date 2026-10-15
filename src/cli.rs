//! prhub CLI. Everything except `daemon` reads local files only and never calls GitHub.

use crate::commits::{self, CommitRef};
use crate::conditions::{evaluate, is_done, one_line, CONDITIONS};
use crate::daemon::Daemon;
use crate::hook;
use crate::refs::{index_clones, PrRef, RefResolver};
use crate::store::{self, Config, PrRecord};
use crate::util::{local_hm, py, take, truthy, which, Clock, RealClock};
use clap::{Parser, Subcommand};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

pub const EXIT_MET: i32 = 0;
pub const EXIT_TIMEOUT: i32 = 1;
pub const EXIT_IMPOSSIBLE: i32 = 3;
pub const EXIT_NO_DAEMON: i32 = 4;

const TARGET: &str =
    "a PR (URL, owner/repo#N, 'nick #N', N) or a commit (owner/repo@sha, 'nick@sha', @sha, commit URL)";
const NOT_POLLING: &str = "prhub daemon is not polling (see `prhub status`); start it with `prhub install-launchd` (macOS) or `prhub install-systemd` (Linux)";

#[derive(Parser, Debug)]
#[command(name = "prhub", version, about = "One GitHub poller for many coding agents.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// current state of a PR or commit (local, instant)
    State {
        #[arg(help = TARGET)]
        pr: String,
        #[arg(long)]
        json: bool,
        /// ask the daemon for a poll first (<= 90 s)
        #[arg(long)]
        fresh: bool,
        /// commits: only runs/statuses whose name contains this
        #[arg(long)]
        workflow: Option<String>,
    },
    /// block until a PR or commit reaches a condition
    Wait {
        #[arg(help = TARGET)]
        pr: String,
        /// PR: done, green, checks, merged, closed, review, change; commit: checks, green, deployed, change (comma = any)
        #[arg(long)]
        until: String,
        /// commits, deployed: environment name as the repo names it, or 'any' (production also matches prod/prd,
        /// staging stage/stg, development dev)
        #[arg(long, default_value = "production")]
        env: String,
        /// commits, checks/green: only runs/statuses whose name contains this
        #[arg(long)]
        workflow: Option<String>,
        /// seconds (default 3600)
        #[arg(long, default_value_t = 3600)]
        timeout: i64,
        /// seconds between local reads (default 3)
        #[arg(long, default_value_t = 3.0)]
        poll: f64,
    },
    /// track PRs the discovery search does not find
    Watch {
        #[arg(required = true)]
        pr: Vec<String>,
    },
    /// stop tracking PRs
    Unwatch {
        #[arg(required = true)]
        pr: Vec<String>,
    },
    /// tracked PRs
    List {
        /// only PRs that meet `done`
        #[arg(long)]
        ready: bool,
        /// include merged/closed
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// daemon health and GitHub budget
    Status,
    /// run the poller (usually under launchd or systemd)
    Daemon {
        /// poll once and exit
        #[arg(long)]
        once: bool,
    },
    /// agent hook that refuses GitHub polling loops
    Hook {
        #[arg(value_parser = ["claude"])]
        agent: String,
    },
    /// run the daemon at login (macOS)
    InstallLaunchd,
    /// run the daemon as a systemd user service (Linux)
    InstallSystemd,
}

/// Where a command writes, what time it is, and what it reads from stdin: real ones in `main`, fakes in tests.
pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub clock: &'a mut dyn Clock,
    pub stdin: &'a mut dyn Read,
}

macro_rules! say {
    ($w:expr, $($arg:tt)*) => {{ let _ = writeln!($w, $($arg)*); }};
}

/// Who is waiting, recorded with each watch: PRHUB_CALLER, else a terminal-multiplexer pane id, else "cli".
pub fn caller() -> String {
    for var in ["PRHUB_CALLER", "HERDR_PANE_ID", "TMUX_PANE"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    "cli".into()
}

fn resolver(cfg: &Config) -> RefResolver {
    let cache = store::home().join("repos.json");
    let fresh = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|age| age.as_secs() <= 6 * 3600);
    let repos: BTreeMap<String, String> =
        store::read_json(&cache).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    let repos = if repos.is_empty() || !fresh {
        let r = index_clones(&cfg.clone_roots, 3);
        let _ = store::write_json(&cache, &r);
        r
    } else {
        repos
    };
    RefResolver::new(&cfg.aliases, &repos)
}

fn interval_max_age() -> f64 {
    let interval = store::daemon_state().get("interval").and_then(Value::as_f64).filter(|v| *v != 0.0).unwrap_or(60.0);
    300f64.max(3.0 * interval)
}

/// The commit's record, asking the daemon for a poll (<= 90 s) when there is none yet or `fresh`.
fn commit_record(r: &CommitRef, fresh: bool, io: &mut Io) -> Option<store::CommitRecord> {
    let _ = store::add_commit_watch(r, &caller(), store::STATE_KEEP);
    let mut rec = store::read_commit(r);
    if rec.is_none() || fresh {
        store::nudge();
        let start = io.clock.now();
        while io.clock.now() < start + 90.0 {
            rec = store::read_commit(r);
            if rec.as_ref().is_some_and(|x| x.fetched_at as f64 >= start - 1.0) {
                break;
            }
            io.clock.sleep(2.0);
        }
    }
    rec
}

fn state_commit(
    text: &str,
    json_out: bool,
    fresh: bool,
    workflow: Option<&str>,
    cfg: &Config,
    io: &mut Io,
) -> Result<i32, String> {
    let r = commits::resolve(text, &resolver(cfg))?;
    let Some(rec) = commit_record(&r, fresh, io) else {
        say!(io.err, "{r}: no data yet (is `prhub daemon` running? see `prhub status`)");
        return Ok(EXIT_NO_DAEMON);
    };
    let s = match (&rec.summary, rec.missing) {
        (Some(s), false) => s.clone(),
        _ => {
            say!(io.err, "{r}: GitHub has no such commit (typo, or not pushed yet)");
            return Ok(2);
        }
    };
    if json_out {
        let (state, failed) = commits::rollup(&s, workflow);
        let mut m = Map::new();
        m.insert("ref".into(), json!(rec.r));
        m.insert("fetched_at".into(), json!(rec.fetched_at));
        m.insert("checks".into(), json!(state));
        m.insert("failing".into(), json!(failed));
        if let Ok(Value::Object(sm)) = serde_json::to_value(&s) {
            m.extend(sm);
        }
        say!(io.out, "{}", Value::Object(m));
    } else {
        let age = (io.clock.now() - rec.fetched_at as f64) as i64;
        say!(io.out, "{}  ({age}s old)", commits::one_line(&rec.r, &s, workflow));
    }
    Ok(0)
}

struct WaitArgs<'a> {
    until: &'a str,
    env: &'a str,
    workflow: Option<&'a str>,
    timeout: i64,
    poll: f64,
}

fn conds_of(until: &str) -> Vec<String> {
    until.split(',').map(|c| c.trim().to_string()).collect()
}

fn wait_commit(text: &str, a: &WaitArgs, cfg: &Config, io: &mut Io) -> Result<i32, String> {
    let r = commits::resolve(text, &resolver(cfg))?;
    let conds = conds_of(a.until);
    if let Some(c) = conds.iter().find(|c| !commits::CONDITIONS.contains(&c.as_str())) {
        say!(io.err, "unknown condition '{c}' for a commit (use: {})", commits::CONDITIONS.join(", "));
        return Ok(2);
    }
    if !store::daemon_healthy(300.0) {
        say!(io.err, "{NOT_POLLING}");
        return Ok(EXIT_NO_DAEMON);
    }
    let _ = store::add_commit_watch(&r, &caller(), store::WAIT_KEEP);
    store::nudge();
    let code = wait_commit_loop(&r, &conds, a, io);
    let _ = store::release_commit_watch(&r);
    Ok(code)
}

fn wait_commit_loop(r: &CommitRef, conds: &[String], a: &WaitArgs, io: &mut Io) -> i32 {
    let start = io.clock.now();
    let mut baseline: Option<commits::CommitSummary> = None;
    let mut last_line: Option<String> = None;
    let mut hint: Option<String> = None;
    let mut next_touch = start + store::WAIT_RENEW as f64;
    while io.clock.now() < start + a.timeout as f64 {
        if let Some(rec) = store::read_commit(r).filter(|x| x.fetched_at as f64 >= start - 1.0) {
            if rec.missing {
                say!(io.err, "{r}: GitHub has no such commit (typo, or not pushed yet)");
                return 2;
            }
            if let Some(s) = rec.summary {
                let base = baseline.get_or_insert_with(|| s.clone()).clone();
                if conds.iter().any(|c| c == "deployed") && hint.is_none() {
                    let h = commits::env_hint(&s, a.env).unwrap_or_default();
                    if !h.is_empty() {
                        say!(io.err, "note: {h}");
                    }
                    hint = Some(h);
                }
                let line = commits::one_line(&rec.r, &s, a.workflow);
                for c in conds {
                    let (met, dead) = commits::evaluate(c, &s, Some(&base), a.env, a.workflow).unwrap_or((false, None));
                    if met {
                        say!(io.out, "{c}: {line}");
                        return EXIT_MET;
                    }
                    if let Some(why) = dead {
                        say!(io.out, "cannot reach {c} ({why}): {line}");
                        return EXIT_IMPOSSIBLE;
                    }
                }
                last_line = Some(line);
            }
        }
        if io.clock.now() >= next_touch {
            // a running wait keeps its commit watch from expiring
            let _ = store::add_commit_watch(r, &caller(), store::WAIT_KEEP);
            next_touch = io.clock.now() + store::WAIT_RENEW as f64;
        }
        if !store::daemon_healthy(interval_max_age()) {
            say!(io.err, "prhub daemon stopped polling while waiting");
            return EXIT_NO_DAEMON;
        }
        io.clock.sleep(a.poll.min((next_touch - io.clock.now()).max(0.0))); // renew in time
    }
    let hint = hint.filter(|h| !h.is_empty()).map(|h| format!("  ({h})")).unwrap_or_default();
    say!(io.out, "timeout: {}{hint}", last_line.unwrap_or_else(|| format!("{r}: no fresh data")));
    EXIT_TIMEOUT
}

fn pr_record(r: &PrRef) -> Option<PrRecord> {
    store::read_pr(r)
}

fn state(
    text: &str,
    json_out: bool,
    fresh: bool,
    workflow: Option<&str>,
    cfg: &Config,
    io: &mut Io,
) -> Result<i32, String> {
    if commits::is_commit_text(text) {
        return state_commit(text, json_out, fresh, workflow, cfg, io);
    }
    let r = resolver(cfg).resolve(text)?;
    let _ = store::add_watch(&r, "cli");
    let mut rec = pr_record(&r);
    if rec.is_none() || fresh {
        store::nudge();
        let start = io.clock.now();
        while io.clock.now() < start + 90.0 {
            rec = pr_record(&r);
            if rec.as_ref().is_some_and(|x| x.fetched_at as f64 >= start - 1.0) {
                break;
            }
            io.clock.sleep(2.0);
        }
    }
    let Some(rec) = rec else {
        say!(io.err, "{r}: no data yet (is `prhub daemon` running? see `prhub status`)");
        return Ok(EXIT_NO_DAEMON);
    };
    if json_out {
        let mut m = Map::new();
        m.insert("ref".into(), json!(rec.r));
        m.insert("fetched_at".into(), json!(rec.fetched_at));
        if let Ok(Value::Object(sm)) = serde_json::to_value(&rec.summary) {
            m.extend(sm);
        }
        m.insert("failing".into(), json!(rec.failing));
        say!(io.out, "{}", Value::Object(m));
    } else {
        let age = (io.clock.now() - rec.fetched_at as f64) as i64;
        say!(io.out, "{}  ({age}s old)", one_line(&rec.r, &rec.summary, &rec.failing));
    }
    Ok(0)
}

fn wait(text: &str, a: &WaitArgs, cfg: &Config, io: &mut Io) -> Result<i32, String> {
    if commits::is_commit_text(text) {
        return wait_commit(text, a, cfg, io);
    }
    let r = resolver(cfg).resolve(text)?;
    let conds = conds_of(a.until);
    if let Some(c) = conds.iter().find(|c| !CONDITIONS.contains(&c.as_str())) {
        say!(io.err, "unknown condition '{c}' (use: {})", CONDITIONS.join(", "));
        return Ok(2);
    }
    if !store::daemon_healthy(300.0) {
        say!(io.err, "{NOT_POLLING}");
        return Ok(EXIT_NO_DAEMON);
    }
    let _ = store::add_watch(&r, &caller());
    store::nudge();
    let start = io.clock.now();
    let deadline = start + a.timeout as f64;
    let mut baseline = None;
    let mut last_line = None;
    while io.clock.now() < deadline {
        if let Some(rec) = pr_record(&r).filter(|x| x.fetched_at as f64 >= start - 1.0) {
            // only judge data fetched after we started waiting
            let base = baseline.get_or_insert_with(|| rec.summary.clone()).clone();
            let line = one_line(&rec.r, &rec.summary, &rec.failing);
            for c in &conds {
                let (met, impossible) = evaluate(c, &rec.summary, Some(&base), &cfg.merge_states)?;
                if met {
                    say!(io.out, "{c}: {line}");
                    return Ok(EXIT_MET);
                }
                if impossible {
                    say!(io.out, "cannot reach {c}: {line}");
                    return Ok(EXIT_IMPOSSIBLE);
                }
            }
            last_line = Some(line);
        }
        if !store::daemon_healthy(interval_max_age()) {
            say!(io.err, "prhub daemon stopped polling while waiting");
            return Ok(EXIT_NO_DAEMON);
        }
        io.clock.sleep(a.poll);
    }
    say!(io.out, "timeout: {}", last_line.unwrap_or_else(|| format!("{r}: no fresh data")));
    Ok(EXIT_TIMEOUT)
}

fn watch(prs: &[String], cfg: &Config, io: &mut Io) -> Result<i32, String> {
    let res = resolver(cfg);
    for p in prs {
        let r = res.resolve(p)?;
        store::add_watch(&r, "cli").map_err(|e| e.to_string())?;
        say!(io.out, "watching {r}");
    }
    store::nudge();
    Ok(0)
}

fn unwatch(prs: &[String], cfg: &Config) -> Result<i32, String> {
    let res = resolver(cfg);
    let keys = prs.iter().map(|p| res.resolve(p).map(|r| r.to_string())).collect::<Result<Vec<_>, _>>()?;
    store::with_watches(|w| {
        for k in &keys {
            w.remove(k);
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(0)
}

fn list(ready: bool, all: bool, json_out: bool, cfg: &Config, io: &mut Io) -> Result<i32, String> {
    let rows: Vec<PrRecord> = store::all_prs()
        .into_iter()
        .filter(|r| !ready || is_done(&r.summary, &cfg.merge_states))
        .filter(|r| all || r.summary.state.as_deref() == Some("OPEN"))
        .collect();
    if json_out {
        let items: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut m = Map::new();
                m.insert("ref".into(), json!(r.r));
                if let Ok(Value::Object(sm)) = serde_json::to_value(&r.summary) {
                    m.extend(sm);
                }
                m.insert("failing".into(), json!(r.failing));
                Value::Object(m)
            })
            .collect();
        say!(io.out, "{}", Value::Array(items));
    } else {
        for r in &rows {
            say!(
                io.out,
                "{}  {}",
                one_line(&r.r, &r.summary, &r.failing),
                take(r.summary.title.as_deref().unwrap_or(""), 60)
            );
        }
    }
    Ok(0)
}

fn status(io: &mut Io) -> i32 {
    let d = store::daemon_state();
    let healthy = store::daemon_healthy(300.0);
    say!(io.out, "prhub {}  home {}", crate::VERSION, store::home().display());
    if d.is_empty() {
        say!(io.out, "daemon: never ran");
        return EXIT_NO_DAEMON;
    }
    let now = io.clock.now();
    let age = (now - d.get("last_poll").and_then(Value::as_f64).unwrap_or(0.0)) as i64;
    let rate = d.get("rate").cloned().unwrap_or(Value::Null);
    let pid = py(d.get("pid"));
    if let Some(until) = store::rate_limited_until(&d) {
        say!(
            io.out,
            "daemon: PAUSED by the GitHub rate limit until {} (pid {pid}, alive). `prhub wait` keeps waiting and resumes by itself. Do not poll GitHub by hand meanwhile: your calls hit the same exhausted limit.",
            local_hm(until)
        );
    }
    if let Some(since) = store::degraded_since(&d) {
        say!(
            io.out,
            "daemon: DEGRADED since {}: GitHub keeps failing its requests (5xx or timeouts), and prhub keeps retrying (pid {pid}, alive). `prhub wait` keeps waiting on the last good state. Do not poll GitHub by hand meanwhile: it is failing for you too.",
            local_hm(since)
        );
    }
    let polling = store::polling_since(&d)
        .map(|since| format!("polling now ({}s)  ", (now - since as f64) as i64))
        .unwrap_or_default();
    let took = d
        .get("poll_seconds")
        .filter(|v| truthy(v))
        .map(|v| format!("last poll took {}s  ", py(Some(v))))
        .unwrap_or_default();
    let budget = |k: &str| rate.get(k).map(|v| py(Some(v))).unwrap_or_else(|| "?".into());
    say!(
        io.out,
        "daemon: {} pid {pid}  last poll {age}s ago  interval {}s  tracked {}  {polling}{took}github budget {} left (reset {})",
        if healthy { "healthy" } else { "NOT polling" },
        py(d.get("interval")),
        py(d.get("tracked")),
        budget("remaining"),
        budget("resetAt")
    );
    if let Some(e) = d.get("last_error").filter(|v| truthy(v)) {
        say!(io.out, "last error: {}", py(Some(e)));
    }
    if let Some(n) = d.get("skipped").filter(|v| truthy(v)) {
        say!(
            io.out,
            "last poll skipped {} PRs/commits GitHub could not return; their previous state is kept",
            py(Some(n))
        );
    }
    say!(
        io.out,
        "watches: {}  PR files: {}  commits watched: {}",
        store::watches().len(),
        store::all_prs().len(),
        store::commit_watches().len()
    );
    if healthy {
        0
    } else {
        EXIT_NO_DAEMON
    }
}

fn hook_cmd(io: &mut Io) -> i32 {
    let mut text = String::new();
    if io.stdin.read_to_string(&mut text).is_err() {
        return 0;
    }
    let Ok(data) = serde_json::from_str::<Value>(&text) else { return 0 };
    if data.get("tool_name").and_then(Value::as_str) != Some("Bash") || !store::daemon_healthy(300.0) {
        return 0; // fail open if prhub isn't running
    }
    let command = data.get("tool_input").and_then(|t| t.get("command")).and_then(Value::as_str).unwrap_or("");
    if let Some(reason) = hook::verdict(command) {
        say!(
            io.out,
            "{}",
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}})
        );
    }
    0
}

const PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.prhub.daemon</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path}</string></dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#;

const UNIT: &str = "[Unit]
Description=prhub: one GitHub poller for many coding agents
After=network-online.target

[Service]
ExecStart={exe} daemon
Restart=always
RestartSec=30
Environment=PATH={path}
Nice=10

[Install]
WantedBy=default.target
";

/// PATH for the service: wherever `gh` and prhub live, then the usual system directories.
pub fn service_path(exe: &str) -> String {
    let parent = |p: &str| PathBuf::from(p).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let gh = which("gh").map(|p| parent(&p.to_string_lossy())).unwrap_or_default();
    let mut dirs: Vec<String> = Vec::new();
    for d in [gh, parent(exe), "/opt/homebrew/bin".into(), "/usr/local/bin".into(), "/usr/bin".into(), "/bin".into()] {
        if !d.is_empty() && !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs.join(":")
}

fn exe() -> String {
    which("prhub")
        .or_else(|| std::env::current_exe().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "prhub".into())
}

fn render(template: &str, exe: &str, path: &str, log: &str) -> String {
    template.replace("{exe}", exe).replace("{path}", path).replace("{log}", log)
}

fn install_launchd(io: &mut Io) -> i32 {
    let exe = exe();
    let plist = crate::util::expand_user("~/Library/LaunchAgents/dev.prhub.daemon.plist");
    let log = store::home().join("daemon.log").to_string_lossy().into_owned();
    if let Some(dir) = plist.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&plist, render(PLIST, &exe, &service_path(&exe), &log)) {
        say!(io.err, "cannot write {}: {e}", plist.display());
        return 1;
    }
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let _ = std::process::Command::new("launchctl").args(["bootout", &format!("gui/{uid}/dev.prhub.daemon")]).output();
    match std::process::Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])
        .output()
    {
        Ok(o) if o.status.success() => {
            say!(io.out, "installed {} (log {log})", plist.display());
            0
        }
        Ok(o) => {
            say!(io.out, "launchctl: {}", String::from_utf8_lossy(&o.stderr).trim());
            o.status.code().unwrap_or(1)
        }
        Err(e) => {
            say!(io.out, "launchctl: {e}");
            1
        }
    }
}

fn install_systemd(io: &mut Io) -> i32 {
    let exe = exe();
    let unit = crate::util::expand_user("~/.config/systemd/user/prhub.service");
    if let Some(dir) = unit.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&unit, render(UNIT, &exe, &service_path(&exe), "")) {
        say!(io.err, "cannot write {}: {e}", unit.display());
        return 1;
    }
    for args in [vec!["--user", "daemon-reload"], vec!["--user", "enable", "--now", "prhub.service"]] {
        match std::process::Command::new("systemctl").args(&args).output() {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                say!(io.err, "systemctl {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim());
                return o.status.code().unwrap_or(1);
            }
            Err(e) => {
                say!(io.err, "systemctl {}: {e}", args.join(" "));
                return 1;
            }
        }
    }
    say!(
        io.out,
        "installed {}; logs: journalctl --user -u prhub. To keep it running while you are logged out: loginctl enable-linger $USER",
        unit.display()
    );
    0
}

/// Run prhub with `args` (argv, program name first) and return the exit code.
pub fn run(args: Vec<String>, io: &mut Io) -> i32 {
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let text = e.render().to_string();
            if e.exit_code() == 0 {
                let _ = write!(io.out, "{text}");
            } else {
                let _ = write!(io.err, "{text}");
            }
            return e.exit_code();
        }
    };
    let cfg = match store::load_config() {
        Ok(c) => c,
        Err(e) => {
            say!(io.err, "prhub: {e}");
            return 2;
        }
    };
    let result = match cli.cmd {
        Cmd::State { pr, json, fresh, workflow } => state(&pr, json, fresh, workflow.as_deref(), &cfg, io),
        Cmd::Wait { pr, until, env, workflow, timeout, poll } => {
            let a = WaitArgs { until: &until, env: &env, workflow: workflow.as_deref(), timeout, poll };
            wait(&pr, &a, &cfg, io)
        }
        Cmd::Watch { pr } => watch(&pr, &cfg, io),
        Cmd::Unwatch { pr } => unwatch(&pr, &cfg),
        Cmd::List { ready, all, json } => list(ready, all, json, &cfg, io),
        Cmd::Status => Ok(status(io)),
        Cmd::Daemon { once } => {
            Daemon::new(cfg).run(once);
            Ok(0)
        }
        Cmd::Hook { .. } => Ok(hook_cmd(io)),
        Cmd::InstallLaunchd => Ok(install_launchd(io)),
        Cmd::InstallSystemd => Ok(install_systemd(io)),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            say!(io.err, "prhub: {e}");
            2
        }
    }
}

pub fn main(args: Vec<String>) -> i32 {
    let (mut out, mut err, mut stdin) = (std::io::stdout(), std::io::stderr(), std::io::stdin());
    let mut io = Io { out: &mut out, err: &mut err, clock: &mut RealClock, stdin: &mut stdin };
    run(args, &mut io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commits::tests::{node_at, suite, Suite, OID};
    use crate::conditions::summary;
    use crate::conditions::tests::{pr, Pr};
    use crate::github::Failing;
    use crate::testutil::{healthy_daemon, Isolated};
    use crate::util::now;

    /// Sleeping advances the clock instead of waiting.
    struct FakeClock(f64);

    impl Clock for FakeClock {
        fn now(&self) -> f64 {
            self.0
        }
        fn sleep(&mut self, seconds: f64) {
            self.0 += seconds;
        }
    }

    struct Ran {
        code: i32,
        out: String,
        err: String,
    }

    fn prhub(args: &[&str]) -> Ran {
        prhub_in(args, "")
    }

    fn prhub_in(args: &[&str], stdin: &str) -> Ran {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut clock = FakeClock(now());
        let mut input = stdin.as_bytes();
        let mut io = Io { out: &mut out, err: &mut err, clock: &mut clock, stdin: &mut input };
        let mut argv = vec!["prhub".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let code = run(argv, &mut io);
        Ran { code, out: String::from_utf8(out).unwrap(), err: String::from_utf8(err).unwrap() }
    }

    fn api() -> PrRef {
        PrRef::new("acme", "api", 1)
    }

    fn put_pr(p: Pr, failing: Vec<Failing>) {
        let r = api();
        let rec = PrRecord {
            r: r.to_string(),
            fetched_at: crate::util::now_i(),
            summary: summary(&pr(p)),
            failing,
            ..Default::default()
        };
        store::write_pr(&r, &rec).unwrap();
    }

    fn put_commit(suites: &[Suite], deployments: &[crate::commits::tests::Dep]) {
        let c = CommitRef::new("acme", "api", "abcdef1");
        let s = commits::summarize(&node_at(OID, suites, &[], deployments, "2026-10-11T08:00:00Z")).unwrap();
        let rec = store::CommitRecord {
            r: c.to_string(),
            fetched_at: crate::util::now_i(),
            summary: Some(s),
            ..Default::default()
        };
        store::write_commit(&c, &rec).unwrap();
    }

    #[test]
    fn version_help_and_usage_errors() {
        let _iso = Isolated::new();
        let v = prhub(&["--version"]);
        assert_eq!((v.code, v.out.trim()), (0, format!("prhub {}", crate::VERSION).as_str()));
        let h = prhub(&["wait", "--help"]);
        assert!(h.code == 0 && h.out.contains("--until") && h.out.contains("--env"));
        assert_eq!(prhub(&["wait", "acme/api#1"]).code, 2); // --until is required
        assert_eq!(prhub(&["hook", "codex"]).code, 2);
        assert_eq!(prhub(&["bogus"]).code, 2);
    }

    #[test]
    fn state_of_a_pr() {
        let _iso = Isolated::new();
        put_pr(
            Pr { checks: Some("FAILURE"), ..Default::default() },
            vec![Failing { name: "test".into(), ..Default::default() }],
        );
        let r = prhub(&["state", "acme/api#1"]);
        assert_eq!(r.code, 0);
        assert!(r.out.starts_with("acme/api#1 OPEN checks=FAILURE") && r.out.contains("failing: test"), "{}", r.out);
        assert!(store::watches().contains_key("acme/api#1"));
        let j: Value = serde_json::from_str(&prhub(&["state", "acme/api#1", "--json"]).out).unwrap();
        assert!(j["ref"] == "acme/api#1" && j["checks"] == "FAILURE" && j["failing"][0]["name"] == "test");
        let none = prhub(&["state", "acme/api#2"]);
        assert!(none.code == EXIT_NO_DAEMON && none.err.contains("no data yet"));
        let bad = prhub(&["state", "not a pr"]);
        assert!(bad.code == 2 && bad.err.starts_with("prhub: cannot read"));
    }

    #[test]
    fn state_of_a_commit() {
        let _iso = Isolated::new();
        put_commit(&[suite("CI")], &[]);
        let r = prhub(&["state", "acme/api@abcdef1"]);
        assert!(r.code == 0 && r.out.contains("checks=SUCCESS") && r.out.contains("s old)"), "{}", r.out);
        let j: Value =
            serde_json::from_str(&prhub(&["state", "acme/api@abcdef1", "--json", "--workflow", "ci"]).out).unwrap();
        assert!(j["checks"] == "SUCCESS" && j["oid"].as_str().unwrap().starts_with("aaaa"));
        assert!(store::commit_watches().contains_key("acme/api@abcdef1"));
        let c = CommitRef::new("acme", "api", "abcdef2");
        store::write_commit(
            &c,
            &store::CommitRecord {
                r: c.to_string(),
                fetched_at: crate::util::now_i(),
                missing: true,
                ..Default::default()
            },
        )
        .unwrap();
        let m = prhub(&["state", "acme/api@abcdef2"]);
        assert!(m.code == 2 && m.err.contains("no such commit"));
        let n = prhub(&["state", "acme/api@abcdef3"]);
        assert!(n.code == EXIT_NO_DAEMON && n.err.contains("no data yet"));
    }

    #[test]
    fn wait_on_a_pr() {
        let _iso = Isolated::new();
        assert_eq!(prhub(&["wait", "acme/api#1", "--until", "done"]).code, EXIT_NO_DAEMON);
        healthy_daemon(json!({}));
        put_pr(Pr::default(), vec![]);
        let r = prhub(&["wait", "acme/api#1", "--until", "done"]);
        assert!(r.code == EXIT_MET && r.out.starts_with("done: acme/api#1 OPEN"), "{}", r.out);
        assert_eq!(prhub(&["wait", "acme/api#1", "--until", "green,merged"]).code, EXIT_MET);
        put_pr(Pr { checks: Some("PENDING"), ..Default::default() }, vec![]);
        let t = prhub(&["wait", "acme/api#1", "--until", "green", "--timeout", "30"]);
        assert!(t.code == EXIT_TIMEOUT && t.out.starts_with("timeout: acme/api#1"));
        put_pr(Pr { state: "CLOSED", ..Default::default() }, vec![]);
        let i = prhub(&["wait", "acme/api#1", "--until", "merged"]);
        assert!(i.code == EXIT_IMPOSSIBLE && i.out.starts_with("cannot reach merged"));
        assert_eq!(prhub(&["wait", "acme/api#1", "--until", "nope"]).code, 2);
        assert!(store::watches()["acme/api#1"].by.is_some());
    }

    #[test]
    fn wait_refuses_to_start_without_a_polling_daemon() {
        let _iso = Isolated::new();
        let old = crate::util::now_i() - 5000;
        healthy_daemon(json!({"last_poll_ok": old, "last_poll_done": old}));
        put_pr(Pr { checks: Some("PENDING"), ..Default::default() }, vec![]);
        let r = prhub(&["wait", "acme/api#1", "--until", "green"]);
        assert!(r.code == EXIT_NO_DAEMON && r.err.contains("not polling"));
        let c = prhub(&["wait", "acme/api@abcdef1", "--until", "green"]);
        assert!(c.code == EXIT_NO_DAEMON && c.err.contains("not polling"));
    }

    #[test]
    fn wait_on_a_commit() {
        let _iso = Isolated::new();
        healthy_daemon(json!({}));
        put_commit(&[suite("CI")], &[(OID, "production", "2026-10-11T09:00:00Z", "SUCCESS", None)]);
        assert_eq!(prhub(&["wait", "acme/api@abcdef1", "--until", "green"]).code, EXIT_MET);
        let d = prhub(&["wait", "acme/api@abcdef1", "--until", "deployed", "--env", "production"]);
        assert!(d.code == EXIT_MET && d.out.contains("production=live"));
        put_commit(&[suite("CI").st("COMPLETED", Some("FAILURE"))], &[]);
        let red = prhub(&["wait", "acme/api@abcdef1", "--until", "green"]);
        assert!(red.code == EXIT_IMPOSSIBLE && red.out.contains("failed: CI"));
        put_commit(&[suite("CI").st("IN_PROGRESS", None)], &[]);
        let t = prhub(&["wait", "acme/api@abcdef1", "--until", "checks", "--timeout", "4000"]);
        assert!(t.code == EXIT_TIMEOUT && t.out.contains("checks=PENDING"), "{} {}", t.out, t.err);
        assert_eq!(prhub(&["wait", "acme/api@abcdef1", "--until", "merged"]).code, 2);
        let c = CommitRef::new("acme", "api", "abcdef1");
        store::write_commit(
            &c,
            &store::CommitRecord {
                r: c.to_string(),
                fetched_at: crate::util::now_i(),
                missing: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(prhub(&["wait", "acme/api@abcdef1", "--until", "green"]).code, 2);
    }

    #[test]
    fn a_finished_wait_lets_its_commit_watch_lapse() {
        let _iso = Isolated::new();
        healthy_daemon(json!({}));
        put_commit(&[suite("CI")], &[]);
        assert_eq!(prhub(&["wait", "acme/api@abcdef1", "--until", "green"]).code, EXIT_MET);
        let e = &store::commit_watches()["acme/api@abcdef1"];
        assert!(e.keep_until.unwrap() <= crate::util::now_i() + store::RELEASE_KEEP + 1);
        put_commit(&[suite("CI").st("IN_PROGRESS", None)], &[]);
        assert_eq!(prhub(&["wait", "acme/api@abcdef1", "--until", "green", "--timeout", "4000"]).code, EXIT_TIMEOUT);
        assert!(
            store::commit_watches()["acme/api@abcdef1"].keep_until.unwrap()
                <= crate::util::now_i() + store::RELEASE_KEEP + 1
        );
    }

    #[test]
    fn wait_deployed_matches_a_short_environment_name_and_names_a_wrong_one() {
        let _iso = Isolated::new();
        healthy_daemon(json!({}));
        put_commit(&[suite("CI")], &[(OID, "prod", "2026-10-11T09:00:00Z", "SUCCESS", None)]);
        assert_eq!(prhub(&["wait", "acme/api@abcdef1", "--until", "deployed"]).code, EXIT_MET);
        let r = prhub(&["wait", "acme/api@abcdef1", "--until", "deployed", "--env", "qa", "--timeout", "4000"]);
        assert_eq!(r.code, EXIT_TIMEOUT);
        assert!(r.err.contains("environments seen: prod") && r.out.contains("environments seen: prod"));
    }

    #[test]
    fn list_and_watches() {
        let _iso = Isolated::new();
        put_pr(Pr::default(), vec![]);
        let l = prhub(&["list", "--ready"]);
        assert!(l.code == 0 && l.out.starts_with("acme/api#1 OPEN") && l.out.trim_end().ends_with("  t"));
        let j: Value = serde_json::from_str(&prhub(&["list", "--json"]).out).unwrap();
        assert_eq!(j[0]["ref"], "acme/api#1");
        put_pr(Pr { state: "MERGED", ..Default::default() }, vec![]);
        assert_eq!(prhub(&["list"]).out, "");
        assert!(prhub(&["list", "--all"]).out.contains("MERGED"));
        let w = prhub(&["watch", "acme/api#5", "acme/web#6"]);
        assert!(w.code == 0 && w.out.contains("watching acme/api#5"));
        assert_eq!(store::watches().len(), 2);
        assert_eq!(prhub(&["unwatch", "acme/api#5"]).code, 0);
        assert_eq!(store::watches().keys().cloned().collect::<Vec<_>>(), vec!["acme/web#6"]);
    }

    #[test]
    fn status_says_what_the_daemon_is_doing() {
        let _iso = Isolated::new();
        let never = prhub(&["status"]);
        assert!(never.code == EXIT_NO_DAEMON && never.out.contains("daemon: never ran"));
        healthy_daemon(
            json!({"rate": {"remaining": 4321, "resetAt": "2026-10-11T20:00:00Z"}, "tracked": 3, "poll_seconds": 12}),
        );
        let ok = prhub(&["status"]);
        assert_eq!(ok.code, 0);
        assert!(
            ok.out.contains("daemon: healthy")
                && ok.out.contains("github budget 4321 left")
                && ok.out.contains("last poll took 12s"),
            "{}",
            ok.out
        );
        let now = crate::util::now_i();
        healthy_daemon(
            json!({"last_poll_ok": now - 5000, "last_poll_done": now - 5000, "rate_limited_until": now + 900}),
        );
        let paused = prhub(&["status"]);
        assert!(paused.code == 0 && paused.out.contains("PAUSED by the GitHub rate limit"));
        healthy_daemon(json!({"last_poll_ok": now - 5000, "last_poll_done": now - 5000, "degraded_since": now - 60,
                              "last_error_transient": true, "last_error": "HTTP 504: gateway timeout", "skipped": 2}));
        let deg = prhub(&["status"]);
        assert!(
            deg.out.contains("DEGRADED since")
                && deg.out.contains("last error: HTTP 504")
                && deg.out.contains("skipped 2")
        );
        healthy_daemon(json!({"last_poll_ok": now - 5000, "last_poll_done": now - 5000, "polling_since": now - 30}));
        assert!(prhub(&["status"]).out.contains("polling now ("));
        healthy_daemon(json!({"last_poll_ok": now - 5000, "last_poll_done": now - 5000, "last_error": "HTTP 401"}));
        let down = prhub(&["status"]);
        assert!(down.code == EXIT_NO_DAEMON && down.out.contains("NOT polling"));
    }

    #[test]
    fn the_hook_refuses_loops_only_while_the_daemon_runs() {
        let _iso = Isolated::new();
        let lp = r#"{"tool_name": "Bash", "tool_input": {"command": "while true; do gh pr view 1; sleep 60; done"}}"#;
        assert_eq!(prhub_in(&["hook", "claude"], lp).out, ""); // fail open: no daemon
        healthy_daemon(json!({}));
        let r = prhub_in(&["hook", "claude"], lp);
        let v: Value = serde_json::from_str(&r.out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().contains("prhub wait"));
        assert_eq!(
            prhub_in(&["hook", "claude"], r#"{"tool_name": "Bash", "tool_input": {"command": "gh pr view 1"}}"#).out,
            ""
        );
        assert_eq!(prhub_in(&["hook", "claude"], r#"{"tool_name": "Edit"}"#).out, "");
        assert_eq!(prhub_in(&["hook", "claude"], "not json").code, 0);
    }

    #[test]
    fn service_files() {
        let p = service_path("/opt/tools/bin/prhub");
        assert!(p.contains("/opt/tools/bin") && p.contains("/usr/bin") && p.ends_with(":/bin"));
        let unit = render(UNIT, "/x/prhub", "/x:/bin", "");
        assert!(unit.contains("ExecStart=/x/prhub daemon") && unit.contains("Environment=PATH=/x:/bin"));
        let plist = render(PLIST, "/x/prhub", "/bin", "/tmp/d.log");
        assert!(
            plist.contains("<string>/x/prhub</string><string>daemon</string>")
                && plist.contains("<string>/tmp/d.log</string>")
        );
        assert!(!caller().is_empty());
    }

    #[test]
    fn a_broken_config_is_a_clear_error() {
        let iso = Isolated::new();
        std::fs::write(iso.dir.path().join("config.toml"), "interval = [").unwrap();
        let r = prhub(&["status"]);
        assert!(r.code == 2 && r.err.contains("bad config"));
    }

    #[test]
    fn nicknames_come_from_config() {
        let iso = Isolated::new();
        std::fs::write(iso.dir.path().join("config.toml"), "[aliases]\nbackend = \"acme/api\"\n").unwrap();
        put_pr(Pr::default(), vec![]);
        let r = prhub(&["state", "backend #1"]);
        assert!(r.code == 0 && r.out.starts_with("acme/api#1"), "{} {}", r.out, r.err);
    }
}
