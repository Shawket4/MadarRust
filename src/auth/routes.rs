use actix_web::web;

use crate::auth::{handlers, middleware::JwtMiddleware};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/auth")
            // Public — no JWT required
            .route("/login", web::post().to(handlers::login))
            // Protected — JWT required
            .service(
                web::scope("")
                    .wrap(JwtMiddleware)
                    .route("/me", web::get().to(handlers::me))
                    .route("/permissions", web::get().to(handlers::permissions)),
            ),
    );
}