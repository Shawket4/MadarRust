//! The public cart quote (owner answer §11.2): on QR and online checkout the
//! server applies the best deals automatically, and the customer sees them
//! before sending the order. The quote prices the cart exactly as the order
//! intake will (combos through `madar_catalog::combo`, deals through
//! `madar_catalog::deal::auto_apply`), so what the page shows is what the
//! order records.

use actix_web::{HttpResponse, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    combos::not_yet,
    deals::types::{CartQuote, PublicCartQuoteRequest},
    errors::{AppError, AppErrorResponse},
};

/// Price an online (storefront) cart at a branch, deals applied.
#[utoipa::path(post, path = "/public/branches/{id}/cart-quote", tag = "delivery",
    operation_id = "public_branch_cart_quote",
    params(("id" = Uuid, Path, description = "Branch ID")),
    request_body = PublicCartQuoteRequest,
    responses((status = 200, description = "The cart as the order will be priced", body = CartQuote), AppErrorResponse))]
pub async fn branch_cart_quote(
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<PublicCartQuoteRequest>,
) -> Result<HttpResponse, AppError> {
    let _ = (pool, *id, body);
    Err(not_yet())
}

/// Price a QR table cart, deals applied.
#[utoipa::path(post, path = "/public/tables/{id}/cart-quote", tag = "open_tickets",
    operation_id = "public_table_cart_quote",
    params(("id" = Uuid, Path, description = "Table ID, from the QR")),
    request_body = PublicCartQuoteRequest,
    responses((status = 200, description = "The cart as the order will be priced", body = CartQuote), AppErrorResponse))]
pub async fn table_cart_quote(
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<PublicCartQuoteRequest>,
) -> Result<HttpResponse, AppError> {
    let _ = (pool, *id, body);
    Err(not_yet())
}
