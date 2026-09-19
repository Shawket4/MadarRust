//! The staff drinks pool — a branch's daily allowance of drinks for its own
//! people (owner design, 2026-09-19).
//!
//! * [`engine`] is the arithmetic, shared with the till by
//!   `tests/fixtures/staff_pool_vectors.json`.
//! * [`settings`] is the org-wide allowance and eligible-item list, overridden
//!   per branch, scoped exactly as `loyalty_settings` is.

pub mod engine;
pub mod record;
pub mod routes;
pub mod settings;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod vectors_tests;
