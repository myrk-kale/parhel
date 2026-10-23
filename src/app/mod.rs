//! Application layer: orchestrates domain logic over the ports.

pub mod add;
pub mod doctor;
pub mod rollback;
pub mod setup;
pub mod store;

#[cfg(test)]
pub(crate) mod fakes;

pub use add::{AddOutcome, Env, Strategy, add};
pub use doctor::{DoctorReport, Finding, doctor};
pub use setup::{SetupOutcome, StoreSummary, setup};
