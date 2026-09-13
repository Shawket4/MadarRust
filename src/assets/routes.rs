//! HTTP surface of the asset store (§11.4). Mounted by `main.rs` with
//! `.configure(crate::assets::routes::configure)`.
use actix_web::web;

pub fn configure(_cfg: &mut web::ServiceConfig) {}
