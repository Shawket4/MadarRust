use crate::{auth::middleware::JwtMiddleware, stocktakes::handlers};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/stocktakes")
            .wrap(JwtMiddleware)
            .route(
                "/branches/{branch_id}",
                web::post().to(handlers::create_stocktake),
            )
            .route(
                "/branches/{branch_id}",
                web::get().to(handlers::list_stocktakes),
            )
            .route("/{id}", web::get().to(handlers::get_stocktake))
            .route("/{id}/items", web::put().to(handlers::upsert_items))
            .route(
                "/{id}/finalize",
                web::post().to(handlers::finalize_stocktake),
            )
            .route("/{id}/cancel", web::post().to(handlers::cancel_stocktake))
            .route(
                "/{id}/variance-report",
                web::get().to(handlers::variance_report),
            ),
    );
}
