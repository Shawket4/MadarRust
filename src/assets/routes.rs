//! HTTP surface of the asset store (§11.4). Mounted by `main.rs` with
//! `.configure(crate::assets::routes::configure)` — which MUST come before
//! `sync::routes::configure`, because the `/sync` scope would otherwise
//! swallow `/sync/assets` and `/sync/asset-bundles/*` (see [B4] amendment).
use actix_web::{guard, web};

use super::handlers;
use crate::auth::middleware::JwtMiddleware;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/assets/jobs/{id}")
            .wrap(JwtMiddleware)
            .route(web::get().to(handlers::get_job)),
    )
    .service(
        web::resource("/assets/{scope}/{file}")
            .route(web::get().to(handlers::serve_asset))
            .route(web::head().to(handlers::serve_asset)),
    )
    .service(
        web::resource("/sync/asset-bundles/{org_id}/{file_name}")
            .wrap(JwtMiddleware)
            .route(web::get().to(handlers::get_bundle))
            .route(web::head().to(handlers::get_bundle)),
    )
    .service(
        web::resource("/sync/assets")
            .wrap(JwtMiddleware)
            .route(web::post().to(handlers::top_up)),
    );
}

/// The GET/HEAD `/uploads/{tail}` fallback for old clients. Registered from
/// `uploads::routes::configure` (already mounted ahead of the static files
/// service) with a method guard so the upload POST routes still match.
pub fn legacy_uploads(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/uploads/{tail:.*}")
            .guard(guard::Any(guard::Get()).or(guard::Head()))
            .to(handlers::legacy_upload),
    );
}
