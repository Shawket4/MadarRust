//! Members, their balances, and the two writes that move points.
//!
//! Both writes take a transaction rather than a pool: earning happens inside the
//! order's own transaction (so a sale and its points commit together or not at
//! all), and redeeming is a single statement whose guard lives in the database.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::earn::{self, EarnRule, Mode, OrderAmounts, OrderLine};
use super::settings::{LoyaltySettings, RewardItem, load_effective, load_effective_rewards};
use crate::errors::AppError;

/// Why a ledger row exists — `loyalty_transactions.source`.
///
/// The kind says what a row IS (an earn, a redeem, an adjustment, a reversal);
/// this says what CAUSED it. The two are paired by a CHECK in the database, so
/// an earn can only ever be a sale and a gift can never masquerade as one. Kept
/// as an enum here so a caller cannot misspell its way past that CHECK into a
/// 500 at the counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Points or stamps for a settled order.
    Sale,
    /// A reward handed over against an order line.
    Redemption,
    /// The order was torn up; its movements are undone.
    Void,
    /// Money went back to the customer; some or all of the earn follows it.
    Refund,
    /// The birthday gift.
    Birthday,
    /// The "we've missed you" sweetener.
    Winback,
    /// A person typed it.
    Manual,
    /// Two memberships of one person became one; the balance moved across.
    Merge,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Sale => "sale",
            Source::Redemption => "redemption",
            Source::Void => "void",
            Source::Refund => "refund",
            Source::Birthday => "birthday",
            Source::Winback => "winback",
            Source::Manual => "manual",
            Source::Merge => "merge",
        }
    }
}

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
    /// A message riding on the card itself — see `wallet::notices`. Present
    /// only while one is outstanding; the pass grows a row for it and loses the
    /// row again once it has been delivered or given up on.
    pub pass_notice: Option<String>,
}

/// Columns of [`MEMBERS`]. Name, phone, locale, birthday and the opt-out are
/// the CUSTOMER's (design §2.2); the view puts them back under the names the
/// card always used.
pub const MEMBER_COLS: &str = "id, org_id, name, phone, member_token, points_balance, \
    visits_balance, lifetime_points, lifetime_visits, locale, apple_serial, apple_auth_token, \
    google_object_id, pass_updated_at, joined_branch_id, enrolled_at, marketing_opt_out, \
    pass_notice";

/// The membership joined to its customer. Every read of a member goes through
/// this; writes go to `loyalty_customers` (programme) or `customers` (person).
pub const MEMBERS: &str = "loyalty_members_v";

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
    // A card retired by a merge keeps finding the member it was merged into
    // for ninety days (`loyalty_token_aliases`). A token that is itself live
    // always wins over an alias.
    Ok(sqlx::query_as(&format!(
        "SELECT {MEMBER_COLS} FROM {MEMBERS} \
          WHERE deleted_at IS NULL \
            AND (member_token = $1 \
                 OR id = (SELECT a.customer_id FROM loyalty_token_aliases a \
                           WHERE a.member_token = $1 AND a.expires_at > now())) \
          ORDER BY (member_token = $1) DESC LIMIT 1"
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
    // Through the customer's unique phone index, not the view's computed
    // column. `phone` is canonical (`crate::phone`).
    Ok(sqlx::query_as(&format!(
        "SELECT {MEMBER_COLS} FROM {MEMBERS} \
          WHERE deleted_at IS NULL \
            AND id = (SELECT c.id FROM customers c \
                       WHERE c.org_id = $1 AND c.phone_key = $2 \
                         AND c.merged_into IS NULL AND c.erased_at IS NULL)"
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
        "SELECT {MEMBER_COLS} FROM {MEMBERS} WHERE id = $1 AND deleted_at IS NULL"
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
///
/// `lines` and `eligible_item_ids` are what per-item stamps needs; both are
/// ignored in points mode and in a per-order stamps programme. They come from
/// the ORDER's own rows and the ORDER's branch settings — never from the
/// request — for the same reason the amounts do: a till sends who, never how
/// many, and that is what makes every path agree.
#[allow(clippy::too_many_arguments)]
pub async fn award_for_order(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    branch_id: Uuid,
    customer_id: Uuid,
    order_id: Uuid,
    amounts: OrderAmounts,
    lines: &[OrderLine],
    eligible_item_ids: &[Uuid],
    rule: EarnRule,
    enabled: bool,
    balance_cap: Option<i32>,
    created_by: Option<Uuid>,
) -> Result<i32, AppError> {
    if !enabled {
        return Ok(0);
    }
    let points = earn::points_for_order(amounts, lines, eligible_item_ids, rule);
    if points <= 0 {
        // A sale below one point still attaches the member to the order (the
        // caller sets `orders.loyalty_customer_id`); it just buys nothing.
        return Ok(0);
    }

    // The shop's ceiling, if it set one.
    //
    // The award is TRIMMED to the cap rather than refused. A full card is not
    // the customer's doing and must never fail their sale — they are standing
    // at a counter having already paid. What stops is the accrual, which is the
    // thing the shop asked to bound; a card at the ceiling simply stays there
    // until something is redeemed off it.
    //
    // Read inside the transaction, and in the ledger's currency: an org running
    // visits caps visits, and its points column is not what is being bounded.
    let points = match balance_cap {
        None => points,
        Some(cap) => {
            let current: i32 = sqlx::query_scalar(&format!(
                "SELECT {column} FROM loyalty_customers WHERE id = $1 FOR UPDATE",
                column = match rule.mode {
                    earn::Mode::Points => "points_balance",
                    earn::Mode::Visits => "visits_balance",
                }
            ))
            .bind(customer_id)
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or(0);
            let room = (cap - current).max(0);
            let trimmed = points.min(room);
            if trimmed <= 0 {
                // At the ceiling. Nothing is written, so the order is not
                // marked as earned either — and if the cap is later raised, the
                // sale can still be claimed.
                return Ok(0);
            }
            trimmed
        }
    };
    let inserted = sqlx::query(
        "INSERT INTO loyalty_transactions \
            (org_id, customer_id, branch_id, kind, currency, points, order_id, basis_piastres, \
             rate_piastres_per_point, created_by, source) \
         VALUES ($1,$2,$3,'earn',$4,$5,$6,$7,$8,$9,'sale') \
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

/// A movement that is a decision rather than a consequence of a sale: an
/// admin's correction in either direction, or a gift the programme hands out.
///
/// `source` says which — the ledger's CHECK only lets an `adjust` claim
/// `manual`, `birthday` or `winback`, and a report on "what did the programme
/// give away" is built on that column, so a gift must not arrive as `manual`.
///
/// A deduction may not overdraw. The database refuses that too (the balance
/// trigger lets only a void or refund clawback go below zero, and only where
/// the programme allows it); the check here is so an admin gets a sentence
/// rather than a constraint name.
#[allow(clippy::too_many_arguments)]
pub async fn adjust(
    pool: &PgPool,
    member: &MemberRow,
    branch_id: Uuid,
    mode: Mode,
    points: i32,
    source: Source,
    note: Option<String>,
    created_by: Option<Uuid>,
) -> Result<MemberView, AppError> {
    if points == 0 {
        return Err(AppError::BadRequest(
            "An adjustment of zero points changes nothing".into(),
        ));
    }
    if points < 0 && member.balance_in(mode) + points < 0 {
        return Err(AppError::BadRequest(format!(
            "{} has {}; that adjustment would go negative",
            member.name,
            member.balance_in(mode)
        )));
    }
    sqlx::query(
        "INSERT INTO loyalty_transactions \
            (org_id, customer_id, branch_id, kind, currency, points, note, created_by, source) \
         VALUES ($1,$2,$3,'adjust',$4,$5,$6,$7,$8)",
    )
    .bind(member.org_id)
    .bind(member.id)
    .bind(branch_id)
    .bind(mode.as_str())
    .bind(points)
    .bind(note)
    .bind(created_by)
    .bind(source.as_str())
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
    /// `earn`, `redeem`, `adjust`, or `reverse_earn` / `reverse_redeem` /
    /// `reverse_adjust` — the last three undo the row named in `reverses_id`.
    pub kind: String,
    /// Why the row exists: `sale`, `redemption`, `void`, `refund`, `birthday`,
    /// `winback` or `manual`. What a till or a dashboard should print as the
    /// reason, instead of guessing from the kind and the note.
    pub source: String,
    /// For a reversal, the row it undoes.
    pub reverses_id: Option<Uuid>,
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
    /// Who wrote the row: the teller who applied a reward or rang the sale, the
    /// admin who adjusted by hand. `None` for the system (birthday, win-back,
    /// a trigger with no actor).
    #[sqlx(default)]
    pub created_by: Option<Uuid>,
    #[sqlx(default)]
    pub created_by_name: Option<String>,
}

pub async fn ledger(
    pool: &PgPool,
    customer_id: Uuid,
    limit: i64,
) -> Result<Vec<LedgerEntry>, AppError> {
    Ok(sqlx::query_as(
        "SELECT t.id, t.kind::text AS kind, t.source, t.reverses_id, t.currency, t.points, \
                t.branch_id, b.name AS branch_name, \
                t.order_id, t.basis_piastres, m.name AS reward_name, t.note, t.created_at, \
                t.created_by, u.name AS created_by_name \
           FROM loyalty_transactions t \
           LEFT JOIN branches b ON b.id = t.branch_id \
           LEFT JOIN users u ON u.id = t.created_by \
           LEFT JOIN menu_items m ON m.id = t.reward_menu_item_id \
          WHERE t.customer_id = $1 ORDER BY t.created_at DESC LIMIT $2",
    )
    .bind(customer_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}
/// Forget a member: the person goes, the books stay.
///
/// The ledger is the SHOP's record of what it gave away and what it was owed —
/// deleting it would change the meaning of every past report, and the database
/// refuses to anyway (`loyalty_transactions` is append-only, and its FK to the
/// member is RESTRICT). So the membership is soft-deleted, and everything that
/// is about the PERSON rather than the money is scrubbed:
///
///   * the person lives on the customer row now (design §2.2), so that row is
///     erased exactly as `POST /customers/{id}/erase` erases it — name, phone,
///     notes and birthday blanked, `erased_at` set, marketing off. That frees
///     the phone: the same number can join again tomorrow as a fresh customer
///     with a fresh card. The numbers they used to have go too;
///   * the member token is rotated, so the barcode on a pass that is still in a
///     wallet resolves to nobody — `find_by_token` already skips deleted rows,
///     but a token that no longer exists cannot be un-skipped by a later bug —
///     and any alias that pointed an older card at this member is dropped;
///   * the Apple auth token goes with it, so a device holding the old pass can
///     no longer authenticate a refetch;
///   * pass devices are dropped, so no update is ever pushed to the phone again;
///   * any notice waiting on the card is cleared.
///
/// Orders keep their `loyalty_customer_id`: which member a sale earned for is
/// part of the sale's history, and the row it points at now says nothing about
/// anyone. Google's object is expired by the caller AFTER commit — it is a
/// network call, and the rule everywhere here is that those happen outside the
/// transaction.
///
/// Returns the row as it was before the scrub, so the caller still knows the
/// Google object to expire. `None` when the member was already gone.
pub async fn forget(pool: &PgPool, member_id: Uuid) -> Result<Option<MemberRow>, AppError> {
    let mut tx = pool.begin().await?;
    // Locked, so two admins forgetting the same member — or a sweep messaging
    // them at the same moment — serialise on the row.
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM loyalty_customers WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(member_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(None);
    }
    let Some(before) = find_by_id(&mut *tx, member_id).await? else {
        return Ok(None);
    };
    sqlx::query(
        "UPDATE loyalty_customers \
            SET deleted_at = now(), \
                member_token = $2, \
                apple_auth_token = NULL, \
                pass_notice = NULL, pass_notice_at = NULL, pass_notice_seen_at = NULL, \
                pass_notice_wallets = NULL, pass_notice_fallback = NULL, \
                updated_at = now() \
          WHERE id = $1",
    )
    .bind(member_id)
    .bind(super::mint_member_token())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE customers \
            SET name = '', phone = NULL, phone_key = NULL, notes = NULL, \
                birth_month = NULL, birth_day = NULL, marketing_opt_out = true, \
                erased_at = now(), updated_at = now() \
          WHERE id = $1 AND erased_at IS NULL",
    )
    .bind(member_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM customer_phone_history WHERE customer_id = $1")
        .bind(member_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM loyalty_token_aliases WHERE customer_id = $1")
        .bind(member_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM loyalty_pass_devices WHERE customer_id = $1")
        .bind(member_id)
        .execute(&mut *tx)
        .await?;
    // The pre-built pass has their name baked into it. In the transaction, so
    // there is no moment at which the person is erased and the bytes are not.
    sqlx::query("DELETE FROM loyalty_pass_cache WHERE customer_id = $1")
        .bind(member_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(before))
}

/// Give a customer a card: the membership row, under THE CUSTOMER'S id (design
/// §2.1). `false` when that customer already has one.
///
/// A membership that was soft-deleted without its customer being erased (no
/// path does that today; "leave the programme" will) is brought back with a
/// fresh token rather than refused: the primary key is the person, and a person
/// may join again.
pub async fn enrol(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    customer_id: Uuid,
    joined_branch_id: Option<Uuid>,
) -> Result<bool, AppError> {
    let done = sqlx::query(
        "INSERT INTO loyalty_customers (id, org_id, member_token, joined_branch_id, apple_auth_token) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (id) DO UPDATE \
            SET deleted_at = NULL, member_token = EXCLUDED.member_token, \
                apple_auth_token = EXCLUDED.apple_auth_token, \
                joined_branch_id = EXCLUDED.joined_branch_id, \
                apple_serial = NULL, google_object_id = NULL, \
                enrolled_at = now(), updated_at = now() \
          WHERE loyalty_customers.deleted_at IS NOT NULL \
            AND loyalty_customers.org_id = EXCLUDED.org_id",
    )
    .bind(customer_id)
    .bind(org_id)
    .bind(super::mint_member_token())
    // Reporting only, and honestly null for an org-wide code: we do not know
    // where they were, and a membership belongs to the shop rather than to a
    // branch.
    .bind(joined_branch_id)
    // Apple authenticates pass updates with this; minted now so a pass issued
    // later needs no second write.
    .bind(super::mint_member_token())
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Two customers being merged are BOTH members (design §2.7): the loser's
/// balances cross to the survivor, the loser's membership is retired, and its
/// card keeps scanning — to the survivor — for ninety days.
///
/// The balance moves as a PAIR of `adjust` rows with `source = 'merge'`, one on
/// each member. Nothing is edited and nothing is reversed, so the append-only
/// ledger holds; and because both are adjustments the pair nets to zero in any
/// report of what the programme gave away. A debt (possible only where the
/// programme lets a clawback go negative) crosses too, as far as the
/// survivor's balance can absorb it — an adjustment may not overdraw.
///
/// Both customer rows are already locked by the caller. Returns the loser as
/// it was, so the caller can void its passes after the commit.
pub async fn merge_memberships(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    loser_id: Uuid,
    survivor_id: Uuid,
    actor: Option<Uuid>,
) -> Result<MemberRow, AppError> {
    let mut ids = [loser_id, survivor_id];
    ids.sort();
    sqlx::query(
        "SELECT id FROM loyalty_customers WHERE id = ANY($1) AND org_id = $2 ORDER BY id FOR UPDATE",
    )
    .bind(&ids[..])
    .bind(org_id)
    .fetch_all(&mut *conn)
    .await?;
    let loser = find_by_id(&mut *conn, loser_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member not found".into()))?;
    let survivor = find_by_id(&mut *conn, survivor_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member to keep not found".into()))?;
    if loser.org_id != org_id || survivor.org_id != org_id {
        return Err(AppError::NotFound("Member not found".into()));
    }

    let moves: Vec<(Mode, i32)> = [Mode::Points, Mode::Visits]
        .into_iter()
        .filter_map(|mode| {
            let have = loser.balance_in(mode);
            let delta = if have >= 0 {
                have
            } else {
                have.max(-survivor.balance_in(mode).max(0))
            };
            (delta != 0).then_some((mode, delta))
        })
        .collect();
    if !moves.is_empty() {
        // A ledger row names a branch. Where the loser joined is the honest
        // one; any branch of the org can carry it.
        let branch: Option<Uuid> = sqlx::query_scalar(
            "SELECT COALESCE($2, $3, (SELECT id FROM branches WHERE org_id = $1 \
                                        AND deleted_at IS NULL ORDER BY created_at LIMIT 1))",
        )
        .bind(org_id)
        .bind(loser.joined_branch_id)
        .bind(survivor.joined_branch_id)
        .fetch_one(&mut *conn)
        .await?;
        let branch = branch.ok_or_else(|| {
            AppError::Conflict("This organisation has no branch to record the transfer at".into())
        })?;
        for (mode, delta) in moves {
            for (member, points, other) in [
                (loser_id, -delta, survivor_id),
                (survivor_id, delta, loser_id),
            ] {
                sqlx::query(
                    "INSERT INTO loyalty_transactions \
                        (org_id, customer_id, branch_id, kind, currency, points, note, created_by, source) \
                     VALUES ($1,$2,$3,'adjust',$4,$5,$6,$7,'merge')",
                )
                .bind(org_id)
                .bind(member)
                .bind(branch)
                .bind(mode.as_str())
                .bind(points)
                .bind(format!("Merged with member {other}"))
                .bind(actor)
                .execute(&mut *conn)
                .await?;
            }
        }
    }

    sqlx::query(
        "UPDATE loyalty_customers \
            SET deleted_at = now(), \
                pass_notice = NULL, pass_notice_at = NULL, pass_notice_seen_at = NULL, \
                pass_notice_wallets = NULL, pass_notice_fallback = NULL, \
                updated_at = now() \
          WHERE id = $1",
    )
    .bind(loser_id)
    .execute(&mut *conn)
    .await?;
    // Cards that already pointed at the loser (an earlier merge) follow it.
    sqlx::query(
        "UPDATE loyalty_token_aliases SET customer_id = $2, was_customer_id = COALESCE(was_customer_id, $1) \
          WHERE customer_id = $1",
    )
    .bind(loser_id)
    .bind(survivor_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO loyalty_token_aliases (member_token, org_id, customer_id, was_customer_id) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (member_token) DO UPDATE \
            SET customer_id = EXCLUDED.customer_id, expires_at = EXCLUDED.expires_at",
    )
    .bind(&loser.member_token)
    .bind(org_id)
    .bind(survivor_id)
    .bind(loser_id)
    .execute(&mut *conn)
    .await?;
    // Both pre-built passes are wrong now: the loser's is a retired card with
    // its name on it, the survivor's shows the balance before the transfer.
    sqlx::query("DELETE FROM loyalty_pass_cache WHERE customer_id = ANY($1)")
        .bind(&[loser_id, survivor_id][..])
        .execute(&mut *conn)
        .await?;
    Ok(loser)
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
            pass_notice: None,
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
