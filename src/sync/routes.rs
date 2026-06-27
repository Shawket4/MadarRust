use actix_web::web;

use crate::auth::middleware::JwtMiddleware;

/// `/sync/*` — authenticated device-replay routes. Behind the same JWT
/// middleware as every other write route; `replay` does its own org/teller
/// attribution checks on top.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/sync")
            .wrap(JwtMiddleware)
            .route("/replay", web::post().to(super::handlers::replay)),
    );
}
