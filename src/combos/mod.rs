//! Combos (COMBOS_CONTRACT.md, owner answers §11): a combo is a menu item of
//! kind `combo` with slots, choices and windows; its price is its `one_size`
//! row. Deals live in [`crate::deals`]; the Bundles report in
//! [`crate::reports::bundles`].

pub mod handlers;
pub mod routes;
pub mod types;

use crate::errors::AppError;

/// The A1 placeholder answer of a route whose handler lands in A2: the route,
/// its guard and its schemas are final.
pub fn not_yet() -> AppError {
    AppError::Coded {
        status: 501,
        code: "NOT_IMPLEMENTED",
        reason: "This endpoint is registered but not implemented yet.".into(),
    }
}
