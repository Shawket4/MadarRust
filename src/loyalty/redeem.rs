//! Spending a balance on lines of a cart.
//!
//! A reward covers whole units of one line, and a basket may carry several — a
//! free coffee among four paid ones, or a free coffee and a free pastry when the
//! balance runs to both. The till names a member and some line indices; every
//! price in here is the server's.
//!
//! Redemption happens at TENDER, not after, because it changes what is owed.
//! (Earning is the opposite: it is a separate act with a 24-hour window — see
//! `loyalty::award`. Pay less now, collect after.)
//!
//! ## Why this cannot be done offline
//! A balance is shared state that any till in the org can spend, and a
//! redemption gives away goods. Two offline tills could each honour the last
//! reward and neither could be undone — the coffee is gone. So the POS refuses
//! to apply a reward while disconnected, and this module is only ever reached
//! by a live request.

use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::model::{self, MemberRow};
use super::settings::{load_effective, load_effective_rewards};
use crate::errors::AppError;
use crate::orders::handlers::{LoyaltyRedemptionInput, OrderItemInput};

/// One priced redemption, ready to apply.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlannedRedemption {
    pub item_index: usize,
    pub menu_item_id: Uuid,
    pub units: i32,
    /// `"points"` or `"visits"`.
    pub currency: String,
    /// Total spent for this line: the reward's cost × units.
    pub cost: i32,
    /// The stamp track that pays: 1 (the programme's card) or 2–4.
    pub track: u8,
}

/// The whole plan for one sale. Empty when nothing was redeemed, which is the
/// overwhelmingly common case and costs nothing to carry.
#[derive(Debug, Clone, Default)]
pub struct RedemptionPlan {
    pub member: Option<MemberRow>,
    pub lines: Vec<PlannedRedemption>,
    /// Why the rewards could not be paid for with points, on a REPLAYED sale.
    ///
    /// A live sale that cannot honour its rewards is refused before anything
    /// is handed over. A replayed one already happened: the till collected the
    /// reduced amount and the customer left with the item. Refusing it would
    /// lose the whole sale over one coffee, so the lines stay covered (the
    /// money in the drawer is the truth), no points move, and the order is
    /// flagged with this sentence for a manager to settle with the customer.
    pub refused: Option<String>,
}

impl RedemptionPlan {
    /// Units of `line_index` a reward pays for, if any.
    pub fn units_for(&self, line_index: usize) -> Option<i32> {
        self.lines
            .iter()
            .find(|l| l.item_index == line_index)
            .map(|l| l.units)
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// What the plan spends, in the member's currency.
    pub fn spent(&self) -> i32 {
        self.lines
            .iter()
            .fold(0i32, |a, l| a.saturating_add(l.cost))
    }
}

/// Minor units a reward takes off one line: whole units at the price the line
/// was charged per unit, modifiers included, never more than the line itself.
/// The one rule both sides share, in madar-shared (`madar_money::loyalty`),
/// pinned by its `loyalty_reward_vectors.json`.
pub use madar_money::loyalty::covered_minor;

/// Price and validate the rewards a sale wants to spend.
///
/// Everything that could make a redemption dishonest is refused here, before a
/// single price is computed: an unknown member, another tenant's member, a line
/// that is not a reward at this branch, more units than the line holds, or a
/// balance that does not cover it.
///
/// `lenient` is the REPLAY of a sale that already happened (see
/// [`RedemptionPlan::refused`]): nothing is refused, lines that cannot be
/// covered at all (no such line, a line with no menu item) are dropped, units are clamped to
/// the line, and the reason the points could not pay is kept.
pub async fn plan(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    customer_id: Option<Uuid>,
    redemptions: &[LoyaltyRedemptionInput],
    items: &[OrderItemInput],
    lenient: bool,
) -> Result<RedemptionPlan, AppError> {
    if redemptions.is_empty() {
        return Ok(RedemptionPlan::default());
    }
    let refused = match plan_strict(pool, org_id, branch_id, customer_id, redemptions, items).await
    {
        Ok(p) => return Ok(p),
        Err(AppError::BadRequest(m) | AppError::Conflict(m) | AppError::NotFound(m)) if lenient => {
            m
        }
        Err(e) => return Err(e),
    };
    let member = match customer_id {
        Some(id) => model::find_by_id(pool, id)
            .await?
            .filter(|m| m.org_id == org_id),
        None => None,
    };
    Ok(RedemptionPlan {
        member,
        lines: structural_lines(redemptions, items),
        refused: Some(refused),
    })
}

/// The sale's lines as madar-shared's planner reads them. The staff-drink pairing is not the planner's to judge here: the
/// order path refuses it live, with its own message.
fn plan_lines(items: &[OrderItemInput]) -> Vec<madar_loyalty::Line> {
    items
        .iter()
        .map(|i| madar_loyalty::Line {
            menu_item_id: i.menu_item_id.map(|id| id.to_string()),
            quantity: i64::from(i.quantity),
            is_staff_drink: false,
            // A combo line (a header or its parts) never takes a reward (C7).
            in_combo: i.combo.is_some(),
        })
        .collect()
}

fn plan_asks(redemptions: &[LoyaltyRedemptionInput]) -> Vec<madar_loyalty::Ask> {
    redemptions
        .iter()
        .map(|r| madar_loyalty::Ask {
            line: r.item_index,
            units: r.units.map(i64::from),
        })
        .collect()
}

fn planned(p: madar_loyalty::Planned, currency: &str) -> PlannedRedemption {
    PlannedRedemption {
        item_index: p.line,
        menu_item_id: Uuid::parse_str(&p.menu_item_id).unwrap_or_default(),
        units: i32::try_from(p.units).unwrap_or(i32::MAX),
        currency: currency.into(),
        cost: i32::try_from(p.cost).unwrap_or(i32::MAX),
        track: 1,
    }
}

/// The lines a replayed sale covered, with no points attached: what the till
/// took off the bill, as far as it can be priced at all (madar-shared's
/// `madar_loyalty::replay_lines`).
fn structural_lines(
    redemptions: &[LoyaltyRedemptionInput],
    items: &[OrderItemInput],
) -> Vec<PlannedRedemption> {
    madar_loyalty::replay_lines(&plan_lines(items), &plan_asks(redemptions))
        .into_iter()
        .map(|p| {
            let track = redemptions
                .iter()
                .find(|r| r.item_index == Some(p.line))
                .and_then(|r| r.track)
                .unwrap_or(1);
            PlannedRedemption {
                track,
                ..planned(p, "")
            }
        })
        .collect()
}

/// A strict plan's refusal, as this server has always said it.
fn refused(r: madar_loyalty::Refusal, member_name: &str) -> AppError {
    use madar_loyalty::Refusal as R;
    match r {
        R::NoLineNamed => AppError::BadRequest("Name the line to reward".into()),
        R::NoSuchLine { line } => AppError::BadRequest(format!("No line {line} to reward")),
        // One redemption per line: the ledger's uniqueness is (order, line), so
        // two rows for one line could not both be recorded, and a silently
        // dropped one is a free item nobody was charged for.
        R::TwiceOnOneLine => {
            AppError::BadRequest("One reward per line — raise the units instead".into())
        }
        R::BelowOneUnit => AppError::BadRequest("A reward must cover at least one unit".into()),
        R::MoreUnitsThanLine { have, asked } => AppError::BadRequest(format!(
            "That line has {have} of them; a reward cannot cover {asked}"
        )),
        // A line with no menu item has nothing to be a reward of; the order
        // path no longer accepts such a line.
        R::NoMenuItem => AppError::BadRequest("That line has no menu item to reward".into()),
        // C7: stamps per item inside a combo, but never a reward.
        R::InCombo => AppError::Coded {
            status: 409,
            code: "REWARD_IN_COMBO",
            reason: "Rewards can't be used inside a combo.".into(),
        },
        R::NotOnOffer => AppError::BadRequest("That item is not a reward at this branch".into()),
        // A reward priced at nothing is a free item bounded by nothing but the
        // cap. Refused rather than honoured, whatever the catalogue row says.
        R::NoPrice => AppError::Conflict(
            "That reward has no price set — ask a manager to fix the catalogue".into(),
        ),
        R::OverCap { max, claimed } => AppError::Conflict(if max == 1 {
            "Only one reward per order here — take the rest next time".into()
        } else {
            format!("Only {max} rewards per order here; this order claims {claimed}")
        }),
        R::BalanceShort { balance, spent } => AppError::Conflict(format!(
            "{member_name} has {balance}; those rewards cost {spent}"
        )),
    }
}

/// A refusal of the planner across tracks: track 1's as ever, a track 2–4's
/// with a stable code the till words (POS_MULTITRACK_DESIGN.md §4.4 #11).
fn refused_on_track(r: madar_loyalty::tracks::TrackRefusal, member_name: &str) -> AppError {
    use madar_loyalty::Refusal as R;
    use madar_loyalty::tracks::TrackRefusalKind as K;
    match (r.track, r.refusal) {
        (t, K::TrackUnknown) => AppError::Refused {
            code: super::tracks::CODE_TRACK_UNKNOWN,
            reason: format!("This card has no track {}", t.unwrap_or(0)),
        },
        (Some(t), K::Plan { reason: R::NotOnOffer }) if t > 1 => AppError::Refused {
            code: super::tracks::CODE_TRACK_NOT_ON_OFFER,
            reason: format!("That item is not one of track {t}'s rewards"),
        },
        (Some(t), K::Plan { reason: R::BalanceShort { balance, spent } }) if t > 1 => {
            AppError::Refused {
                code: super::tracks::CODE_TRACK_BALANCE_SHORT,
                reason: format!("{member_name} has {balance} on track {t}; those rewards cost {spent}"),
            }
        }
        (_, K::Plan { reason }) => refused(reason, member_name),
    }
}

async fn plan_strict(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    customer_id: Option<Uuid>,
    redemptions: &[LoyaltyRedemptionInput],
    items: &[OrderItemInput],
) -> Result<RedemptionPlan, AppError> {
    let customer_id = customer_id
        .ok_or_else(|| AppError::BadRequest("A reward needs the member it belongs to".into()))?;

    let member = model::find_by_id(pool, customer_id)
        .await?
        .filter(|m| m.org_id == org_id)
        .ok_or_else(|| AppError::NotFound("No member for that card".into()))?;

    let settings = load_effective(pool, org_id, branch_id).await?;
    if !settings.enabled {
        return Err(AppError::Conflict(
            "The loyalty program is switched off for this branch".into(),
        ));
    }
    let mode = settings.mode();
    let (catalogue, _) = load_effective_rewards(pool, org_id, branch_id).await?;

    // Which of the asked rewards the sale takes: madar-shared's planner, in
    // its strict (server) mode — the till trims with the same rules, so a sale
    // it sends is one this does not refuse. By this point every index is
    // resolved: the cart path sends it, and the ticket-settle path has had
    // `ticket_line_id` translated into it. Priced in this branch's currency
    // by the loader, so the item id is the whole question.
    //
    // The shop's ceiling is counted in ITEMS, not lines (a single line with
    // six units is six free coffees), and refused rather than trimmed: the
    // customer has not paid yet and the teller has not promised anything. The
    // balance is checked against the whole basket, not one line at a time: a
    // balance that covers the first reward but not the second must fail the
    // sale, not hand over half of what the teller told the customer.
    let programme = madar_loyalty::Programme {
        rewards: catalogue
            .iter()
            .map(|c| madar_loyalty::Reward {
                menu_item_id: c.menu_item_id.to_string(),
                cost: i64::from(c.cost_amount),
            })
            .collect(),
        // "Collect five, get anything." The catalogue stops being a list of
        // what may be claimed and the scope's default cost applies to
        // everything — per-item pricing is what the catalogue is FOR, so the
        // two are alternatives rather than layers.
        any_item: settings.reward_any_item,
        any_item_cost: i64::from(settings.default_reward_cost),
        max_per_order: settings.max_rewards_per_order.map(i64::from),
        balance: i64::from(member.balance_in(mode)),
    };
    // A sale asking a stamp track 2–4 for a reward: madar-shared's planner
    // across tracks (one reward per line, each track its own balance, one
    // shared ceiling). A sale on track 1 alone never reaches it.
    if redemptions.iter().any(|r| r.track.unwrap_or(1) != 1) {
        let defs = super::tracks::load(pool, org_id, mode).await?;
        let balances = super::tracks::balances(pool, member.id).await?;
        let mut programmes = vec![madar_loyalty::tracks::TrackProgramme {
            track: 1,
            programme: programme.clone(),
        }];
        for d in defs.iter().filter(|d| d.position > 1) {
            programmes.push(madar_loyalty::tracks::track_programme(
                d.position,
                &d.items.iter().map(Uuid::to_string).collect::<Vec<_>>(),
                i64::from(d.size.unwrap_or(0)),
                i64::from(balances.get(&d.position).copied().unwrap_or(0)),
            ));
        }
        let asks: Vec<madar_loyalty::tracks::TrackAsk> = redemptions
            .iter()
            .map(|r| madar_loyalty::tracks::TrackAsk {
                line: r.item_index,
                units: r.units.map(i64::from),
                track: r.track,
            })
            .collect();
        let plan = madar_loyalty::tracks::plan_tracks(
            &plan_lines(items),
            &programmes,
            &asks,
            settings.max_rewards_per_order.map(i64::from),
            madar_loyalty::Mode::Server,
        )
        .map_err(|r| refused_on_track(r, &member.name))?;
        let lines = plan
            .tracks
            .into_iter()
            .flat_map(|t| {
                let track = t.track;
                let currency = if track == 1 { mode.as_str() } else { "visits" };
                t.plan.lines.into_iter().map(move |p| PlannedRedemption {
                    track,
                    ..planned(p, currency)
                })
            })
            .collect();
        return Ok(RedemptionPlan {
            member: Some(member),
            lines,
            refused: None,
        });
    }

    let plan = madar_loyalty::plan(
        &plan_lines(items),
        &programme,
        &plan_asks(redemptions),
        madar_loyalty::Mode::Server,
    )
    .map_err(|r| refused(r, &member.name))?;
    let lines = plan
        .lines
        .into_iter()
        .map(|p| planned(p, mode.as_str()))
        .collect();

    Ok(RedemptionPlan {
        member: Some(member),
        lines,
        refused: None,
    })
}

/// Record the plan against a placed order, inside that order's transaction.
///
/// Idempotent per covered line (`loyalty_transactions_redeem_line_key`), so a
/// retried checkout lands the same free coffee exactly once.
///
/// ## Two tills, one balance
/// `plan` reads the balance outside any lock, so two tills can each be told the
/// last reward is affordable. The member's row is locked HERE, inside the order
/// transaction, and the balance re-read under it: the second till waits for the
/// first to commit and then sees what is left. A live sale that lost the race
/// is refused (nothing has been handed over yet); a replayed one keeps its
/// cover, moves no points and says why — the same rule as
/// [`RedemptionPlan::refused`]. `created_by` is the audit trail: the teller
/// who applied the reward.
///
/// `item_ids[i]` is the `order_items.id` of line `i`, so a refund can find the
/// redemption that paid for the line it returns.
///
/// Returns the refusal to stamp on the order, if there is one.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    tx: &mut Transaction<'_, Postgres>,
    plan: &RedemptionPlan,
    org_id: Uuid,
    branch_id: Uuid,
    order_id: Uuid,
    item_ids: &[Uuid],
    created_by: Option<Uuid>,
    lenient: bool,
) -> Result<Option<String>, AppError> {
    if plan.is_empty() {
        return Ok(None);
    }
    if let Some(refused) = &plan.refused {
        return Ok(Some(refused.clone()));
    }
    let Some(member) = &plan.member else {
        return Ok(None);
    };
    // Track 1's lines as ever; a track 2–4's against its own balance, locked
    // the same way (POS_MULTITRACK_DESIGN.md §2).
    let one: Vec<&PlannedRedemption> = plan.lines.iter().filter(|l| l.track <= 1).collect();
    let currency = one
        .first()
        .map(|l| l.currency.clone())
        .unwrap_or_else(|| "visits".into());
    // A retry of an order whose rows already landed must not be judged against
    // the balance those rows already spent.
    let already: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_transactions WHERE order_id = $1 AND kind = 'redeem'",
    )
    .bind(order_id)
    .fetch_one(&mut **tx)
    .await?;
    if !one.is_empty() {
        let balance: Option<i32> = sqlx::query_scalar(
            "SELECT CASE WHEN $2 = 'visits' THEN visits_balance ELSE points_balance END \
               FROM loyalty_memberships WHERE id = $1 FOR UPDATE",
        )
        .bind(member.id)
        .bind(&currency)
        .fetch_optional(&mut **tx)
        .await?;
        let spent = one.iter().fold(0i32, |a, l| a.saturating_add(l.cost));
        if already == 0 && balance.unwrap_or(0) < spent {
            let why = format!(
                "{} has {}; those rewards cost {spent}",
                member.name,
                balance.unwrap_or(0)
            );
            if lenient {
                return Ok(Some(why));
            }
            return Err(AppError::Conflict(why));
        }
    }
    let mut tracks: Vec<u8> = plan.lines.iter().map(|l| l.track).filter(|t| *t > 1).collect();
    tracks.sort_unstable();
    tracks.dedup();
    for t in tracks {
        let balance: Option<i32> = sqlx::query_scalar(
            "SELECT balance FROM loyalty_track_balances WHERE customer_id = $1 AND position = $2 FOR UPDATE",
        )
        .bind(member.id)
        .bind(i16::from(t))
        .fetch_optional(&mut **tx)
        .await?;
        let spent = plan
            .lines
            .iter()
            .filter(|l| l.track == t)
            .fold(0i32, |a, l| a.saturating_add(l.cost));
        if already == 0 && balance.unwrap_or(0) < spent {
            let why = format!(
                "{} has {} on track {t}; those rewards cost {spent}",
                member.name,
                balance.unwrap_or(0)
            );
            if lenient {
                return Ok(Some(why));
            }
            return Err(AppError::Refused {
                code: super::tracks::CODE_TRACK_BALANCE_SHORT,
                reason: why,
            });
        }
    }
    for line in &plan.lines {
        let track = (line.track > 1).then_some(i16::from(line.track));
        let item_id = item_ids.get(line.item_index).copied();
        sqlx::query(
            "INSERT INTO loyalty_transactions \
                (org_id, customer_id, branch_id, kind, currency, points, order_id, \
                 order_line_index, reward_menu_item_id, created_by, source, order_item_id, track) \
             VALUES ($1,$2,$3,'redeem',$4,$5,$6,$7,$8,$9,'redemption',$10,$11) \
             ON CONFLICT DO NOTHING",
        )
        .bind(org_id)
        .bind(member.id)
        .bind(branch_id)
        .bind(&line.currency)
        .bind(-line.cost)
        .bind(order_id)
        .bind(line.item_index as i32)
        .bind(line.menu_item_id)
        .bind(created_by)
        .bind(item_id)
        .bind(track)
        .execute(&mut **tx)
        .await?;
        // The order line remembers which track paid (receipts, history).
        if let (Some(t), Some(id)) = (track, item_id) {
            sqlx::query("UPDATE order_items SET reward_track = $2 WHERE id = $1")
                .bind(id)
                .bind(t)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod unit {
    use super::covered_minor;

    #[test]
    fn a_reward_covers_whole_units_with_their_modifiers_and_never_more_than_the_line() {
        assert_eq!(covered_minor(6_500, 13_000, 1), 6_500);
        assert_eq!(covered_minor(6_500, 13_000, 3), 13_000);
        assert_eq!(covered_minor(6_500, 13_000, 0), 0);
        assert_eq!(covered_minor(-5, 100, 1), 0);
    }
}

/// Give back the points a refunded reward spent.
///
/// ## The rule
/// A reward is goods handed over against points, so when the goods come back
/// the points do — in proportion to the reward UNITS returned, never to the
/// money, because a reward unit carried no money:
///
/// * A refund line on a line a reward covered returns reward units only once
///   its paid units are exhausted (`paid = quantity − reward_units`): three
///   lattes with one free, one returned for cash, is a paid latte. A unit
///   refunded for **nothing** (`amount = 0`) is a free unit coming back.
/// * A refund with no lines that brings the order to fully refunded returns
///   every redemption on it — the whole sale was undone.
/// * Refunding a paid line of a basket that also had a reward restores nothing.
///
/// Cumulative and monotonic: each call tops the reversal up to the target for
/// everything refunded so far, so retries and a sequence of partial refunds
/// land exactly once. Written through `loyalty_reverse(source = 'refund')`,
/// which also refuses to reverse more than was spent. The EARN clawback stays
/// the `order_refunds` trigger's; this only ever writes `reverse_redeem`.
///
/// Returns the members whose balance moved, for the wallet push after commit.
pub async fn restore_on_refund(
    tx: &mut Transaction<'_, Postgres>,
    order_id: Uuid,
    lines: &[crate::refunds::handlers::RefundLineInput],
    by: Uuid,
    note: Option<&str>,
) -> Result<Vec<Uuid>, AppError> {
    // (redeem row, member, |points|, already reversed, item's reward units,
    //  item quantity, item id)
    type Row = (Uuid, Uuid, i32, i64, Option<i32>, Option<i32>, Option<Uuid>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT t.id, t.customer_id, abs(t.points), \
                COALESCE((SELECT SUM(abs(r.points)) FROM loyalty_transactions r \
                           WHERE r.reverses_id = t.id), 0)::bigint, \
                oi.reward_units, oi.quantity, t.order_item_id \
           FROM loyalty_transactions t \
           LEFT JOIN order_items oi ON oi.id = t.order_item_id \
          WHERE t.order_id = $1 AND t.kind = 'redeem' AND t.reverses_id IS NULL \
          ORDER BY t.created_at, t.id",
    )
    .bind(order_id)
    .fetch_all(&mut **tx)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let fully_refunded: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT SUM(amount) FROM order_refunds WHERE order_id = $1), 0) \
                >= o.total_amount \
           FROM orders o WHERE o.id = $1",
    )
    .bind(order_id)
    .fetch_one(&mut **tx)
    .await?;

    let mut moved = Vec::new();
    for (txn, member, spent, reversed, reward_units, quantity, item) in rows {
        let target: i64 = if lines.is_empty() {
            if fully_refunded { spent as i64 } else { 0 }
        } else {
            let (Some(item), Some(units), Some(qty)) = (item, reward_units, quantity) else {
                continue;
            };
            if !lines.iter().any(|l| l.order_item_id == item) {
                continue;
            };
            if units <= 0 {
                continue;
            }
            let (free, paid): (i64, i64) = sqlx::query_as(
                "SELECT COALESCE(SUM(quantity) FILTER (WHERE amount = 0), 0)::bigint, \
                        COALESCE(SUM(quantity) FILTER (WHERE amount > 0), 0)::bigint \
                   FROM order_refund_lines WHERE order_item_id = $1",
            )
            .bind(item)
            .fetch_one(&mut **tx)
            .await?;
            let reward_returned =
                reward_units_returned(free, paid, i64::from(qty), i64::from(units));
            i64::from(spent) * reward_returned / i64::from(units)
        };
        if target > reversed {
            sqlx::query("SELECT loyalty_reverse($1, $2, 'refund', $3, $4)")
                .bind(txn)
                .bind((target - reversed) as i32)
                .bind(by)
                .bind(note)
                .execute(&mut **tx)
                .await?;
            moved.push(member);
        }
    }
    moved.dedup();
    Ok(moved)
}

/// How many of a line's reward units have come back: every unit refunded for
/// nothing is a free one, and units refunded for money are free only once the
/// paid units (`quantity − reward_units`) are all back. See [`restore_on_refund`].
pub fn reward_units_returned(free: i64, paid: i64, quantity: i64, reward_units: i64) -> i64 {
    let reward_units = reward_units.clamp(0, quantity.max(0));
    let paid_units = quantity - reward_units;
    (free.max(0) + (paid - paid_units).max(0)).clamp(0, reward_units)
}

#[cfg(test)]
mod refund_rule {
    use super::reward_units_returned;

    #[test]
    fn paid_units_come_back_before_free_ones() {
        // Three lattes, one free: the first two returned for cash are paid.
        assert_eq!(reward_units_returned(0, 1, 3, 1), 0);
        assert_eq!(reward_units_returned(0, 2, 3, 1), 0);
        assert_eq!(reward_units_returned(0, 3, 3, 1), 1);
    }

    #[test]
    fn a_line_refunded_for_nothing_is_the_free_unit() {
        assert_eq!(reward_units_returned(1, 0, 3, 1), 1);
        assert_eq!(reward_units_returned(1, 1, 3, 2), 1);
        assert_eq!(reward_units_returned(3, 0, 3, 2), 2);
    }

    #[test]
    fn a_fully_free_line_returns_its_units() {
        assert_eq!(reward_units_returned(0, 2, 2, 2), 2);
        assert_eq!(reward_units_returned(0, 9, 2, 2), 2);
    }
}
