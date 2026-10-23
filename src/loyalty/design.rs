//! The card designer's API (REWARDS_DASHBOARD_DESIGN.md §3).
//!
//! The dashboard edits a DRAFT, checks it with madar-card in the browser (the
//! same `validate` this server runs before every write), previews it live and
//! as this server renders it, then PUBLISHES. Publishing bumps the revision,
//! clears the stored passes and marks every installed card stale, so the
//! existing sweep pushes the new look (200 per tick). Unpublishing returns the
//! shop to the classic look.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::tracks::TrackDef;
use super::wallet::design as wd;
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::check_permission;

pub const CODE_DESIGN_INVALID: &str = "LOYALTY_DESIGN_INVALID";
pub const CODE_NOTHING_TO_PUBLISH: &str = "LOYALTY_DESIGN_NO_DRAFT";

/// Everything the designer needs to open.
#[derive(Debug, Serialize, ToSchema)]
pub struct CardDesignView {
    /// The design being edited, if one was saved.
    pub draft: Option<madar_card::CardDesign>,
    /// The design the passes wear; `None` = the classic look (or, with
    /// several tracks, the automatic one).
    pub published: Option<madar_card::CardDesign>,
    pub revision: i32,
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The design a shop starts from (madar-card's default for it).
    pub default_design: madar_card::CardDesign,
    /// What designs are checked against: mode, tracks, photo.
    pub ctx: madar_card::Ctx,
    /// The tracks, for labels and colours in the preview.
    pub tracks: Vec<madar_card::Track>,
    /// The pass colours the shop has today.
    pub colours: madar_card::Colours,
    /// False: the shop wears Madar's palette (custom branding is a super
    /// admin's switch) — the designer locks the colours and the photograph.
    pub custom_branding: bool,
    pub programme_name: String,
    pub programme_name_ar: Option<String>,
    /// The shop's card photograph and logo (upload URLs) for the live preview.
    pub photo_url: Option<String>,
    pub logo_url: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PutCardDesign {
    pub design: madar_card::CardDesign,
}

/// Render a design as the wallet will get it.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CardPreviewRequest {
    pub design: madar_card::CardDesign,
    pub store: madar_card::Store,
    #[serde(default)]
    pub arabic: bool,
    /// Sample balances per track (`{position, balance}`); absent = half full.
    #[serde(default)]
    pub balances: Vec<PreviewBalance>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PreviewBalance {
    pub position: u8,
    pub balance: i32,
}

struct Shop {
    settings: super::settings::LoyaltySettings,
    brand: crate::orgs::branding::OrgBrand,
    defs: Vec<TrackDef>,
    track_one_size: i32,
}

async fn shop(pool: &sqlx::PgPool, org_id: uuid::Uuid) -> Result<Shop, AppError> {
    let settings = super::settings::load_scope(pool, org_id, None)
        .await?
        .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(org_id, None));
    let brand = crate::orgs::branding::load(pool, org_id).await?;
    let defs = super::tracks::load(pool, org_id, settings.mode()).await?;
    let rewards = super::settings::load_effective_rewards_org(pool, org_id).await?;
    let track_one_size = super::model::reward_target(&settings, &rewards);
    Ok(Shop {
        settings,
        brand,
        defs,
        track_one_size,
    })
}

impl Shop {
    fn ctx(&self) -> madar_card::Ctx {
        wd::ctx_of(
            &self.defs,
            self.settings.mode(),
            self.track_one_size,
            &self.brand,
        )
    }

    fn tracks(&self) -> Vec<madar_card::Track> {
        self.defs.iter().map(TrackDef::to_card).collect()
    }

    /// The design's problems for this shop, refused as one sentence.
    fn check(&self, d: &madar_card::CardDesign) -> Result<(), AppError> {
        let problems = madar_card::validate(d, &self.ctx());
        if madar_card::validate::is_valid(&problems) {
            return Ok(());
        }
        Err(AppError::Refused {
            code: CODE_DESIGN_INVALID,
            reason: problems
                .iter()
                .filter(|p| p.severity == madar_card::Severity::Error)
                .map(|p| format!("{} ({})", p.code, p.path))
                .collect::<Vec<_>>()
                .join(", "),
        })
    }
}

async fn view(pool: &sqlx::PgPool, org_id: uuid::Uuid) -> Result<CardDesignView, AppError> {
    let s = shop(pool, org_id).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        draft: Option<serde_json::Value>,
        published: Option<serde_json::Value>,
        revision: i32,
        published_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT draft, published, revision, published_at FROM loyalty_card_designs WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let parse = |v: Option<serde_json::Value>| v.and_then(|j| serde_json::from_value(j).ok());
    let colours = wd::colours_of(&s.brand);
    let default_design = madar_card::templates::default_design(&colours, &s.ctx(), &s.tracks());
    Ok(CardDesignView {
        draft: row.as_ref().and_then(|r| parse(r.draft.clone())),
        published: row.as_ref().and_then(|r| parse(r.published.clone())),
        revision: row.as_ref().map_or(0, |r| r.revision),
        published_at: row.as_ref().and_then(|r| r.published_at),
        default_design,
        ctx: s.ctx(),
        tracks: s.tracks(),
        colours,
        custom_branding: s.brand.custom_branding,
        programme_name: s.settings.program_name.clone(),
        programme_name_ar: s.settings.program_name_ar.clone(),
        photo_url: s
            .brand
            .custom_branding
            .then(|| s.brand.card_image_url.clone())
            .flatten(),
        logo_url: s
            .brand
            .custom_branding
            .then(|| s.brand.logo_url.clone())
            .flatten(),
    })
}

#[utoipa::path(get, path = "/loyalty/card-design", tag = "loyalty", operation_id = "get_loyalty_card_design",
    responses((status = 200, body = CardDesignView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_design(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let (org_id, claims) = super::settings::scope_org(pool.get_ref(), &req, None).await?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    Ok(HttpResponse::Ok().json(view(pool.get_ref(), org_id).await?))
}

async fn may_design(req: &HttpRequest, pool: &sqlx::PgPool) -> Result<uuid::Uuid, AppError> {
    let (org_id, claims) = super::settings::scope_org(pool, req, None).await?;
    crate::authz::require::require(pool, &claims, crate::authz::Cap::LoyaltyCardDesign, None)
        .await?;
    Ok(org_id)
}

#[utoipa::path(put, path = "/loyalty/card-design", tag = "loyalty", operation_id = "put_loyalty_card_design",
    request_body = PutCardDesign,
    responses((status = 200, body = CardDesignView),
        (status = 409, description = "LOYALTY_DESIGN_INVALID: the madar-card problem codes, in the reason"),
        AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn put_design(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PutCardDesign>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let org_id = may_design(&req, pool).await?;
    let claims = crate::orgs::handlers::extract_claims(&req)?;
    let s = shop(pool, org_id).await?;
    let d = wd::enforce_branding(body.into_inner().design, &s.brand);
    s.check(&d)?;
    sqlx::query(
        "INSERT INTO loyalty_card_designs (org_id, draft, updated_at, updated_by) VALUES ($1, $2, now(), $3) \
         ON CONFLICT (org_id) DO UPDATE SET draft = EXCLUDED.draft, updated_at = now(), updated_by = EXCLUDED.updated_by",
    )
    .bind(org_id)
    .bind(serde_json::to_value(&d).map_err(|_| AppError::Internal)?)
    .bind(claims.user_id_safe().ok())
    .execute(pool)
    .await?;
    Ok(HttpResponse::Ok().json(view(pool, org_id).await?))
}

#[utoipa::path(post, path = "/loyalty/card-design/publish", tag = "loyalty",
    operation_id = "publish_loyalty_card_design",
    responses((status = 200, body = CardDesignView),
        (status = 409, description = "LOYALTY_DESIGN_NO_DRAFT, LOYALTY_DESIGN_INVALID"),
        AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn publish(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let org_id = may_design(&req, pool).await?;
    let claims = crate::orgs::handlers::extract_claims(&req)?;
    let s = shop(pool, org_id).await?;
    let draft: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT draft FROM loyalty_card_designs WHERE org_id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    let d: madar_card::CardDesign = draft
        .and_then(|j| serde_json::from_value(j).ok())
        .ok_or_else(|| AppError::Refused {
            code: CODE_NOTHING_TO_PUBLISH,
            reason: "Save a design before publishing it".into(),
        })?;
    // Checked again: the tracks or the photo may have changed since the save.
    s.check(&d)?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE loyalty_card_designs SET published = draft, revision = revision + 1, \
                published_at = now(), published_by = $2 WHERE org_id = $1",
    )
    .bind(org_id)
    .bind(claims.user_id_safe().ok())
    .execute(&mut *tx)
    .await?;
    super::wallet::mark_org_stale(&mut tx, org_id).await?;
    tx.commit().await?;
    super::wallet::store::purge_org(pool, org_id).await;
    Ok(HttpResponse::Ok().json(view(pool, org_id).await?))
}

#[utoipa::path(delete, path = "/loyalty/card-design/published", tag = "loyalty",
    operation_id = "unpublish_loyalty_card_design",
    responses((status = 200, body = CardDesignView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn unpublish(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let org_id = may_design(&req, pool).await?;
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "UPDATE loyalty_card_designs SET published = NULL, revision = revision + 1, published_at = NULL \
          WHERE org_id = $1 AND published IS NOT NULL",
    )
    .bind(org_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed > 0 {
        super::wallet::mark_org_stale(&mut tx, org_id).await?;
    }
    tx.commit().await?;
    if changed > 0 {
        super::wallet::store::purge_org(pool, org_id).await;
    }
    Ok(HttpResponse::Ok().json(view(pool, org_id).await?))
}

/// The strip (Apple @2x) or the hero (Google) exactly as the wallet will get
/// it, for the designer's "as the wallet renders it" view. Not saved.
#[utoipa::path(post, path = "/loyalty/card-design/preview", tag = "loyalty",
    operation_id = "preview_loyalty_card_design", request_body = CardPreviewRequest,
    responses((status = 200, description = "PNG", content_type = "image/png"),
        (status = 409, description = "LOYALTY_DESIGN_INVALID"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn preview(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CardPreviewRequest>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let org_id = may_design(&req, pool).await?;
    let s = shop(pool, org_id).await?;
    let body = body.into_inner();
    let design = wd::enforce_branding(body.design, &s.brand);
    s.check(&design)?;
    let mut balances = std::collections::HashMap::new();
    for d in &s.defs {
        let size = if d.position == 1 {
            s.track_one_size
        } else {
            d.size.unwrap_or(1)
        };
        let sample = body
            .balances
            .iter()
            .find(|b| b.position == d.position)
            .map_or(size / 2, |b| b.balance.clamp(0, 10_000));
        balances.insert(d.position, sample);
    }
    let card = wd::DesignedCard {
        org_id,
        revision: -1,
        design,
        defs: s.defs.clone(),
        mode: s.settings.mode(),
        track_one_size: s.track_one_size,
        programme: s.settings.program_name.clone(),
        programme_ar: s.settings.program_name_ar.clone(),
        photo_url: s
            .brand
            .custom_branding
            .then(|| s.brand.card_image_url.clone())
            .flatten(),
        logo_url: s
            .brand
            .custom_branding
            .then(|| s.brand.logo_url.clone())
            .flatten(),
        logo_is_mark: s.brand.logo_is_mark,
        balances,
        member_name: String::new(),
    };
    let (store, arabic) = (body.store, body.arabic);
    let png = tokio::task::spawn_blocking(move || {
        wd::render_png(
            &card,
            store,
            if store == madar_card::Store::Apple {
                2
            } else {
                1
            },
            arabic,
        )
    })
    .await
    .map_err(|_| AppError::Internal)?
    .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok()
        .content_type("image/png")
        .insert_header(("Cache-Control", "no-store"))
        .body(png))
}

/// The designed Google hero at one progress state (public: Google fetches it).
/// No member data: the org, the revision and each track's progress, every
/// number checked against the shop's tracks.
#[utoipa::path(get, path = "/public/loyalty/brand/{org_id}/card/{revision}/{state}", tag = "loyalty-public",
    operation_id = "public_loyalty_card_hero",
    params(("org_id" = uuid::Uuid, Path), ("revision" = i32, Path), ("state" = String, Path, description = "e.g. `3-0-en.png`")),
    responses((status = 200, description = "PNG", content_type = "image/png"), (status = 404)))]
pub async fn public_hero(
    pool: crate::db::PublicDb,
    path: web::Path<(uuid::Uuid, i32, String)>,
) -> Result<HttpResponse, AppError> {
    let (org_id, revision, state) = path.into_inner();
    let state = state.strip_suffix(".png").unwrap_or(&state).to_string();
    if state.len() > 32 || !state.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(AppError::NotFound("Not found".into()));
    }
    crate::plan::public_gate_org(pool.resolver(), org_id, madar_plan::Feature::Loyalty).await?;
    let pool = pool.scoped(org_id).await;
    let png = wd::hero_for_state(pool.get_ref(), org_id, revision, &state)
        .await
        .ok_or_else(|| AppError::NotFound("Not found".into()))?;
    Ok(HttpResponse::Ok()
        .content_type("image/png")
        .insert_header(("Cache-Control", "public, max-age=86400, immutable"))
        .body(png))
}
