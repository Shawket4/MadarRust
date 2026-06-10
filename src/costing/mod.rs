//! Canonical cost engine.
//!
//! One resolution path for "what does X cost?", used by order creation
//! (snapshot at sale time), menu/addon cost endpoints, reports, and the
//! Menu Advisor adapter fallback.
//!
//! Conventions:
//! * Ingredient costs are stored in EGP as `numeric(15,2)`
//!   (`org_ingredients.cost_per_unit`, `ingredient_cost_history.cost_per_unit`).
//! * Everything this module RETURNS is integer **piastres** (`i64`), matching
//!   the price columns. Conversion is `round(egp * 100)`.
//! * `None` means *unknown*, never zero. `Some(0)` means genuinely free.

pub mod handlers;
pub mod routes;
pub mod service;

#[cfg(test)]
mod tests;

pub use service::*;
