//! Stamp tracks: one card, up to four rewards (REWARDS_DASHBOARD_DESIGN.md §2;
//! POS_MULTITRACK_DESIGN.md D1–D9, owner-confirmed 2026-10-23).
//!
//! Track 1 is the programme's own card and is NOT stored here beyond its name,
//! shape and colour: its balance is `visits_balance`/`points_balance`, its size
//! the programme's, its earning list `loyalty_earning_items`. Tracks 2–4 live in
//! `loyalty_stamp_tracks` + `loyalty_track_items`, their balances in
//! `loyalty_track_balances` (kept by the ledger trigger), and every ledger row
//! of theirs carries `track`.
//!
//! - A POS sale earns every track its paid lines qualify for, in the award's
//!   own transaction ([`award_for_order`]), one row per (order, track).
//! - A track's reward is one unit of any of its items at the track's size
//!   (madar-shared `madar_loyalty::tracks`).
//! - A track with no items is counter-only (stand-alone shops).
//! - A points programme has one track: tracks 2–4 are ignored while it is in
//!   points mode, and refused when saved.

use std::collections::HashMap;

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::earn::{Mode, OrderLine};
use super::model::{MemberRow, Source};
use super::settings::RewardItem;
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::check_permission;

pub const CODE_TRACK_UNKNOWN: &str = "LOYALTY_TRACK_UNKNOWN";
pub const CODE_TRACK_BALANCE_SHORT: &str = "LOYALTY_TRACK_BALANCE_SHORT";
pub const CODE_TRACK_NOT_ON_OFFER: &str = "LOYALTY_TRACK_NOT_ON_OFFER";
pub const CODE_TRACKS_INVALID: &str = "LOYALTY_TRACKS_INVALID";

/// One track as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackDef {
    pub position: u8,
    pub name_en: String,
    pub name_ar: String,
    /// `None` on track 1.
    pub size: Option<i32>,
    pub shape: Option<String>,
    pub colour: Option<String>,
    pub per_item: bool,
    pub cap: Option<i32>,
    pub items: Vec<Uuid>,
}

impl TrackDef {
    /// madar-card's shape (the wire's).
    pub fn to_card(&self) -> madar_card::Track {
        madar_card::Track {
            position: self.position,
            name_en: self.name_en.clone(),
            name_ar: self.name_ar.clone(),
            size: self.size,
            shape: self
                .shape
                .as_deref()
                .and_then(|s| serde_json::from_value(serde_json::Value::String(s.into())).ok()),
            colour: self.colour.clone(),
            items: self.items.iter().map(Uuid::to_string).collect(),
            per_item: self.per_item,
            cap: self.cap,
        }
    }
}

/// The org's tracks, in position order, as they apply under `mode`: track 1
/// always (a stored row only adds its name/shape/colour), tracks 2–4 only in a
/// stamps programme.
pub async fn load<'e, E>(exec: E, org_id: Uuid, mode: Mode) -> Result<Vec<TrackDef>, AppError>
where
    E: sqlx::PgExecutor<'e> + Copy + 'e,
{
    // Cached per org (rewards_cache): every lookup, award, redemption and pass
    // build reads this; only `PUT /loyalty/tracks` changes it.
    let rows = super::rewards_cache::tracks(org_id, || async move {
        Ok(std::sync::Arc::new(stored(exec, org_id).await?))
    })
    .await?;
    let mut out: Vec<TrackDef> = Vec::new();
    let one = rows.iter().find(|r| r.position == 1);
    out.push(TrackDef {
        position: 1,
        name_en: one.map(|r| r.name_en.clone()).unwrap_or_default(),
        name_ar: one.map(|r| r.name_ar.clone()).unwrap_or_default(),
        size: None,
        shape: one.and_then(|r| r.shape.clone()),
        colour: one.and_then(|r| r.colour.clone()),
        per_item: true,
        cap: None,
        items: Vec::new(),
    });
    if mode == Mode::Visits {
        out.extend(rows.iter().filter(|r| r.position > 1).cloned());
    }
    Ok(out)
}

/// The org's track rows as stored (every position), one round trip.
async fn stored<'e, E>(exec: E, org_id: Uuid) -> Result<Vec<TrackDef>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    #[derive(sqlx::FromRow)]
    struct Row {
        position: i16,
        name_en: String,
        name_ar: String,
        size: Option<i32>,
        shape: Option<String>,
        colour: Option<String>,
        per_item: bool,
        cap: Option<i32>,
        items: Vec<Uuid>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT t.position, t.name_en, t.name_ar, t.size, t.shape, t.colour, t.per_item, t.cap, \
                COALESCE(array_agg(i.menu_item_id ORDER BY i.menu_item_id) \
                         FILTER (WHERE i.menu_item_id IS NOT NULL), '{}') AS items \
           FROM loyalty_stamp_tracks t \
           LEFT JOIN loyalty_track_items i ON i.org_id = t.org_id AND i.position = t.position \
          WHERE t.org_id = $1 \
          GROUP BY t.org_id, t.position ORDER BY t.position",
    )
    .bind(org_id)
    .fetch_all(exec)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| TrackDef {
            position: u8::try_from(r.position).unwrap_or(0),
            name_en: r.name_en,
            name_ar: r.name_ar,
            size: r.size,
            shape: r.shape,
            colour: r.colour,
            per_item: r.per_item,
            cap: r.cap,
            items: r.items,
        })
        .collect())
}

/// A member's balances on tracks 2–4.
pub async fn balances<'e, E>(exec: E, customer_id: Uuid) -> Result<HashMap<u8, i32>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows: Vec<(i16, i32)> = sqlx::query_as(
        "SELECT position, balance FROM loyalty_track_balances WHERE customer_id = $1",
    )
    .bind(customer_id)
    .fetch_all(exec)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(p, b)| (u8::try_from(p).unwrap_or(0), b))
        .collect())
}

/// One track on a member's card, with the card's figures (madar-card's
/// `track_card`). Track 1's figures equal the member's top-level ones.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MemberTrack {
    pub position: u8,
    pub name_en: String,
    pub name_ar: String,
    pub size: i32,
    pub balance: i32,
    pub rewards_ready: i32,
    pub progress_to_next: i32,
    pub points_to_next_reward: i32,
    pub can_redeem: bool,
    #[serde(default)]
    pub shape: Option<String>,
    #[serde(default)]
    pub colour: Option<String>,
    /// The track is bound to menu items (a POS sale earns it); `false` = a
    /// counter-only track.
    #[serde(default)]
    pub earns_from_sales: bool,
}

/// Every track of a member's card, or `None` for a one-track card (the
/// field stays out of the response: today's shape).
pub fn member_tracks(
    defs: &[TrackDef],
    track_one_balance: i32,
    track_one_size: i32,
    balances: &HashMap<u8, i32>,
) -> Option<Vec<MemberTrack>> {
    if defs.len() <= 1 {
        return None;
    }
    Some(
        defs.iter()
            .map(|d| {
                let (size, balance) = if d.position == 1 {
                    (track_one_size, track_one_balance)
                } else {
                    (
                        d.size.unwrap_or(1),
                        balances.get(&d.position).copied().unwrap_or(0),
                    )
                };
                let c = madar_card::tracks::track_card(balance, size);
                MemberTrack {
                    position: d.position,
                    name_en: d.name_en.clone(),
                    name_ar: d.name_ar.clone(),
                    size,
                    balance,
                    rewards_ready: c.rewards_ready,
                    progress_to_next: c.progress_to_next,
                    points_to_next_reward: c.points_to_next_reward,
                    can_redeem: c.can_redeem,
                    shape: d.shape.clone(),
                    colour: d.colour.clone(),
                    earns_from_sales: d.position == 1 || !d.items.is_empty(),
                }
            })
            .collect(),
    )
}

/// `member_tracks` for one member, reading what it needs.
pub async fn for_member(
    pool: &PgPool,
    member: &MemberRow,
    mode: Mode,
    track_one_size: i32,
) -> Result<Option<Vec<MemberTrack>>, AppError> {
    let defs = load(pool, member.org_id, mode).await?;
    if defs.len() <= 1 {
        return Ok(None);
    }
    let b = balances(pool, member.id).await?;
    Ok(member_tracks(
        &defs,
        member.balance_in(mode),
        track_one_size,
        &b,
    ))
}

/// What a track 2–4 offers at the till: each of its items at the track's size.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TrackRewards {
    pub position: u8,
    /// Only what the member's balance on this track can buy now (as
    /// `ScanResult.rewards` is for track 1).
    pub rewards: Vec<RewardItem>,
}

/// Tracks 2–4's rewards for the scan screen; `None` for a one-track card.
pub async fn track_rewards(
    pool: &PgPool,
    defs: &[TrackDef],
    member_id: Uuid,
) -> Result<Option<Vec<TrackRewards>>, AppError> {
    if defs.len() <= 1 {
        return Ok(None);
    }
    let b = balances(pool, member_id).await?;
    let mut out = Vec::new();
    for d in defs.iter().filter(|d| d.position > 1) {
        let size = d.size.unwrap_or(0);
        let affordable = size > 0 && b.get(&d.position).copied().unwrap_or(0) >= size;
        let rewards: Vec<RewardItem> = if affordable && !d.items.is_empty() {
            #[derive(sqlx::FromRow)]
            struct Item {
                id: Uuid,
                name: String,
                image_url: Option<String>,
                base_price: i32,
            }
            let items: Vec<Item> = sqlx::query_as(
                "SELECT id, name, image_url, base_price FROM menu_items \
                  WHERE id = ANY($1) AND deleted_at IS NULL ORDER BY name, id",
            )
            .bind(&d.items)
            .fetch_all(pool)
            .await?;
            items
                .into_iter()
                .enumerate()
                .map(|(i, m)| RewardItem {
                    menu_item_id: m.id,
                    name: m.name,
                    image_url: m.image_url,
                    base_price: m.base_price,
                    cost_currency: "visits".into(),
                    cost_amount: size,
                    sort_order: i32::try_from(i).unwrap_or(0),
                })
                .collect()
        } else {
            Vec::new()
        };
        out.push(TrackRewards {
            position: d.position,
            rewards,
        });
    }
    Ok(Some(out))
}

/// Stamps a sale earns on one track 2–4: its items' paid units, or one for a
/// sale holding any of them with `per_item` off. A covered unit earns nothing.
pub fn stamps_for_track(def: &TrackDef, lines: &[OrderLine]) -> i32 {
    let paid: i32 = lines
        .iter()
        .filter(|l| def.items.contains(&l.menu_item_id))
        .map(|l| (l.quantity - l.redeemed_units).max(0))
        .sum();
    if def.per_item {
        paid
    } else {
        i32::from(paid > 0)
    }
}

/// What one track earned on an award.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TrackAward {
    pub position: u8,
    pub points_awarded: i32,
}

/// Earn tracks 2–4 for a settled order, inside the award's transaction, one
/// row per (order, track) (`loyalty_transactions_earn_order_track_key`). Each
/// track's ceiling trims, never refuses. Returns what each track earned NOW
/// (0 for a track already earned by an earlier press).
#[allow(clippy::too_many_arguments)]
pub async fn award_for_order(
    tx: &mut Transaction<'_, Postgres>,
    defs: &[TrackDef],
    org_id: Uuid,
    branch_id: Uuid,
    customer_id: Uuid,
    order_id: Uuid,
    lines: &[OrderLine],
    created_by: Option<Uuid>,
) -> Result<Vec<TrackAward>, AppError> {
    let mut out = Vec::new();
    for d in defs
        .iter()
        .filter(|d| d.position > 1 && !d.items.is_empty())
    {
        let mut stamps = stamps_for_track(d, lines);
        if stamps > 0 && d.cap.is_some() {
            let current: i32 = sqlx::query_scalar(
                "SELECT balance FROM loyalty_track_balances WHERE customer_id = $1 AND position = $2 FOR UPDATE",
            )
            .bind(customer_id)
            .bind(i16::from(d.position))
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or(0);
            stamps = madar_card::tracks::stamps_within_cap(current, stamps, d.cap);
        }
        let mut awarded = 0;
        if stamps > 0 {
            let inserted = sqlx::query(
                "INSERT INTO loyalty_transactions \
                    (org_id, customer_id, branch_id, kind, currency, points, order_id, created_by, source, track) \
                 VALUES ($1,$2,$3,'earn','visits',$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
            )
            .bind(org_id)
            .bind(customer_id)
            .bind(branch_id)
            .bind(stamps)
            .bind(order_id)
            .bind(created_by)
            .bind(Source::Sale.as_str())
            .bind(i16::from(d.position))
            .execute(&mut **tx)
            .await?;
            if inserted.rows_affected() == 1 {
                awarded = stamps;
            }
        }
        out.push(TrackAward {
            position: d.position,
            points_awarded: awarded,
        });
    }
    Ok(out)
}

/// The definition of one track, or a coded refusal.
pub fn find(defs: &[TrackDef], position: u8) -> Result<&TrackDef, AppError> {
    defs.iter()
        .find(|d| d.position == position)
        .ok_or_else(|| AppError::Refused {
            code: CODE_TRACK_UNKNOWN,
            reason: format!("This card has no track {position}"),
        })
}

// ── The dashboard: GET / PUT /loyalty/tracks ─────────────────────────────────

/// The shop's tracks, for the dashboard (and anyone setting up a till).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TracksView {
    /// `points` or `visits` (the org default's).
    pub mode: String,
    /// Track 1's size: the programme's card size.
    pub track_one_size: i32,
    /// What track 1 counts: `points`, `order`, `item` or `stamp` (tracks 2–4
    /// count stamps). madar-card `text::stamp_unit`.
    pub stamp_unit: String,
    /// Every track in position order; track 1 always first. In a points
    /// programme only track 1 (tracks 2–4 are kept but inactive).
    pub tracks: Vec<madar_card::Track>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PutTracks {
    /// The complete list, in position order, track 1 first. A track left out
    /// is removed (its members' balances are kept, and come back with it).
    pub tracks: Vec<madar_card::Track>,
}

async fn view(pool: &PgPool, org_id: Uuid) -> Result<TracksView, AppError> {
    let settings = super::settings::load_scope(pool, org_id, None)
        .await?
        .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(org_id, None));
    let mode = settings.mode();
    let defs = load(pool, org_id, mode).await?;
    let rewards = super::settings::load_effective_rewards_org(pool, org_id).await?;
    Ok(TracksView {
        mode: mode.as_str().into(),
        track_one_size: super::model::reward_target(&settings, &rewards),
        stamp_unit: super::wallet::design::unit_of(pool, org_id, &settings)
            .await
            .to_string(),
        tracks: defs.iter().map(TrackDef::to_card).collect(),
    })
}

#[utoipa::path(get, path = "/loyalty/tracks", tag = "loyalty", operation_id = "get_loyalty_tracks",
    responses((status = 200, body = TracksView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_tracks(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let (org_id, claims) = super::settings::scope_org(pool.get_ref(), &req, None).await?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    Ok(HttpResponse::Ok().json(view(pool.get_ref(), org_id).await?))
}

#[utoipa::path(put, path = "/loyalty/tracks", tag = "loyalty", operation_id = "put_loyalty_tracks",
    request_body = PutTracks,
    responses((status = 200, body = TracksView),
        (status = 409, description = "LOYALTY_TRACKS_INVALID (the problems are in the reason)"),
        AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn put_tracks(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PutTracks>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let (org_id, claims) = super::settings::scope_org(pool, &req, None).await?;
    crate::authz::require::require(pool, &claims, crate::authz::Cap::LoyaltyCardDesign, None)
        .await?;
    let settings = super::settings::load_scope(pool, org_id, None)
        .await?
        .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(org_id, None));
    let mode = match settings.mode() {
        Mode::Points => madar_card::tracks::Mode::Points,
        Mode::Visits => madar_card::tracks::Mode::Visits,
    };
    let tracks = &body.tracks;
    let problems = madar_card::tracks::validate_tracks(tracks, mode);
    if !problems.is_empty() {
        return Err(AppError::Refused {
            code: CODE_TRACKS_INVALID,
            reason: problems
                .iter()
                .map(|p| format!("{} ({})", p.code, p.path))
                .collect::<Vec<_>>()
                .join(", "),
        });
    }
    // Every item must be this org's live menu item.
    let mut items: Vec<(i16, Uuid)> = Vec::new();
    for t in tracks.iter().filter(|t| t.position > 1) {
        for i in &t.items {
            let id = Uuid::parse_str(i)
                .map_err(|_| AppError::BadRequest(format!("{i} is not a menu item id")))?;
            items.push((i16::from(t.position), id));
        }
    }
    let ids: Vec<Uuid> = items.iter().map(|(_, i)| *i).collect();
    let known: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT id) FROM menu_items WHERE id = ANY($1) AND org_id = $2 AND deleted_at IS NULL",
    )
    .bind(&ids)
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    let distinct: std::collections::HashSet<Uuid> = ids.iter().copied().collect();
    if usize::try_from(known).unwrap_or(0) != distinct.len() {
        return Err(AppError::BadRequest(
            "A track names a menu item this shop does not have".into(),
        ));
    }

    let mut tx = pool.begin().await?;
    let keep: Vec<i16> = tracks.iter().map(|t| i16::from(t.position)).collect();
    sqlx::query("DELETE FROM loyalty_stamp_tracks WHERE org_id = $1 AND NOT (position = ANY($2))")
        .bind(org_id)
        .bind(&keep)
        .execute(&mut *tx)
        .await?;
    for t in tracks {
        let shape = t.shape.map(|s| {
            serde_json::to_value(s)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        });
        sqlx::query(
            "INSERT INTO loyalty_stamp_tracks (org_id, position, name_en, name_ar, size, shape, colour, per_item, cap) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
             ON CONFLICT (org_id, position) DO UPDATE SET name_en = EXCLUDED.name_en, \
                name_ar = EXCLUDED.name_ar, size = EXCLUDED.size, shape = EXCLUDED.shape, \
                colour = EXCLUDED.colour, per_item = EXCLUDED.per_item, cap = EXCLUDED.cap, updated_at = now()",
        )
        .bind(org_id)
        .bind(i16::from(t.position))
        .bind(t.name_en.trim())
        .bind(t.name_ar.trim())
        .bind(t.size)
        .bind(shape)
        .bind(&t.colour)
        .bind(t.per_item)
        .bind(t.cap)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM loyalty_track_items WHERE org_id = $1")
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
    for (p, i) in &items {
        sqlx::query(
            "INSERT INTO loyalty_track_items (org_id, position, menu_item_id) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING",
        )
        .bind(org_id)
        .bind(p)
        .bind(i)
        .execute(&mut *tx)
        .await?;
    }
    // The cards change look: rebuilt by the sweep, as after a settings save.
    super::wallet::mark_org_stale(&mut tx, org_id).await?;
    tx.commit().await?;
    super::rewards_cache::invalidate_tracks(org_id).await;
    super::wallet::store::purge_org(pool, org_id).await;
    Ok(HttpResponse::Ok().json(view(pool, org_id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(items: Vec<Uuid>, per_item: bool) -> TrackDef {
        TrackDef {
            position: 2,
            name_en: "Sandwich".into(),
            name_ar: String::new(),
            size: Some(6),
            shape: None,
            colour: None,
            per_item,
            cap: None,
            items,
        }
    }

    #[test]
    fn a_track_earns_its_own_items_paid_units() {
        let s = Uuid::from_u128(1);
        let latte = Uuid::from_u128(2);
        let lines = [
            OrderLine {
                menu_item_id: s,
                quantity: 3,
                redeemed_units: 1,
            },
            OrderLine {
                menu_item_id: latte,
                quantity: 2,
                redeemed_units: 0,
            },
        ];
        // Three sandwiches, one free: two paid units earn two stamps.
        assert_eq!(stamps_for_track(&def(vec![s], true), &lines), 2);
        // Per sale: one stamp for a sale holding any.
        assert_eq!(stamps_for_track(&def(vec![s], false), &lines), 1);
        // No items: nothing from a sale (a counter-only track).
        assert_eq!(stamps_for_track(&def(vec![], true), &lines), 0);
    }
}
