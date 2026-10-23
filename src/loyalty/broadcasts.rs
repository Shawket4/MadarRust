//! Messages to members' cards from the dashboard (REWARDS_DASHBOARD_DESIGN.md §4).
//!
//! A broadcast is many [`notices::announce_with`] calls — the existing
//! delivery path, not a parallel one: the line rides on the card (Apple: a
//! field with `changeMessage`; Google: a message), WhatsApp only when the
//! sender asked for it.
//!
//! Guards (madar-card `message`, both sides agree): one line of at most 100
//! characters per language, no links; at most 3 per shop in any 24 hours; a
//! member gets at most one in 20 hours (later ones skip them); scheduled 5
//! minutes to 30 days ahead; never to a member who opted out of marketing; a
//! capability of its own (`loyalty.broadcast`).
//!
//! Sending runs in the background, 200 recipients per pass: "send now" starts
//! a pass at once, and the refresh tick (every 5 minutes) starts scheduled ones
//! and finishes long ones. The audience is resolved when sending starts.
//!
//! [`notices::announce_with`]: super::wallet::notices::announce_with

use actix_web::{HttpRequest, HttpResponse, web};
use madar_card::audience::Audience;
use madar_card::message::{LIMITS, Limits, Message};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::errors::{AppError, AppErrorResponse};

pub const CODE_MESSAGE_INVALID: &str = "LOYALTY_MESSAGE_INVALID";
pub const CODE_DAILY_LIMIT: &str = "LOYALTY_MESSAGE_DAILY_LIMIT";
pub const CODE_SCHEDULE: &str = "LOYALTY_MESSAGE_SCHEDULE";
pub const CODE_NOBODY: &str = "LOYALTY_MESSAGE_NOBODY";
pub const CODE_NOT_CANCELLABLE: &str = "LOYALTY_MESSAGE_NOT_CANCELLABLE";

/// Recipients handled per pass.
const BATCH: i64 = 200;

#[derive(Debug, Deserialize, ToSchema)]
pub struct AudienceRequest {
    #[serde(default)]
    pub audience: Audience,
}

/// Who an audience reaches, and how.
#[derive(Debug, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct AudienceCount {
    /// Members matching the rules (opted-out ones included in this figure).
    pub members: i64,
    /// Of those, opted out of marketing: never sent to.
    pub opted_out: i64,
    /// Reachable: holding an Apple card / a Google card / neither (WhatsApp
    /// only, and only if asked for).
    pub apple: i64,
    pub google: i64,
    pub no_card: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BroadcastInput {
    pub message: Message,
    #[serde(default)]
    pub audience: Audience,
    /// Absent = now. Otherwise 5 minutes to 30 days ahead.
    #[serde(default)]
    pub scheduled_for: Option<chrono::DateTime<chrono::Utc>>,
    /// Also WhatsApp members with no card in a wallet (costs the shop).
    #[serde(default)]
    pub whatsapp_fallback: bool,
}

/// What the log can honestly say about a broadcast.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Broadcast {
    pub id: Uuid,
    pub line_en: String,
    pub line_ar: String,
    #[sqlx(json)]
    pub audience: Audience,
    pub whatsapp_fallback: bool,
    /// `scheduled`, `sending`, `sent` or `cancelled`.
    pub status: String,
    pub scheduled_for: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_by_name: Option<String>,
    pub recipients: i64,
    pub pending: i64,
    pub sent: i64,
    /// Reached an Apple card (pushed).
    pub apple: i64,
    /// Accepted by Google (Google gives no delivery signal).
    pub google: i64,
    pub whatsapp: i64,
    /// Apple devices that fetched the card after the push: delivered.
    pub seen: i64,
    pub skipped_recent: i64,
    pub skipped_opted_out: i64,
    /// No card and no WhatsApp asked for.
    pub unreachable: i64,
    pub failed: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BroadcastsPage {
    pub items: Vec<Broadcast>,
    pub total: i64,
    /// Broadcasts in the last 24 hours (cancelled ones excluded), against `limits.daily_limit`.
    pub sent_last_24h: i64,
    pub limits: Limits,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BroadcastsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// A broadcast with its counters, the counters read per broadcast from the
/// recipients' primary key (broadcast_id, …) — never across every recipient.
const BROADCAST_SELECT: &str = "SELECT b.id, b.line_en, b.line_ar, b.audience, b.whatsapp_fallback, b.status, \
    b.scheduled_for, b.created_at, b.started_at, b.finished_at, u.name AS created_by_name, \
    k.recipients, k.pending, k.sent, k.apple, k.google, k.whatsapp, k.seen, k.skipped_recent, \
    k.skipped_opted_out, k.unreachable, k.failed \
  FROM loyalty_broadcasts b \
  LEFT JOIN users u ON u.id = b.created_by \
  CROSS JOIN LATERAL ( \
    SELECT count(r.customer_id) AS recipients, \
           count(*) FILTER (WHERE r.outcome = 'pending') AS pending, \
           count(*) FILTER (WHERE r.outcome = 'sent') AS sent, \
           count(*) FILTER (WHERE r.channel LIKE '%apple%') AS apple, \
           count(*) FILTER (WHERE r.channel LIKE '%google%') AS google, \
           count(*) FILTER (WHERE r.channel = 'whatsapp') AS whatsapp, \
           count(r.seen_at) AS seen, \
           count(*) FILTER (WHERE r.outcome = 'skipped_recent') AS skipped_recent, \
           count(*) FILTER (WHERE r.outcome = 'skipped_opted_out') AS skipped_opted_out, \
           count(*) FILTER (WHERE r.outcome = 'sent' AND r.channel = 'none') AS unreachable, \
           count(*) FILTER (WHERE r.outcome = 'failed') AS failed \
      FROM loyalty_broadcast_recipients r WHERE r.broadcast_id = b.id) k";

async fn may_read(req: &HttpRequest, pool: &PgPool) -> Result<Uuid, AppError> {
    let (org_id, claims) = super::settings::scope_org(pool, req, None).await?;
    crate::authz::require::require(pool, &claims, crate::authz::Cap::LoyaltyMembersList, None)
        .await?;
    Ok(org_id)
}

async fn may_send(req: &HttpRequest, pool: &PgPool) -> Result<(Uuid, Option<Uuid>), AppError> {
    let (org_id, claims) = super::settings::scope_org(pool, req, None).await?;
    crate::authz::require::require(pool, &claims, crate::authz::Cap::LoyaltyBroadcast, None)
        .await?;
    Ok((org_id, claims.user_id_safe().ok()))
}

/// The members an audience names, as `FROM … WHERE …` over `m`.
fn audience_from(
    qb: &mut QueryBuilder<'_, Postgres>,
    org_id: Uuid,
    audience: &Audience,
    mode: super::earn::Mode,
    sizes: &super::crm::Sizes,
) {
    qb.push(" FROM loyalty_memberships m JOIN customers c ON c.id = m.id WHERE m.org_id = ")
        .push_bind(org_id)
        .push(" AND m.deleted_at IS NULL AND c.erased_at IS NULL");
    super::crm::push_audience(qb, audience, mode, sizes);
}

async fn count(
    pool: &PgPool,
    org_id: Uuid,
    audience: &Audience,
) -> Result<AudienceCount, AppError> {
    let (mode, sizes, _, _) = super::crm::programme(pool, org_id).await?;
    super::crm::check_audience(audience, &sizes)?;
    let apple = "EXISTS (SELECT 1 FROM loyalty_pass_devices d WHERE d.customer_id = m.id)";
    let key = format!(
        "audience|{org_id}|{}",
        serde_json::to_string(audience).unwrap_or_default()
    );
    let cached = super::rewards_cache::stats(key, || async {
    let mut qb = QueryBuilder::<Postgres>::new(format!(
        "SELECT count(*) AS members, count(*) FILTER (WHERE c.marketing_opt_out) AS opted_out, \
                count(*) FILTER (WHERE NOT c.marketing_opt_out AND {apple}) AS apple, \
                count(*) FILTER (WHERE NOT c.marketing_opt_out AND m.google_object_id IS NOT NULL) AS google, \
                count(*) FILTER (WHERE NOT c.marketing_opt_out AND NOT {apple} AND m.google_object_id IS NULL) AS no_card"
    ));
    audience_from(&mut qb, org_id, audience, mode, &sizes);
    let n: AudienceCount = qb.build_query_as().fetch_one(pool).await?;
    Ok(std::sync::Arc::new(serde_json::to_value(n).map_err(|_| AppError::Internal)?))
    })
    .await?;
    // A minute-old count, like the CRM total (rewards_cache).
    serde_json::from_value((*cached).clone()).map_err(|_| AppError::Internal)
}

/// How many members an audience reaches, before sending.
#[utoipa::path(post, path = "/loyalty/broadcasts/audience", tag = "loyalty",
    operation_id = "count_loyalty_broadcast_audience", request_body = AudienceRequest,
    responses((status = 200, body = AudienceCount),
        (status = 409, description = "LOYALTY_AUDIENCE_INVALID"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn audience_count(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<AudienceRequest>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let org_id = may_read(&req, pool).await?;
    Ok(HttpResponse::Ok().json(count(pool, org_id, &body.audience).await?))
}

async fn one(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<Broadcast, AppError> {
    sqlx::query_as(&format!(
        "{BROADCAST_SELECT} WHERE b.id = $1 AND b.org_id = $2"
    ))
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("No such message".into()))
}

async fn sent_last_24h(pool: &PgPool, org_id: Uuid) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM loyalty_broadcasts WHERE org_id = $1 AND status <> 'cancelled' \
            AND COALESCE(started_at, scheduled_for) > now() - interval '24 hours' \
            AND COALESCE(started_at, scheduled_for) <= now() + interval '1 minute'",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await?)
}

/// Send now, or schedule.
#[utoipa::path(post, path = "/loyalty/broadcasts", tag = "loyalty", operation_id = "create_loyalty_broadcast",
    request_body = BroadcastInput,
    responses((status = 200, body = Broadcast),
        (status = 409, description = "LOYALTY_MESSAGE_INVALID, LOYALTY_AUDIENCE_INVALID, LOYALTY_MESSAGE_DAILY_LIMIT, LOYALTY_MESSAGE_SCHEDULE, LOYALTY_MESSAGE_NOBODY"),
        AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn create(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<BroadcastInput>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let (org_id, actor) = may_send(&req, pool).await?;
    let body = body.into_inner();
    let problems = madar_card::message::validate_message(&body.message);
    if !problems.is_empty() {
        return Err(AppError::Refused {
            code: CODE_MESSAGE_INVALID,
            reason: problems
                .iter()
                .map(|p| format!("{} ({})", p.code, p.path))
                .collect::<Vec<_>>()
                .join(", "),
        });
    }
    let reach = count(pool, org_id, &body.audience).await?;
    if reach.members - reach.opted_out <= 0 {
        return Err(AppError::Refused {
            code: CODE_NOBODY,
            reason: "Nobody in that audience can be sent a message".into(),
        });
    }
    let now = chrono::Utc::now();
    let when = match body.scheduled_for {
        None => now,
        Some(t) => {
            if !madar_card::message::schedule_ok((t - now).num_minutes()) {
                return Err(AppError::Refused {
                    code: CODE_SCHEDULE,
                    reason: format!(
                        "Schedule a message {} minutes to {} days ahead",
                        LIMITS.schedule_min_minutes, LIMITS.schedule_max_days
                    ),
                });
            }
            t
        }
    };
    // The daily limit counts what goes out in the 24 hours around a send: the
    // ones already sent or due now. A scheduled one is checked again when due.
    if body.scheduled_for.is_none() {
        let used = u32::try_from(sent_last_24h(pool, org_id).await?).unwrap_or(u32::MAX);
        if madar_card::message::may_send(used).is_err() {
            return Err(AppError::Refused {
                code: CODE_DAILY_LIMIT,
                reason: format!("A shop can send {} messages a day", LIMITS.daily_limit),
            });
        }
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO loyalty_broadcasts (org_id, line_en, line_ar, audience, whatsapp_fallback, scheduled_for, created_by) \
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id",
    )
    .bind(org_id)
    .bind(body.message.en.trim())
    .bind(body.message.ar.trim())
    .bind(serde_json::to_value(&body.audience).map_err(|_| AppError::Internal)?)
    .bind(body.whatsapp_fallback)
    .bind(when)
    .bind(actor)
    .fetch_one(pool)
    .await?;
    if body.scheduled_for.is_none() {
        let p = pool.clone();
        crate::db::spawn(async move {
            if let Err(e) = run_one(&p, id).await {
                tracing::warn!(broadcast = %id, error = %e, "loyalty: broadcast pass failed; the tick retries");
            }
        });
    }
    Ok(HttpResponse::Ok().json(one(pool, org_id, id).await?))
}

#[utoipa::path(get, path = "/loyalty/broadcasts", tag = "loyalty", operation_id = "list_loyalty_broadcasts",
    params(BroadcastsQuery), responses((status = 200, body = BroadcastsPage), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BroadcastsQuery>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let org_id = may_read(&req, pool).await?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);
    let items: Vec<Broadcast> = sqlx::query_as(&format!(
        "{BROADCAST_SELECT} WHERE b.org_id = $1 ORDER BY b.created_at DESC LIMIT $2 OFFSET $3"
    ))
    .bind(org_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM loyalty_broadcasts WHERE org_id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await?;
    Ok(HttpResponse::Ok().json(BroadcastsPage {
        items,
        total,
        sent_last_24h: sent_last_24h(pool, org_id).await?,
        limits: LIMITS,
    }))
}

#[utoipa::path(get, path = "/loyalty/broadcasts/{id}", tag = "loyalty", operation_id = "get_loyalty_broadcast",
    params(("id" = Uuid, Path)), responses((status = 200, body = Broadcast), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let org_id = may_read(&req, pool).await?;
    Ok(HttpResponse::Ok().json(one(pool, org_id, *id).await?))
}

/// Cancel a scheduled message (one already sending finishes).
#[utoipa::path(post, path = "/loyalty/broadcasts/{id}/cancel", tag = "loyalty",
    operation_id = "cancel_loyalty_broadcast", params(("id" = Uuid, Path)),
    responses((status = 200, body = Broadcast),
        (status = 409, description = "LOYALTY_MESSAGE_NOT_CANCELLABLE"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn cancel(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let (org_id, _) = may_send(&req, pool).await?;
    let done = sqlx::query(
        "UPDATE loyalty_broadcasts SET status = 'cancelled', cancelled_at = now() \
          WHERE id = $1 AND org_id = $2 AND status = 'scheduled'",
    )
    .bind(*id)
    .bind(org_id)
    .execute(pool)
    .await?
    .rows_affected();
    if done == 0 {
        one(pool, org_id, *id).await?;
        return Err(AppError::Refused {
            code: CODE_NOT_CANCELLABLE,
            reason: "Only a scheduled message can be cancelled".into(),
        });
    }
    Ok(HttpResponse::Ok().json(one(pool, org_id, *id).await?))
}

/// Start what is due and continue what is sending (the refresh tick). One
/// broadcast's failure is logged and skipped, never the tick's.
pub async fn run_due(pool: &PgPool) -> Result<(), AppError> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM loyalty_broadcasts WHERE status IN ('scheduled', 'sending') AND scheduled_for <= now() \
          ORDER BY scheduled_for LIMIT 20",
    )
    .fetch_all(pool)
    .await?;
    for id in due {
        if let Err(e) = run_one(pool, id).await {
            tracing::warn!(broadcast = %id, error = %e, "loyalty: broadcast pass failed");
        }
    }
    Ok(())
}

/// One pass of one broadcast: start it (resolve its audience) if scheduled,
/// then deliver up to [`BATCH`] pending recipients; mark it sent when none are
/// left. Safe to run twice at once: each recipient is claimed by an UPDATE.
pub async fn run_one(pool: &PgPool, id: Uuid) -> Result<(), AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        org_id: Uuid,
        line_en: String,
        line_ar: String,
        #[sqlx(json)]
        audience: Audience,
        whatsapp_fallback: bool,
        status: String,
    }
    let Some(b): Option<Row> = sqlx::query_as(
        "SELECT org_id, line_en, line_ar, audience, whatsapp_fallback, status FROM loyalty_broadcasts WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(());
    };
    if b.status == "scheduled" {
        // The daily limit, checked when it actually goes.
        let used = u32::try_from(sent_last_24h(pool, b.org_id).await?.saturating_sub(1))
            .unwrap_or(u32::MAX);
        if madar_card::message::may_send(used).is_err() {
            tracing::warn!(broadcast = %id, "loyalty: scheduled message over the daily limit; cancelled");
            sqlx::query("UPDATE loyalty_broadcasts SET status = 'cancelled', cancelled_at = now() WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await?;
            return Ok(());
        }
        // The flip and the audience in ONE transaction: a second runner waits
        // on the row, then finds it sending with its recipients in place.
        // Read before the transaction: never a second pooled connection while one is held.
        let (mode, sizes, _, _) = super::crm::programme(pool, b.org_id).await?;
        let mut tx = pool.begin().await?;
        let started = sqlx::query(
            "UPDATE loyalty_broadcasts SET status = 'sending', started_at = now() WHERE id = $1 AND status = 'scheduled'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if started == 1 {
            let mut qb = QueryBuilder::<Postgres>::new(
                "INSERT INTO loyalty_broadcast_recipients (broadcast_id, org_id, customer_id, outcome, processed_at) SELECT ",
            );
            qb.push_bind(id).push(
                ", m.org_id, m.id, CASE WHEN c.marketing_opt_out THEN 'skipped_opted_out' ELSE 'pending' END, \
                 CASE WHEN c.marketing_opt_out THEN now() END",
            );
            audience_from(&mut qb, b.org_id, &b.audience, mode, &sizes);
            qb.push(" ON CONFLICT DO NOTHING");
            qb.build().execute(&mut *tx).await?;
        }
        tx.commit().await?;
    } else if b.status != "sending" {
        return Ok(());
    }

    // A runner that died mid-send left its claims stamped and pending: after
    // ten minutes they are anyone's again (at worst one member hears twice).
    sqlx::query(
        "UPDATE loyalty_broadcast_recipients SET processed_at = NULL \
          WHERE broadcast_id = $1 AND outcome = 'pending' AND processed_at < now() - interval '10 minutes'",
    )
    .bind(id)
    .execute(pool)
    .await?;
    let batch: Vec<Uuid> = sqlx::query_scalar(
        "SELECT customer_id FROM loyalty_broadcast_recipients \
          WHERE broadcast_id = $1 AND outcome = 'pending' AND processed_at IS NULL \
          ORDER BY customer_id LIMIT $2",
    )
    .bind(id)
    .bind(BATCH)
    .fetch_all(pool)
    .await?;
    let message = Message {
        en: b.line_en.clone(),
        ar: b.line_ar.clone(),
    };
    for customer in batch {
        // Claimed first (stamped, still pending), so a second runner never
        // sends it again and the broadcast stays `sending` until it lands.
        let claimed = sqlx::query(
            "UPDATE loyalty_broadcast_recipients SET processed_at = now() \
              WHERE broadcast_id = $1 AND customer_id = $2 AND outcome = 'pending' AND processed_at IS NULL",
        )
        .bind(id)
        .bind(customer)
        .execute(pool)
        .await?
        .rows_affected();
        if claimed == 0 {
            continue;
        }
        let recent: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM loyalty_broadcast_recipients \
              WHERE customer_id = $1 AND broadcast_id <> $2 AND outcome = 'sent' \
                AND processed_at > now() - make_interval(hours => $3))",
        )
        .bind(customer)
        .bind(id)
        .bind(i32::try_from(LIMITS.member_gap_hours).unwrap_or(20))
        .fetch_one(pool)
        .await?;
        let (outcome, channel) = if recent {
            ("skipped_recent", None)
        } else {
            match super::model::find_by_id(pool, customer).await? {
                Some(member) if !member.marketing_opt_out => {
                    let line = message.line_for(member.locale == "ar").to_string();
                    let whatsapp = b.whatsapp_fallback.then(|| line.clone());
                    match super::wallet::notices::announce_with(
                        pool,
                        &member,
                        &line,
                        whatsapp.as_deref(),
                        Some(id),
                    )
                    .await
                    {
                        Ok(ch) => ("sent", Some(ch)),
                        Err(e) => {
                            tracing::warn!(broadcast = %id, customer_id = %customer, error = %e, "loyalty: message not delivered");
                            ("failed", None)
                        }
                    }
                }
                Some(_) => ("skipped_opted_out", None),
                None => ("failed", None),
            }
        };
        sqlx::query(
            "UPDATE loyalty_broadcast_recipients SET outcome = $3, channel = $4, processed_at = now() \
              WHERE broadcast_id = $1 AND customer_id = $2",
        )
        .bind(id)
        .bind(customer)
        .bind(outcome)
        .bind(channel)
        .execute(pool)
        .await?;
    }
    sqlx::query(
        "UPDATE loyalty_broadcasts SET status = 'sent', finished_at = now() \
          WHERE id = $1 AND status = 'sending' \
            AND NOT EXISTS (SELECT 1 FROM loyalty_broadcast_recipients WHERE broadcast_id = $1 AND outcome = 'pending')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}
