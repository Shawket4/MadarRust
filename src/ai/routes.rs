use actix_web::web;

use crate::auth::middleware::JwtMiddleware;

use super::handlers;

/// `/ai/*` — merchant analytics chat. Behind `JwtMiddleware`; the handler
/// further requires an org-scoped account and `reports:read`.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/ai")
            .wrap(JwtMiddleware)
            .route("/chat", web::post().to(handlers::chat)),
    );
}
