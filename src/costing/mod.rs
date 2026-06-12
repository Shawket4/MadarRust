//! Canonical cost engine.
//!
//! One resolution path for "what does X cost?", used by order creation
//! (snapshot at sale time), menu/addon cost endpoints, reports, and the
//! Menu Advisor adapter fallback.
//!
//! Conventions:
//! * Ingredient costs are stored in PIASTRES as `numeric(15,2)`
//!   (`org_ingredients.cost_per_unit`, `ingredient_cost_history.cost_per_unit`);
//!   fractional piastres are allowed for precise per-gram costs. The dashboard
//!   converts EGP input on entry — the backend never multiplies by 100.
//! * Everything this module RETURNS is integer **piastres** (`i64`), matching
//!   the price columns: rollups sum and round, nothing more.
//! * `None` means *unknown*, never zero. `Some(0)` means genuinely free.

pub mod backfill;
pub mod handlers;
pub mod routes;
pub mod service;

#[cfg(test)]
mod tests;

pub use service::*;
