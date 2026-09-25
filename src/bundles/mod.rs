//! Combos / bundles were REMOVED (2026-09-25, owner: "rip it out completely";
//! a new module will be designed from scratch). What is left here is one
//! compatibility stub and nothing else.
//!
//! OLD TILLS: every POS release in the field (v0.5.0 through v0.8.x) fetches
//! `GET /bundles?status=active&per_page=500` as a REQUIRED stream of its
//! catalog refresh (`refresh_catalog_inner`): a 404 there aborts the whole
//! refresh, and the till stops picking up menu changes. So the route answers
//! an empty page, in the exact `PaginatedBundles` shape those builds decode.
//! It reads nothing and is deliberately NOT in the OpenAPI document, so no
//! regenerated client grows a bundles API again.
//!
//! Delete this once no till older than the combos removal is in the field.
use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::json;

use crate::auth::middleware::JwtMiddleware;

#[derive(Deserialize)]
struct PageQuery {
    page: Option<i64>,
    per_page: Option<i64>,
}

async fn empty_bundles_page(query: web::Query<PageQuery>) -> HttpResponse {
    HttpResponse::Ok().json(json!({
        "data": [],
        "total": 0,
        "page": query.page.unwrap_or(1).max(1),
        "per_page": query.per_page.unwrap_or(20).clamp(1, 500),
        "total_pages": 0,
    }))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/bundles")
            .wrap(JwtMiddleware)
            .route("", web::get().to(empty_bundles_page)),
    );
}
