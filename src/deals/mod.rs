//! Deal rules (COMBOS_CONTRACT.md §5; C8, owner answers §11): mix & match
//! (`n_for_price`) and multi-buy (`buy_get`), evaluated over a cart by
//! `madar_catalog::deal`. On the till the POS suggests and the teller applies;
//! on QR and online checkout the server applies the best deals itself.

pub mod handlers;
pub mod load;
pub mod order;
pub mod public;
pub mod types;
