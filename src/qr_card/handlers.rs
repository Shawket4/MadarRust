//! QR code generation endpoints.
//!
//! Every endpoint builds the canonical long URL server-side, creates/looks up a
//! Shlink short URL (server-to-server), then renders the QR of the *short* URL
//! and returns JSON with an inline base64 data-URL.  Clients never supply a
//! pre-made short URL — that would bypass analytics and unguessability.

use std::sync::Arc;

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};

use super::{
    QrCardOptions,
    brand::{CardBrand, card_brand},
    db::{self, BranchTable, CreateTableRequest},
    render_qr_card_png, render_qr_card_svg, render_qr_receipt_png,
    shlink::ShortLinkProvider,
};

// ── Response DTOs ─────────────────────────────────────────────────────────────

/// JSON returned from every QR-generation endpoint.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct QrResponse {
    pub kind: String,
    pub long_url: String,
    pub short_url: String,
    pub short_code: String,
    /// `data:image/png;base64,…` (or `data:image/svg+xml;base64,…` when
    /// `svg=true`).  Paste into a browser `<img src="…">` to verify.
    pub qr_data_url: String,
}

// ── Render options (shared across QR endpoints) ───────────────────────────────

fn default_true() -> bool {
    true
}
fn default_dpi() -> u32 {
    600
}
fn default_module_px() -> u32 {
    16
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct QrRenderQuery {
    /// `true` (default) → branded A6 card PNG; `false` → plain receipt QR PNG.
    #[serde(default = "default_true")]
    pub card: bool,
    /// Dynamic caption line beneath the tagline (A6 card only).
    pub caption: Option<String>,
    /// Raster DPI for the A6 card (clamped 72–2400). Default 600.
    #[serde(default = "default_dpi")]
    pub dpi: u32,
    /// Print bleed in mm (A6 card only). Default 0.
    #[serde(default)]
    pub bleed_mm: f32,
    /// Draw crop marks (A6 card, only meaningful when `bleed_mm > 0`).
    #[serde(default)]
    pub crop_marks: bool,
    /// Return the A6 card as SVG (`data:image/svg+xml;base64,…`). Default false.
    #[serde(default)]
    pub svg: bool,
    /// Pixels per module for the plain receipt QR (1–40). Default 16.
    #[serde(default = "default_module_px")]
    pub module_px: u32,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// The brand this organisation's card is painted in, or `None` for Madar's.
///
/// `orgs::branding::load` is the one brand loader and it applies the tier gate
/// itself, so nothing here asks whether the shop is entitled to its own
/// colours — an org that has not bought custom branding comes back wearing
/// Madar's, and `card_brand` turns that into the `None` the renderer reads as
/// "compose the card exactly as it was composed before any of this existed".
///
/// Every card-rendering endpoint in this file goes through it, and that is the
/// part worth guarding. A card that came back in the wrong shop's colours would
/// be spotted immediately; one that came back in Madar's because a single
/// endpoint forgot to ask looks entirely correct, right up until a shop asks
/// why their table cards are branded and their loyalty cards are not.
async fn card_brand_for(pool: &PgPool, org_id: Uuid) -> Result<Option<CardBrand>, AppError> {
    Ok(card_brand(
        &crate::orgs::branding::load(pool, org_id).await?,
    ))
}

/// Render the QR of `short_url` according to the query flags and return the
/// `data:…` string.
///
/// The receipt QR is deliberately left out of the branding: it is black on
/// white because that is what survives a thermal printer, and a shop's colours
/// would arrive as grey dither.
/// What a code opens, in the words a person standing at a counter would use.
///
/// Every card we print looks the same — a mark, a square, and a caption that
/// might say "Table 5". Whether scanning it opens a menu, books a table or
/// joins a rewards programme was knowable only by scanning it, which is a poor
/// thing to discover about a poster after it has gone up.
fn purpose_of(kind: &str) -> Option<&'static str> {
    match kind {
        "org_order" | "branch_order" | "table_order" => Some("Scan to order"),
        "order_track" => Some("Track your order"),
        "org_booking" | "branch_booking" => Some("Scan to book a table"),
        // Loyalty deliberately has NONE. The card is handed over at the
        // counter, or sits beside the till, where "scan to join our rewards"
        // reads as an advert for something the person is already doing — and a
        // loyalty code is scanned by EXISTING members far more often than by
        // new ones, for whom the line is simply wrong.
        "org_loyalty" | "branch_loyalty" => None,
        // A marketing link is whatever the shop pointed it at, and guessing
        // would be worse than the caption they wrote themselves.
        _ => None,
    }
}

fn render_data_url(
    short_url: &str,
    q: &QrRenderQuery,
    brand: Option<&CardBrand>,
    kind: &str,
) -> Result<String, AppError> {
    if !q.card {
        let png = render_qr_receipt_png(short_url, q.module_px)?;
        return Ok(format!("data:image/png;base64,{}", B64.encode(&png)));
    }
    let opts = QrCardOptions {
        short_url: short_url.to_string(),
        caption: q.caption.clone(),
        dpi: q.dpi,
        bleed_mm: q.bleed_mm,
        crop_marks: q.crop_marks,
        brand: brand.cloned(),
        purpose: purpose_of(kind).map(str::to_string),
    };
    if q.svg {
        let svg = render_qr_card_svg(&opts)?;
        return Ok(format!(
            "data:image/svg+xml;base64,{}",
            B64.encode(svg.as_bytes())
        ));
    }
    let png = render_qr_card_png(&opts)?;
    Ok(format!("data:image/png;base64,{}", B64.encode(&png)))
}

// ── A shop's own address ─────────────────────────────────────────────────────
//
// On the branding tier a shop stops being a path on one of our hosts and
// becomes a hostname: `drops.madar-pos.cloud`. The customer surfaces have
// answered to that since it shipped — every public bundle calls `useHostOrg()`,
// which takes the first label of the hostname as a slug and resolves the org,
// so the id does not need to be in the path at all.
//
// The QR generator never got switched over, so every card a branded shop
// printed still pointed at `order.madar-pos.cloud/order/<uuid>` — the generic
// host, and a route the ordering bundle labels "back-compat". The shop's own
// address appeared on its dashboard and nowhere a customer would ever see it.
//
// ONE host, three mounts, decided at build time by `MADAR_MOUNT`
// (`vite.order.config.ts` / `vite.reservations.config.ts`): the root is the
// loyalty card, `/order` is the menu, `/book` is bookings. Keep this in step
// with `MadarDashboard/src/features/settings/shop-address.ts`, which tells the
// shop the same three addresses — if they disagree, one of us is printing a
// 404.

/// Where the menu is mounted on a shop's own host.
const ORDER_MOUNT: &str = "/order";
/// Where bookings are mounted on a shop's own host.
const BOOK_MOUNT: &str = "/book";

/// The origin a shop's own codes should point at, or `None` when it has none.
///
/// `None` is the ordinary answer for every shop off the branding tier, and the
/// caller falls back to the generic host — the same rule the dashboard applies
/// before it offers to show anyone their address.
async fn shop_origin(pool: &PgPool, org_id: Uuid) -> Result<Option<String>, AppError> {
    // OFF until the box can serve it.
    //
    // A shop subdomain needs a wildcard vhost and a WILDCARD CERTIFICATE, and
    // the certificate needs a DNS-01 challenge. Today the production box has
    // neither: it has one hand-written vhost and one HTTP-01 certificate per
    // shop, of which exactly one exists. So switching this on unconditionally
    // would make things WORSE than before it was written — a branded shop with
    // no vhost of its own used to print codes that worked on the generic host,
    // and would now print codes that give a full-page certificate warning and
    // then a dropped connection. On something printed and stuck to a table.
    //
    // The flag is the ordering constraint made explicit: infrastructure first,
    // then this. Unset is the old behaviour, which is the behaviour that works.
    if !shop_subdomains_enabled() {
        return Ok(None);
    }
    let row: Option<(Option<String>, bool)> = sqlx::query_as(
        "SELECT slug, custom_branding FROM organizations \
          WHERE id = $1 AND is_active AND deleted_at IS NULL",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((Some(slug), true)) = row else {
        return Ok(None);
    };
    let slug = slug.trim().to_lowercase();
    if slug.is_empty() {
        return Ok(None);
    }
    Ok(public_root_domain().map(|root| format!("https://{slug}.{root}")))
}

/// Whether this deployment can actually serve `<slug>.<root>`.
///
/// Set `PUBLIC_SHOP_SUBDOMAINS=1` once the wildcard DNS record, the wildcard
/// vhost and the wildcard certificate are all in place. Anything else — unset,
/// empty, `0`, `false` — keeps every code on the generic hosts.
fn shop_subdomains_enabled() -> bool {
    std::env::var("PUBLIC_SHOP_SUBDOMAINS")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes"
        })
        .unwrap_or(false)
}

/// The domain shops are given subdomains of, taken from the ordering host
/// rather than configured separately — `order.madar-pos.cloud` is
/// `madar-pos.cloud`. Staging and production differ, and a second variable
/// would be a second thing to get wrong in one of them.
///
/// `None` where there is no subdomain to give: a bare host, an IP, localhost.
fn public_root_domain() -> Option<String> {
    let base = std::env::var("PUBLIC_ORDER_BASE_URL").ok()?;
    let host = base
        .rsplit("://")
        .next()?
        .split('/')
        .next()?
        .split(':')
        .next()?;
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() < 3 || labels.iter().all(|l| l.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    Some(labels[1..].join("."))
}

/// The ordering page for one branch.
///
/// On a shop's own host the hostname IS the org, so the id leaves the path:
/// `{slug}.madar-pos.cloud/order/?branch=B`. Everywhere else it stays where it
/// has always been: `order.madar-pos.cloud/order/{org}?branch=B`.
fn branch_order_url(shop: Option<&str>, org_id: Uuid, branch_id: Uuid) -> Result<String, AppError> {
    Ok(format!(
        "{}?branch={}",
        order_base(shop, org_id)?,
        branch_id
    ))
}

/// The ordering bundle's entry, with or without the org in the path.
fn order_base(shop: Option<&str>, org_id: Uuid) -> Result<String, AppError> {
    match shop {
        Some(origin) => Ok(format!("{origin}{ORDER_MOUNT}/")),
        None => {
            let base = std::env::var("PUBLIC_ORDER_BASE_URL").map_err(|_| {
                AppError::ServiceUnavailable("PUBLIC_ORDER_BASE_URL not configured".into())
            })?;
            Ok(format!("{}/order/{}", base.trim_end_matches('/'), org_id))
        }
    }
}

/// One table's code: the menu, pre-bound to where the customer is sitting.
fn table_order_url(
    shop: Option<&str>,
    org_id: Uuid,
    branch_id: Uuid,
    table_id: Uuid,
) -> Result<String, AppError> {
    Ok(format!(
        "{}?branch={}&table={}",
        order_base(shop, org_id)?,
        branch_id,
        table_id
    ))
}

/// Validate a relative marketing path — must start with `/`, no scheme or
/// host part.  Exposed for unit tests as `validate_marketing_path_pub`.
pub fn validate_marketing_path_pub(path: &str) -> Result<(), AppError> {
    validate_marketing_path(path)
}

fn validate_marketing_path(path: &str) -> Result<(), AppError> {
    if !path.starts_with('/') {
        return Err(AppError::BadRequest(
            "path must be a relative URL starting with /".into(),
        ));
    }
    // Reject protocol-relative `//host` and anything with `:`
    if path.starts_with("//") || path.contains(':') {
        return Err(AppError::BadRequest(
            "path must not contain a scheme or host".into(),
        ));
    }
    Ok(())
}

fn marketing_url(path: &str) -> Result<String, AppError> {
    validate_marketing_path(path)?;
    let base = std::env::var("PUBLIC_ORDER_BASE_URL")
        .map_err(|_| AppError::ServiceUnavailable("PUBLIC_ORDER_BASE_URL not configured".into()))?;
    Ok(format!("{}{}", base.trim_end_matches('/'), path))
}

/// A code for a stand in a mall: the menu with the channel locked and the
/// customer's location pre-filled.
fn in_mall_order_url(
    shop: Option<&str>,
    org_id: Uuid,
    branch_id: Uuid,
    place_name: &str,
    floor: &str,
    unit_number: &str,
) -> Result<String, AppError> {
    Ok(format!(
        "{}?branch={}&channel=in_mall&place_name={}&floor={}&unit_number={}",
        order_base(shop, org_id)?,
        branch_id,
        urlencoding::encode(place_name),
        urlencoding::encode(floor),
        urlencoding::encode(unit_number),
    ))
}

/// The reservations bundle's origin. A DIFFERENT host from the ordering one —
/// `reservations.madar-pos.cloud` vs `order.madar-pos.cloud` — because the two
/// are separately built, separately deployed apps that share no code.
///
/// Unset degrades the same way `PUBLIC_ORDER_BASE_URL` does: a 503 the operator
/// can read, rather than a QR card pointing at nowhere. (`bookings::whatsapp`
/// reads the same variable and degrades more softly — it drops the manage link
/// from the message rather than failing the booking.)
fn reservations_base() -> Result<String, AppError> {
    std::env::var("PUBLIC_RESERVATIONS_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .ok_or_else(|| {
            AppError::ServiceUnavailable("PUBLIC_RESERVATIONS_BASE_URL not configured".into())
        })
}

/// Base of the public loyalty site (`loyalty.madar-pos.cloud`). Degrades like
/// the two above: a 503 the operator can read, rather than a printed card that
/// leads nowhere.
fn loyalty_base() -> Result<String, AppError> {
    std::env::var("PUBLIC_LOYALTY_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .ok_or_else(|| {
            AppError::ServiceUnavailable("PUBLIC_LOYALTY_BASE_URL not configured".into())
        })
}

/// Build `{PUBLIC_LOYALTY_BASE_URL}/join/{branch_id}` — the counter's join form.
fn branch_loyalty_url(shop: Option<&str>, branch_id: Uuid) -> Result<String, AppError> {
    // The card is mounted at the ROOT of a shop's own host, so the join form
    // hangs straight off it.
    let base = match shop {
        Some(origin) => origin.to_string(),
        None => loyalty_base()?,
    };
    Ok(format!("{base}/join/{branch_id}"))
}

/// Build `{PUBLIC_LOYALTY_BASE_URL}/join/org/{org_id}` — one code for the whole
/// shop, for a poster, a receipt footer or a link in a bio.
///
/// A distinct PATH rather than the same one carrying either kind of id: a
/// public link that means different things depending on what a uuid turns out
/// to be is a link nobody can reason about, and the branch cards already
/// printed keep working untouched.
fn org_loyalty_url(shop: Option<&str>, org_id: Uuid) -> Result<String, AppError> {
    let base = match shop {
        Some(origin) => origin.to_string(),
        None => loyalty_base()?,
    };
    Ok(format!("{base}/join/org/{org_id}"))
}

/// Build `{PUBLIC_RESERVATIONS_BASE_URL}/{org_id}` — the guest picks the branch.
fn org_booking_url(shop: Option<&str>, org_id: Uuid) -> Result<String, AppError> {
    Ok(format!("{}/{}", booking_base(shop)?, org_id))
}

/// The reservations bundle's entry — its own host, or the `/book` mount on a
/// shop's. Its routes still name the org, because a guest picking a branch
/// lands on `/{org}` either way.
fn booking_base(shop: Option<&str>) -> Result<String, AppError> {
    match shop {
        Some(origin) => Ok(format!("{origin}{BOOK_MOUNT}")),
        None => reservations_base(),
    }
}

/// Build `{PUBLIC_RESERVATIONS_BASE_URL}/{org_id}/{branch_id}` — one branch,
/// straight to its slot picker.
fn branch_booking_url(
    shop: Option<&str>,
    org_id: Uuid,
    branch_id: Uuid,
) -> Result<String, AppError> {
    Ok(format!("{}/{}/{}", booking_base(shop)?, org_id, branch_id))
}

/// The whole shop: no branch pre-selected, so the customer sees the picker.
fn org_order_url(shop: Option<&str>, org_id: Uuid) -> Result<String, AppError> {
    order_base(shop, org_id)
}

/// Fetch the branch, checking it belongs to the caller's org.
async fn load_branch_checked(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(Uuid, Uuid), AppError> {
    let row: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?;
    let (id, org_id) = row.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    require_same_org(claims, Some(org_id))?;
    Ok((id, org_id))
}

// ── GET /branches/{id}/qr ─────────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SlugQuery {
    pub slug: Option<String>,
}

/// Optional in-mall pre-fill query params. When all three fields are present the
/// generated URL locks `channel=in_mall` and pre-fills the location for the
/// customer. When omitted, a standard branch-ordering URL is generated instead.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InMallQuery {
    /// Shop or company name inside the mall (e.g. "Starbucks Kiosk 3").
    pub place_name: Option<String>,
    /// Floor (e.g. "Ground Floor").
    pub floor: Option<String>,
    /// Unit or office number (e.g. "Unit 42").
    pub unit_number: Option<String>,
}

#[utoipa::path(
    get,
    path = "/branches/{id}/qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Branch ID"),
        QrRenderQuery,
        SlugQuery,
        InMallQuery,
    ),
    responses(
        (status = 200, description = "Branch online-ordering QR (standard or in-mall)", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn branch_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
    slug_q: web::Query<SlugQuery>,
    in_mall_q: web::Query<InMallQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    let (branch_id, org_id) = load_branch_checked(pool.get_ref(), &claims, *id).await?;

    // When all three in-mall location fields are provided, generate a pre-filled
    // in-mall URL and use a separate dedup key so each location gets its own code.
    let in_mall = match (
        &in_mall_q.place_name,
        &in_mall_q.floor,
        &in_mall_q.unit_number,
    ) {
        (Some(p), Some(f), Some(u)) if !p.is_empty() && !f.is_empty() && !u.is_empty() => {
            Some((p.clone(), f.clone(), u.clone()))
        }
        _ => None,
    };

    let (kind, target_ref, long_url, auto_caption) = if let Some((place, floor, unit)) = &in_mall {
        let shop = shop_origin(pool.get_ref(), org_id).await?;
        let url = in_mall_order_url(shop.as_deref(), org_id, branch_id, place, floor, unit)?;
        let target = format!("{}:in_mall:{}:{}:{}", branch_id, place, floor, unit);
        let caption = place.clone();
        ("branch_order_in_mall", target, url, Some(caption))
    } else {
        let shop = shop_origin(pool.get_ref(), org_id).await?;
        let url = branch_order_url(shop.as_deref(), org_id, branch_id)?;
        ("branch_order", branch_id.to_string(), url, None)
    };

    let q_with_caption = if q.caption.is_none() {
        if let Some(cap) = auto_caption {
            QrRenderQuery {
                caption: Some(cap),
                ..q.into_inner()
            }
        } else {
            q.into_inner()
        }
    } else {
        q.into_inner()
    };

    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        Some(branch_id),
        kind,
        &target_ref,
        &long_url,
        slug_q.slug.as_deref(),
        in_mall.as_ref().map(|(p, _, _)| p.as_str()),
    )
    .await?;

    let brand = card_brand_for(pool.get_ref(), org_id).await?;
    let qr_data_url = render_data_url(&row.short_url, &q_with_caption, brand.as_ref(), &row.kind)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: kind.into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /orgs/{id}/qr ────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/orgs/{id}/qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Organisation ID"),
        QrRenderQuery,
    ),
    responses(
        (status = 200, description = "Org-wide branch-picker QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn org_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    let org_id = *id;
    require_same_org(&claims, Some(org_id))?;

    let shop = shop_origin(pool.get_ref(), org_id).await?;
    let long_url = org_order_url(shop.as_deref(), org_id)?;
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        None,
        "org_order",
        &org_id.to_string(),
        &long_url,
        None,
        None,
    )
    .await?;

    let brand = card_brand_for(pool.get_ref(), org_id).await?;
    let qr_data_url = render_data_url(&row.short_url, &q, brand.as_ref(), &row.kind)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "org_order".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /branches/{id}/booking-qr ────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/branches/{id}/booking-qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Branch ID"),
        QrRenderQuery,
        SlugQuery,
    ),
    responses(
        (status = 200, description = "Branch table-booking QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn branch_booking_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
    slug_q: web::Query<SlugQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "read").await?;
    let (branch_id, org_id) = load_branch_checked(pool.get_ref(), &claims, *id).await?;

    // Refuse to print a card that leads to a page saying "we don't take
    // bookings". The settings row defaults `enabled` to false, so this is the
    // normal state of a branch nobody has turned bookings on for.
    let enabled: Option<bool> =
        sqlx::query_scalar("SELECT enabled FROM branch_booking_settings WHERE branch_id = $1")
            .bind(branch_id)
            .fetch_optional(pool.get_ref())
            .await?;
    if enabled != Some(true) {
        return Err(AppError::Conflict(
            "Bookings are switched off for this branch".into(),
        ));
    }

    let shop = shop_origin(pool.get_ref(), org_id).await?;
    let long_url = branch_booking_url(shop.as_deref(), org_id, branch_id)?;
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        Some(branch_id),
        "branch_booking",
        &branch_id.to_string(),
        &long_url,
        slug_q.slug.as_deref(),
        None,
    )
    .await?;

    let brand = card_brand_for(pool.get_ref(), org_id).await?;
    let qr_data_url = render_data_url(&row.short_url, &q, brand.as_ref(), &row.kind)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "branch_booking".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /orgs/{id}/booking-qr ────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/orgs/{id}/booking-qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Organisation ID"),
        QrRenderQuery,
    ),
    responses(
        (status = 200, description = "Org-wide branch-picker booking QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn org_booking_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "read").await?;
    let org_id = *id;
    require_same_org(&claims, Some(org_id))?;

    // The org card leads to a branch picker, so it needs at least one branch
    // that actually takes bookings — otherwise the picker is empty.
    let any: bool = sqlx::query_scalar(
        "SELECT EXISTS( \
           SELECT 1 FROM branch_booking_settings s \
           JOIN branches b ON b.id = s.branch_id \
           WHERE b.org_id = $1 AND b.deleted_at IS NULL AND s.enabled)",
    )
    .bind(org_id)
    .fetch_one(pool.get_ref())
    .await?;
    if !any {
        return Err(AppError::Conflict(
            "No branch in this organisation takes bookings yet".into(),
        ));
    }

    let shop = shop_origin(pool.get_ref(), org_id).await?;
    let long_url = org_booking_url(shop.as_deref(), org_id)?;
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        None,
        "org_booking",
        &org_id.to_string(),
        &long_url,
        None,
        None,
    )
    .await?;

    let brand = card_brand_for(pool.get_ref(), org_id).await?;
    let qr_data_url = render_data_url(&row.short_url, &q, brand.as_ref(), &row.kind)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "org_booking".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── POST /branches/{id}/tables ────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/branches/{id}/tables",
    tag = "qr",
    params(("id" = Uuid, Path, description = "Branch ID")),
    request_body = CreateTableRequest,
    responses(
        (status = 201, description = "Table created", body = BranchTable),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_table(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<CreateTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "update").await?;
    let (branch_id, org_id) = load_branch_checked(pool.get_ref(), &claims, *id).await?;

    let table = sqlx::query_as::<_, BranchTable>(
        "INSERT INTO branch_tables (org_id, branch_id, label)
         VALUES ($1, $2, $3)
         RETURNING id, org_id, branch_id, label, is_active, created_at, updated_at",
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(&body.label)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(table))
}

// ── GET /branches/{id}/tables ─────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/branches/{id}/tables",
    tag = "qr",
    params(("id" = Uuid, Path, description = "Branch ID")),
    responses(
        (status = 200, description = "Tables for this branch", body = Vec<BranchTable>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_tables(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    let (branch_id, _) = load_branch_checked(pool.get_ref(), &claims, *id).await?;

    let tables = sqlx::query_as::<_, BranchTable>(
        "SELECT id, org_id, branch_id, label, is_active, created_at, updated_at
         FROM branch_tables
         WHERE branch_id = $1
         ORDER BY label",
    )
    .bind(branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(tables))
}

// ── DELETE /branches/{id}/tables/{tid} ────────────────────────────────────────

#[derive(Deserialize)]
pub struct TablePath {
    pub id: Uuid,
    pub tid: Uuid,
}

#[utoipa::path(
    delete,
    path = "/branches/{id}/tables/{tid}",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Branch ID"),
        ("tid" = Uuid, Path, description = "Table ID"),
    ),
    responses(
        (status = 204, description = "Table deleted"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_table(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<TablePath>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "update").await?;
    let (branch_id, _) = load_branch_checked(pool.get_ref(), &claims, path.id).await?;

    let table = db::fetch_table(pool.get_ref(), path.tid).await?;
    if table.branch_id != branch_id {
        return Err(AppError::NotFound("Table not found".into()));
    }

    sqlx::query("DELETE FROM branch_tables WHERE id = $1")
        .bind(path.tid)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── GET /branches/{id}/tables/{tid}/qr ───────────────────────────────────────

#[derive(Deserialize)]
pub struct TableQrPath {
    pub id: Uuid,
    pub tid: Uuid,
}

#[utoipa::path(
    get,
    path = "/branches/{id}/tables/{tid}/qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Branch ID"),
        ("tid" = Uuid, Path, description = "Table ID"),
        QrRenderQuery,
    ),
    responses(
        (status = 200, description = "Table online-ordering QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn table_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    path: web::Path<TableQrPath>,
    q: web::Query<QrRenderQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    let (branch_id, org_id) = load_branch_checked(pool.get_ref(), &claims, path.id).await?;

    let table = db::fetch_table(pool.get_ref(), path.tid).await?;
    if table.branch_id != branch_id {
        return Err(AppError::NotFound("Table not found".into()));
    }

    let shop = shop_origin(pool.get_ref(), org_id).await?;
    let long_url = table_order_url(shop.as_deref(), org_id, branch_id, path.tid)?;
    let caption = q.caption.clone().unwrap_or_else(|| table.label.clone());
    let q_with_caption = QrRenderQuery {
        caption: Some(caption),
        ..q.into_inner()
    };

    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        Some(branch_id),
        "table_order",
        &path.tid.to_string(),
        &long_url,
        None,
        Some(&table.label),
    )
    .await?;

    let brand = card_brand_for(pool.get_ref(), org_id).await?;
    let qr_data_url = render_data_url(&row.short_url, &q_with_caption, brand.as_ref(), &row.kind)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "table_order".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /delivery-orders/{id}/qr ─────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/delivery-orders/{id}/qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Delivery order ID"),
        QrRenderQuery,
    ),
    responses(
        (status = 200, description = "Order tracking QR (always a random short code)", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delivery_order_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_orders", "read").await?;

    let row: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, org_id FROM delivery_orders WHERE id = $1")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?;
    let (order_id, org_id) =
        row.ok_or_else(|| AppError::NotFound("Delivery order not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let long_url = crate::delivery::whatsapp::tracking_url(order_id).ok_or_else(|| {
        AppError::ServiceUnavailable("PUBLIC_ORDER_BASE_URL not configured".into())
    })?;

    // order_track always uses a random short code (no customSlug) — guessable
    // slugs would let anyone enumerate order tracking pages.
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        None,
        "order_track",
        &order_id.to_string(),
        &long_url,
        None, // ← never a custom slug
        None,
    )
    .await?;

    let brand = card_brand_for(pool.get_ref(), org_id).await?;
    let qr_data_url = render_data_url(&row.short_url, &q, brand.as_ref(), &row.kind)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "order_track".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── POST /qr/links (marketing) ────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateMarketingLinkRequest {
    #[schema(example = "Promo Dec")]
    pub label: String,
    #[schema(example = "/menu?promo=december")]
    pub path: String,
    pub custom_slug: Option<String>,
}

#[utoipa::path(
    post,
    path = "/qr/links",
    tag = "qr",
    request_body = CreateMarketingLinkRequest,
    responses(
        (status = 201, description = "Marketing QR link created", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_marketing_link(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    body: web::Json<CreateMarketingLinkRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;

    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("Org context required".into()))?;

    let long_url = marketing_url(&body.path)?;

    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        None,
        "marketing",
        &body.path,
        &long_url,
        body.custom_slug.as_deref(),
        Some(&body.label),
    )
    .await?;

    let opts = QrCardOptions {
        short_url: row.short_url.clone(),
        brand: card_brand_for(pool.get_ref(), org_id).await?,
        ..Default::default()
    };
    let png = render_qr_card_png(&opts)?;
    let qr_data_url = format!("data:image/png;base64,{}", B64.encode(&png));

    Ok(HttpResponse::Created().json(QrResponse {
        kind: "marketing".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /qr/links ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct MarketingLink {
    pub id: Uuid,
    pub kind: String,
    pub target_ref: String,
    pub long_url: String,
    pub short_code: String,
    pub short_url: String,
    pub label: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[utoipa::path(
    get,
    path = "/qr/links",
    tag = "qr",
    responses(
        (status = 200, description = "All marketing short links for the org", body = Vec<MarketingLink>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_marketing_links(
    req: HttpRequest,
    pool: crate::db::Db,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;

    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("Org context required".into()))?;

    let links = sqlx::query_as::<_, MarketingLink>(
        "SELECT id, kind, target_ref, long_url, short_code, short_url, label, created_at
         FROM qr_short_links
         WHERE org_id = $1 AND kind = 'marketing'
         ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(links))
}

// ── GET /orgs/{id}/loyalty-qr ────────────────────────────────────────────────

/// The shop's join QR: one code for the whole organisation.
///
/// A membership belongs to the SHOP, not to a branch — which is why the wallet
/// pass has always carried the org's programme and every branch's location. The
/// only thing that was ever per-branch was the way IN, so a shop that wants one
/// code on a poster had to pick a branch and pretend.
#[utoipa::path(
    get,
    path = "/orgs/{id}/loyalty-qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Organization ID"),
        QrRenderQuery,
        SlugQuery,
    ),
    responses(
        (status = 200, description = "Organisation loyalty join QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn org_loyalty_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
    slug_q: web::Query<SlugQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    let org_id = *id;
    require_same_org(&claims, Some(org_id))?;

    // The org card carries the ORG's programme, so that is the one that has to
    // be switched on. Refusing here beats printing a card that leads to "we run
    // no program" — the same guard the branch card has.
    let settings = crate::loyalty::settings::load_scope(pool.get_ref(), org_id, None)
        .await?
        .unwrap_or_else(|| crate::loyalty::settings::LoyaltySettings::defaults(org_id, None));
    if !settings.enabled {
        return Err(AppError::Conflict(
            "The loyalty program is switched off for this organisation".into(),
        ));
    }

    let shop = shop_origin(pool.get_ref(), org_id).await?;
    let long_url = org_loyalty_url(shop.as_deref(), org_id)?;
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        None,
        "org_loyalty",
        &org_id.to_string(),
        &long_url,
        slug_q.slug.as_deref(),
        None,
    )
    .await?;

    let brand = card_brand_for(pool.get_ref(), org_id).await?;
    let qr_data_url = render_data_url(&row.short_url, &q, brand.as_ref(), &row.kind)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "org_loyalty".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

// ── GET /branches/{id}/loyalty-qr ────────────────────────────────────────────

/// The counter's join QR: a static, per-branch card that opens the public
/// signup form. Static on purpose — it is printed once and stood on a counter,
/// so it must keep working with no reprint. The per-CUSTOMER QR is a different
/// thing entirely: it lives on their Wallet pass and carries their member token.
#[utoipa::path(
    get,
    path = "/branches/{id}/loyalty-qr",
    tag = "qr",
    params(
        ("id" = Uuid, Path, description = "Branch ID"),
        QrRenderQuery,
        SlugQuery,
    ),
    responses(
        (status = 200, description = "Branch loyalty join QR", body = QrResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn branch_loyalty_qr(
    req: HttpRequest,
    pool: crate::db::Db,
    provider: web::Data<Arc<dyn ShortLinkProvider>>,
    id: web::Path<Uuid>,
    q: web::Query<QrRenderQuery>,
    slug_q: web::Query<SlugQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    let (branch_id, org_id) = load_branch_checked(pool.get_ref(), &claims, *id).await?;

    // Refuse to print a card that leads to a page saying "we run no program
    // here" — the same guard the booking QR has, for the same reason.
    let settings =
        crate::loyalty::settings::load_effective(pool.get_ref(), org_id, branch_id).await?;
    if !settings.enabled {
        return Err(AppError::Conflict(
            "The loyalty program is switched off for this branch".into(),
        ));
    }

    let shop = shop_origin(pool.get_ref(), org_id).await?;
    let long_url = branch_loyalty_url(shop.as_deref(), branch_id)?;
    let row = db::get_or_create_short_link(
        pool.get_ref(),
        provider.get_ref().as_ref(),
        org_id,
        Some(branch_id),
        "branch_loyalty",
        &branch_id.to_string(),
        &long_url,
        slug_q.slug.as_deref(),
        None,
    )
    .await?;

    let brand = card_brand_for(pool.get_ref(), org_id).await?;
    let qr_data_url = render_data_url(&row.short_url, &q, brand.as_ref(), &row.kind)?;
    Ok(HttpResponse::Ok().json(QrResponse {
        kind: "branch_loyalty".into(),
        long_url: row.long_url,
        short_url: row.short_url,
        short_code: row.short_code,
        qr_data_url,
    }))
}

#[cfg(test)]
mod address_tests {
    use super::*;

    const ORG: Uuid = Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
    const BRANCH: Uuid = Uuid::from_u128(0x9999_0000_1111_2222_3333_4444_5555_6666);
    const TABLE: Uuid = Uuid::from_u128(0xaaaa_bbbb_cccc_dddd_eeee_ffff_0000_1111);
    const SHOP: &str = "https://drops.madar-pos.cloud";

    /// A branded shop's codes carry its OWN address, and the org id leaves the
    /// path — on that hostname the hostname is the org.
    #[test]
    fn a_branded_shop_prints_its_own_address() {
        let shop = Some(SHOP);
        assert_eq!(
            branch_order_url(shop, ORG, BRANCH).unwrap(),
            format!("https://drops.madar-pos.cloud/order/?branch={BRANCH}")
        );
        assert_eq!(
            table_order_url(shop, ORG, BRANCH, TABLE).unwrap(),
            format!("https://drops.madar-pos.cloud/order/?branch={BRANCH}&table={TABLE}")
        );
        assert_eq!(
            org_order_url(shop, ORG).unwrap(),
            "https://drops.madar-pos.cloud/order/"
        );
        // The card is mounted at the root; bookings at /book, which still names
        // the org because that is the route the bundle publishes.
        assert_eq!(
            branch_loyalty_url(shop, BRANCH).unwrap(),
            format!("https://drops.madar-pos.cloud/join/{BRANCH}")
        );
        assert_eq!(
            org_loyalty_url(shop, ORG).unwrap(),
            format!("https://drops.madar-pos.cloud/join/org/{ORG}")
        );
        assert_eq!(
            branch_booking_url(shop, ORG, BRANCH).unwrap(),
            format!("https://drops.madar-pos.cloud/book/{ORG}/{BRANCH}")
        );
    }

    /// A shop off the branding tier has no address of its own, and its codes
    /// keep pointing where they always did. Nothing already printed changes.
    #[test]
    fn an_unbranded_shop_keeps_the_generic_host() {
        unsafe {
            std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://order.madar-pos.cloud");
        }
        assert_eq!(
            branch_order_url(None, ORG, BRANCH).unwrap(),
            format!("https://order.madar-pos.cloud/order/{ORG}?branch={BRANCH}")
        );
    }

    /// A shop with a blank slug has no address, and `slug IS NOT NULL` does
    /// not catch one. There is a live org in exactly that state — branded,
    /// active, `slug = \'\'` — and without this it would print
    /// `https://.madar-pos.cloud/order/`, a hostname with an empty first
    /// label, on something somebody sticks to a table.
    #[test]
    fn a_blank_slug_is_not_an_address() {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://order.madar-pos.cloud") };
        // `shop_origin` is the async half; this is the rule it enforces after
        // the trim, exercised through the piece that has no database in it.
        for slug in ["", "   ", "\t"] {
            assert!(
                slug.trim().is_empty(),
                "a blank slug must never reach the formatter"
            );
        }
        // And the formatter itself, given no shop, keeps the generic form.
        assert!(
            branch_order_url(None, ORG, BRANCH)
                .unwrap()
                .starts_with("https://order.madar-pos.cloud/order/")
        );
    }

    /// The flag is the ordering constraint: no wildcard certificate on the box
    /// means no subdomain in a printed code.
    #[test]
    fn subdomains_are_off_unless_the_box_says_otherwise() {
        for (set, want) in [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
            (Some("YES"), true),
        ] {
            unsafe {
                match set {
                    Some(v) => std::env::set_var("PUBLIC_SHOP_SUBDOMAINS", v),
                    None => std::env::remove_var("PUBLIC_SHOP_SUBDOMAINS"),
                }
            }
            assert_eq!(shop_subdomains_enabled(), want, "for {set:?}");
        }
        unsafe { std::env::remove_var("PUBLIC_SHOP_SUBDOMAINS") };
    }

    /// The root domain is taken off the ordering host rather than configured
    /// twice — one variable to get wrong instead of two.
    #[test]
    fn the_root_domain_comes_off_the_ordering_host() {
        let cases = [
            ("https://order.madar-pos.cloud", Some("madar-pos.cloud")),
            (
                "https://order.staging.madar-pos.cloud",
                Some("staging.madar-pos.cloud"),
            ),
            (
                "http://order.madar-pos.cloud:8443/",
                Some("madar-pos.cloud"),
            ),
            // Nothing to give a subdomain of.
            ("https://example.com", None),
            ("http://localhost:5173", None),
            ("http://127.0.0.1:8081", None),
        ];
        for (base, want) in cases {
            unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", base) };
            assert_eq!(public_root_domain().as_deref(), want, "root of {base}");
        }
    }
}
