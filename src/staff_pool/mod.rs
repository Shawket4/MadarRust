//! The staff drinks pool — a branch's daily allowance of drinks for its own
//! people (owner design, 2026-09-19).
//!
//! * [`engine`] is the arithmetic, shared with the till through madar-shared
//!   (`madar_money::staff_pool`).
//! * [`settings`] is the org-wide allowance and eligible-item list, overridden
//!   per branch, scoped exactly as `loyalty_settings` is.

pub mod comp;
pub mod engine;
pub mod order_line;
pub mod record;
pub mod routes;
pub mod settings;

