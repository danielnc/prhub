//! Parse the many ways people and agents name a pull request into one canonical `owner/repo#N`.

use crate::util::{expand_user, run_timeout};
use regex::Regex;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

pub(crate) fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("a valid built-in regex"))
}

static URL_RE: OnceLock<Regex> = OnceLock::new();
static FULL_RE: OnceLock<Regex> = OnceLock::new();
static NICK_RE: OnceLock<Regex> = OnceLock::new();
static NUM_RE: OnceLock<Regex> = OnceLock::new();
static REMOTE_RE: OnceLock<Regex> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl fmt::Display for PrRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

impl PrRef {
    pub fn new(owner: &str, repo: &str, number: u64) -> Self {
        PrRef { owner: owner.into(), repo: repo.into(), number }
    }

    /// Filesystem-safe name for this PR's state file.
    pub fn slug(&self) -> String {
        format!("{}__{}__{}", self.owner, self.repo, self.number)
    }

    pub fn url(&self) -> String {
        format!("https://github.com/{}/{}/pull/{}", self.owner, self.repo, self.number)
    }
}

/// owner, repo of the git checkout at `path` (its `origin` remote), or None.
pub fn remote_of(path: &str) -> Option<(String, String)> {
    let (_, url) = run_timeout("git", &["-C", path, "remote", "get-url", "origin"], Duration::from_secs(5)).ok()?;
    let rx = re(&REMOTE_RE, r"github\.com[:/]([\w.-]+)/([\w.-]+?)(?:\.git)?/?$");
    let m = rx.captures(url.trim())?;
    Some((m[1].to_string(), m[2].to_string()))
}

/// repo name (lowercased) -> 'owner/repo', from the origin remotes of git clones under `roots`, up to `depth` levels
/// down. Shallower clones win; hidden folders are skipped.
pub fn index_clones(roots: &[String], depth: usize) -> BTreeMap<String, String> {
    let mut idx = BTreeMap::new();
    for root in roots {
        let mut level = vec![expand_user(root)];
        for _ in 0..depth {
            let mut next = Vec::new();
            for dir in &level {
                let Ok(rd) = std::fs::read_dir(dir) else { continue };
                let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
                entries.sort();
                for p in entries {
                    let hidden = p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with('.')).unwrap_or(true);
                    if hidden || !p.is_dir() {
                        continue;
                    }
                    if p.join(".git").exists() {
                        if let Some((owner, repo)) = remote_of(&p.to_string_lossy()) {
                            idx.entry(repo.to_lowercase()).or_insert(format!("{owner}/{repo}"));
                        }
                    }
                    next.push(p);
                }
            }
            level = next;
        }
    }
    idx
}

/// Turns user text into a PrRef. `aliases` maps nicknames ("backend") to repo names or owner/repo; `repos` maps repo
/// names to owner/repo (usually from `index_clones`).
#[derive(Clone)]
pub struct RefResolver {
    pub aliases: BTreeMap<String, String>,
    pub repos: BTreeMap<String, String>,
    pub cwd: Option<String>,
    /// How to read a checkout's origin remote (replaceable so fuzzing never shells out to git).
    pub remote: fn(&str) -> Option<(String, String)>,
}

impl Default for RefResolver {
    fn default() -> Self {
        RefResolver { aliases: BTreeMap::new(), repos: BTreeMap::new(), cwd: None, remote: remote_of }
    }
}

impl RefResolver {
    pub fn new(aliases: &BTreeMap<String, String>, repos: &BTreeMap<String, String>) -> Self {
        RefResolver {
            aliases: aliases.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect(),
            repos: repos.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect(),
            ..Default::default()
        }
    }

    pub fn repo_for(&self, name: &str) -> Option<String> {
        let name = name.trim().to_lowercase();
        let target = self.aliases.get(&name).cloned().unwrap_or(name);
        if target.contains('/') {
            return Some(target);
        }
        self.repos.get(&target.to_lowercase()).cloned()
    }

    /// The origin remote of the current checkout (or `cwd`).
    pub fn here(&self) -> Option<(String, String)> {
        let cwd =
            self.cwd.clone().or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))?;
        (self.remote)(&cwd)
    }

    pub fn resolve(&self, text: &str) -> Result<PrRef, String> {
        let t = text.trim();
        let num = |s: &str| {
            s.parse::<u64>().map_err(|_| format!("cannot read '{t}' as a pull request (try a URL or owner/repo#N)"))
        };
        if let Some(m) = re(&URL_RE, r"github\.com/([\w.-]+)/([\w.-]+)/pull/(\d+)").captures(t) {
            return Ok(PrRef::new(&m[1], &m[2], num(&m[3])?));
        }
        if let Some(m) = re(&FULL_RE, r"^([\w.-]+)/([\w.-]+)#(\d+)$").captures(t) {
            return Ok(PrRef::new(&m[1], &m[2], num(&m[3])?));
        }
        if let Some(m) = re(&NUM_RE, r"^#?(\d+)$").captures(t) {
            let n = num(&m[1])?;
            let (owner, repo) = self
                .here()
                .ok_or_else(|| format!("'{t}' is a bare number but the current directory is not a GitHub checkout"))?;
            return Ok(PrRef::new(&owner, &repo, n));
        }
        if let Some(m) = re(&NICK_RE, r"^([\w .-]+?)\s*#\s*(\d+)$").captures(t) {
            return match self.repo_for(&m[1]) {
                Some(repo) => {
                    let (owner, name) = repo.split_once('/').unwrap_or((repo.as_str(), ""));
                    Ok(PrRef::new(owner, name, num(&m[2])?))
                }
                None => Err(format!("unknown repo nickname '{}': add it to [aliases] in the prhub config", &m[1])),
            };
        }
        Err(format!("cannot read '{t}' as a pull request (try a URL or owner/repo#N)"))
    }
}

/// Parse a canonical `owner/repo#N` (what the state files store) without consulting git or the config.
pub fn parse(text: &str) -> Option<PrRef> {
    RefResolver { remote: |_| None, ..Default::default() }.resolve(text).ok()
}

/// Whether a git checkout lives at `path` (a `.git` folder or file).
pub fn is_checkout(path: &Path) -> bool {
    path.join(".git").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> RefResolver {
        let aliases =
            BTreeMap::from([("backend".to_string(), "api".to_string()), ("web".into(), "acme/web-app".into())]);
        let repos = BTreeMap::from([("api".to_string(), "acme/api".to_string())]);
        RefResolver { remote: |_| Some(("acme".into(), "api".into())), ..RefResolver::new(&aliases, &repos) }
    }

    #[test]
    fn resolves_every_way_of_naming_a_pr() {
        let r = resolver();
        for (text, want) in [
            ("https://github.com/acme/api/pull/42", PrRef::new("acme", "api", 42)),
            ("https://github.com/acme/api/pull/42/files", PrRef::new("acme", "api", 42)),
            ("acme/api#7", PrRef::new("acme", "api", 7)),
            ("backend #2814", PrRef::new("acme", "api", 2814)),
            ("API#9", PrRef::new("acme", "api", 9)),
            ("web#3", PrRef::new("acme", "web-app", 3)),
            ("12", PrRef::new("acme", "api", 12)),
            ("#13", PrRef::new("acme", "api", 13)),
        ] {
            assert_eq!(r.resolve(text).unwrap(), want, "{text}");
        }
    }

    #[test]
    fn unreadable_names_are_errors() {
        assert!(RefResolver::default().resolve("mystery #1").unwrap_err().contains("unknown repo nickname"));
        assert!(resolver().resolve("not a pr").unwrap_err().contains("cannot read"));
        assert!(resolver().resolve("acme/api#99999999999999999999999").is_err());
        let nowhere = RefResolver { remote: |_| None, ..Default::default() };
        assert!(nowhere.resolve("12").unwrap_err().contains("bare number"));
        assert_eq!(parse("acme/api#5"), Some(PrRef::new("acme", "api", 5)));
        assert_eq!(parse("5"), None);
    }

    #[test]
    fn slug_and_url() {
        let r = PrRef::new("acme", "api", 42);
        assert_eq!(r.to_string(), "acme/api#42");
        assert_eq!(r.slug(), "acme__api__42");
        assert!(r.url().ends_with("/acme/api/pull/42"));
    }

    #[test]
    fn clones_become_nicknames() {
        let root = crate::testutil::TempDir::new();
        let clone = |rel: &str, url: &str| {
            let p = root.path().join(rel);
            std::fs::create_dir_all(&p).unwrap();
            let run = |args: &[&str]| std::process::Command::new("git").args(args).output().unwrap();
            run(&["init", "-q", p.to_str().unwrap()]);
            run(&["-C", p.to_str().unwrap(), "remote", "add", "origin", url]);
            p
        };
        let a = clone("work/api", "git@github.com:acme/api.git");
        clone("work/deep/web-app", "https://github.com/acme/web-app");
        clone(".hidden/secret", "https://github.com/acme/secret.git");
        assert_eq!(remote_of(a.to_str().unwrap()), Some(("acme".into(), "api".into())));
        assert!(is_checkout(&a));
        let idx = index_clones(&[root.path().to_string_lossy().into_owned()], 3);
        assert_eq!(idx.get("api").map(String::as_str), Some("acme/api"));
        assert_eq!(idx.get("web-app").map(String::as_str), Some("acme/web-app"));
        assert!(!idx.contains_key("secret"));
        assert_eq!(remote_of(root.path().to_str().unwrap()), None);
    }
}
