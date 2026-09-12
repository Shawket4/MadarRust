//! Who a customer-facing page belongs to, before it knows anything else.
//!
//! Three guest surfaces — ordering, bookings and the loyalty card — all need
//! the same first answer: whose shop is this, and what does it look like? The
//! loyalty pages have had it since they were written, embedded in their own
//! responses. Ordering and bookings never did, so they have been Madar-shaped
//! regardless of whether the shop was paying to look like itself.
//!
//! This is that answer, in one place, and deliberately through
//! [`crate::orgs::branding::load`] rather than by reading the columns: the
//! branding TIER is enforced inside that loader, so a surface that goes through
//! it cannot accidentally hand a shop's colours to a page it has not paid for,
//! and a surface that does not go through it will drift the day the rule
//! changes. There is one loader for a reason.
//!
//! It also resolves a SLUG, which is how a per-shop subdomain works: the page
//! at `rue.madar-pos.cloud` reads the first label of its own hostname and asks
//! here who that is. The alternative — trusting a `Host` header forwarded
//! through a proxy — is a header anyone can set.

use actix_web::{HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::errors::{AppError, AppErrorResponse};

/// A shop, as a guest page needs to know it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PublicBrand {
    pub org_id: Uuid,
    /// Always the shop's own name, at every tier. A page that does not say
    /// whose it is helps nobody, and that was never the thing being sold.
    pub name: String,
    /// `None` when the shop has no address of its own — reached by `org_id`,
    /// which every page that already knows the shop uses.
    pub slug: Option<String>,
    /// Whether the rest of this is the shop's or Madar's.
    ///
    /// The page does not need it to render — the palette below is already
    /// resolved — but it decides how loudly Madar signs the footer.
    pub custom_branding: bool,
    pub logo_url: Option<String>,
    /// True when the logo is a shape on transparency and may be repainted for
    /// contrast. See `orgs::branding::is_mark`.
    pub logo_is_mark: bool,
    pub card_image_url: Option<String>,
    /// `#RRGGBB`. Madar's own when the shop is not on the tier.
    pub background_color: String,
    pub foreground_color: String,
    pub accent_color: String,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BrandQuery {
    /// The shop, when the page already knows which one it is.
    pub org_id: Option<Uuid>,
    /// The first label of the hostname, when it does not — `rue` for
    /// `rue.madar-pos.cloud`.
    pub slug: Option<String>,
}

/// The shop behind a guest page.
///
/// Public and unauthenticated by necessity: it is the first request a customer's
/// browser makes, before there is any notion of a session. Nothing here is
/// private — a name, a logo and three colours are on the shopfront.
#[utoipa::path(get, path = "/public/orgs/brand", tag = "orgs",
    operation_id = "public_org_brand", params(BrandQuery),
    responses((status = 200, body = PublicBrand), AppErrorResponse))]
pub async fn brand(
    pool: web::Data<PgPool>,
    query: web::Query<BrandQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = resolve_org(pool.get_ref(), &query).await?;

    let org = crate::orgs::branding::load(pool.get_ref(), org_id).await?;
    // Two different absences, and only the outer one is a missing shop: the row
    // may exist and simply have no address of its own, which is what a shop
    // that has never been given a slug looks like.
    let slug: Option<String> =
        sqlx::query_scalar::<_, Option<String>>("SELECT slug FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("No shop at that address".into()))?;

    Ok(HttpResponse::Ok().json(PublicBrand {
        org_id,
        name: org.name,
        slug,
        custom_branding: org.custom_branding,
        logo_url: org.logo_url,
        logo_is_mark: org.logo_is_mark,
        card_image_url: org.card_image_url,
        background_color: org.palette.background,
        foreground_color: org.palette.foreground,
        accent_color: org.palette.accent,
    }))
}

/// Which shop a guest page is asking about: by id when it already knows, by
/// the first label of its own hostname when it does not.
///
/// Shared by every public per-shop surface, so they cannot drift on the one
/// question that decides whose logo and whose colours a customer sees.
async fn resolve_org(pool: &PgPool, query: &BrandQuery) -> Result<Uuid, AppError> {
    match (
        query.org_id,
        query
            .slug
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    ) {
        (Some(id), _) => Ok(id),
        (None, Some(slug)) => {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM organizations \
                  WHERE slug = $1 AND is_active AND deleted_at IS NULL",
            )
            .bind(slug.trim().to_lowercase())
            .fetch_optional(pool)
            .await?
            // Deliberately the same answer as a shop that does not exist. A
            // wildcard subdomain answers for every name anyone types, and
            // distinguishing "no such shop" from "that shop is switched off"
            // would make this an enumeration tool for our whole customer list.
            .ok_or_else(|| AppError::NotFound("No shop at that address".into()))
        }
        // Including `?slug=`, which is not a shop that might exist — it is no
        // name at all. It used to MATCH: an organisation carrying the legacy
        // empty-string slug is precisely the one meant to have no address of
        // its own, and a blank query handed it back, branding and all. No
        // hostname can produce it (a first label is never empty), so this was
        // never a wildcard enumeration hole — but a public endpoint should not
        // answer a question nobody asked.
        (None, None) => Err(AppError::BadRequest(
            "Name the shop, by id or by short name".into(),
        )),
    }
}

/// The sizes a favicon is served at. A browser tab wants 32; an iOS home
/// screen wants 180. Anything else is rounded up to the nearest of these
/// rather than rendered on demand, so the work is bounded no matter what a
/// caller asks for.
const FAVICON_SIZES: [u32; 3] = [32, 180, 512];

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FaviconQuery {
    pub org_id: Option<Uuid>,
    pub slug: Option<String>,
    /// Rounded up to 32, 180 or 512. Defaults to 180 — big enough for a home
    /// screen, and a browser downsamples for the tab perfectly well.
    pub size: Option<u32>,
}

/// The shop's own logo, as a favicon.
///
/// Square, opaque, and on the shop's own ground — the same treatment the
/// wallet badge gets, and for the same reason: a browser tab and an iOS home
/// screen both draw this against a background they choose, so a mark on
/// transparency is a coin flip and a wide wordmark cropped to a square loses
/// the shop's name. [`crate::orgs::branding::on_ground`] fits the artwork
/// whole and centres it, which is why a wordmark reads as a band rather than
/// as two letters.
///
/// The inset is wider than the wallet's. Nothing masks a favicon to a circle,
/// so there is no reason to leave the corners empty.
///
/// A shop with no logo gets a 404, and the page falls back to whatever icon it
/// shipped with — Madar's. That is the honest answer: this endpoint serves a
/// shop's logo, and there isn't one.
#[utoipa::path(get, path = "/public/orgs/favicon", tag = "orgs",
    operation_id = "public_org_favicon", params(FaviconQuery),
    responses((status = 200, description = "Square PNG", content_type = "image/png"),
              AppErrorResponse))]
pub async fn favicon(
    pool: web::Data<PgPool>,
    query: web::Query<FaviconQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = resolve_org(
        pool.get_ref(),
        &BrandQuery {
            org_id: query.org_id,
            slug: query.slug.clone(),
        },
    )
    .await?;
    let want = query.size.unwrap_or(180);
    let size = FAVICON_SIZES
        .into_iter()
        .find(|s| *s >= want)
        .unwrap_or(FAVICON_SIZES[FAVICON_SIZES.len() - 1]);

    let brand = crate::orgs::branding::load(pool.get_ref(), org_id).await?;
    let logo = brand
        .logo_url
        .as_deref()
        .and_then(crate::orgs::branding::read_upload);
    let logo = logo.ok_or_else(|| AppError::NotFound("That shop has no logo".into()))?;
    let tint = brand
        .logo_is_mark
        .then_some(brand.palette.foreground.as_str());
    let img = crate::orgs::branding::on_ground(&logo, &brand.palette.background, tint, size, 0.82);

    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|_| AppError::Internal)?;
    Ok(HttpResponse::Ok()
        .content_type("image/png")
        // An hour, not a year. The URL carries no version of the file — it
        // cannot, a favicon is requested by a fixed address — so a shop that
        // changes its logo has to be able to see the change the same day
        // rather than never.
        .insert_header(("Cache-Control", "public, max-age=3600"))
        .body(buf.into_inner()))
}
