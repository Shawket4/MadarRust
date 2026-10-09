//! Combos / bundles were REMOVED (2026-09-25, owner: "rip it out completely";
//! a new module will be designed from scratch). What is left here is one
//! compatibility stub and nothing else.
//!
//! OLD TILLS: every POS release in the field (v0.5.0 through v0.8.x) fetches
//! `GET /bundles?status=active&per_page=500` as a REQUIRED stream of its
//! catalog refresh (`refresh_catalog_inner`): a 404 there aborts the whole
//! refresh, and the till stops picking up menu changes. So the route answers
//! an empty page, in the exact `PaginatedBundles` shape those builds decode,
//! behind the same `menu_items:read` guard it always had. It reads no data and
//! is deliberately NOT in the OpenAPI document, so no regenerated client grows
//! a bundles API again.
//!
//! Delete this once no till older than the combos removal is in the field.
use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::Deserialize;
use serde_json::json;

use crate::auth::jwt::Claims;
use crate::auth::middleware::JwtMiddleware;
use crate::errors::AppError;
use crate::permissions::checker::check_permission;

#[derive(Deserialize)]
struct PageQuery {
    page: Option<i64>,
    per_page: Option<i64>,
}

async fn empty_bundles_page(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<PageQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    Ok(HttpResponse::Ok().json(json!({
        "data": [],
        "total": 0,
        "page": query.page.unwrap_or(1).max(1),
        "per_page": query.per_page.unwrap_or(20).clamp(1, 500),
        "total_pages": 0,
    })))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/bundles")
            .wrap(JwtMiddleware)
            .route("", web::get().to(empty_bundles_page)),
    );
}
