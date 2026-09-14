use actix_web::web;

use crate::{auth::middleware::JwtMiddleware, devices::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/devices")
            .wrap(JwtMiddleware)
            .route("/register", web::post().to(handlers::register_device))
            .route("", web::get().to(handlers::list_devices))
            .route(
                "/client-versions",
                web::get().to(crate::client_seen::handlers::list_client_versions),
            )
            .route("/{id}", web::patch().to(handlers::update_device)),
    );
}
