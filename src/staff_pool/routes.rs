//! The staff pool's routes. Additive: nothing here existed before, so no
//! client that predates it can be affected by it.

use actix_web::web;

use crate::auth::middleware::JwtMiddleware;

use super::{record, settings};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/staff-pool")
            .wrap(JwtMiddleware)
            .route("/settings", web::get().to(settings::get_settings))
            .route("/settings", web::put().to(settings::put_settings))
            .route("/settings", web::delete().to(settings::delete_settings))
            .route("/today", web::get().to(record::get_today))
            .route("/drinks/summary", web::get().to(record::summary))
            .route("/drinks", web::get().to(record::list))
            .route("/drinks", web::post().to(record::record)),
    );
}
