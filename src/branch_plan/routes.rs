use actix_web::web;

use super::handlers;
use crate::auth::middleware::JwtMiddleware;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/branch-plan")
            .wrap(JwtMiddleware)
            .route("", web::get().to(handlers::get_plan))
            .route("", web::put().to(handlers::save_plan))
            .route("/versions", web::get().to(handlers::list_versions)),
    );
}
