use actix_web::web;

use crate::{auth::middleware::JwtMiddleware, devices::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Device-authenticated (its own credential), so outside the JWT scope and
    // registered before it.
    cfg.service(
        web::resource("/devices/me/authz-snapshot")
            .route(web::get().to(crate::authz::snapshot::device_snapshot)),
    );
    cfg.service(
        web::scope("/devices")
            .wrap(JwtMiddleware)
            .route("/register", web::post().to(handlers::register_device))
            .route(
                "/activation-codes",
                web::post().to(crate::devices::activation::create_code),
            )
            .route(
                "/activation-codes",
                web::get().to(crate::devices::activation::list_codes),
            )
            .route(
                "/activation-codes/{id}/revoke",
                web::post().to(crate::devices::activation::revoke_code),
            )
            .route("", web::get().to(handlers::list_devices))
            .route(
                "/client-versions",
                web::get().to(crate::client_seen::handlers::list_client_versions),
            )
            .route("/{id}", web::patch().to(handlers::update_device)),
    );
}
