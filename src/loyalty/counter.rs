//! The counter: a stamp card stamped, or completed, with no order (Rewards by Madar).
//!
//! A shop that sells loyalty without Madar's POS (plan Free + the `loyalty`
//! add-on) has no orders to earn from. Its teller page names the member and
//! either stamps their card or completes a full one. Same ledger, same branch,
//! same balances as order earning, so a shop that later moves to the paid plan
//! keeps every card as it is.
//!
//! Rules (owner, 2026-10-19):
//! - **Stamps only.** A counter visit earns stamps, never points: there is no
//!   bill to apply a rate to. A points programme is refused
//!   ([`CODE_STAMPS_ONLY`]).
//! - **How many.** The teller enters how many items were bought. The existing
//!   per-item rule decides what that is worth (`earn::points_for_order`): with
//!   "a stamp per item" on it is that many, off it is one. At most
//!   [`MAX_ITEMS`] in one visit.
//! - **No window.** The stamp is for a visit happening now; there is no sale to
//!   date it against.
//! - **Completing a card** deducts exactly the card's size (`reward_target`,
//!   what the pass counts towards); stamps past it carry over. A card that is
//!   not full is refused ([`CODE_CARD_NOT_FULL`]).
//! - **Once per key.** The page makes one key per attempt; a retry answers
//!   with the first outcome (`already: true`) instead of stamping twice.
//!
//! Online only: the page has no offline queue, so there is no replay path.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use super::earn::{Mode, OrderAmounts, OrderLine};
use super::model::{self, EarnOrigin, MemberView};
use super::settings::{load_effective, load_effective_rewards};
use super::{resolve_branch_org, wallet};
use crate::delivery::require_branch_access;
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::permissions::checker::check_permission;

/// The most items one counter visit may stamp for. A real visit is a handful;
/// this only stops a slipped finger from filling a card.
pub const MAX_ITEMS: i32 = 50;

pub const CODE_STAMPS_ONLY: &str = "LOYALTY_COUNTER_STAMPS_ONLY";
pub const CODE_CARD_NOT_FULL: &str = "LOYALTY_CARD_NOT_FULL";
pub const CODE_KEY_REUSED: &str = "LOYALTY_KEY_REUSED";
pub const CODE_CARD_AT_CAP: &str = "LOYALTY_CARD_AT_CAP";

#[derive(Debug, Deserialize, ToSchema)]
pub struct CounterStampRequest {
    pub branch_id: Uuid,
    /// The member, as `/loyalty/lookup` resolved them.
    pub customer_id: Uuid,
    /// Items bought on this visit, 1 to 50.
    #[schema(minimum = 1, maximum = 50, example = 1)]
    pub items: i32,
    /// One per attempt, made by the page (a UUID). A retry with the same key
    /// stamps nothing new.
    pub idempotency_key: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CounterRedeemRequest {
    pub branch_id: Uuid,
    pub customer_id: Uuid,
    /// One per attempt, made by the page (a UUID).
    pub idempotency_key: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CounterResult {
    /// The member after the change, with the card's figures.
    pub member: MemberView,
    /// Stamps this call moved: positive for a stamp, negative for a completed
    /// card. On a retry, what the first call moved.
    pub stamps: i32,
    /// True when this key had already been used: nothing new was written.
    pub already: bool,
}

fn refused(code: &'static str, reason: &str) -> AppError {
    AppError::Refused {
        code,
        reason: reason.into(),
    }
}

pub(super) fn valid_key(key: &str) -> Result<&str, AppError> {
    let k = key.trim();
    if (8..=64).contains(&k.len()) && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(k)
    } else {
        Err(AppError::BadRequest(
            "idempotency_key: 8 to 64 letters, digits or dashes".into(),
        ))
    }
}

/// A key already used in this org: the row it wrote, if it was for this
/// member and this act. Another member or act under the same key is a client
/// bug, refused rather than silently answered with someone else's outcome.
async fn prior(
    pool: &PgPool,
    org_id: Uuid,
    key: &str,
    customer_id: Uuid,
    kind: &str,
) -> Result<Option<i32>, AppError> {
    let row: Option<(Uuid, String, i32)> = sqlx::query_as(
        "SELECT customer_id, kind::text, points FROM loyalty_transactions
          WHERE org_id = $1 AND idempotency_key = $2",
    )
    .bind(org_id)
    .bind(key)
    .fetch_optional(pool)
    .await?;
    match row {
        None => Ok(None),
        Some((c, k, p)) if c == customer_id && k == kind => Ok(Some(p)),
        Some(_) => Err(refused(
            CODE_KEY_REUSED,
            "That request key was already used for something else",
        )),
    }
}

struct Ctx {
    org_id: Uuid,
    member: model::MemberRow,
    target: i32,
    /// The shop's balance ceiling, if any (trims a stamp, never fails it).
    cap: Option<i32>,
    settings: super::settings::LoyaltySettings,
}

/// Who is asking, and may they: `loyalty.use` (the teller's "scan cards,
/// award points and redeem rewards"). First, before any input is checked, so
/// a person without it gets 403 and learns nothing about what a valid request
/// looks like.
async fn authorize(
    req: &HttpRequest,
    pool: &PgPool,
) -> Result<crate::auth::jwt::Claims, AppError> {
    let claims = extract_claims(req)?;
    check_permission(pool, &claims, "loyalty", "update").await?;
    Ok(claims)
}

async fn context(
    claims: &crate::auth::jwt::Claims,
    pool: &PgPool,
    branch_id: Uuid,
    customer_id: Uuid,
) -> Result<(Ctx, Option<Uuid>), AppError> {
    require_branch_access(pool, claims, branch_id).await?;
    let org_id = resolve_branch_org(pool, branch_id).await?;
    let member = model::find_by_id(pool, customer_id)
        .await?
        .filter(|m| m.org_id == org_id)
        .ok_or_else(|| AppError::NotFound("No member for that card".into()))?;
    let settings = load_effective(pool, org_id, branch_id).await?;
    if !settings.enabled {
        return Err(refused(
            super::award::CODE_PROGRAM_OFF,
            "The loyalty program is switched off for this branch",
        ));
    }
    if settings.mode() != Mode::Visits {
        return Err(refused(
            CODE_STAMPS_ONLY,
            "The counter stamps cards; this programme collects points",
        ));
    }
    let (catalogue, _) = load_effective_rewards(pool, org_id, branch_id).await?;
    let target = model::reward_target(&settings, &catalogue);
    let cap = super::settings::effective_balance_cap(&settings, &catalogue);
    Ok((
        Ctx {
            org_id,
            member,
            target,
            cap,
            settings,
        },
        claims.user_id_safe().ok(),
    ))
}

async fn result(
    pool: &PgPool,
    ctx: &Ctx,
    stamps: i32,
    already: bool,
) -> Result<HttpResponse, AppError> {
    let fresh = model::find_by_id(pool, ctx.member.id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member not found".into()))?;
    Ok(HttpResponse::Ok().json(CounterResult {
        member: fresh.view(Mode::Visits, ctx.target),
        stamps,
        already,
    }))
}

/// Stamp a member's card for a visit (no order).
#[utoipa::path(post, path = "/loyalty/counter/stamp", tag = "loyalty",
    operation_id = "loyalty_counter_stamp", request_body = CounterStampRequest,
    responses(
        (status = 200, body = CounterResult),
        (status = 409, description = "LOYALTY_PROGRAM_OFF, LOYALTY_COUNTER_STAMPS_ONLY, LOYALTY_CARD_AT_CAP, LOYALTY_KEY_REUSED"),
        AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn stamp(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CounterStampRequest>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let claims = authorize(&req, pool).await?;
    let key = valid_key(&body.idempotency_key)?;
    if !(1..=MAX_ITEMS).contains(&body.items) {
        return Err(AppError::BadRequest(format!(
            "items: 1 to {MAX_ITEMS} on one visit"
        )));
    }
    let (ctx, actor) = context(&claims, pool, body.branch_id, body.customer_id).await?;
    if let Some(p) = prior(pool, ctx.org_id, key, ctx.member.id, "earn").await? {
        return result(pool, &ctx, p, true).await;
    }

    // The visit as the stamp rule sees a sale: one line of `items` units, no
    // item named (an empty eligible list, so every unit counts).
    let lines = [OrderLine {
        menu_item_id: Uuid::nil(),
        quantity: body.items,
        redeemed_units: 0,
    }];
    let rule = ctx.settings.rule();
    let mut tx = pool.begin().await?;
    let stamps = model::award_for_order(
        &mut tx,
        ctx.org_id,
        body.branch_id,
        ctx.member.id,
        EarnOrigin::Counter {
            idempotency_key: key,
        },
        OrderAmounts {
            subtotal: 0,
            discount_amount: 0,
            tax_amount: 0,
        },
        &lines,
        &[],
        rule,
        true,
        ctx.cap,
        actor,
    )
    .await?;
    tx.commit().await?;

    if stamps == 0 {
        // Nothing written: a racing retry with this key won, or the card is at
        // the shop's ceiling.
        if let Some(p) = prior(pool, ctx.org_id, key, ctx.member.id, "earn").await? {
            return result(pool, &ctx, p, true).await;
        }
        return Err(refused(
            CODE_CARD_AT_CAP,
            "This card is at the shop's limit; complete it before stamping more",
        ));
    }
    wallet::push_update(pool, ctx.member.id);
    result(pool, &ctx, stamps, false).await
}

/// Complete a full card: deduct exactly the card's size; extra stamps stay.
#[utoipa::path(post, path = "/loyalty/counter/redeem", tag = "loyalty",
    operation_id = "loyalty_counter_redeem", request_body = CounterRedeemRequest,
    responses(
        (status = 200, body = CounterResult),
        (status = 409, description = "LOYALTY_PROGRAM_OFF, LOYALTY_COUNTER_STAMPS_ONLY, LOYALTY_CARD_NOT_FULL, LOYALTY_KEY_REUSED"),
        AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn redeem(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CounterRedeemRequest>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let claims = authorize(&req, pool).await?;
    let key = valid_key(&body.idempotency_key)?;
    let (ctx, actor) = context(&claims, pool, body.branch_id, body.customer_id).await?;
    if let Some(p) = prior(pool, ctx.org_id, key, ctx.member.id, "redeem").await? {
        return result(pool, &ctx, p, true).await;
    }
    let size = ctx.target;
    let mut tx = pool.begin().await?;
    // Locked, so two completions racing on one card cannot both pass the check.
    let balance: i32 = sqlx::query_scalar(
        "SELECT visits_balance FROM loyalty_memberships WHERE id = $1 FOR UPDATE",
    )
    .bind(ctx.member.id)
    .fetch_one(&mut *tx)
    .await?;
    if size <= 0 || balance < size {
        return Err(refused(
            CODE_CARD_NOT_FULL,
            &format!("This card has {balance} of {size} stamps; it is not full yet"),
        ));
    }
    let inserted = sqlx::query(
        "INSERT INTO loyalty_transactions
            (org_id, customer_id, branch_id, kind, currency, points, created_by, source,
             idempotency_key)
         VALUES ($1, $2, $3, 'redeem', 'visits', $4, $5, $6, $7)
         ON CONFLICT DO NOTHING",
    )
    .bind(ctx.org_id)
    .bind(ctx.member.id)
    .bind(body.branch_id)
    .bind(-size)
    .bind(actor)
    .bind(model::Source::Counter.as_str())
    .bind(key)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    if inserted == 0 {
        return match prior(pool, ctx.org_id, key, ctx.member.id, "redeem").await? {
            Some(p) => result(pool, &ctx, p, true).await,
            None => Err(AppError::Internal),
        };
    }
    wallet::push_update(pool, ctx.member.id);
    result(pool, &ctx, -size, false).await
}
