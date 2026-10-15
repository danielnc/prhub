//! prhub: one GitHub poller per machine for many coding agents. The daemon reads GitHub and writes plain JSON files;
//! every other command reads those files and never calls GitHub.

pub mod cli;
pub mod commits;
pub mod conditions;
pub mod daemon;
pub mod github;
pub mod hook;
pub mod refs;
pub mod store;
pub mod util;

#[cfg(test)]
pub(crate) mod testutil;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
