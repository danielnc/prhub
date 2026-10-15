//! Test helpers: throwaway folders, and a per-test prhub home and config so tests can run in parallel.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("prhub-test-{}-{n}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fresh prhub home and an empty config for the current test thread; both go away with the guard.
pub struct Isolated {
    pub dir: TempDir,
}

impl Isolated {
    pub fn new() -> Self {
        let dir = TempDir::new();
        crate::store::set_overrides(Some(dir.path().join("home")), Some(dir.path().join("config.toml")));
        Isolated { dir }
    }
}

impl Drop for Isolated {
    fn drop(&mut self) {
        crate::store::set_overrides(None, None);
    }
}

/// Mark the daemon as having just finished a good poll (what most CLI tests need).
pub fn healthy_daemon(extra: serde_json::Value) {
    let now = crate::util::now_i();
    let mut d = serde_json::json!({"pid": std::process::id(), "last_poll": now, "last_poll_ok": now,
                                    "last_poll_done": now, "interval": 60});
    if let (Some(m), serde_json::Value::Object(e)) = (d.as_object_mut(), extra) {
        m.extend(e);
    }
    crate::store::write_json(&crate::store::home().join("daemon.json"), &d).unwrap();
}
