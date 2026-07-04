//! Madar backend library crate.
//!
//! All modules live here so they can be shared between the main server
//! binary (`src/main.rs`) and ancillary binaries like the OpenAPI
//! exporter (`src/bin/export_openapi.rs`). The previous `main.rs` owned
//! these module declarations directly; moving them to the library lets
//! `cargo run --bin export-openapi` reach `ApiDoc` without spinning up
//! the HTTP server.

pub mod auth;
pub mod branches;
pub mod bundles;
pub mod cache;
pub mod clock;
pub mod costing;
pub mod delivery;
pub mod demo;
pub mod discounts;
pub mod errors;
pub mod geo;
pub mod inventory;
pub mod kitchen;
pub mod menu;
pub mod menu_unification;
pub mod models;
pub mod openapi;
pub mod orders;
pub mod orgs;
pub mod payment_methods;
pub mod permissions;
pub mod purchasing;
pub mod qr_card;
pub mod rate_limit;
pub mod realtime;
pub mod recipes;
pub mod reports;
pub mod reservations;
pub mod shifts;
pub mod stocktakes;
pub mod sync;
pub mod tickets;
pub mod tills;
pub mod translation;
pub mod units;
pub mod uploads;
pub mod users;

#[cfg(test)]
pub mod e2e_tests;
