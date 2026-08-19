use actix_web::web;

use crate::{auth::middleware::JwtMiddleware, held_orders::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/held-orders")
            .wrap(JwtMiddleware)
            .route("", web::get().to(handlers::list_held_orders))
            .route("", web::post().to(handlers::park_held_order))
            .route("/{id}", web::patch().to(handlers::update_held_order))
            .route("/{id}/claim", web::post().to(handlers::claim_held_order))
            .route(
                "/{id}/release",
                web::post().to(handlers::release_held_order),
            )
            .route(
                "/{id}/discard",
                web::post().to(handlers::discard_held_order),
            )
            .route(
                "/{id}/complete",
                web::post().to(handlers::complete_held_order),
            )
            .route(
                "/{id}/table",
                web::post().to(handlers::assign_held_order_table),
            ),
    )
    // The cross-entity floor operations. A sibling `/floor` scope also lives in
    // reservations::routes (sections/tables authoring) — actix tries scopes in
    // registration order and falls through when no inner route matches, so the
    // two coexist.
    .service(
        web::scope("/floor")
            .wrap(JwtMiddleware)
            .route("/tables/swap", web::post().to(handlers::swap_tables))
            .route(
                "/tables/{id}/state",
                web::patch().to(handlers::update_table_state),
            )
            .route("/transfers", web::get().to(handlers::list_floor_transfers))
            .route(
                "/transfers",
                web::post().to(handlers::create_floor_transfer),
            )
            .route(
                "/transfers/{id}/cancel",
                web::post().to(handlers::cancel_transfer),
            )
            .route(
                "/transfers/{id}/fulfill",
                web::post().to(handlers::fulfill_transfer),
            ),
    );
}
