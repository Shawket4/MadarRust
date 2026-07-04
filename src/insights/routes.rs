use actix_web::web;

use crate::auth::middleware::JwtMiddleware;

use super::handlers;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/insights")
            .wrap(JwtMiddleware)
            .route(
                "/branches/{branch_id}/menu-margin",
                web::get().to(handlers::menu_margin_ledger),
            )
            .route(
                "/branches/{branch_id}/margin-watch",
                web::get().to(handlers::margin_watch),
            )
            .route(
                "/margin-target",
                web::get().to(handlers::get_margin_targets),
            )
            .route("/margin-target", web::put().to(handlers::put_margin_target))
            .route("/decisions", web::post().to(handlers::create_decision))
            .route("/decisions", web::get().to(handlers::list_decisions)),
    );
}
