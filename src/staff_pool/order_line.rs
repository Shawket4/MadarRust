//! A cart line put on the staff pool — priced and recorded WITH its order.
//!
//! Before this, "on the pool" only wrote a `staff_drinks` side record
//! ([`super::record`]); nothing linked it to the sale's price. Now a line of
//! `POST /orders` (and of the `CreateOrder` replay op) may carry
//! [`StaffDrinkLine`], and the order's own transaction decides the pool, comps
//! the line with [`super::comp`], and writes the `staff_drinks` row.
//!
//! **The backend owns the money.** Live, the server computes the comp and a
//! figure the client sent is read by nothing. On REPLAY the sale already
//! happened: the till's figure is what the customer was charged, so it is what
//! the books record — and the server's own verdict is stored beside it
//! (`staff_drinks.comp_minor` vs `comp_minor_reported`) with a
//! `orders.staff_drink.record:comp_mismatch` flag when they differ. Nothing
//! here ever refuses a replayed sale.
//!
//! The record-only endpoint and the `RecordStaffDrink` op stay exactly as they
//! were: POS v0.5.0–v0.7.12 send neither this field nor a comp.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::authz::Cap;
use crate::errors::AppError;

use super::comp::{self, CompInput, CompResult};
use super::engine::{self, StaffDrinkRefusal, StaffPoolSettings};

/// `staff_drink` on an order line. Additive: a client that never heard of it
/// omits it and the line is an ordinary paid line.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct StaffDrinkLine {
    /// Client-minted; the idempotency key AND the `staff_drinks` row's id. A
    /// row an older flow already recorded under this id is reused and the
    /// order attached to it — never a second drink off the allowance.
    pub id: Uuid,
    /// REQUIRED. Who the drink is for and why, in the teller's own words.
    pub note: String,
    /// What the TILL comped on this line (whole line, minor units). Read ONLY
    /// when a queued offline sale is replayed; live, the server prices the comp
    /// and this is ignored.
    #[serde(default)]
    pub comp_minor: Option<i32>,
    /// Whether the till believed this drink went past the allowance. Replay
    /// only, and only to tell a convergence from a surprise.
    #[serde(default)]
    pub overspent: Option<bool>,
}

/// The note stored when a REPLAYED sale carried a blank one. The column cannot
/// hold a blank and the sale cannot be refused, so the absence is written down
/// as what it is, and flagged.
pub const NO_NOTE: &str = "(no note given)";

/// What the branch's pool says about staff lines today — read once per order,
/// before the order's transaction opens.
pub(crate) struct PoolContext {
    pub settings: StaffPoolSettings,
    pub allowance: i32,
    pub day: NaiveDate,
}

pub(crate) async fn pool_context(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    at: DateTime<Utc>,
) -> Result<PoolContext, AppError> {
    let tz = crate::tz::effective_tz(pool, branch_id).await?;
    let s = super::settings::load_effective(pool, org_id, branch_id).await?;
    Ok(PoolContext {
        allowance: s.daily_allowance,
        settings: s.for_engine(),
        day: engine::business_date_of(tz, at),
    })
}

/// The refusals that do not depend on the day's count. A bundle is never on
/// the pool: it has no size and no choice groups of its own to be the base of.
pub(crate) fn refusal_of(
    ctx: &PoolContext,
    menu_item_id: Option<Uuid>,
    note: &str,
) -> Option<StaffDrinkRefusal> {
    let Some(item) = menu_item_id else {
        return Some(StaffDrinkRefusal::ItemNotEligible);
    };
    engine::decide(
        &ctx.settings,
        &ctx.day.to_string(),
        &item.to_string(),
        note,
        0,
    )
    .refusal
}

/// A live refusal, carrying the engine's own token as its `code`.
pub(crate) fn refused(r: StaffDrinkRefusal) -> AppError {
    AppError::Coded {
        status: 400,
        code: r.token(),
        reason: r.message().to_string(),
    }
}

/// A priced staff line, carried on the resolved order line until it is stored.
pub(crate) struct LineComp {
    pub drink: StaffDrinkLine,
    /// Why the pool would not have allowed it. Only ever `Some` on replay —
    /// live, a refusal never gets this far.
    pub refusal: Option<StaffDrinkRefusal>,
    /// The SERVER's verdict.
    pub server: CompResult,
    /// What the till said, on replay.
    pub reported: Option<i32>,
    /// What actually came off the money (whole line): the server's live, the
    /// till's on replay, never more than the line rang at.
    pub applied: i32,
    /// `applied`, split: off `order_items.line_total`…
    pub applied_base: i32,
    /// …and off each `order_item_addons.line_total`, in addon order.
    pub applied_addons: Vec<i32>,
}

impl LineComp {
    /// Settle what is applied and where it lands.
    ///
    /// `base_line` is `unit_price × quantity`; `addon_lines` each addon's whole
    /// line. The server's own comp lands where the rule put it (the size part
    /// on the base, each group's part on the picks that earned it). A till
    /// figure that differs has no breakdown to follow, so it fills the base
    /// first and then the picks in order — it can never exceed what rang.
    pub(crate) fn settle(
        drink: StaffDrinkLine,
        refusal: Option<StaffDrinkRefusal>,
        server: CompResult,
        take_reported: bool,
        quantity: i32,
        base_line: i32,
        addon_lines: &[i32],
    ) -> Self {
        let reported = drink.comp_minor.filter(|_| take_reported).map(|c| c.max(0));
        let rang: i32 = base_line + addon_lines.iter().sum::<i32>();
        let applied = reported.unwrap_or(server.line_comp).clamp(0, rang.max(0));

        let (applied_base, applied_addons) =
            if applied == server.line_comp && server.breakdown.picks.len() == addon_lines.len() {
                (
                    server.breakdown.size_comp * quantity,
                    server
                        .breakdown
                        .picks
                        .iter()
                        .map(|p| p.comp * quantity)
                        .collect(),
                )
            } else {
                let base = applied.min(base_line.max(0));
                let mut left = applied - base;
                let addons = addon_lines
                    .iter()
                    .map(|l| {
                        let take = left.min((*l).max(0));
                        left -= take;
                        take
                    })
                    .collect();
                (base, addons)
            };
        Self {
            drink,
            refusal,
            server,
            reported,
            applied,
            applied_base,
            applied_addons,
        }
    }

    /// The till and the server disagree about what was free.
    pub(crate) fn mismatch(&self) -> bool {
        self.reported.is_some_and(|r| r != self.server.line_comp)
    }
}

/// What writing one staff line's row produced.
pub(crate) struct RecordedLine {
    /// `capability:detail` tokens for `authz_replay_flags`. Only meaningful on
    /// replay; live, the caller drops them (an overspend is on the row).
    pub flags: Vec<String>,
    /// Past today's allowance, as the server counted it. `false` for a reused
    /// row, which was counted when it was first recorded.
    pub overspent: bool,
}

/// Serialise every pooled sale of one branch-day, so two tills ringing the last
/// drink of the allowance at once cannot both read "not over".
pub(crate) async fn lock_day(
    tx: &mut Transaction<'_, Postgres>,
    branch_id: Uuid,
    day: NaiveDate,
) -> Result<(), AppError> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('staff_pool:' || $1::text || ':' || $2::text))",
    )
    .bind(branch_id)
    .bind(day)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Write (or reuse) the `staff_drinks` row of one pooled line, inside the
/// order's transaction.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_line(
    tx: &mut Transaction<'_, Postgres>,
    ctx: &PoolContext,
    org_id: Uuid,
    branch_id: Uuid,
    till_id: Uuid,
    order_id: Uuid,
    actor: Uuid,
    device_id: Option<Uuid>,
    recorded_at: DateTime<Utc>,
    replay: bool,
    line: &LineComp,
    menu_item_id: Option<Uuid>,
    item_name: &str,
    size_label: Option<&str>,
    quantity: i32,
    extras_minor: i32,
    cost_minor: Option<i32>,
) -> Result<RecordedLine, AppError> {
    let cap = Cap::OrdersStaffDrinkRecord.key();
    let mut flags = Vec::new();
    if let Some(r) = line.refusal {
        flags.push(format!("{cap}:{}", r.token()));
    }
    if line.mismatch() {
        flags.push(format!("{cap}:comp_mismatch"));
    }

    // An older flow (or a retry of this one) may have recorded the drink first.
    let existing: Option<(Option<Uuid>,)> = sqlx::query_as(
        "SELECT order_id FROM staff_drinks WHERE id = $1 AND org_id = $2 FOR UPDATE",
    )
    .bind(line.drink.id)
    .bind(org_id)
    .fetch_optional(&mut **tx)
    .await?;

    if let Some((attached,)) = existing {
        if attached.is_some_and(|o| o != order_id) {
            // The same drink id on a SECOND sale would comp it without ever
            // counting it. Live that is refused; a replayed sale lands, flagged.
            if !replay {
                return Err(AppError::Conflict(
                    "This staff drink was already rung on another order".into(),
                ));
            }
            flags.push(format!("{cap}:duplicate_id"));
        }
        // Already counted off the allowance when it was first recorded: only
        // the money and the order are attached. The pool is not touched.
        sqlx::query(
            "UPDATE staff_drinks SET order_id = COALESCE(order_id, $2), comp_minor = $3, \
                    extras_minor = $4, comp_minor_reported = $5, \
                    cost_minor = COALESCE(cost_minor, $6), updated_at = now() \
              WHERE id = $1",
        )
        .bind(line.drink.id)
        .bind(order_id)
        .bind(line.server.line_comp)
        .bind(extras_minor)
        .bind(line.reported)
        .bind(cost_minor)
        .execute(&mut **tx)
        .await?;
        return Ok(RecordedLine {
            flags,
            overspent: false,
        });
    }

    // THE count, under the day's lock. A line of n drinks is n off the
    // allowance: the shared engine adds the one being decided, so it is told
    // about the other n − 1.
    let used = super::record::used_on(&mut **tx, branch_id, ctx.day).await?;
    let decision = engine::decide(
        &ctx.settings,
        &ctx.day.to_string(),
        &menu_item_id.map(|i| i.to_string()).unwrap_or_default(),
        &line.drink.note,
        used + (quantity - 1).max(0),
    );
    // A refused-but-landed drink is outside the rules by definition, exactly
    // as the record-only path counts it.
    let overspent = decision.overspent || line.refusal.is_some();
    let device_said = line.drink.overspent.unwrap_or(false);
    let overspent_on_replay = replay && overspent && !device_said;
    if overspent_on_replay && line.refusal.is_none() {
        flags.push(format!("{cap}:overspent"));
    }
    if replay && device_said && !overspent {
        flags.push(format!("{cap}:device_overcounted"));
    }

    let note = if engine::note_is_given(&line.drink.note) {
        line.drink.note.trim()
    } else {
        NO_NOTE
    };
    sqlx::query(
        "INSERT INTO staff_drinks \
           (id, org_id, branch_id, till_id, order_id, menu_item_id, item_name, size_label, \
            quantity, note, business_date, allowance_at_record, used_before, overspent, \
            overspent_on_replay, cost_minor, recorded_by, device_id, recorded_at, \
            comp_minor, extras_minor, comp_minor_reported) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22)",
    )
    .bind(line.drink.id)
    .bind(org_id)
    .bind(branch_id)
    .bind(till_id)
    .bind(order_id)
    .bind(menu_item_id)
    .bind(item_name)
    .bind(size_label)
    .bind(quantity)
    .bind(note)
    .bind(ctx.day)
    .bind(ctx.allowance)
    .bind(used)
    .bind(overspent)
    .bind(overspent_on_replay)
    .bind(cost_minor)
    .bind(actor)
    .bind(device_id)
    .bind(recorded_at)
    .bind(line.server.line_comp)
    .bind(extras_minor)
    .bind(line.reported)
    .execute(&mut **tx)
    .await?;

    Ok(RecordedLine { flags, overspent })
}

/// The server's comp for one resolved line. Thin, so the order path reads as
/// "load, then the pure rule".
pub(crate) fn run(input: &CompInput) -> CompResult {
    comp::comp(input)
}
