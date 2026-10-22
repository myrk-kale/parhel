//! Adapter to the `git` executable.

pub mod cli;
pub mod parse;
pub mod runner;

pub use cli::GitCli;
pub use runner::GitRunner;
