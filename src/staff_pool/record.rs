//! Recording a staff drink — live, and again at replay.
//!
//! Both paths land in [`record_inner`], which is why it takes no headers: the
//! offline till drains through `/sync/replay` and must be decided by exactly the
//! code the live route runs, or the two would disagree about the same drink.
//!
//! ## The server re-decides, and accepts anyway
//!
//! The till decided from ITS copy of today's pool. The server holds the true
//! count, so it recomputes `used` from the rows and re-runs the shared engine.
//! When the two disagree the server's verdict is what is stored — but it never
//! rejects a drink that already happened. That is the locked accept-and-flag
//! rule for money ops (PERMISSIONS_ARCHITECTURE §4.4.5): the drink was made and
//! the sale was rung, so refusing the record now would only lose the evidence.
//!
//! The ONE exception is a blank note. A staff drink without a note cannot be
//! stored at all — the column is `NOT NULL CHECK (btrim(note) <> '')` — and a
//! client sending one is broken or ancient, not a teller doing their job. That
//! is refused, loudly, on both paths.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};

use super::engine::{self, StaffDrinkRefusal};
use super::settings::load_effective;

/// The body both the live route and the replay op carry.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecordStaffDrinkRequest {
    /// Client-minted, and the idempotency key: replaying the same drink twice
    /// is the same row, not a second one off the allowance.
    pub id: Uuid,
    pub branch_id: Uuid,
    #[serde(default)]
    pub till_id: Option<Uuid>,
    /// The zero-priced sale this drink rang as, when there is one.
    #[serde(default)]
    pub order_id: Option<Uuid>,
    pub menu_item_id: Uuid,
    /// Frozen at the till, so a later rename never rewrites history.
    #[serde(default)]
    pub item_name: Option<String>,
    #[serde(default)]
    pub size_label: Option<String>,
    #[serde(default = "one")]
    pub quantity: i32,
    /// REQUIRED. Who the drink is for and why, in the teller's own words.
    pub note: String,
    /// What the DEVICE believed the pool stood at. Kept for the owner to
    /// compare against what the server recomputed; never trusted.
    #[serde(default)]
    pub allowance_at_record: Option<i32>,
    #[serde(default)]
    pub used_before: Option<i32>,
    #[serde(default)]
    pub overspent: Option<bool>,
    #[serde(default)]
    pub cost_minor: Option<i32>,
    #[serde(default)]
    pub device_id: Option<Uuid>,
    /// When the teller rang it. Defaults to now; the business day is derived
    /// from this in the BRANCH's timezone, never from the server's clock date.
    #[serde(default)]
    pub recorded_at: Option<DateTime<Utc>>,
}

fn one() -> i32 {
    1
}

/// One recorded staff drink, as every reader sees it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct StaffDrink {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub order_id: Option<Uuid>,
    pub menu_item_id: Option<Uuid>,
    pub item_name: String,
    pub size_label: Option<String>,
    pub quantity: i32,
    pub note: String,
    pub business_date: NaiveDate,
    pub allowance_at_record: i32,
    pub used_before: i32,
    /// Past the allowance, as the SERVER recounted it.
    pub overspent: bool,
    /// The server made it an overspend and the till had not.
    pub overspent_on_replay: bool,
    pub cost_minor: Option<i32>,
    pub recorded_by: Option<Uuid>,
    pub recorded_at: DateTime<Utc>,
    /// What the pool comped on the sale's line, minor units, as the SERVER
    /// prices it. `null` on a record-only drink (no priced line behind it).
    #[sqlx(default)]
    #[serde(default)]
    pub comp_minor: Option<i32>,
    /// What that line was still charged: a bigger size, extras, pricier picks.
    #[sqlx(default)]
    #[serde(default)]
    pub extras_minor: Option<i32>,
    /// What the TILL said the comp was, on a replayed sale. Differs from
    /// `comp_minor` exactly when `orders.staff_drink.record:comp_mismatch` was
    /// flagged.
    #[sqlx(default)]
    #[serde(default)]
    pub comp_minor_reported: Option<i32>,
}

/// The outcome of recording one drink.
pub struct Recorded {
    pub drink: StaffDrink,
    /// Divergences the owner should see. Empty when the till and the server
    /// agreed. Each is a `capability:detail` token, the vocabulary
    /// `authz_replay_flags` already speaks.
    pub flags: Vec<String>,
    /// The row already existed: this was a retry, and nothing was spent.
    pub deduplicated: bool,
}

/// How many drinks a branch has already recorded on a business day. THE count
/// the pool is measured against — every till of the branch, every device,
/// whatever rang them.
pub async fn used_on<'e, E>(exec: E, branch_id: Uuid, day: NaiveDate) -> Result<i32, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let n: Option<i64> = sqlx::query_scalar(
        "SELECT sum(quantity)::bigint FROM staff_drinks WHERE branch_id = $1 AND business_date = $2",
    )
    .bind(branch_id)
    .bind(day)
    .fetch_one(exec)
    .await?;
    Ok(n.unwrap_or(0) as i32)
}

/// Record one staff drink. Shared by the live route and `/sync/replay`.
///
/// `actor` is who rang it — never who drank it. `from_replay` only changes how
/// loudly a divergence is reported, never whether the drink lands.
pub async fn record_inner(
    pool: &PgPool,
    org_id: Uuid,
    actor: Option<Uuid>,
    req: &RecordStaffDrinkRequest,
    from_replay: bool,
) -> Result<Recorded, AppError> {
    // A blank note is the one refusal that survives everything. The column
    // cannot hold one and the note is the entire record of who drank it.
    if !engine::note_is_given(&req.note) {
        return Err(AppError::BadRequest(
            StaffDrinkRefusal::NoteRequired.message().to_string(),
        ));
    }
    if req.quantity <= 0 {
        return Err(AppError::BadRequest(
            "A staff drink is at least one drink".into(),
        ));
    }

    // Idempotency first: a retry must never spend a second drink.
    if let Some(existing) = fetch(pool, req.id).await? {
        return Ok(Recorded { drink: existing, flags: Vec::new(), deduplicated: true });
    }

    let recorded_at = req.recorded_at.unwrap_or_else(Utc::now);
    // THE business day, drawn by the branch, not by UTC and not by the till.
    let tz = crate::tz::effective_tz(pool, req.branch_id).await?;
    let day = engine::business_date_of(tz, recorded_at);

    let settings = load_effective(pool, org_id, req.branch_id).await?;
    let used = used_on(pool, req.branch_id, day).await?;
    let decision = engine::decide(
        &settings.for_engine(),
        &day.to_string(),
        &req.menu_item_id.to_string(),
        &req.note,
        used,
    );

    let mut flags = Vec::new();
    let cap = Cap::OrdersStaffDrinkRecord.key();

    if let Some(refusal) = decision.refusal {
        if !from_replay {
            // Nothing has happened yet on the live path: an honest refusal.
            return Err(AppError::BadRequest(refusal.message().to_string()));
        }
        // The drink was already made. It lands, and the owner is told why it
        // should not have.
        flags.push(format!("{cap}:{}", refusal.token()));
    }

    // The server's verdict wins. A refused-but-accepted drink is by definition
    // outside the rules, so it counts as an overspend for the owner's eye.
    let overspent = decision.overspent || decision.refusal.is_some();
    let device_said = req.overspent.unwrap_or(false);
    let overspent_on_replay = overspent && !device_said;
    if overspent_on_replay {
        flags.push(format!("{cap}:overspent"));
    }
    // The till thought it was over and the server does not: also worth seeing,
    // because it means two devices disagreed about the day's count.
    if device_said && !overspent {
        flags.push(format!("{cap}:device_overcounted"));
    }

    let item_name = req
        .item_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "Staff drink".to_string());

    let drink: Option<StaffDrink> = sqlx::query_as(
        "INSERT INTO staff_drinks \
           (id, org_id, branch_id, till_id, order_id, menu_item_id, item_name, size_label, \
            quantity, note, business_date, allowance_at_record, used_before, overspent, \
            overspent_on_replay, cost_minor, recorded_by, device_id, recorded_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19) \
         ON CONFLICT (id) DO NOTHING \
         RETURNING id, branch_id, order_id, menu_item_id, item_name, size_label, quantity, note, \
                   business_date, allowance_at_record, used_before, overspent, overspent_on_replay, \
                   cost_minor, recorded_by, recorded_at, comp_minor, extras_minor, comp_minor_reported",
    )
    .bind(req.id)
    .bind(org_id)
    .bind(req.branch_id)
    .bind(req.till_id)
    .bind(req.order_id)
    .bind(req.menu_item_id)
    .bind(&item_name)
    .bind(&req.size_label)
    .bind(req.quantity)
    .bind(req.note.trim())
    .bind(day)
    .bind(settings.daily_allowance)
    .bind(used)
    .bind(overspent)
    .bind(overspent_on_replay)
    .bind(req.cost_minor)
    .bind(actor)
    .bind(req.device_id)
    .bind(recorded_at)
    .fetch_optional(pool)
    .await?;

    match drink {
        Some(d) => Ok(Recorded { drink: d, flags, deduplicated: false }),
        // Another connection won the race on the same client-minted id: that is
        // the same drink, not a second one.
        None => {
            let existing = fetch(pool, req.id)
                .await?
                .ok_or_else(|| AppError::Internal)?;
            Ok(Recorded { drink: existing, flags: Vec::new(), deduplicated: true })
        }
    }
}

async fn fetch(pool: &PgPool, id: Uuid) -> Result<Option<StaffDrink>, AppError> {
    Ok(sqlx::query_as(
        "SELECT id, branch_id, order_id, menu_item_id, item_name, size_label, quantity, note, \
                business_date, allowance_at_record, used_before, overspent, overspent_on_replay, \
                cost_minor, recorded_by, recorded_at, comp_minor, extras_minor, comp_minor_reported \
           FROM staff_drinks WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

// ── The pool, as a till or the dashboard asks for it ────────────────────────

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StaffPoolToday {
    pub branch_id: Uuid,
    pub business_date: NaiveDate,
    pub enabled: bool,
    pub allowance: i32,
    pub used: i32,
    pub remaining: i32,
    pub over: i32,
    pub eligible_item_ids: Vec<Uuid>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PoolQuery {
    pub branch_id: Uuid,
    /// The business day to ask about. Defaults to the branch's today.
    #[serde(default)]
    pub business_date: Option<NaiveDate>,
}

#[utoipa::path(get, path = "/staff-pool/today", tag = "staff_pool",
    operation_id = "get_staff_pool_today", params(PoolQuery),
    responses((status = 200, body = StaffPoolToday), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_today(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<PoolQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = crate::orgs::handlers::extract_claims(&req)?;
    let org_id = crate::loyalty::resolve_branch_org(pool.get_ref(), query.branch_id).await?;
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        Cap::OrdersStaffDrinkRecord,
        Some(query.branch_id),
    )
    .await?;
    crate::delivery::require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let tz = crate::tz::effective_tz(pool.get_ref(), query.branch_id).await?;
    let day = query
        .business_date
        .unwrap_or_else(|| engine::business_date_of(tz, Utc::now()));
    let settings = load_effective(pool.get_ref(), org_id, query.branch_id).await?;
    let used = used_on(pool.get_ref(), query.branch_id, day).await?;
    let state = engine::pool_state(&day.to_string(), settings.daily_allowance, used);

    Ok(HttpResponse::Ok().json(StaffPoolToday {
        branch_id: query.branch_id,
        business_date: day,
        // The engine's own view of "on": the switch AND something to spend on.
        enabled: settings.enabled && !settings.eligible_item_ids.is_empty(),
        allowance: state.allowance,
        used: state.used,
        remaining: state.remaining,
        over: state.over,
        eligible_item_ids: settings.eligible_item_ids,
    }))
}

#[utoipa::path(post, path = "/staff-pool/drinks", tag = "staff_pool",
    operation_id = "record_staff_drink", request_body = RecordStaffDrinkRequest,
    responses((status = 201, body = StaffDrink), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn record(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<RecordStaffDrinkRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = crate::orgs::handlers::extract_claims(&req)?;
    let body = body.into_inner();
    let org_id = crate::loyalty::resolve_branch_org(pool.get_ref(), body.branch_id).await?;
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        Cap::OrdersStaffDrinkRecord,
        Some(body.branch_id),
    )
    .await?;
    crate::delivery::require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    let actor = Some(claims.user_id());
    let done = record_inner(pool.get_ref(), org_id, actor, &body, false).await?;
    let status = if done.deduplicated {
        actix_web::http::StatusCode::OK
    } else {
        actix_web::http::StatusCode::CREATED
    };
    Ok(HttpResponse::build(status).json(done.drink))
}

// ── The report: the drinks themselves, with their notes ─────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    pub branch_id: Uuid,
    /// Business days, inclusive. Both default to the branch's today.
    #[serde(default)]
    pub from: Option<NaiveDate>,
    #[serde(default)]
    pub to: Option<NaiveDate>,
    /// Only the drinks that went past the allowance.
    #[serde(default)]
    pub overspent_only: Option<bool>,
}

/// The staff drinks of a branch over a range of business days, newest first.
///
/// This is the whole point of the note: with no "who is this for" field by
/// design, the note is the only record of who drank it, and this is where an
/// owner reads it.
#[utoipa::path(get, path = "/staff-pool/drinks", tag = "staff_pool",
    operation_id = "list_staff_drinks", params(ListQuery),
    responses((status = 200, body = Vec<StaffDrink>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = crate::orgs::handlers::extract_claims(&req)?;
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        Cap::OrdersStaffDrinkRecord,
        Some(query.branch_id),
    )
    .await?;
    crate::delivery::require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let tz = crate::tz::effective_tz(pool.get_ref(), query.branch_id).await?;
    let today = engine::business_date_of(tz, Utc::now());
    let from = query.from.unwrap_or(today);
    let to = query.to.unwrap_or(today);
    if to < from {
        return Err(AppError::BadRequest(
            "The end of the range comes before its start".into(),
        ));
    }

    let drinks: Vec<StaffDrink> = sqlx::query_as(
        "SELECT id, branch_id, order_id, menu_item_id, item_name, size_label, quantity, note, \
                business_date, allowance_at_record, used_before, overspent, overspent_on_replay, \
                cost_minor, recorded_by, recorded_at, comp_minor, extras_minor, comp_minor_reported \
           FROM staff_drinks \
          WHERE branch_id = $1 AND business_date BETWEEN $2 AND $3 \
            AND ($4::boolean IS NOT TRUE OR overspent) \
          ORDER BY business_date DESC, recorded_at DESC, id",
    )
    .bind(query.branch_id)
    .bind(from)
    .bind(to)
    .bind(query.overspent_only)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(drinks))
}

// ── The range, added up ─────────────────────────────────────────────────────

/// What the staff pool gave away and took in over a range of business days.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct StaffDrinksSummary {
    /// Rows (lines put on the pool).
    pub drinks: i64,
    /// Drinks (the sum of their quantities) — what the allowance is measured in.
    pub quantity: i64,
    /// Of those rows, how many went past the allowance.
    pub overspent: i64,
    /// What the pool comped, minor units (server-priced).
    pub comp_minor: i64,
    /// What those lines were still charged — the only part that is revenue.
    pub extras_minor: i64,
    /// What the drinks cost to make, where known. Counts in FULL: the drink
    /// was made whether or not anyone paid for it.
    pub cost_minor: i64,
    /// Rows whose till-reported comp differs from the server's.
    pub comp_mismatches: i64,
    /// Record-only rows (no priced sale line behind them; POS ≤ v0.7.12).
    pub unpriced: i64,
}

/// Totals for the same range and filter as `GET /staff-pool/drinks`.
#[utoipa::path(get, path = "/staff-pool/drinks/summary", tag = "staff_pool",
    operation_id = "summarize_staff_drinks", params(ListQuery),
    responses((status = 200, body = StaffDrinksSummary), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn summary(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = crate::orgs::handlers::extract_claims(&req)?;
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        Cap::OrdersStaffDrinkRecord,
        Some(query.branch_id),
    )
    .await?;
    crate::delivery::require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let tz = crate::tz::effective_tz(pool.get_ref(), query.branch_id).await?;
    let today = engine::business_date_of(tz, Utc::now());
    let from = query.from.unwrap_or(today);
    let to = query.to.unwrap_or(today);
    if to < from {
        return Err(AppError::BadRequest(
            "The end of the range comes before its start".into(),
        ));
    }

    let totals: StaffDrinksSummary = sqlx::query_as(
        "SELECT count(*)::bigint AS drinks, \
                COALESCE(sum(quantity), 0)::bigint AS quantity, \
                count(*) FILTER (WHERE overspent)::bigint AS overspent, \
                COALESCE(sum(comp_minor), 0)::bigint AS comp_minor, \
                COALESCE(sum(extras_minor), 0)::bigint AS extras_minor, \
                COALESCE(sum(cost_minor), 0)::bigint AS cost_minor, \
                count(*) FILTER (WHERE comp_minor_reported IS NOT NULL \
                                   AND comp_minor_reported IS DISTINCT FROM comp_minor)::bigint \
                    AS comp_mismatches, \
                count(*) FILTER (WHERE comp_minor IS NULL)::bigint AS unpriced \
           FROM staff_drinks \
          WHERE branch_id = $1 AND business_date BETWEEN $2 AND $3 \
            AND ($4::boolean IS NOT TRUE OR overspent)",
    )
    .bind(query.branch_id)
    .bind(from)
    .bind(to)
    .bind(query.overspent_only)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(totals))
}
