use crate::{auth::middleware::JwtMiddleware, tills::handlers};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/tills")
            .wrap(JwtMiddleware)
            .route(
                "/branches/{branch_id}/current",
                web::get().to(handlers::get_current_till),
            )
            .route(
                "/branches/{branch_id}/open",
                web::post().to(handlers::open_till),
            )
            .route(
                "/branches/{branch_id}/open",
                web::get().to(handlers::list_open_tills),
            )
            .route(
                "/branches/{branch_id}/open-bills-notice",
                web::get().to(handlers::get_open_bills_notice),
            )
            .route("/branches/{branch_id}", web::get().to(handlers::list_tills))
            // Legacy entity list/CRUD (POS v0.5.1/v0.6.0 settings till picker).
            .route(
                "",
                web::get().to(crate::tills::legacy_routes::legacy_list_till_entities),
            )
            .route(
                "",
                web::post().to(crate::tills::legacy_routes::till_entity_gone),
            )
            .route(
                "/{till_id}/report",
                web::get().to(handlers::get_till_report),
            )
            .route(
                "/{till_id}/close-preview",
                web::get().to(handlers::close_preview),
            )
            .route(
                "/{till_id}/cash-movements",
                web::post().to(handlers::add_cash_movement),
            )
            .route(
                "/{till_id}/cash-movements",
                web::get().to(handlers::list_cash_movements),
            )
            .route(
                "/{till_id}/spot-checks",
                web::post().to(crate::tills::spot_checks::create_spot_check),
            )
            .route(
                "/{till_id}/spot-checks",
                web::get().to(crate::tills::spot_checks::list_spot_checks),
            )
            .route("/{till_id}/close", web::post().to(handlers::close_till))
            .route(
                "/{till_id}/force-close",
                web::post().to(handlers::force_close_till),
            )
            .route(
                "/{till_id}/refunds",
                web::get().to(crate::refunds::handlers::list_till_refunds),
            )
            .route("/{till_id}", web::get().to(handlers::get_till))
            .route(
                "/{till_id}",
                web::patch().to(crate::tills::legacy_routes::till_entity_gone),
            )
            .route(
                "/{till_id}",
                web::delete().to(crate::tills::legacy_routes::delete_till_or_entity),
            ),
    );
}
