//! Fuzz the code that reads text agents and users type: PR and commit names, and the hook's command matcher.
//!
//! Run by ClusterFuzzLite (see .clusterfuzzlite/). A crash is a panic, or an answer that breaks an invariant: a parsed
//! ref without an owner or repo, or a refusal that does not point the agent at `prhub wait`. The regex crate matches in
//! linear time, so a slow input cannot hang the hook.
#![no_main]

use libfuzzer_sys::fuzz_target;
use prhub::{commits, hook, refs::RefResolver};
use std::collections::BTreeMap;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let aliases = BTreeMap::from([("api".to_string(), "acme/api".to_string()), ("web".into(), "acme/web-app".into())]);
    let repos = BTreeMap::from([("api".to_string(), "acme/api".to_string())]);
    // never shell out to git while fuzzing: a bare number or "@sha" names something in "the current clone"
    let r = RefResolver { remote: |_| Some(("acme".into(), "api".into())), ..RefResolver::new(&aliases, &repos) };
    if let Ok(p) = r.resolve(text) {
        assert!(!p.owner.is_empty() && !p.repo.is_empty() && !p.to_string().is_empty());
    }
    if let Ok(c) = commits::resolve(text, &r) {
        assert!(!c.owner.is_empty() && !c.repo.is_empty() && c.sha.len() >= 7);
    }
    let _ = commits::is_commit_text(text);
    if let Some(reason) = hook::verdict(text) {
        assert!(reason.contains("prhub wait"));
    }
});
