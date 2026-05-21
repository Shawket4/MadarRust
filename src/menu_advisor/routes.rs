//! Route registration for the Menu Advisor module.

use actix_web::web;
use super::handlers::get_report;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/menu-advisor")
            .route("/report", web::get().to(get_report))
    );
}
