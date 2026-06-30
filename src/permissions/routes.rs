use crate::{auth::middleware::JwtMiddleware, permissions::handlers};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/permissions")
            .wrap(JwtMiddleware)
            // Role defaults
            .route("/roles", web::get().to(handlers::get_role_permissions))
            .route("/roles", web::put().to(handlers::upsert_role_permission))
            // User permission matrix (resolved)
            .route(
                "/matrix/{user_id}",
                web::get().to(handlers::get_permission_matrix),
            )
            // User overrides
            .route(
                "/user/{user_id}",
                web::get().to(handlers::get_user_permissions),
            )
            .route(
                "/user/{user_id}",
                web::put().to(handlers::upsert_user_permission),
            )
            .route(
                "/user/{user_id}/{resource}/{action}",
                web::delete().to(handlers::delete_user_permission),
            ),
    );
}
