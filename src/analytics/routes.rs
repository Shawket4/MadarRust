//! `/metrics/*` route wiring.

use actix_web::web;
use sqlx::PgPool;

use crate::auth::middleware::JwtMiddleware;

use super::handlers;

/// `read_pool` overrides the app-wide write pool for this scope only. Every
/// handler here extracts `web::Data<PgPool>` via [`crate::db::Db`], and actix
/// resolves scope-level `app_data` ahead of app-level, so the whole metrics
/// surface runs against the read replica when `READ_DATABASE_URL` is set — the
/// same trick `/reports` uses. Metrics are read-only by construction.
pub fn configure(cfg: &mut web::ServiceConfig, read_pool: web::Data<PgPool>) {
    cfg.service(
        web::scope("/metrics")
            .app_data(read_pool)
            .wrap(JwtMiddleware)
            .route("/schema", web::get().to(handlers::schema))
            .route("/query", web::post().to(handlers::query)),
    );
}
