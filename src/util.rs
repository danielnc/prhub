//! Small helpers the standard library does not have: clocks, local time, ISO dates, child processes with a timeout.

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Seconds since the epoch.
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn now_i() -> i64 {
    now() as i64
}

/// The current time and sleeping, so the CLI's wait loops can be tested without waiting.
pub trait Clock {
    fn now(&self) -> f64;
    fn sleep(&mut self, seconds: f64);
}

pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> f64 {
        now()
    }
    fn sleep(&mut self, seconds: f64) {
        if seconds > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(seconds));
        }
    }
}

fn local_tm(t: i64) -> libc::tm {
    // SAFETY: localtime_r only writes the struct we hand it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let tt = t as libc::time_t;
        libc::localtime_r(&tt, &mut tm);
        tm
    }
}

/// `YYYY-MM-DD HH:MM:SS` in local time.
pub fn local_datetime(t: i64) -> String {
    let tm = local_tm(t);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// `HH:MM` in local time.
pub fn local_hm(t: i64) -> String {
    let tm = local_tm(t);
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Epoch seconds of an ISO 8601 time like GitHub's `2026-10-15T20:09:42Z` (also `+hh:mm` offsets and fractions).
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !(b[10] == b'T' || b[10] == b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };
    let (y, mo, d, h, mi, se) = (num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        let digits = r.chars().take_while(|c| c.is_ascii_digit()).count();
        rest = &r[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hhmm = rest.get(1..)?.replace(':', "");
            if hhmm.len() != 4 || !hhmm.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let oh: i64 = hhmm.get(..2)?.parse().ok()?;
            let om: i64 = hhmm.get(2..)?.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se - offset)
}

pub enum RunError {
    Timeout,
    Spawn(String),
}

/// Run a command, killing it after `timeout`. Its exit code and stdout, whatever the code.
pub fn run_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Result<(i32, String), RunError> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| RunError::Spawn(e.to_string()))?;
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(so) = stdout.as_mut() {
            let _ = so.read_to_string(&mut out);
        }
        out
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().unwrap_or_default();
                return Ok((status.code().unwrap_or(-1), out));
            }
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(RunError::Spawn(e.to_string())),
        }
    }
}

/// Whether a process with this pid exists (one we may not signal still counts).
pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `~/x` -> `$HOME/x`.
pub fn expand_user(p: &str) -> PathBuf {
    if (p == "~" || p.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        let mut out = PathBuf::from(home);
        if p.len() > 2 {
            out.push(&p[2..]);
        }
        return out;
    }
    PathBuf::from(p)
}

/// The first executable called `name` on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false))
}

/// A JSON number as an integer, whether it was written as 12 or 12.0; anything else is 0.
pub fn de_i64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).unwrap_or(0))
}

/// Like `de_i64`, but null or a non-number is None.
pub fn de_opt_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

/// A JSON value as a number (ints and floats), for the loosely typed daemon.json.
pub fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64())
}

/// How Python printed a value in the original prhub: None for null or missing, strings bare.
pub fn py(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(other) => other.to_string(),
    }
}

/// The first `n` characters.
pub fn take(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Walk a JSON value by keys; arrays are indexed by numeric keys. Null when any step is missing.
pub fn dig<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    let mut cur = v;
    for k in path {
        cur = match cur {
            Value::Object(m) => m.get(*k).unwrap_or(&Value::Null),
            Value::Array(a) => k.parse::<usize>().ok().and_then(|i| a.get(i)).unwrap_or(&Value::Null),
            _ => &Value::Null,
        };
    }
    cur
}

/// A JSON string field, None when missing, null or not a string.
pub fn s(v: &Value, path: &[&str]) -> Option<String> {
    dig(v, path).as_str().map(String::from)
}

/// JSON truthiness as Python saw it: null, false, 0, "", [] and {} are false.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_times() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-10-15T20:09:42Z"), Some(1792094982));
        assert_eq!(parse_iso("2026-10-15T20:09:42.123+02:00"), Some(1792094982 - 7200));
        assert_eq!(parse_iso("garbage"), None);
        assert_eq!(parse_iso("2026-13-15T20:09:42Z"), None);
        assert_eq!(parse_iso("2026-10-15T20:09:42X"), None);
        assert_eq!(parse_iso("2026-10-15T20:09:42+1é2"), None); // a non-ASCII offset is rejected, not a panic
        assert_eq!(parse_iso("2026-10-15T20:09:42+"), None);
        assert_eq!(parse_iso("2026-10-1é20:09:42Z"), None);
    }

    #[test]
    fn processes_and_paths() {
        assert!(pid_alive(std::process::id() as i64));
        assert!(!pid_alive(0) && !pid_alive(-5) && !pid_alive(i64::MAX));
        assert!(matches!(run_timeout("sh", &["-c", "echo hi"], Duration::from_secs(5)), Ok((0, ref o)) if o == "hi\n"));
        assert!(matches!(run_timeout("sh", &["-c", "sleep 5"], Duration::from_millis(100)), Err(RunError::Timeout)));
        assert!(matches!(run_timeout("/no/such/binary", &[], Duration::from_secs(1)), Err(RunError::Spawn(_))));
        assert!(which("sh").is_some() && which("no-such-binary-anywhere").is_none());
        assert_eq!(expand_user("/abs").to_str(), Some("/abs"));
    }

    #[test]
    fn json_helpers() {
        let v: Value = serde_json::json!({"a": {"b": [{"c": "x"}]}, "n": 0, "e": ""});
        assert_eq!(s(&v, &["a", "b", "0", "c"]), Some("x".into()));
        assert!(dig(&v, &["a", "zz", "c"]).is_null());
        assert!(!truthy(&v["n"]) && !truthy(&v["e"]) && truthy(&v["a"]) && !truthy(&Value::Null));
        assert_eq!(py(None), "None");
        assert_eq!(py(Some(&serde_json::json!(true))), "True");
        assert_eq!(py(Some(&serde_json::json!(60))), "60");
        assert_eq!(take("héllo", 2), "hé");
        assert_eq!(local_hm(0).len(), 5);
        assert_eq!(local_datetime(0).len(), 19);
        assert!(now() > 1.7e9 && now_i() > 1_700_000_000);
    }
}
