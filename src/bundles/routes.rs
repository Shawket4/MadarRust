use crate::{auth::middleware::JwtMiddleware, bundles::handlers::*};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/bundles")
            .wrap(JwtMiddleware)
            .route("", web::get().to(list_bundles))
            .route("", web::post().to(create_bundle))
            .route("/available", web::get().to(available_bundles))
            .route("/{id}", web::get().to(get_bundle))
            .route("/{id}", web::patch().to(update_bundle))
            .route("/{id}", web::delete().to(delete_bundle))
            .route("/{id}/activate", web::post().to(activate_bundle))
            .route("/{id}/archive", web::post().to(archive_bundle))
            .route("/{id}/performance", web::get().to(bundle_performance)),
    );
}
