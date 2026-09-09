//! Members, their balances, and the two writes that move points.
//!
//! Both writes take a transaction rather than a pool: earning happens inside the
//! order's own transaction (so a sale and its points commit together or not at
//! all), and redeeming is a single statement whose guard lives in the database.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::earn::{self, EarnRule, Mode, OrderAmounts};
use super::settings::{LoyaltySettings, RewardItem, load_effective, load_effective_rewards};
use crate::errors::AppError;

/// A member as the teller, the admin and the pass all see them.
///
/// Both balances travel, because an org may switch mode (or run points at one
/// branch and stamps at another) and what a customer earned under the old rules
/// is still theirs. `mode` says which one is LIVE where the question was asked,
/// and `balance` is that one — so a caller never has to pick.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MemberView {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub phone: String,
    pub points_balance: i32,
    pub visits_balance: i32,
    pub lifetime_points: i32,
    pub lifetime_visits: i32,
    /// `"points"` or `"visits"` — what the branch that asked collects.
    pub mode: String,
    /// The live balance, in `mode`'s currency.
    pub balance: i32,
    /// The cheapest reward on offer here, in `mode`'s currency — what the
    /// progress line counts towards. Falls back to the scope's default cost
    /// when no rewards have been curated.
    pub next_reward_cost: i32,
    /// How many rewards the balance has ALREADY earned.
    ///
    /// A card does not stop at full. Six stamps against a five-stamp reward is
    /// one reward earned and one stamp towards the next, not "five and a bit
    /// wasted" — and a customer who has been in eleven times is owed two
    /// rewards, whether or not they claimed the first.
    pub rewards_ready: i32,
    /// Progress towards the NEXT reward, after the earned ones are set aside.
    /// `balance % next_reward_cost`.
    pub progress_to_next: i32,
    /// What that next reward still needs. Equals `next_reward_cost` on an exact
    /// multiple, because a fresh card is the honest thing to show there.
    pub points_to_next_reward: i32,
    /// The balance affords at least one reward on offer here.
    pub can_redeem: bool,
    pub locale: String,
    pub enrolled_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MemberRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub phone: String,
    pub member_token: String,
    /// The branch whose counter code recruited them. Reporting — and the best
    /// guess at where they actually shop, for choosing which branches fit on
    /// their card.
    pub joined_branch_id: Option<Uuid>,
    pub points_balance: i32,
    pub visits_balance: i32,
    pub lifetime_points: i32,
    pub lifetime_visits: i32,
    pub locale: String,
    pub apple_serial: Option<String>,
    pub apple_auth_token: Option<String>,
    pub google_object_id: Option<String>,
    /// When the pass this member holds was last rebuilt — the tag Apple's
    /// devices ask "has anything changed since?" against.
    pub pass_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub enrolled_at: chrono::DateTime<chrono::Utc>,
    /// They have asked this shop to stop sending them things. One flag for
    /// every unprompted message, not one per campaign.
    pub marketing_opt_out: bool,
}

pub const MEMBER_COLS: &str = "id, org_id, name, phone, member_token, points_balance, \
    visits_balance, lifetime_points, lifetime_visits, locale, apple_serial, apple_auth_token, \
    google_object_id, pass_updated_at, joined_branch_id, enrolled_at, marketing_opt_out";

impl MemberRow {
    /// The balance that counts under `mode`.
    pub fn balance_in(&self, mode: Mode) -> i32 {
        match mode {
            Mode::Points => self.points_balance,
            Mode::Visits => self.visits_balance,
        }
    }

    /// Dress the row with the mode and cheapest reward in force where it was
    /// asked for.
    pub fn view(self, mode: Mode, next_reward_cost: i32) -> MemberView {
        let balance = self.balance_in(mode);
        let (rewards_ready, progress_to_next) = earned_and_progress(balance, next_reward_cost);
        MemberView {
            balance,
            mode: mode.as_str().into(),
            next_reward_cost,
            rewards_ready,
            progress_to_next,
            points_to_next_reward: (next_reward_cost - progress_to_next).max(0),
            can_redeem: rewards_ready > 0,
            id: self.id,
            org_id: self.org_id,
            name: self.name,
            phone: self.phone,
            points_balance: self.points_balance,
            visits_balance: self.visits_balance,
            lifetime_points: self.lifetime_points,
            lifetime_visits: self.lifetime_visits,
            locale: self.locale,
            enrolled_at: self.enrolled_at,
        }
    }
}

/// What the card counts towards, for this scope.
///
/// Normally the cheapest reward on offer: a customer who can afford the espresso
/// HAS earned a reward, whatever the cake costs. With `reward_any_item` on there
/// is no cheapest — every item costs the same — so the scope's default is the
/// answer, and reading the catalogue there would aim the card at a price that no
/// longer applies to anything.
pub fn reward_target(settings: &LoyaltySettings, rewards: &[RewardItem]) -> i32 {
    if settings.reward_any_item {
        return settings.default_reward_cost;
    }
    cheapest_cost(rewards).unwrap_or(settings.default_reward_cost)
}

/// Split a balance into rewards already earned and progress towards the next.
///
/// The card does not stop at full, which is the whole point: six stamps against
/// a five-stamp reward is ONE earned and ONE towards the next. Showing that as a
/// full card and nothing else tells a customer their sixth visit did not count.
///
/// A zero or negative cost earns nothing rather than dividing by it — the column
/// is `CHECK (> 0)`, but a card telling every customer they had infinite rewards
/// would be a poor way to discover otherwise.
pub fn earned_and_progress(balance: i32, cost: i32) -> (i32, i32) {
    if cost <= 0 || balance <= 0 {
        return (0, balance.max(0));
    }
    (balance / cost, balance % cost)
}

/// The cheapest reward on offer, or `None` when the catalogue is empty.
///
/// This is what the pass counts towards. Aiming at the cheapest is the only
/// target that is always true: a customer who can afford the espresso HAS
/// earned a reward, whatever the cake costs.
///
/// Takes no mode. A catalogue arrives priced in its scope's own currency
/// (`settings::in_mode`), so there is nothing to filter by — and filtering by
/// the row's stored copy is what made this return `None` for a shop with a full
/// catalogue, quietly resetting every customer's target to the program default.
pub fn cheapest_cost(rewards: &[RewardItem]) -> Option<i32> {
    rewards.iter().map(|r| r.cost_amount).min()
}

/// Look a member up by the token their pass barcode carries.
///
/// The token is globally unique and carries no org, because that is all a
/// scanned QR gives us — the caller checks that the member's org matches theirs.
pub async fn find_by_token<'e, E>(exec: E, token: &str) -> Result<Option<MemberRow>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_as(&format!(
        "SELECT {MEMBER_COLS} FROM loyalty_customers \
         WHERE member_token = $1 AND deleted_at IS NULL"
    ))
    .bind(token)
    .fetch_optional(exec)
    .await?)
}

/// The manual fallback for a customer whose phone is dead.
pub async fn find_by_phone<'e, E>(
    exec: E,
    org_id: Uuid,
    phone: &str,
) -> Result<Option<MemberRow>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_as(&format!(
        "SELECT {MEMBER_COLS} FROM loyalty_customers \
         WHERE org_id = $1 AND phone = $2 AND deleted_at IS NULL"
    ))
    .bind(org_id)
    .bind(phone)
    .fetch_optional(exec)
    .await?)
}

pub async fn find_by_id<'e, E>(exec: E, id: Uuid) -> Result<Option<MemberRow>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_as(&format!(
        "SELECT {MEMBER_COLS} FROM loyalty_customers WHERE id = $1 AND deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(exec)
    .await?)
}

/// The member plus the numbers in force at a branch, and what they could claim.
pub async fn member_with_context(
    pool: &PgPool,
    row: MemberRow,
    branch_id: Uuid,
) -> Result<(MemberView, Vec<RewardItem>), AppError> {
    let settings = load_effective(pool, row.org_id, branch_id).await?;
    let (rewards, _) = load_effective_rewards(pool, row.org_id, branch_id).await?;
    let mode = settings.mode();
    let target = reward_target(&settings, &rewards);
    Ok((row.view(mode, target), rewards))
}

/// Award the points a settled order is worth.
///
/// Called from inside `create_order_inner`'s transaction, on both the live and
/// the `/sync/replay` path, so an offline till awards on drain with no extra
/// code. Returns the points awarded (0 when the program is off for the branch,
/// the sale was too small, or this order already earned).
///
/// Idempotency is the database's: `loyalty_transactions_earn_order_key` allows
/// one earn per order, so a replayed order cannot award twice however many times
/// it is flushed.
#[allow(clippy::too_many_arguments)]
pub async fn award_for_order(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    branch_id: Uuid,
    customer_id: Uuid,
    order_id: Uuid,
    amounts: OrderAmounts,
    rule: EarnRule,
    enabled: bool,
    created_by: Option<Uuid>,
) -> Result<i32, AppError> {
    if !enabled {
        return Ok(0);
    }
    let points = earn::points_for(amounts, rule);
    if points <= 0 {
        // A sale below one point still attaches the member to the order (the
        // caller sets `orders.loyalty_customer_id`); it just buys nothing.
        return Ok(0);
    }
    let inserted = sqlx::query(
        "INSERT INTO loyalty_transactions \
            (org_id, customer_id, branch_id, kind, currency, points, order_id, basis_piastres, \
             rate_piastres_per_point, created_by) \
         VALUES ($1,$2,$3,'earn',$4,$5,$6,$7,$8,$9) \
         ON CONFLICT DO NOTHING",
    )
    .bind(org_id)
    .bind(customer_id)
    .bind(branch_id)
    .bind(rule.mode.as_str())
    .bind(points)
    .bind(order_id)
    .bind(earn::basis_piastres(amounts, rule))
    .bind(rule.piastres_per_point)
    .bind(created_by)
    .execute(&mut **tx)
    .await?;
    // Zero rows means this order had already earned — a replay, not a failure.
    Ok(if inserted.rows_affected() == 1 {
        points
    } else {
        0
    })
}

/// An admin correction, in either direction.
pub async fn adjust(
    pool: &PgPool,
    member: &MemberRow,
    branch_id: Uuid,
    mode: Mode,
    points: i32,
    note: Option<String>,
    created_by: Option<Uuid>,
) -> Result<MemberView, AppError> {
    if points == 0 {
        return Err(AppError::BadRequest(
            "An adjustment of zero points changes nothing".into(),
        ));
    }
    if member.balance_in(mode) + points < 0 {
        return Err(AppError::BadRequest(format!(
            "{} has {}; that adjustment would go negative",
            member.name,
            member.balance_in(mode)
        )));
    }
    sqlx::query(
        "INSERT INTO loyalty_transactions \
            (org_id, customer_id, branch_id, kind, currency, points, note, created_by) \
         VALUES ($1,$2,$3,'adjust',$4,$5,$6,$7)",
    )
    .bind(member.org_id)
    .bind(member.id)
    .bind(branch_id)
    .bind(mode.as_str())
    .bind(points)
    .bind(note)
    .bind(created_by)
    .execute(pool)
    .await?;
    let fresh = find_by_id(pool, member.id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member not found".into()))?;
    Ok(fresh.view(mode, 0))
}

/// One line of a member's history.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct LedgerEntry {
    pub id: Uuid,
    pub kind: String,
    /// `"points"` or `"visits"` — which balance this row moved.
    pub currency: String,
    pub points: i32,
    pub branch_id: Uuid,
    pub branch_name: Option<String>,
    pub order_id: Option<Uuid>,
    /// Piastres the rule was applied to (earns only).
    pub basis_piastres: Option<i32>,
    pub reward_name: Option<String>,
    pub note: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn ledger(
    pool: &PgPool,
    customer_id: Uuid,
    limit: i64,
) -> Result<Vec<LedgerEntry>, AppError> {
    Ok(sqlx::query_as(
        "SELECT t.id, t.kind::text AS kind, t.currency, t.points, t.branch_id, b.name AS branch_name, \
                t.order_id, t.basis_piastres, m.name AS reward_name, t.note, t.created_at \
           FROM loyalty_transactions t \
           LEFT JOIN branches b ON b.id = t.branch_id \
           LEFT JOIN menu_items m ON m.id = t.reward_menu_item_id \
          WHERE t.customer_id = $1 ORDER BY t.created_at DESC LIMIT $2",
    )
    .bind(customer_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}
#[cfg(test)]
mod overflow_tests {
    use super::*;

    #[test]
    fn a_full_card_starts_the_next_one() {
        // The report this exists for: six orders against a five-order reward.
        // One earned, one towards the next — not "five and a bit wasted".
        assert_eq!(earned_and_progress(6, 5), (1, 1));
        // Exactly full: one earned, and a FRESH card rather than a stuck one.
        assert_eq!(earned_and_progress(5, 5), (1, 0));
        // Someone who has not claimed in a while is owed more than one.
        assert_eq!(earned_and_progress(11, 5), (2, 1));
        assert_eq!(earned_and_progress(20, 5), (4, 0));
        // Below the first target, nothing is earned yet.
        assert_eq!(earned_and_progress(3, 5), (0, 3));
        assert_eq!(earned_and_progress(0, 5), (0, 0));
    }

    #[test]
    fn a_broken_target_earns_nothing_rather_than_everything() {
        // The column is CHECK (> 0), but dividing by it anyway would tell every
        // customer they had infinite rewards, which is a poor way to find out.
        assert_eq!(earned_and_progress(9, 0), (0, 9));
        assert_eq!(earned_and_progress(9, -5), (0, 9));
        // A negative balance is an adjustment gone past zero, not a reward.
        assert_eq!(earned_and_progress(-3, 5), (0, 0));
    }

    #[test]
    fn the_view_counts_earned_cards_not_just_a_full_one() {
        let m = MemberRow {
            id: Uuid::nil(),
            org_id: Uuid::nil(),
            name: "Ali".into(),
            phone: "201000000001".into(),
            member_token: "Mtoken".into(),
            points_balance: 0,
            visits_balance: 6,
            lifetime_points: 0,
            lifetime_visits: 6,
            locale: "en".into(),
            apple_serial: None,
            apple_auth_token: None,
            google_object_id: None,
            pass_updated_at: None,
            joined_branch_id: None,
            enrolled_at: chrono::Utc::now(),
            marketing_opt_out: false,
        };
        let v = m.view(Mode::Visits, 5);
        assert_eq!(v.balance, 6);
        assert_eq!(v.rewards_ready, 1, "the sixth order did not vanish");
        assert_eq!(v.progress_to_next, 1);
        assert_eq!(v.points_to_next_reward, 4);
        assert!(v.can_redeem);
    }

    #[test]
    fn any_item_mode_aims_the_card_at_the_flat_price() {
        let mut s = LoyaltySettings::defaults(Uuid::nil(), None);
        s.default_reward_cost = 8;
        let catalogue = vec![RewardItem {
            menu_item_id: Uuid::nil(),
            name: "Espresso".into(),
            image_url: None,
            base_price: 5000,
            cost_currency: "visits".into(),
            cost_amount: 3,
            sort_order: 0,
        }];
        // Normally the cheapest thing on offer is what the card counts towards.
        assert_eq!(reward_target(&s, &catalogue), 3);
        // With any item claimable there is no cheapest — everything costs the
        // same — and aiming at 3 would point the card at a price that no longer
        // applies to anything.
        s.reward_any_item = true;
        assert_eq!(reward_target(&s, &catalogue), 8);
        assert_eq!(reward_target(&s, &[]), 8);
    }
}
