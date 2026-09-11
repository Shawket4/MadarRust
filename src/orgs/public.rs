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
    pub slug: String,
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
    let org_id = match (
        query.org_id,
        query
            .slug
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    ) {
        (Some(id), _) => id,
        (None, Some(slug)) => {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM organizations \
                  WHERE slug = $1 AND is_active AND deleted_at IS NULL",
            )
            .bind(slug.trim().to_lowercase())
            .fetch_optional(pool.get_ref())
            .await?
            // Deliberately the same answer as a shop that does not exist. A
            // wildcard subdomain answers for every name anyone types, and
            // distinguishing "no such shop" from "that shop is switched off"
            // would make this an enumeration tool for our whole customer list.
            .ok_or_else(|| AppError::NotFound("No shop at that address".into()))?
        }
        // Including `?slug=`, which is not a shop that might exist — it is no
        // name at all. It used to MATCH: an organisation carrying the legacy
        // empty-string slug is precisely the one meant to have no address of
        // its own, and a blank query handed it back, branding and all. No
        // hostname can produce it (a first label is never empty), so this was
        // never a wildcard enumeration hole — but a public endpoint should not
        // answer a question nobody asked.
        (None, None) => {
            return Err(AppError::BadRequest(
                "Name the shop, by id or by short name".into(),
            ));
        }
    };

    let org = crate::orgs::branding::load(pool.get_ref(), org_id).await?;
    let slug: String = sqlx::query_scalar("SELECT slug FROM organizations WHERE id = $1")
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
