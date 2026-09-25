use actix_web::web;

use crate::{auth::middleware::JwtMiddleware, combos::handlers, deals};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/combos")
            .wrap(JwtMiddleware)
            .route("", web::get().to(handlers::list_combos))
            .route("", web::post().to(handlers::create_combo))
            .route("/economics", web::post().to(handlers::combo_economics))
            .route("/{id}", web::get().to(handlers::get_combo))
            .route("/{id}", web::put().to(handlers::update_combo)),
    )
    .service(
        web::scope("/settings/combos")
            .wrap(JwtMiddleware)
            .route("", web::get().to(handlers::get_settings))
            .route("", web::put().to(handlers::put_settings))
            .route(
                "/branches/{branch_id}",
                web::put().to(handlers::put_branch_channels),
            )
            .route(
                "/branches/{branch_id}",
                web::delete().to(handlers::delete_branch_channels),
            ),
    )
    .service(
        web::scope("/deals")
            .wrap(JwtMiddleware)
            .route("", web::get().to(deals::handlers::list_deals))
            .route("", web::post().to(deals::handlers::create_deal))
            .route("/{id}", web::put().to(deals::handlers::update_deal))
            .route("/{id}", web::delete().to(deals::handlers::delete_deal))
            .route(
                "/{id}/branches/{branch_id}",
                web::put().to(deals::handlers::put_deal_branch),
            )
            .route(
                "/{id}/branches/{branch_id}",
                web::delete().to(deals::handlers::delete_deal_branch),
            ),
    );
}
