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
    deals::types::{CartQuote, PublicCartQuoteRequest},
    delivery::snapshot::{CartLineInput, resolve_cart_as},
    errors::{AppError, AppErrorResponse},
};

/// The request's lines as the intake reads them (prices never taken).
fn cart_lines(body: &PublicCartQuoteRequest) -> Result<Vec<CartLineInput>, AppError> {
    body.items
        .iter()
        .map(|it| {
            Ok(CartLineInput {
                menu_item_id: it.menu_item_id.ok_or_else(|| {
                    AppError::BadRequest("Each line item must have a menu_item_id".into())
                })?,
                size_label: it.size_label.clone(),
                quantity: it.quantity,
                addons: it.addons.clone(),
                optional_field_ids: it.optional_field_ids.clone(),
                notes: it.notes.clone(),
                combo: it.combo.clone(),
            })
        })
        .collect()
}

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
    let org_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(*id)
    .fetch_optional(pool.get_ref())
    .await?;
    let org_id = org_id.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    use crate::delivery::{CHANNEL_IN_MALL, CHANNEL_OUTSIDE, CHANNEL_PICKUP, CHANNEL_UMBRELLA};
    let channel = body.channel.as_deref().unwrap_or(CHANNEL_PICKUP);
    if ![
        CHANNEL_IN_MALL,
        CHANNEL_OUTSIDE,
        CHANNEL_UMBRELLA,
        CHANNEL_PICKUP,
    ]
    .contains(&channel)
    {
        return Err(AppError::BadRequest(format!("Unknown channel '{channel}'")));
    }
    let lines = cart_lines(&body)?;
    let cart = resolve_cart_as(
        pool.get_ref(),
        org_id,
        *id,
        Some(channel),
        madar_catalog::combo::Channel::Online,
        &lines,
        chrono::Utc::now(),
    )
    .await?;
    Ok(HttpResponse::Ok().json(cart.quote))
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
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT t.branch_id, b.org_id FROM branch_tables t JOIN branches b ON b.id = t.branch_id \
          WHERE t.id = $1 AND t.is_active AND b.is_active AND b.deleted_at IS NULL",
    )
    .bind(*id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (branch_id, org_id) = row.ok_or_else(|| AppError::NotFound("Table not found".into()))?;
    let lines = cart_lines(&body)?;
    // A table is dine-in: no delivery sub-channel prices.
    let cart = resolve_cart_as(
        pool.get_ref(),
        org_id,
        branch_id,
        None,
        madar_catalog::combo::Channel::Qr,
        &lines,
        chrono::Utc::now(),
    )
    .await?;
    Ok(HttpResponse::Ok().json(cart.quote))
}
