//! Sufrix backend library crate.
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
pub mod discounts;
pub mod errors;
pub mod inventory;
pub mod menu;
pub mod menu_advisor;
pub mod models;
pub mod openapi;
pub mod orders;
pub mod orgs;
pub mod payment_methods;
pub mod permissions;
pub mod recipes;
pub mod reports;
pub mod shifts;
pub mod translation;
pub mod uploads;
pub mod users;

#[cfg(test)]
pub mod e2e_tests;