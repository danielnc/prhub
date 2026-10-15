//! Claude Code PreToolUse hook: send agents to `prhub wait` instead of their own GitHub polling loops.

use crate::refs::re;
use regex::Regex;
use std::sync::OnceLock;

// A command word starts the line or follows ; & | ( { or a shell keyword, so quoted mentions (a grep pattern, a heredoc
// of notes) do not count. An `rtk` / `rtk proxy` wrapper is seen through.
const CMD: &str = r"(?:^|[;&|({]|\b(?:then|do|while|until|if|xargs|time)\b|!)\s*(?:rtk\s+(?:proxy\s+)?)?";

static LOOP: OnceLock<Regex> = OnceLock::new();
static GH_READ: OnceLock<Regex> = OnceLock::new();
static GH_WATCH: OnceLock<Regex> = OnceLock::new();

// A loop only polls if it sleeps: `sleep` must be a command, so `time.sleep(…)` in a python -c, a `for` inside a script
// string or a file called watch.json do not count.
fn loop_re() -> &'static Regex {
    let p = format!(r"(?m)(?:^|[;&|({{]|\bdo\b)\s*(while|until|for)\b[\s\S]*{CMD}sleep\b|{CMD}watch\s+-n\b");
    re(&LOOP, &p)
}

fn gh_read_re() -> &'static Regex {
    let p = format!(r"(?m){CMD}gh\s+(pr\s+(checks|view|status|list)|run\s+(view|list)|api)\b");
    re(&GH_READ, &p)
}

fn gh_watch_re() -> &'static Regex {
    let p =
        format!(r"(?m){CMD}(gh\s+(pr\s+checks\b[^\n|;&]*--watch|run\s+watch)\b|watch\b[^\n|;&]*\bgh\s+(pr|run|api)\b)");
    re(&GH_WATCH, &p)
}

pub const REASON: &str = "Don't poll GitHub in a loop: every agent shares one rate limit. prhub already watches PRs \
for you, at no API cost per agent:\n  prhub wait <pr> --until done|green|checks|merged|review|change [--timeout 3600]\n  \
prhub wait <owner/repo@sha> --until checks|green|deployed [--env production] [--workflow NAME]\n  prhub state <pr>      \
(current state, instant)\n  prhub list --ready    (your PRs ready to merge)\n<pr> can be a URL, owner/repo#N, a nickname \
like 'backend #2814', or a number in this repo. Run it in the background (or ScheduleWakeup) if you need to keep \
working. One-off reads like `gh pr view` without a loop are fine.";

/// A reason to refuse `command`, or None to let it run.
pub fn verdict(command: &str) -> Option<&'static str> {
    if command.contains("PRHUB_ALLOW=1") {
        return None;
    }
    if gh_watch_re().is_match(command) || (loop_re().is_match(command) && gh_read_re().is_match(command)) {
        return Some(REASON);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_polling_loops() {
        for cmd in [
            "gh pr checks 12 --watch",
            "gh run watch 123",
            "while true; do gh pr view 12 --json state; sleep 60; done",
            "until gh pr checks 12; do sleep 30; done",
            "for i in $(seq 20); do gh api repos/a/b/pulls/1 >/dev/null; sleep 90; done",
            "rtk gh pr checks 12 --watch",
            "while :; do rtk proxy gh pr view 12; sleep 60; done",
            "watch -n 30 gh pr checks 12",
        ] {
            assert!(verdict(cmd).is_some(), "{cmd}");
        }
    }

    #[test]
    fn allows_one_off_reads_and_unrelated_loops() {
        for cmd in [
            "gh pr view 12 --json state",
            "gh pr create --fill",
            "gh api repos/a/b/pulls -X POST -f base=main",
            "PRHUB_ALLOW=1 while true; do gh pr view 1; sleep 5; done",
            "for f in *.ts; do echo $f; done",
            "grep -rn 'gh pr checks --watch' docs/",
            "for o in a b; do gh api search/issues -f q=org:$o; done; cat ~/.local/state/prhub/watch.json",
            "python3 -c \"\nfor e in rows: print(e)\" ; gh api graphql -f query='{viewer{login}}'",
        ] {
            assert!(verdict(cmd).is_none(), "{cmd}");
        }
    }

    #[test]
    fn the_reason_reads_like_the_original() {
        assert!(
            REASON.starts_with("Don't poll GitHub in a loop") && REASON.contains("\n  prhub state <pr>      (current")
        );
        assert!(REASON.ends_with("without a loop are fine."));
    }
}
