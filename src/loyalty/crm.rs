//! The Rewards CRM: the members table with segments, a member's wallet, and
//! the programme's figures (REWARDS_DASHBOARD_DESIGN.md §5).
//!
//! Segments are madar-card `Audience`s — the same rules a dashboard message is
//! sent to, checked by the same code on both sides, turned into SQL here
//! ([`push_audience`]) and nowhere else.

use std::collections::HashMap;

use actix_web::{HttpRequest, HttpResponse, web};
use madar_card::audience::{Audience, Rule, Wallet};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, QueryBuilder};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::earn::Mode;
use super::tracks::MemberTrack;
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::check_permission;

pub const CODE_AUDIENCE_INVALID: &str = "LOYALTY_AUDIENCE_INVALID";

/// Each track's size, by position (track 1's is the programme's).
pub type Sizes = HashMap<u8, i32>;

/// A member's balance on `track`, as SQL over the row aliased `m`.
fn balance_sql(track: u8, mode: Mode) -> String {
    if track <= 1 {
        match mode {
            Mode::Points => "m.points_balance".into(),
            Mode::Visits => "m.visits_balance".into(),
        }
    } else {
        format!(
            "COALESCE((SELECT b.balance FROM loyalty_track_balances b WHERE b.customer_id = m.id AND b.position = {track}), 0)"
        )
    }
}

/// Track `track` (2–4) holds at least `value` (a value of 0 or less is everyone).
fn at_least(qb: &mut QueryBuilder<'_, Postgres>, track: u8, value: i32) {
    if value <= 0 {
        return;
    }
    qb.push(format!(
        " AND EXISTS (SELECT 1 FROM loyalty_track_balances b \
           WHERE b.customer_id = m.id AND b.position = {track} AND b.balance >= "
    ))
    .push_bind(value)
    .push(")");
}

/// When a member was last seen: their latest stamp, point or reward
/// (`loyalty_memberships.last_seen_at`, kept by the ledger trigger and indexed),
/// else when they joined.
pub const LAST_SEEN: &str = "m.last_seen_at";

/// `AND …` for every rule of `audience`, over `loyalty_memberships m JOIN customers c`. The rules
/// have been validated (madar-card) before this is called; numbers are bound.
pub fn push_audience(
    qb: &mut QueryBuilder<'_, Postgres>,
    audience: &Audience,
    mode: Mode,
    sizes: &Sizes,
) {
    for r in &audience.rules {
        match *r {
            // Tracks 2–4 as a semi-join on their own (org, position, balance)
            // index; a member with no row holds 0 on that track.
            Rule::BalanceAtLeast { track, value } if track > 1 => {
                at_least(qb, track, value);
            }
            Rule::RewardReady { track } if track > 1 => {
                let size = sizes.get(&track).copied().unwrap_or(i32::MAX).max(1);
                at_least(qb, track, size);
            }
            Rule::BalanceAtMost { track, value } if track > 1 => {
                qb.push(format!(
                    " AND NOT EXISTS (SELECT 1 FROM loyalty_track_balances b \
                       WHERE b.customer_id = m.id AND b.position = {track} AND b.balance > "
                ))
                .push_bind(value)
                .push(")");
            }
            Rule::BalanceAtLeast { track, value } => {
                qb.push(format!(" AND {} >= ", balance_sql(track, mode)))
                    .push_bind(value);
            }
            Rule::BalanceAtMost { track, value } => {
                qb.push(format!(" AND {} <= ", balance_sql(track, mode)))
                    .push_bind(value);
            }
            Rule::RewardReady { track } => {
                let size = sizes.get(&track).copied().unwrap_or(i32::MAX).max(1);
                qb.push(format!(" AND {} >= ", balance_sql(track, mode)))
                    .push_bind(size);
            }
            Rule::VisitedWithin { days } => {
                qb.push(format!(
                    " AND {LAST_SEEN} >= now() - make_interval(days => "
                ))
                .push_bind(i32::try_from(days).unwrap_or(365))
                .push(")");
            }
            Rule::NotVisitedFor { days } => {
                qb.push(format!(" AND {LAST_SEEN} < now() - make_interval(days => "))
                    .push_bind(i32::try_from(days).unwrap_or(365))
                    .push(")");
            }
            Rule::JoinedWithin { days } => {
                qb.push(" AND m.enrolled_at >= now() - make_interval(days => ")
                    .push_bind(i32::try_from(days).unwrap_or(365))
                    .push(")");
            }
            Rule::BirthdayMonth { month } => {
                qb.push(" AND c.birth_month = ").push_bind(i16::from(month));
            }
            Rule::HasWallet { wallet } => {
                let apple =
                    "EXISTS (SELECT 1 FROM loyalty_pass_devices d WHERE d.customer_id = m.id)";
                match wallet {
                    Wallet::Apple => qb.push(format!(" AND {apple}")),
                    Wallet::Google => qb.push(" AND m.google_object_id IS NOT NULL"),
                    Wallet::None => {
                        qb.push(format!(" AND NOT {apple} AND m.google_object_id IS NULL"))
                    }
                };
            }
        }
    }
}

/// The shop's mode, track sizes and track definitions.
pub async fn programme(
    pool: &sqlx::PgPool,
    org_id: Uuid,
) -> Result<(Mode, Sizes, Vec<super::tracks::TrackDef>, i32), AppError> {
    let settings = super::settings::load_scope(pool, org_id, None)
        .await?
        .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(org_id, None));
    let mode = settings.mode();
    let rewards = super::settings::load_effective_rewards_org(pool, org_id).await?;
    let one = super::model::reward_target(&settings, &rewards);
    let defs = super::tracks::load(pool, org_id, mode).await?;
    let mut sizes: Sizes = HashMap::new();
    for d in &defs {
        sizes.insert(
            d.position,
            if d.position == 1 {
                one
            } else {
                d.size.unwrap_or(1)
            },
        );
    }
    Ok((mode, sizes, defs, one))
}

/// An audience checked with madar-card, or a coded refusal.
pub fn check_audience(audience: &Audience, sizes: &Sizes) -> Result<(), AppError> {
    let problems =
        madar_card::audience::validate_audience(audience, u8::try_from(sizes.len()).unwrap_or(1));
    if problems.is_empty() {
        return Ok(());
    }
    Err(AppError::Refused {
        code: CODE_AUDIENCE_INVALID,
        reason: problems
            .iter()
            .map(|p| format!("{} ({})", p.code, p.path))
            .collect::<Vec<_>>()
            .join(", "),
    })
}

// ── The members table ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemberSort {
    #[default]
    Joined,
    LastVisit,
    Balance,
    Name,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct MembersQueryRequest {
    /// Name or phone fragment.
    #[serde(default)]
    pub q: Option<String>,
    /// A segment (madar-card's audience rules); empty = every member.
    #[serde(default)]
    pub audience: Audience,
    #[serde(default)]
    pub sort: MemberSort,
    /// Newest / most / latest first when true (the default).
    #[serde(default = "yes")]
    pub desc: bool,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

fn yes() -> bool {
    true
}

/// One row of the CRM table.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct CrmMember {
    pub id: Uuid,
    pub name: String,
    pub phone: String,
    /// Track 1's balance in the programme's currency.
    pub balance: i32,
    pub lifetime: i32,
    pub enrolled_at: chrono::DateTime<chrono::Utc>,
    /// Their latest stamp, point or reward (or when they joined).
    pub last_visit_at: chrono::DateTime<chrono::Utc>,
    /// Apple devices holding the card.
    pub apple_devices: i64,
    /// A Google Wallet object was created for them.
    pub google_card: bool,
    pub marketing_opt_out: bool,
    pub birth_month: Option<i16>,
    pub locale: String,
    /// Rewards earned on track 1, not yet taken.
    #[sqlx(default)]
    pub rewards_ready: i32,
    /// Every track, when the card has more than one.
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracks: Option<Vec<MemberTrack>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CrmMembersPage {
    pub rows: Vec<CrmMember>,
    pub total: i64,
}

fn needle(q: Option<&str>) -> Option<String> {
    q.map(str::trim).filter(|s| !s.is_empty()).map(|s| {
        format!(
            "%{}%",
            s.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    })
}

/// Search, filter by a segment, sort and page the members.
#[utoipa::path(post, path = "/loyalty/members/query", tag = "loyalty", operation_id = "query_loyalty_members",
    request_body = MembersQueryRequest,
    responses((status = 200, body = CrmMembersPage),
        (status = 409, description = "LOYALTY_AUDIENCE_INVALID"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn query_members(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<MembersQueryRequest>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let (org_id, claims) = super::settings::scope_org(pool, &req, None).await?;
    crate::authz::require::require(pool, &claims, crate::authz::Cap::LoyaltyMembersList, None)
        .await?;
    let (mode, sizes, defs, one) = programme(pool, org_id).await?;
    check_audience(&body.audience, &sizes)?;
    let limit = body.limit.unwrap_or(50).clamp(1, 200);
    let offset = body.offset.unwrap_or(0).max(0);
    let n = needle(body.q.as_deref());
    let bal = balance_sql(1, mode);
    let lifetime = match mode {
        Mode::Points => "m.lifetime_points",
        Mode::Visits => "m.lifetime_visits",
    };

    let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(" FROM loyalty_memberships m JOIN customers c ON c.id = m.id WHERE m.org_id = ")
            .push_bind(org_id)
            .push(" AND m.deleted_at IS NULL AND c.erased_at IS NULL");
        if let Some(n) = &n {
            qb.push(" AND (c.name ILIKE ")
                .push_bind(n.clone())
                .push(" OR c.phone_key ILIKE ")
                .push_bind(n.clone())
                .push(")");
        }
        push_audience(qb, &body.audience, mode, &sizes);
    };

    let mut qb = QueryBuilder::<Postgres>::new(format!(
        "SELECT m.id, c.name, COALESCE(c.phone_key, c.phone, '') AS phone, {bal} AS balance, {lifetime} AS lifetime, m.enrolled_at, \
                {LAST_SEEN} AS last_visit_at, \
                (SELECT count(*) FROM loyalty_pass_devices d WHERE d.customer_id = m.id) AS apple_devices, \
                (m.google_object_id IS NOT NULL) AS google_card, c.marketing_opt_out, c.birth_month, COALESCE(c.locale, 'en') AS locale"
    ));
    filter(&mut qb);
    let dir = if body.desc { "DESC" } else { "ASC" };
    let order = match body.sort {
        MemberSort::Joined => format!("m.enrolled_at {dir}"),
        MemberSort::LastVisit => format!("last_visit_at {dir}"),
        MemberSort::Balance => format!("balance {dir}"),
        MemberSort::Name => format!("lower(c.name) {dir}"),
    };
    qb.push(format!(" ORDER BY {order}, m.id LIMIT "))
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset);
    let mut rows: Vec<CrmMember> = qb.build_query_as().fetch_all(pool).await?;

    // The total is an aggregate over the org: counted once a minute per filter
    // (rewards_cache), not on every page turn.
    let key = format!(
        "members|{org_id}|{}|{}",
        n.as_deref().unwrap_or(""),
        serde_json::to_string(&body.audience).unwrap_or_default()
    );
    let total: i64 = super::rewards_cache::count(key, || async {
        let mut count = QueryBuilder::<Postgres>::new("SELECT count(*)");
        filter(&mut count);
        Ok(count.build_query_scalar().fetch_one(pool).await?)
    })
    .await?;

    let multi = defs.len() > 1;
    for r in &mut rows {
        r.rewards_ready = madar_card::tracks::track_card(r.balance, one).rewards_ready;
        if multi {
            let b = super::tracks::balances(pool, r.id).await?;
            r.tracks = super::tracks::member_tracks(&defs, r.balance, one, &b);
        }
    }
    Ok(HttpResponse::Ok().json(CrmMembersPage { rows, total }))
}

/// How the member's card is held, and the last message it carried.
#[derive(Debug, Serialize, ToSchema)]
pub struct MemberWallet {
    pub apple_devices: i64,
    pub apple_registered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub google_card: bool,
    /// When the pass was last rebuilt for them.
    pub pass_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub notice: Option<String>,
    pub notice_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Apple's device fetched the pass after the push.
    pub notice_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub notice_wallets: Option<String>,
    /// Dashboard messages this member was sent.
    pub messages_received: i64,
    pub marketing_opt_out: bool,
    /// Their birthday gift while open, or claimed and still in its window;
    /// absent otherwise (`gifts::current`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub birthday_gift: Option<super::gifts::BirthdayGiftView>,
}

#[utoipa::path(get, path = "/loyalty/members/{id}/wallet", tag = "loyalty",
    operation_id = "get_loyalty_member_wallet",
    params(("id" = Uuid, Path, description = "Member ID")),
    responses((status = 200, body = MemberWallet), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn member_wallet(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let (org_id, claims) = super::settings::scope_org(pool, &req, None).await?;
    crate::authz::require::require(pool, &claims, crate::authz::Cap::LoyaltyMembersList, None)
        .await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        apple_devices: i64,
        apple_registered_at: Option<chrono::DateTime<chrono::Utc>>,
        google_card: bool,
        pass_updated_at: Option<chrono::DateTime<chrono::Utc>>,
        pass_notice: Option<String>,
        pass_notice_at: Option<chrono::DateTime<chrono::Utc>>,
        pass_notice_seen_at: Option<chrono::DateTime<chrono::Utc>>,
        pass_notice_wallets: Option<String>,
        messages_received: i64,
        marketing_opt_out: bool,
    }
    let r: Row = sqlx::query_as(
        "SELECT (SELECT count(*) FROM loyalty_pass_devices d WHERE d.customer_id = m.id) AS apple_devices, \
                (SELECT min(d.created_at) FROM loyalty_pass_devices d WHERE d.customer_id = m.id) AS apple_registered_at, \
                (m.google_object_id IS NOT NULL) AS google_card, m.pass_updated_at, m.pass_notice, \
                m.pass_notice_at, m.pass_notice_seen_at, m.pass_notice_wallets, \
                (SELECT count(*) FROM loyalty_broadcast_recipients r WHERE r.customer_id = m.id AND r.outcome = 'sent') AS messages_received, \
                c.marketing_opt_out \
           FROM loyalty_memberships m JOIN customers c ON c.id = m.id WHERE m.id = $1 AND m.org_id = $2 AND m.deleted_at IS NULL",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Member not found".into()))?;
    let birthday_gift = super::gifts::current(pool, *id).await?;
    Ok(HttpResponse::Ok().json(MemberWallet {
        birthday_gift,
        apple_devices: r.apple_devices,
        apple_registered_at: r.apple_registered_at,
        google_card: r.google_card,
        pass_updated_at: r.pass_updated_at,
        notice: r.pass_notice,
        notice_at: r.pass_notice_at,
        notice_seen_at: r.pass_notice_seen_at,
        notice_wallets: r.pass_notice_wallets,
        messages_received: r.messages_received,
        marketing_opt_out: r.marketing_opt_out,
    }))
}

// ── The figures ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatsQuery {
    /// Start of the range (an instant, like the other loyalty analytics); its
    /// day in the shop's time zone is the first day. Default: 29 days before `to`.
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    /// End of the range (inclusive); its day in the shop's zone is the last.
    /// Default: today.
    pub to: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct StatsDay {
    pub date: chrono::NaiveDate,
    pub new_members: i64,
    /// Stamps (or points) earned, every track.
    pub earned: i64,
    /// Rewards taken (redemption rows), every track.
    pub redemptions: i64,
    /// Members with any ledger row that day.
    pub active: i64,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct StatsTrack {
    pub position: i16,
    pub earned: i64,
    pub redemptions: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RewardsStats {
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
    /// `points` or `visits`.
    pub mode: String,
    /// What track 1 counts: `points`, `order`, `item` or `stamp` (tracks 2–4 count stamps).
    pub stamp_unit: String,
    pub members_total: i64,
    pub new_members: i64,
    pub active_members: i64,
    pub earned: i64,
    pub redemptions: i64,
    /// Members holding the card in a wallet (Apple device or Google object).
    pub members_with_card: i64,
    /// Messages that reached a card or WhatsApp (not "no card, no WhatsApp").
    pub messages_sent: i64,
    pub messages_seen: i64,
    pub per_track: Vec<StatsTrack>,
    pub daily: Vec<StatsDay>,
}

#[utoipa::path(get, path = "/loyalty/stats", tag = "loyalty", operation_id = "get_loyalty_stats",
    params(StatsQuery), responses((status = 200, body = RewardsStats), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn stats(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<StatsQuery>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let (org_id, claims) = super::settings::scope_org(pool, &req, None).await?;
    check_permission(pool, &claims, "loyalty", "read").await?;
    crate::authz::require::require(pool, &claims, crate::authz::Cap::LoyaltyMembersList, None)
        .await?;
    let (mode, _, _, _) = programme(pool, org_id).await?;
    let tz: String = sqlx::query_scalar(
        "SELECT COALESCE(timezone::text, 'Africa/Cairo') FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or_else(|| "Africa/Cairo".into());
    let day_of = |t: Option<chrono::DateTime<chrono::Utc>>| {
        let tz = tz.clone();
        async move {
            sqlx::query_scalar::<_, chrono::NaiveDate>(
                "SELECT (COALESCE($1, now()) AT TIME ZONE $2)::date",
            )
            .bind(t)
            .bind(tz)
            .fetch_one(pool)
            .await
        }
    };
    let to = day_of(query.to).await?;
    let from = match query.from {
        Some(_) => day_of(query.from).await?,
        None => to - chrono::Duration::days(29),
    };
    if from > to || (to - from).num_days() > 366 {
        return Err(AppError::BadRequest(
            "from must be before to, at most a year apart".into(),
        ));
    }
    // An aggregate over the org's ledger: computed at most once a minute per
    // range (rewards_cache), from the (org_id, created_at) index.
    let key = format!("stats|{org_id}|{from}|{to}");
    let settings = super::settings::load_scope(pool, org_id, None)
        .await?
        .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(org_id, None));
    let stamp_unit = super::wallet::design::unit_of(pool, org_id, &settings)
        .await
        .to_string();
    let tz2 = tz.clone();
    let value = super::rewards_cache::stats(key, || async move {
        let tz = tz2;
        // Days run [from 00:00, to+1 00:00) in the shop's zone.

        let daily: Vec<StatsDay> = sqlx::query_as(&format!(
            "WITH days AS (SELECT generate_series($2::date, $3::date, '1 day')::date AS date), \
                  led AS (SELECT (t.created_at AT TIME ZONE $4)::date AS date, t.customer_id, t.kind::text AS kind, t.points \
                            FROM loyalty_transactions t \
                           WHERE t.org_id = $1 AND t.created_at >= {lo} AND t.created_at < {hi}), \
                  joined AS (SELECT (m.enrolled_at AT TIME ZONE $4)::date AS date, count(*) AS n FROM loyalty_memberships m \
                              WHERE m.org_id = $1 AND m.enrolled_at >= {lo} AND m.enrolled_at < {hi} GROUP BY 1) \
             SELECT d.date, COALESCE(j.n, 0)::bigint AS new_members, \
                    COALESCE((SELECT sum(points) FROM led WHERE led.date = d.date AND kind = 'earn'), 0)::bigint AS earned, \
                    COALESCE((SELECT count(*) FROM led WHERE led.date = d.date AND kind = 'redeem'), 0)::bigint AS redemptions, \
                    COALESCE((SELECT count(DISTINCT customer_id) FROM led WHERE led.date = d.date), 0)::bigint AS active \
               FROM days d LEFT JOIN joined j ON j.date = d.date ORDER BY d.date",
            lo = "($2::date::timestamp AT TIME ZONE $4)",
            hi = "(($3::date + 1)::timestamp AT TIME ZONE $4)"
        ))
        .bind(org_id)
        .bind(from)
        .bind(to)
        .bind(&tz)
        .fetch_all(pool)
        .await?;

        let per_track: Vec<StatsTrack> = sqlx::query_as(
            "SELECT COALESCE(t.track, 1)::int2 AS position, \
                    COALESCE(sum(t.points) FILTER (WHERE t.kind = 'earn'), 0)::bigint AS earned, \
                    count(*) FILTER (WHERE t.kind = 'redeem')::bigint AS redemptions \
               FROM loyalty_transactions t \
              WHERE t.org_id = $1 AND t.created_at >= ($2::date::timestamp AT TIME ZONE $4) \
                AND t.created_at < (($3::date + 1)::timestamp AT TIME ZONE $4) \
              GROUP BY 1 ORDER BY 1",
        )
        .bind(org_id)
        .bind(from)
        .bind(to)
        .bind(&tz)
        .fetch_all(pool)
        .await?;

        let (members_total, members_with_card): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE m.google_object_id IS NOT NULL \
                        OR EXISTS (SELECT 1 FROM loyalty_pass_devices d WHERE d.customer_id = m.id)) \
               FROM loyalty_memberships m WHERE m.org_id = $1 AND m.deleted_at IS NULL",
        )
        .bind(org_id)
        .fetch_one(pool)
        .await?;
        let active_members: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT customer_id) FROM loyalty_transactions \
              WHERE org_id = $1 AND created_at >= ($2::date::timestamp AT TIME ZONE $4) \
                AND created_at < (($3::date + 1)::timestamp AT TIME ZONE $4)",
        )
        .bind(org_id)
        .bind(from)
        .bind(to)
        .bind(&tz)
        .fetch_one(pool)
        .await?;
        let (messages_sent, messages_seen): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE r.outcome = 'sent' AND r.channel <> 'none'), count(r.seen_at) \
               FROM loyalty_broadcast_recipients r JOIN loyalty_broadcasts b ON b.id = r.broadcast_id \
              WHERE b.org_id = $1 AND r.processed_at >= ($2::date::timestamp AT TIME ZONE $4) \
                AND r.processed_at < (($3::date + 1)::timestamp AT TIME ZONE $4)",
        )
        .bind(org_id)
        .bind(from)
        .bind(to)
        .bind(&tz)
        .fetch_one(pool)
        .await?;

            Ok(std::sync::Arc::new(serde_json::to_value(RewardsStats {
            from,
            to,
            mode: mode.as_str().into(),
            stamp_unit,
            members_total,
            new_members: daily.iter().map(|d| d.new_members).sum(),
            active_members,
            earned: daily.iter().map(|d| d.earned).sum(),
            redemptions: daily.iter().map(|d| d.redemptions).sum(),
            members_with_card,
            messages_sent,
            messages_seen,
            per_track,
            daily,
            }).map_err(|_| AppError::Internal)?))
    })
    .await?;
    Ok(HttpResponse::Ok().json(&*value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_balance_reads_its_own_row() {
        assert_eq!(balance_sql(1, Mode::Visits), "m.visits_balance");
        assert_eq!(balance_sql(1, Mode::Points), "m.points_balance");
        assert!(balance_sql(3, Mode::Visits).contains("b.position = 3"));
    }
}
