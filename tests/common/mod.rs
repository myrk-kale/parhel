//! Shared integration-test harness. Each test binary uses a subset.
#![allow(dead_code, unused_imports, unreachable_pub)]

pub mod cli;
pub mod cow;
#[cfg(target_os = "linux")]
pub mod fiemap;
pub mod fixtures;
pub mod repo;
pub mod store;
pub mod volume;
pub mod xattr;

pub use cli::*;
pub use fixtures::*;
pub use repo::*;
