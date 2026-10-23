//! Pure domain logic: no process spawning, no filesystem access.

pub mod clone_plan;
pub mod error;
pub mod exclusions;
pub mod mounts;
pub mod paths;
pub mod relocate;
pub mod request;
pub mod store;

pub use error::{Error, ErrorCode, Result};
pub use request::{AddRequest, BranchMode, DoctorRequest, SetupRequest, SubmodulePolicy};
