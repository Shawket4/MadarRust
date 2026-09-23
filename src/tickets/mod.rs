//! Waiter open tickets (fire-now-pay-later). A waiter opens a dine-in ticket and
//! fires items in rounds; a cashier settles it later into a paid `orders` row via
//! the shared delivery snapshot machinery. The bill (priced lines) lives in
//! `open_ticket_items`; the kitchen copy is emitted to the source-agnostic
//! `kitchen_tickets` substrate so the KDS shows waiter + counter orders alike.
//!
//! A ticket's `status` is what the BILL is — `open`, `settled`, `voided` — and
//! nothing else. Whether the kitchen has plated it is not a bill state: it is
//! read from the ticket's `kitchen_tickets` whenever a view is built, one per
//! round, and never copied onto the bill where it could drift.

pub mod handlers;
pub mod public;
pub mod routes;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::AppError;
use crate::kitchen::{EmitKitchen, KitchenLine, KitchenSource, emit_kitchen_ticket};
use crate::orders::handlers::{OrderItemInput, resolve_order_line};
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

pub(crate) use crate::delivery::require_branch_access;
pub(crate) use crate::orgs::handlers::extract_claims;

// ── Read models ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpenTicketItemView {
    pub id: Uuid,
    pub round_number: i32,
    /// When the round this line came in on was fired. A bill is read as a
    /// sequence of visits to the table — "the drinks at seven, the food at
    /// half past" — and without the clock a till can only show a flat list
    /// that says nothing about how the evening went.
    pub round_fired_at: DateTime<Utc>,
    pub menu_item_id: Option<Uuid>,
    /// The frozen priced SnapshotLine (name, size, addons, totals).
    pub line: serde_json::Value,
    pub line_total: i32,
    pub voided: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpenTicketView {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub table_id: Option<Uuid>,
    pub ticket_ref: Option<String>,
    /// The bill: `open`, `settled` or `voided`. Never `ready` — see [`Self::ready`].
    pub status: String,
    /// The kitchen has plated every line of every round. DERIVED from the
    /// ticket's `kitchen_tickets` at read time, so it is always what the KDS
    /// says now. `false` for a ticket nothing was ever fired to the kitchen for
    /// (routing mode `off`): there is nothing to be ready.
    #[serde(default)]
    pub ready: bool,
    pub opened_by: Uuid,
    pub opened_by_name: Option<String>,
    /// The customer this bill belongs to, when one is known (design §2.5): a
    /// table-QR guest who gave a phone, the party's booking, or one the waiter
    /// attached. `customer_name` is the free-text snapshot and may be set
    /// without it. Settling carries it onto the sale.
    #[serde(default)]
    pub customer_id: Option<Uuid>,
    pub customer_name: Option<String>,
    pub notes: Option<String>,
    pub guest_count: Option<i32>,
    pub subtotal: i32,
    /// The discount the waiter put on the bill at fire time, if any. Shown so
    /// the cashier can SEE what a settle will inherit — and clear it with an
    /// explicit `discount_type: "none"` rather than have it applied silently.
    #[serde(default)]
    pub discount_id: Option<Uuid>,
    #[serde(default)]
    pub discount_type: Option<String>,
    #[serde(default)]
    pub discount_value: Option<rust_decimal::Decimal>,
    /// The bill as the SERVER prices it — see [`TicketBill`]. This is the
    /// figure the till shows and the drawer collects, because it is the figure
    /// the settle will book; `subtotal` above is only its first line.
    #[serde(default)]
    pub bill: TicketBill,
    pub order_id: Option<Uuid>,
    /// The booking this ticket seated, if the party had one.
    pub booking_id: Option<Uuid>,
    pub opened_at: DateTime<Utc>,
    /// The last moment the kitchen had the whole ticket plated. History for
    /// the timing reports; `ready` is the live fact.
    pub ready_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub voided_at: Option<DateTime<Utc>>,
    /// Categorised like an order void, so void-rate reports read dine-in and
    /// counter alike.
    #[serde(default)]
    pub void_reason: Option<String>,
    #[serde(default)]
    pub void_note: Option<String>,
    pub items: Vec<OpenTicketItemView>,
    /// The branch's effective IANA timezone (see `crate::tz`) — the zone this
    /// ticket's times are shown in. Additive.
    #[serde(default)]
    pub timezone: Option<String>,
}

/// What the party owes, priced where the books are priced.
///
/// The till used to show the ticket's running `subtotal` and collect that,
/// while the settle booked subtotal − discount + service charge + tax. Every
/// such drawer was short by the tax on every table sale, and the drift check
/// at settle now refuses that figure outright — so the till must be shown the
/// right one, and only the server knows the branch's policy. Priced under the
/// same engine and the same resolved policy as `create_order_inner`, with the
/// service charge on (a ticket is dine-in by definition, ruling 2). For a
/// settled ticket the figures are the ORDER's, as booked, not a repricing
/// under today's policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct TicketBill {
    /// Live lines as charged, before discount. Gross when tax-inclusive.
    pub subtotal: i32,
    /// The waiter's discount, resolved (a `discount_id` is looked up the way
    /// the settle looks it up). A cashier who clears it at settle will see a
    /// different total than this one, and that is the point of showing it.
    pub discount_amount: i32,
    pub service_charge_amount: i32,
    /// Inside the total when `tax_inclusive`, on top of it otherwise.
    pub tax_amount: i32,
    pub tax_inclusive: bool,
    /// What the drawer must collect.
    pub total: i32,
    /// The rates the figures were computed under, for the printed bill.
    pub tax_rate: Decimal,
    pub service_charge_rate: Decimal,
    /// Whether the service charge sits inside the tax base. Frozen on the bill
    /// with the rates, so a till re-pricing the bill (a discount, a reward, a
    /// voided line) prices it the way the settle will. Additive: a bill from
    /// an older server decodes as `true`, `TaxPolicy::default()`'s value.
    #[serde(default = "crate::tickets::default_true")]
    pub service_charge_taxable: bool,
}

pub(crate) fn default_true() -> bool {
    true
}

/// Price an OPEN ticket's bill under the branch's current policy.
///
/// `discount_id` wins over a typed discount, exactly as `create_order_inner`
/// resolves it, so the bill the till shows is the bill the settle will book.
/// A discount id that no longer resolves (deleted, deactivated) prices as no
/// discount here — the settle will refuse it with a message, and a preview
/// that guessed a figure would only make that refusal a surprise.
fn price_bill_under(
    policy: &crate::tax::TaxPolicy,
    subtotal: i32,
    // The resolved discount: `(type, value)` of the live `discount_id` row when
    // the ticket names one, else the typed discount.
    dtype: Option<&str>,
    dvalue: Decimal,
) -> TicketBill {
    // A bill's own lines are priced by this server when each round is fired, so
    // a negative subtotal here would mean a voided line took more off than the
    // rounds put on. It cannot be booked, and `clamp(0, subtotal)` below would
    // PANIC on it (`min > max`) rather than say so — floor it, and the settle's
    // own guard refuses the sale with a message the cashier can act on.
    let subtotal = subtotal.max(0);
    let discount_amount =
        crate::discounts::handlers::calc_discount(dtype, dvalue, subtotal).clamp(0, subtotal);
    let b = crate::tax::compute(subtotal as i64, discount_amount as i64, policy);
    TicketBill {
        subtotal,
        discount_amount,
        service_charge_amount: b.service_charge as i32,
        tax_amount: b.tax as i32,
        tax_inclusive: policy.tax_inclusive,
        total: b.total as i32,
        tax_rate: policy.tax_rate,
        service_charge_rate: policy.service_charge_rate,
        service_charge_taxable: policy.service_charge_taxable,
    }
}

/// A settled ticket's bill is what its order booked — read, never repriced.
async fn booked_bills(
    conn: &mut sqlx::PgConnection,
    order_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, TicketBill>, AppError> {
    if order_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        Uuid,
        i32,
        i32,
        i32,
        i32,
        bool,
        i32,
        Option<Decimal>,
        Option<Decimal>,
        Option<bool>,
    )> = sqlx::query_as(
        "SELECT id, subtotal, discount_amount, service_charge_amount, tax_amount, tax_inclusive, \
                    total_amount, tax_rate_applied, service_charge_rate_applied, \
                    service_charge_taxable_applied \
             FROM orders WHERE id = ANY($1)",
    )
    .bind(order_ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                subtotal,
                discount_amount,
                service_charge_amount,
                tax_amount,
                tax_inclusive,
                total,
                tr,
                sr,
                st,
            )| {
                (
                    id,
                    TicketBill {
                        subtotal,
                        discount_amount,
                        service_charge_amount,
                        tax_amount,
                        tax_inclusive,
                        total,
                        tax_rate: tr.unwrap_or(Decimal::ZERO),
                        service_charge_rate: sr.unwrap_or(Decimal::ZERO),
                        service_charge_taxable: st.unwrap_or(true),
                    },
                )
            },
        )
        .collect())
}

/// One row of `open_tickets` as the view reads it. Named rather than a tuple
/// because the tuple had passed sixteen fields.
#[derive(sqlx::FromRow)]
struct TicketRow {
    org_id: Uuid,
    id: Uuid,
    branch_id: Uuid,
    table_id: Option<Uuid>,
    ticket_ref: Option<String>,
    status: String,
    ready: bool,
    opened_by: Uuid,
    opened_by_name: Option<String>,
    customer_id: Option<Uuid>,
    customer_name: Option<String>,
    notes: Option<String>,
    guest_count: Option<i32>,
    subtotal: i32,
    discount_id: Option<Uuid>,
    discount_type: Option<String>,
    discount_value: Option<rust_decimal::Decimal>,
    order_id: Option<Uuid>,
    booking_id: Option<Uuid>,
    opened_at: DateTime<Utc>,
    ready_at: Option<DateTime<Utc>>,
    settled_at: Option<DateTime<Utc>>,
    voided_at: Option<DateTime<Utc>>,
    void_reason: Option<String>,
    void_note: Option<String>,
    timezone: Option<String>,
    tax_rate_applied: Option<Decimal>,
    tax_inclusive_applied: Option<bool>,
    service_charge_rate_applied: Option<Decimal>,
    service_charge_taxable_applied: Option<bool>,
}

impl TicketRow {
    /// The policy this bill froze when it was opened, if it did.
    fn frozen_policy(&self) -> Option<crate::tax::TaxPolicy> {
        frozen_policy(
            self.tax_rate_applied,
            self.tax_inclusive_applied,
            self.service_charge_rate_applied,
            self.service_charge_taxable_applied,
        )
    }
}

/// A ticket's frozen policy from its four columns: all of them, or none (the
/// table's CHECK makes a half-frozen row impossible; this stays total anyway).
pub(crate) fn frozen_policy(
    tax_rate: Option<Decimal>,
    tax_inclusive: Option<bool>,
    service_charge_rate: Option<Decimal>,
    service_charge_taxable: Option<bool>,
) -> Option<crate::tax::TaxPolicy> {
    Some(crate::tax::TaxPolicy {
        tax_rate: tax_rate?,
        tax_inclusive: tax_inclusive?,
        service_charge_rate: service_charge_rate?,
        service_charge_taxable: service_charge_taxable?,
    })
}

pub(crate) async fn open_ticket_view(
    pool: &PgPool,
    ticket_id: Uuid,
) -> Result<Option<OpenTicketView>, AppError> {
    Ok(open_ticket_views(pool, &[ticket_id]).await?.pop())
}

/// [`open_ticket_views_on`] on a pooled connection.
pub(crate) async fn open_ticket_views(
    pool: &PgPool,
    ids: &[Uuid],
) -> Result<Vec<OpenTicketView>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut conn = pool.acquire().await?;
    open_ticket_views_on(&mut conn, ids).await
}

/// Many views in a fixed number of queries -- tickets, lines, booked bills,
/// named discounts, and one tax policy per branch -- in the order of `ids`
/// (missing ids are skipped), all on the caller's connection.
pub(crate) async fn open_ticket_views_on(
    conn: &mut sqlx::PgConnection,
    ids: &[Uuid],
) -> Result<Vec<OpenTicketView>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // `ready` is the per-round readiness folded over the whole bill: at least
    // one kitchen ticket exists and none of them has a live line still to bump.
    let rows: Vec<TicketRow> = sqlx::query_as(
        "SELECT ot.org_id, ot.id, ot.branch_id, ot.table_id, ot.ticket_ref, ot.status::text AS status, \
                EXISTS (SELECT 1 FROM kitchen_tickets kt WHERE kt.open_ticket_id = ot.id) \
                AND NOT EXISTS ( \
                    SELECT 1 FROM kitchen_tickets kt \
                    JOIN kitchen_ticket_items kti ON kti.kitchen_ticket_id = kt.id \
                    WHERE kt.open_ticket_id = ot.id \
                      AND kti.voided_at IS NULL AND kti.bumped_at IS NULL) AS ready, \
                ot.opened_by, u.name AS opened_by_name, ot.customer_id, ot.customer_name, ot.notes, ot.guest_count, \
                ot.subtotal, ot.discount_id, ot.discount_type, ot.discount_value, \
                ot.order_id, ot.booking_id, ot.opened_at, ot.ready_at, ot.settled_at, \
                ot.voided_at, ot.void_reason::text AS void_reason, ot.void_note, \
                effective_timezone(ot.branch_id) AS timezone, \
                ot.tax_rate_applied, ot.tax_inclusive_applied, ot.service_charge_rate_applied, \
                ot.service_charge_taxable_applied \
         FROM open_tickets ot LEFT JOIN users u ON u.id = ot.opened_by WHERE ot.id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;

    #[allow(clippy::type_complexity)]
    let lines: Vec<(
        Uuid,
        Uuid,
        i32,
        DateTime<Utc>,
        Option<Uuid>,
        serde_json::Value,
        i32,
        bool,
    )> = sqlx::query_as(
        "SELECT oti.open_ticket_id, oti.id, r.round_number, r.fired_at, oti.menu_item_id, oti.line, \
                oti.line_total, (oti.voided_at IS NOT NULL) AS voided \
         FROM open_ticket_items oti JOIN open_ticket_rounds r ON r.id = oti.round_id \
         WHERE oti.open_ticket_id = ANY($1) ORDER BY r.round_number, oti.created_at",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut items_of: std::collections::HashMap<Uuid, Vec<OpenTicketItemView>> =
        std::collections::HashMap::new();
    for (ticket_id, id, round_number, round_fired_at, menu_item_id, line, line_total, voided) in
        lines
    {
        items_of
            .entry(ticket_id)
            .or_default()
            .push(OpenTicketItemView {
                id,
                round_number,
                round_fired_at,
                menu_item_id,
                line,
                line_total,
                voided,
            });
    }

    let order_ids: Vec<Uuid> = rows.iter().filter_map(|r| r.order_id).collect();
    let booked = booked_bills(&mut *conn, &order_ids).await?;
    // `discount_id` wins over a typed discount, exactly as `create_order_inner`
    // resolves it. An id that no longer resolves (deleted, deactivated) prices
    // as no discount — the settle refuses it with a message.
    let discount_ids: Vec<Uuid> = rows.iter().filter_map(|r| r.discount_id).collect();
    let named: std::collections::HashMap<(Uuid, Uuid), (String, Decimal)> = if discount_ids
        .is_empty()
    {
        std::collections::HashMap::new()
    } else {
        sqlx::query_as::<_, (Uuid, Uuid, String, Decimal)>(
            "SELECT id, org_id, type::text, value FROM discounts WHERE id = ANY($1) AND is_active = true",
        )
        .bind(&discount_ids)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|(id, org, t, v)| ((id, org), (t, v)))
        .collect()
    };
    let mut policies: std::collections::HashMap<Uuid, crate::tax::TaxPolicy> =
        std::collections::HashMap::new();
    let mut by_id: std::collections::HashMap<Uuid, OpenTicketView> =
        std::collections::HashMap::with_capacity(rows.len());
    for r in rows {
        // Settled: what was booked. Otherwise: what would be, under the policy
        // the bill froze when it was opened (today's, for a ticket from before
        // bills froze theirs) — for a voided ticket that is history's
        // curiosity, but a bill that prices to nothing would read as a defect.
        let bill = match r.order_id.and_then(|o| booked.get(&o).cloned()) {
            Some(b) => b,
            None => {
                let frozen = r.frozen_policy();
                #[allow(clippy::map_entry)] // the insert awaits
                if frozen.is_none() && !policies.contains_key(&r.branch_id) {
                    let p = crate::tax::policy::for_branch(&mut *conn, r.branch_id).await?;
                    policies.insert(r.branch_id, p);
                }
                let (dtype, dvalue): (Option<String>, Decimal) = match r.discount_id {
                    Some(id) => named
                        .get(&(id, r.org_id))
                        .map(|(t, v)| (Some(t.clone()), *v))
                        .unwrap_or((None, Decimal::ZERO)),
                    None => (
                        r.discount_type.clone(),
                        r.discount_value.unwrap_or(Decimal::ZERO),
                    ),
                };
                price_bill_under(
                    frozen.as_ref().unwrap_or_else(|| &policies[&r.branch_id]),
                    r.subtotal,
                    dtype.as_deref(),
                    dvalue,
                )
            }
        };
        let items = items_of.remove(&r.id).unwrap_or_default();
        by_id.insert(
            r.id,
            OpenTicketView {
                id: r.id,
                branch_id: r.branch_id,
                table_id: r.table_id,
                ticket_ref: r.ticket_ref,
                status: r.status,
                ready: r.ready,
                opened_by: r.opened_by,
                opened_by_name: r.opened_by_name,
                customer_id: r.customer_id,
                customer_name: r.customer_name,
                notes: r.notes,
                guest_count: r.guest_count,
                subtotal: r.subtotal,
                discount_id: r.discount_id,
                discount_type: r.discount_type,
                discount_value: r.discount_value,
                bill,
                order_id: r.order_id,
                booking_id: r.booking_id,
                opened_at: r.opened_at,
                ready_at: r.ready_at,
                settled_at: r.settled_at,
                voided_at: r.voided_at,
                void_reason: r.void_reason,
                void_note: r.void_note,
                items,
                timezone: r.timezone,
            },
        );
    }
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

// ── Shared fire logic (CLIENT-authoritative, like the teller) ─────
//
// The waiter client prices the cart itself (same as the POS create-order path) so
// it can fire OFFLINE; the server records the prices verbatim and only resolves
// display names + a fallback price for the kitchen/bill. Settlement replays the
// stored items through `create_order_inner` (the exact client-authoritative path),
// which computes deductions/inventory/tax/discount and mints the paid order.

/// A fired line: the client's priced `OrderItemInput` (stored as JSON for the
/// settle replay) plus a frozen display projection (bill + kitchen).
#[derive(Serialize, Deserialize)]
pub(crate) struct StoredTicketLine {
    /// Serialized `OrderItemInput` — replayed verbatim at settle. The unit and
    /// addon prices inside it are FILLED IN at fire time (the till's where it
    /// sent one, the catalog's otherwise), so the settle reprices the line at
    /// what the bill showed, not at whatever the catalog says hours later.
    pub input: serde_json::Value,
    pub name: String,
    pub size_label: Option<String>,
    pub modifiers: Vec<String>,
    pub qty: i32,
    pub unit_price: i32,
    pub line_total: i32,
}

fn to_kitchen_line(l: &StoredTicketLine) -> KitchenLine {
    let menu_item_id = l
        .input
        .get("menu_item_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let notes = l
        .input
        .get("notes")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    KitchenLine {
        menu_item_id,
        name: l.name.clone(),
        qty: l.qty,
        size_label: l.size_label.clone(),
        modifiers: l.modifiers.clone(),
        notes,
        kitchen_item_id: None, // assigned by `fire_round` from the round idem key
        open_ticket_item_id: None, // assigned by `fire_round` from the INSERT
    }
}

/// Resolve and price the client's items for the bill and the kitchen.
///
/// The SAME per-line resolution the till's checkout uses
/// (`orders::handlers::resolve_order_line`), so a line on the bill is priced
/// with its optionals and bundle surcharges exactly as the settle will charge
/// it. Pricing stays client-authoritative — a `unit_price` the client sent is
/// kept — and the resolved prices are written back into the stored input so
/// the settle replays THIS bill rather than repricing against a later catalog.
/// Runs server-side (online or at replay), so an offline-fired ticket gets its
/// names when it syncs.
pub(crate) async fn resolve_ticket_lines(
    pool: &sqlx::PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    items: &[OrderItemInput],
    prices: crate::orders::handlers::ClientPrices,
) -> Result<Vec<StoredTicketLine>, AppError> {
    let fired_at = Utc::now();
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        // A STAFF DRINK IS A COUNTER SALE. A table's bill is priced when each
        // round is fired and settled hours later, under a frozen policy; the
        // pool is counted per business day at the moment of sale. The two
        // clocks do not agree, so a ticket line never goes on the pool. Live,
        // that is said; a round fired OFFLINE already reached the kitchen, so
        // it lands as an ordinary paid line and the settle charges it in full.
        if it.staff_drink.is_some() && prices == crate::orders::handlers::ClientPrices::Ignore {
            return Err(AppError::Coded {
                status: 400,
                code: "staff_drink_not_on_ticket",
                reason: "A staff drink is rung at the till, not on a table's bill".into(),
            });
        }
        let resolved = resolve_order_line(pool, org_id, branch_id, fired_at, it, prices).await?;

        let mut frozen = it.clone();
        if frozen.staff_drink.take().is_some() {
            tracing::warn!(%branch_id, "a replayed ticket line named the staff pool; rung as a paid line");
        }
        frozen.unit_price = Some(resolved.unit_price);
        // Bundle-component addons are server-priced through the surcharge and
        // the resolver ignores a client price for them; a plain item's addons
        // are overlaid one-to-one in input order, which is how they resolve.
        if frozen.bundle_id.is_none() {
            for (a, r) in frozen.addons.iter_mut().zip(resolved.addons.iter()) {
                a.unit_price = Some(r.unit_price);
            }
        }

        out.push(StoredTicketLine {
            input: serde_json::to_value(&frozen).unwrap_or(serde_json::Value::Null),
            name: resolved.item_name.clone(),
            size_label: it.size_label.clone(),
            modifiers: resolved.kitchen_modifiers(),
            qty: it.quantity,
            unit_price: resolved.unit_price,
            line_total: resolved.charged_subtotal(),
        });
    }
    Ok(out)
}

/// Fire a round of resolved, client-priced lines onto an open ticket inside `tx`: store the
/// bill lines (with the client input for replay), bump the running subtotal, and
/// emit a kitchen ticket (returns its id for the post-commit publish).
///
/// The round number is not chosen here. Inserting the round with it NULL lets
/// the trigger on `open_ticket_rounds` hand one out under the ticket's row
/// lock, so two waiters firing on one table in the same second get consecutive
/// numbers instead of a duplicate-key 500.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fire_round(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    branch_id: Uuid,
    open_ticket_id: Uuid,
    fired_by: Uuid,
    round_idem: Option<Uuid>,
    // The round's lines, resolved by [`resolve_ticket_lines`] BEFORE the caller
    // opened `tx` — resolving reads the catalogue through the pool, and doing
    // that while holding `tx` would take a second connection per request.
    lines: Vec<StoredTicketLine>,
    table_label: Option<&str>,
    ticket_ref: Option<&str>,
) -> Result<Option<Uuid>, AppError> {
    let (round_id, round_number): (Uuid, i32) = sqlx::query_as(
        "INSERT INTO open_ticket_rounds (open_ticket_id, round_number, fired_by, idempotency_key) \
         VALUES ($1, NULL, $2, $3) RETURNING id, round_number",
    )
    .bind(open_ticket_id)
    .bind(fired_by)
    .bind(round_idem)
    .fetch_one(&mut **tx)
    .await?;

    let mut round_subtotal: i32 = 0;
    let mut bill_line_ids: Vec<Uuid> = Vec::with_capacity(lines.len());
    for line in &lines {
        let menu_item_id = line
            .input
            .get("menu_item_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());
        // `line_total` the column and `line_total` inside the snapshot are the
        // same figure from the same struct; the CHECK on the table holds them to it.
        // The id comes back so the kitchen copy can carry it: voiding this
        // bill line has to be able to find the plate it ordered.
        let item_id: Uuid = sqlx::query_scalar(
            "INSERT INTO open_ticket_items \
                (open_ticket_id, round_id, menu_item_id, line, line_total) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(open_ticket_id)
        .bind(round_id)
        .bind(menu_item_id)
        .bind(serde_json::to_value(line).unwrap_or(serde_json::Value::Null))
        .bind(line.line_total)
        .fetch_one(&mut **tx)
        .await?;
        bill_line_ids.push(item_id);
        round_subtotal += line.line_total;
    }

    // Only the money moves on the bill. Readiness is the kitchen's to say and
    // `ready_at` is history — a new round does not unhappen the last plating.
    sqlx::query(
        "UPDATE open_tickets SET subtotal = subtotal + $2, updated_at = now() WHERE id = $1",
    )
    .bind(open_ticket_id)
    .bind(round_subtotal)
    .execute(&mut **tx)
    .await?;

    // Derive the kitchen-ticket + per-line ids from the round's CLIENT idempotency
    // key, so an offline device that fired this round predicted the SAME ids (its KDS
    // projection + a later bump reconcile on sync). No key (a non-client fire) → the
    // server generates ids as before.
    let kitchen_ticket_id = round_idem.map(crate::kitchen::derive_kitchen_ticket_id);
    let mut klines: Vec<KitchenLine> = lines.iter().map(to_kitchen_line).collect();
    // Same order, same loop, same list — the nth kitchen line IS the nth bill
    // line here. It stops being true inside `emit_kitchen_ticket`, which drops
    // unrouted lines in `kds` mode, which is why the link is carried on the row
    // rather than recomputed from a position later.
    for (kl, id) in klines.iter_mut().zip(bill_line_ids.iter()) {
        kl.open_ticket_item_id = Some(*id);
    }
    if let Some(kt) = kitchen_ticket_id {
        for (i, kl) in klines.iter_mut().enumerate() {
            kl.kitchen_item_id = Some(crate::kitchen::derive_kitchen_item_id(kt, i));
        }
    }
    let kt_id = emit_kitchen_ticket(
        tx,
        &EmitKitchen {
            org_id,
            branch_id,
            source: KitchenSource::Round {
                open_ticket_id,
                round_id,
            },
            round_number,
            table_label,
            kitchen_ref: ticket_ref,
            kitchen_ticket_id,
        },
        &klines,
    )
    .await?;

    Ok(kt_id)
}

/// Who caused a fire, carried on its `ticket.*` / `kitchen.fired` events.
///
/// The server mints its own ticket id, so the id on the event is never one the
/// firing device knows — it cannot recognise its own echo by id. What it DOES
/// know is what it sent: its device id and the client-minted keys. They ride
/// along under `origin`, and a device skips the alert (never the board update)
/// for an event whose origin is itself. Per ROUND, so a later round fired on
/// the same ticket by another device still alerts here.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct EventOrigin {
    /// The firing device's stable id (`X-Madar-Device-Id` live, the replay
    /// envelope's `origin_device_id` for a queued op).
    pub device_id: Option<String>,
    /// The ticket's client idempotency key.
    pub ticket_idempotency_key: Option<Uuid>,
    /// The round's client idempotency key.
    pub round_idempotency_key: Option<Uuid>,
}

/// The header a device names itself with on a live ticket write.
pub const DEVICE_ID_HEADER: &str = "X-Madar-Device-Id";

/// The device id a live request names, if any (trimmed, capped, never empty).
pub(crate) fn device_id_from(req: &actix_web::HttpRequest) -> Option<String> {
    clean_device_id(
        req.headers()
            .get(DEVICE_ID_HEADER)
            .and_then(|v| v.to_str().ok()),
    )
}

pub(crate) fn clean_device_id(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(128).collect())
}

/// `payload` with `origin` added (the payload's own fields stay top-level, so
/// every existing consumer decodes it unchanged).
pub(crate) fn with_origin(payload: &impl Serialize, origin: &EventOrigin) -> serde_json::Value {
    let mut v = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut v {
        map.insert(
            "origin".into(),
            serde_json::to_value(origin).unwrap_or(serde_json::Value::Null),
        );
    }
    v
}

/// Publish ticket + kitchen events after a fire commits. The ticket event always
/// fires; the kitchen event only when a kitchen ticket was actually emitted (it
/// isn't, e.g., in `off` mode). Both carry `origin`.
pub(crate) async fn publish_fired(
    pool: &sqlx::PgPool,
    hub: &BranchEventHub,
    branch_id: Uuid,
    open_ticket_id: Uuid,
    kitchen_ticket_id: Option<Uuid>,
    event_type: &str,
    origin: &EventOrigin,
) {
    if let Ok(Some(view)) = open_ticket_view(pool, open_ticket_id).await {
        hub.publish(
            branch_id,
            BranchEvent::new(Topic::Tickets, event_type, &with_origin(&view, origin)),
        );
    }
    if let Some(kt_id) = kitchen_ticket_id {
        if let Ok(Some(view)) = crate::kitchen::kitchen_ticket_view(pool, kt_id).await {
            hub.publish(
                branch_id,
                BranchEvent::new(Topic::Kitchen, "kitchen.fired", &with_origin(&view, origin)),
            );
        }
    }
}

/// Publish a table's current status on the Floor topic (post-commit) so every
/// canvas — teller tills, waiter tills, the dashboard board — updates live.
pub(crate) async fn publish_table_status(
    pool: &sqlx::PgPool,
    hub: &BranchEventHub,
    branch_id: Uuid,
    table_id: Uuid,
) {
    let status = crate::floor_ops::table_status(pool, table_id)
        .await
        .ok()
        .flatten();
    if let Some(status) = status {
        hub.publish(
            branch_id,
            BranchEvent::new(
                Topic::Floor,
                "table.status_changed",
                &serde_json::json!({ "branch_id": branch_id, "table_id": table_id, "status": status }),
            ),
        );
    }
}

/// Mint a human-readable ticket ref `T-<branchcode>-<YYMMDD>-<NNNN>`.
pub(crate) async fn mint_ticket_ref(
    tx: &mut Transaction<'_, Postgres>,
    branch_id: Uuid,
    at: DateTime<Utc>,
) -> Result<String, AppError> {
    let (branch_code, biz_date): (String, chrono::NaiveDate) = sqlx::query_as(
        "SELECT COALESCE(b.code, 'T'), \
                ($1::timestamptz AT TIME ZONE COALESCE(b.timezone, o.timezone)::text)::date \
         FROM branches b JOIN organizations o ON o.id = b.org_id WHERE b.id = $2",
    )
    .bind(at)
    .bind(branch_id)
    .fetch_one(&mut **tx)
    .await?;
    let seq: i32 = sqlx::query_scalar(
        "INSERT INTO ticket_ref_counters (branch_id, business_date, last_seq) VALUES ($1, $2, 1) \
         ON CONFLICT (branch_id, business_date) \
         DO UPDATE SET last_seq = ticket_ref_counters.last_seq + 1 RETURNING last_seq",
    )
    .bind(branch_id)
    .bind(biz_date)
    .fetch_one(&mut **tx)
    .await?;
    Ok(format!(
        "T-{}-{}-{:04}",
        branch_code,
        madar_time::yymmdd(biz_date),
        seq
    ))
}
