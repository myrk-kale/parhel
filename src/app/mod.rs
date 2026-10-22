//! Application layer: orchestrates domain logic over the ports.

pub mod add;
pub mod doctor;
pub mod rollback;

#[cfg(test)]
pub(crate) mod fakes;

pub use add::{AddOutcome, Env, Strategy, add};
pub use doctor::{DoctorReport, Finding, doctor};
