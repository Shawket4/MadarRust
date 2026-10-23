//! The birthday gift: one free item, whatever it costs (owner, 2026-10-23;
//! Madar `BIRTHDAY_GIFT_DESIGN.md`).
//!
//! The birthday job grants it (`birthdays::greet`, programmes with
//! `birthday_gift = 'item'`), valid `birthday_gift_days` from the birthday. It
//! is claimed ONCE and moves no balance:
//! - at a till, by naming one order line (`loyalty_birthday_gift` on
//!   `POST /orders`): that line's one unit is covered at zero, like a reward;
//! - at a Rewards by Madar counter (`POST /loyalty/counter/birthday-gift`).
//!
//! A void of the claiming order (trigger) or a refund of the gift's unit
//! ([`reopen_on_refund`]) re-opens it while it is still valid. A retired
//! membership forfeits it (trigger).
//!
//! ponytail: the eligibility rule lives here; it belongs beside
//! `madar_loyalty::plan` in madar-shared once the POS core pre-checks it.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::model::{self, MemberView};
use super::settings::{load_effective, load_effective_rewards};
use crate::errors::{AppError, AppErrorResponse};
use crate::orders::handlers::OrderItemInput;

/// No open gift: none was granted, it was already claimed, or it expired.
pub const CODE_GIFT_NONE: &str = "LOYALTY_GIFT_NONE";
/// The item is not a reward here (not in the catalogue, no "any item").
pub const CODE_GIFT_NOT_A_REWARD: &str = "LOYALTY_GIFT_NOT_A_REWARD";
/// The named line cannot take the gift: no such line, no menu item, a staff
/// drink, a combo, or a line a points reward already covers.
pub const CODE_GIFT_LINE: &str = "LOYALTY_GIFT_LINE";

/// The member's birthday gift, while it can be used or was used this time.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BirthdayGiftView {
    /// `open` (ready to claim) or `claimed`.
    #[schema(example = "open")]
    pub status: String,
    /// The gift is usable until this instant (end of its last day, shop time).
    pub valid_until: DateTime<Utc>,
}

/// One order line named as the birthday gift.
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema)]
pub struct LoyaltyGiftInput {
    /// Index into `items`.
    #[serde(default)]
    pub item_index: Option<usize>,
    /// A ticket settle names the line by `open_ticket_items.id` instead; the
    /// server fills in `item_index`.
    #[serde(default)]
    pub ticket_line_id: Option<Uuid>,
}

fn refused(code: &'static str, reason: &str) -> AppError {
    AppError::Refused {
        code,
        reason: reason.into(),
    }
}

/// The member's current birthday gift, if one is open or was claimed and is
/// still inside its window. Expired gifts are not shown.
pub async fn current<'e, E>(
    exec: E,
    customer_id: Uuid,
) -> Result<Option<BirthdayGiftView>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let row: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT status, valid_until FROM loyalty_gifts \
          WHERE customer_id = $1 AND kind = 'birthday' \
            AND status IN ('open', 'claimed') AND valid_until > now() \
          ORDER BY year DESC LIMIT 1",
    )
    .bind(customer_id)
    .fetch_optional(exec)
    .await?;
    Ok(row.map(|(status, valid_until)| BirthdayGiftView {
        status,
        valid_until,
    }))
}

/// A gift a sale will claim.
#[derive(Debug, Clone)]
pub struct GiftPlan {
    pub gift_id: Option<Uuid>,
    pub member_id: Uuid,
    pub item_index: usize,
    pub menu_item_id: Uuid,
    /// The ledger row's currency (the programme's mode).
    pub currency: String,
    /// A REPLAYED sale whose gift could not be honoured: the line stays covered
    /// (the till took the reduced money), nothing is claimed, and the order is
    /// flagged with this.
    pub refused: Option<String>,
}

impl GiftPlan {
    pub fn units_for(&self, line: usize) -> Option<i32> {
        (line == self.item_index).then_some(1)
    }
}

/// Check the gift a sale names. `None` when it names none. `covered_by_reward`
/// and `in_combo` are the order path's own judgements of a line.
///
/// Live, every problem is a coded 409 before anything is handed over. On a
/// replay (`lenient`) a line that exists keeps its cover and the reason is kept
/// for the flag; a line that does not exist is dropped.
#[allow(clippy::too_many_arguments)]
pub async fn plan(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    customer_id: Option<Uuid>,
    input: Option<&LoyaltyGiftInput>,
    items: &[OrderItemInput],
    covered_by_reward: impl Fn(usize) -> bool,
    in_combo: impl Fn(usize) -> bool,
    lenient: bool,
) -> Result<Option<GiftPlan>, AppError> {
    let Some(input) = input else {
        return Ok(None);
    };
    let line = input
        .item_index
        .and_then(|i| items.get(i).map(|it| (i, it)));
    let structural = match line {
        Some((i, it)) => it.menu_item_id.map(|m| (i, m)),
        None => None,
    };
    let strict = async {
        let customer_id = customer_id.ok_or_else(|| {
            AppError::BadRequest("A birthday gift needs the member it belongs to".into())
        })?;
        let member = model::find_by_id(pool, customer_id)
            .await?
            .filter(|m| m.org_id == org_id)
            .ok_or_else(|| AppError::NotFound("No member for that card".into()))?;
        let (i, it) = line.ok_or_else(|| refused(CODE_GIFT_LINE, "No such line for the gift"))?;
        let menu_item_id = it
            .menu_item_id
            .ok_or_else(|| refused(CODE_GIFT_LINE, "That line has no menu item to give"))?;
        if it.quantity < 1
            || it.staff_drink.is_some()
            || it.combo.is_some()
            || in_combo(i)
            || covered_by_reward(i)
        {
            return Err(refused(
                CODE_GIFT_LINE,
                "The gift can't go on a staff drink, a combo or a line a reward already covers",
            ));
        }
        let settings = load_effective(pool, org_id, branch_id).await?;
        if !settings.enabled {
            return Err(refused(
                super::award::CODE_PROGRAM_OFF,
                "The loyalty program is switched off for this branch",
            ));
        }
        if !settings.reward_any_item {
            let (catalogue, _) = load_effective_rewards(pool, org_id, branch_id).await?;
            if !catalogue.iter().any(|r| r.menu_item_id == menu_item_id) {
                return Err(refused(
                    CODE_GIFT_NOT_A_REWARD,
                    "That item is not one of the rewards here",
                ));
            }
        }
        let gift_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM loyalty_gifts \
              WHERE customer_id = $1 AND status = 'open' AND valid_until > now() \
              ORDER BY year DESC LIMIT 1",
        )
        .bind(member.id)
        .fetch_optional(pool)
        .await?;
        let gift_id = gift_id.ok_or_else(|| {
            refused(
                CODE_GIFT_NONE,
                &format!("{} has no birthday gift to claim", member.name),
            )
        })?;
        Ok(GiftPlan {
            gift_id: Some(gift_id),
            member_id: member.id,
            item_index: i,
            menu_item_id,
            currency: settings.mode().as_str().into(),
            refused: None,
        })
    };
    match strict.await {
        Ok(p) => Ok(Some(p)),
        Err(e) if lenient => match (structural, customer_id) {
            (Some((i, m)), Some(member_id)) => Ok(Some(GiftPlan {
                gift_id: None,
                member_id,
                item_index: i,
                menu_item_id: m,
                currency: String::new(),
                refused: Some(e.to_string()),
            })),
            _ => Ok(None),
        },
        Err(e) => Err(e),
    }
}

/// Claim the planned gift inside the order's transaction. Returns the refusal
/// to flag a replayed order with, if any.
///
/// Locked by the UPDATE itself: of two tills claiming one gift, one wins and
/// the other's live sale is refused. A retry of this order finds its own claim.
#[allow(clippy::too_many_arguments)]
pub async fn claim_on_order(
    tx: &mut Transaction<'_, Postgres>,
    plan: &GiftPlan,
    org_id: Uuid,
    branch_id: Uuid,
    order_id: Uuid,
    item_id: Option<Uuid>,
    by: Option<Uuid>,
    lenient: bool,
) -> Result<Option<String>, AppError> {
    if let Some(why) = &plan.refused {
        return Ok(Some(why.clone()));
    }
    let Some(gift_id) = plan.gift_id else {
        return Ok(None);
    };
    let claimed = sqlx::query(
        "UPDATE loyalty_gifts \
            SET status = 'claimed', claimed_at = now(), claimed_source = 'order', \
                claimed_order_id = $2, claimed_order_item_id = $3, claimed_line_index = $4, \
                claimed_menu_item_id = $5, claimed_branch_id = $6, claimed_by = $7 \
          WHERE id = $1 AND status = 'open' AND valid_until > now()",
    )
    .bind(gift_id)
    .bind(order_id)
    .bind(item_id)
    .bind(plan.item_index as i32)
    .bind(plan.menu_item_id)
    .bind(branch_id)
    .bind(by)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if claimed == 0 {
        let ours: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM loyalty_gifts WHERE id = $1 AND claimed_order_id = $2)",
        )
        .bind(gift_id)
        .bind(order_id)
        .fetch_one(&mut **tx)
        .await?;
        if ours {
            return Ok(None);
        }
        let why = "The birthday gift was already claimed or has expired".to_string();
        if lenient {
            return Ok(Some(why));
        }
        return Err(refused(CODE_GIFT_NONE, &why));
    }
    sqlx::query(
        "INSERT INTO loyalty_transactions \
            (org_id, customer_id, branch_id, kind, currency, points, order_id, \
             order_line_index, reward_menu_item_id, created_by, source, order_item_id, note) \
         VALUES ($1,$2,$3,'redeem',$4,0,$5,$6,$7,$8,'gift',$9,'Birthday gift') \
         ON CONFLICT DO NOTHING",
    )
    .bind(org_id)
    .bind(plan.member_id)
    .bind(branch_id)
    .bind(&plan.currency)
    .bind(order_id)
    .bind(plan.item_index as i32)
    .bind(plan.menu_item_id)
    .bind(by)
    .bind(item_id)
    .execute(&mut **tx)
    .await?;
    Ok(None)
}

/// A refund that brings the gift's unit back re-opens the gift while it is
/// still valid: the line refunded for nothing, or every paid unit of it
/// returned first (`redeem::reward_units_returned`), or the whole order
/// refunded. Runs in the refund's transaction, after its lines are written.
pub async fn reopen_on_refund(
    tx: &mut Transaction<'_, Postgres>,
    order_id: Uuid,
    lines: &[crate::refunds::handlers::RefundLineInput],
) -> Result<(), AppError> {
    let gift: Option<(Uuid, Option<Uuid>, Option<i32>)> = sqlx::query_as(
        "SELECT g.id, g.claimed_order_item_id, oi.quantity \
           FROM loyalty_gifts g \
           LEFT JOIN order_items oi ON oi.id = g.claimed_order_item_id \
          WHERE g.claimed_order_id = $1 AND g.status = 'claimed' AND g.valid_until > now() \
          FOR UPDATE OF g",
    )
    .bind(order_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((gift_id, item, quantity)) = gift else {
        return Ok(());
    };
    let back = if lines.is_empty() {
        sqlx::query_scalar::<_, bool>(
            "SELECT COALESCE((SELECT SUM(amount) FROM order_refunds WHERE order_id = $1), 0) \
                    >= o.total_amount FROM orders o WHERE o.id = $1",
        )
        .bind(order_id)
        .fetch_one(&mut **tx)
        .await?
    } else {
        let (Some(item), Some(qty)) = (item, quantity) else {
            return Ok(());
        };
        if !lines.iter().any(|l| l.order_item_id == item) {
            return Ok(());
        }
        let (free, paid): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(quantity) FILTER (WHERE amount = 0), 0)::bigint, \
                    COALESCE(SUM(quantity) FILTER (WHERE amount > 0), 0)::bigint \
               FROM order_refund_lines WHERE order_item_id = $1",
        )
        .bind(item)
        .fetch_one(&mut **tx)
        .await?;
        super::redeem::reward_units_returned(free, paid, i64::from(qty), 1) >= 1
    };
    if back {
        sqlx::query(
            "UPDATE loyalty_gifts \
                SET status = 'open', claimed_at = NULL, claimed_source = NULL, \
                    claimed_order_id = NULL, claimed_order_item_id = NULL, \
                    claimed_line_index = NULL, claimed_menu_item_id = NULL, \
                    claimed_branch_id = NULL, claimed_by = NULL, claim_key = NULL \
              WHERE id = $1",
        )
        .bind(gift_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Grant this year's birthday gift. `None` when it was already granted (a
/// rerun of the job). Returns the last day it is valid, in the shop's time.
pub async fn grant(
    pool: &PgPool,
    org_id: Uuid,
    member_id: Uuid,
    year: i32,
    birthday: NaiveDate,
    days: i32,
) -> Result<Option<NaiveDate>, AppError> {
    Ok(sqlx::query_scalar(
        "INSERT INTO loyalty_gifts (org_id, customer_id, kind, year, valid_from, valid_until) \
         SELECT o.id, $2, 'birthday', $3, \
                $4::date::timestamp AT TIME ZONE o.timezone::text, \
                ($4::date + $5::int)::timestamp AT TIME ZONE o.timezone::text \
           FROM organizations o WHERE o.id = $1 \
         ON CONFLICT (customer_id, kind, year) DO NOTHING \
         RETURNING ($4::date + $5::int - 1)",
    )
    .bind(org_id)
    .bind(member_id)
    .bind(year)
    .bind(birthday)
    .bind(days)
    .fetch_optional(pool)
    .await?)
}

/// Turn open gifts past their window to `expired`. Bookkeeping for reports:
/// every read already treats them as gone.
pub async fn expire_due(pool: &PgPool) -> Result<u64, AppError> {
    Ok(sqlx::query(
        "UPDATE loyalty_gifts SET status = 'expired' WHERE status = 'open' AND valid_until <= now()",
    )
    .execute(pool)
    .await?
    .rows_affected())
}

/// "21 Nov" / "21 نوفمبر": the gift's last day in a message.
pub fn day_label(d: NaiveDate, arabic: bool) -> String {
    const AR: [&str; 12] = [
        "يناير",
        "فبراير",
        "مارس",
        "أبريل",
        "مايو",
        "يونيو",
        "يوليو",
        "أغسطس",
        "سبتمبر",
        "أكتوبر",
        "نوفمبر",
        "ديسمبر",
    ];
    if arabic {
        format!("{} {}", d.day(), AR[d.month0() as usize])
    } else {
        format!("{} {}", d.day(), d.format("%b"))
    }
}

// ── The counter ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct CounterGiftRequest {
    pub branch_id: Uuid,
    /// The member, as `/loyalty/lookup` resolved them.
    pub customer_id: Uuid,
    /// One per attempt, made by the page (a UUID). A retry with the same key
    /// claims nothing new.
    pub idempotency_key: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CounterGiftResult {
    pub member: MemberView,
    /// The gift, now `claimed`.
    pub birthday_gift: BirthdayGiftView,
    /// True when this key had already claimed it: nothing new was written.
    pub already: bool,
}

/// Hand over the member's birthday gift at the counter: one free item, the
/// shop's pick. No balance moves.
#[utoipa::path(post, path = "/loyalty/counter/birthday-gift", tag = "loyalty",
    operation_id = "loyalty_counter_birthday_gift", request_body = CounterGiftRequest,
    responses(
        (status = 200, body = CounterGiftResult),
        (status = 409, description = "LOYALTY_PROGRAM_OFF, LOYALTY_GIFT_NONE, LOYALTY_KEY_REUSED"),
        AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn counter_claim(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CounterGiftRequest>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    // `loyalty.use`, like stamping: the teller hands rewards over.
    let claims = crate::orgs::handlers::extract_claims(&req)?;
    crate::permissions::checker::check_permission(pool, &claims, "loyalty", "update").await?;
    let key = super::counter::valid_key(&body.idempotency_key)?;
    crate::delivery::require_branch_access(pool, &claims, body.branch_id).await?;
    let org_id = super::resolve_branch_org(pool, body.branch_id).await?;
    let member = model::find_by_id(pool, body.customer_id)
        .await?
        .filter(|m| m.org_id == org_id)
        .ok_or_else(|| AppError::NotFound("No member for that card".into()))?;
    let settings = load_effective(pool, org_id, body.branch_id).await?;
    if !settings.enabled {
        return Err(refused(
            super::award::CODE_PROGRAM_OFF,
            "The loyalty program is switched off for this branch",
        ));
    }
    let actor = claims.user_id_safe().ok();

    let (branch_id, member_id, programme) = (body.branch_id, member.id, &settings);
    let answer = move |already: bool, gift: BirthdayGiftView| async move {
        let (catalogue, _) = load_effective_rewards(pool, org_id, branch_id).await?;
        let fresh = model::find_by_id(pool, member_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Member not found".into()))?;
        Ok::<_, AppError>(HttpResponse::Ok().json(CounterGiftResult {
            member: fresh.view(programme.mode(), model::card_size(programme, &catalogue)),
            birthday_gift: gift,
            already,
        }))
    };
    let mut tx = pool.begin().await?;
    let claimed: Option<DateTime<Utc>> = sqlx::query_scalar(
        "UPDATE loyalty_gifts \
            SET status = 'claimed', claimed_at = now(), claimed_source = 'counter', \
                claimed_branch_id = $3, claimed_by = $4, claim_key = $5 \
          WHERE id = (SELECT id FROM loyalty_gifts \
                       WHERE customer_id = $1 AND org_id = $2 AND status = 'open' \
                         AND valid_until > now() \
                       ORDER BY year DESC LIMIT 1 FOR UPDATE) \
            AND status = 'open' \
          RETURNING valid_until",
    )
    .bind(member.id)
    .bind(org_id)
    .bind(body.branch_id)
    .bind(actor)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await
    .or_else(|e| match &e {
        // A racing retry under the same key won: answered below.
        sqlx::Error::Database(d) if d.is_unique_violation() => Ok(None),
        _ => Err(e),
    })?;
    let Some(valid_until) = claimed else {
        drop(tx);
        let by_key: Option<(Uuid, DateTime<Utc>)> = sqlx::query_as(
            "SELECT customer_id, valid_until FROM loyalty_gifts WHERE org_id = $1 AND claim_key = $2",
        )
        .bind(org_id)
        .bind(key)
        .fetch_optional(pool)
        .await?;
        return match by_key {
            Some((c, until)) if c == member.id => {
                answer(
                    true,
                    BirthdayGiftView {
                        status: "claimed".into(),
                        valid_until: until,
                    },
                )
                .await
            }
            Some(_) => Err(refused(
                super::counter::CODE_KEY_REUSED,
                "That request key was already used for something else",
            )),
            None => Err(refused(
                CODE_GIFT_NONE,
                &format!("{} has no birthday gift to claim", member.name),
            )),
        };
    };
    sqlx::query(
        "INSERT INTO loyalty_transactions \
            (org_id, customer_id, branch_id, kind, currency, points, created_by, source, \
             idempotency_key, note) \
         VALUES ($1,$2,$3,'redeem',$4,0,$5,'gift',$6,'Birthday gift')",
    )
    .bind(org_id)
    .bind(member.id)
    .bind(body.branch_id)
    .bind(settings.mode().as_str())
    .bind(actor)
    .bind(key)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    answer(
        false,
        BirthdayGiftView {
            status: "claimed".into(),
            valid_until,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_day_reads_in_both_languages() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 21).unwrap();
        assert_eq!(day_label(d, false), "21 Nov");
        assert_eq!(day_label(d, true), "21 نوفمبر");
    }

    #[test]
    fn a_gift_covers_one_unit_of_its_own_line_only() {
        let p = GiftPlan {
            gift_id: None,
            member_id: Uuid::nil(),
            item_index: 2,
            menu_item_id: Uuid::nil(),
            currency: String::new(),
            refused: None,
        };
        assert_eq!(p.units_for(2), Some(1));
        assert_eq!(p.units_for(1), None);
    }
}
