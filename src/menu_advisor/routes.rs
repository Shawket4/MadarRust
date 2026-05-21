//! Route registration for the Menu Advisor module.

use actix_web::web;
use super::handlers::get_report;
use crate::auth::middleware::JwtMiddleware;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/menu-advisor")
            .wrap(JwtMiddleware)
            .route("/report", web::get().to(get_report))
    );
}
