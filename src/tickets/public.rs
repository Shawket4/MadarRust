//! Ordering from the code on the table.
//!
//! A customer sits at table 7, scans, and the shop's menu opens already knowing
//! where they are. They add things and send them to the kitchen. There is no
//! branch to pick — they are sitting in it — no channel, no phone number and no
//! location, because the code on the table answered all four before they
//! touched it.
//!
//! ## What the order IS
//!
//! An OPEN TICKET on that table: the same bill a waiter would have opened, on
//! the same floor, going to the same kitchen, settled at the same till. A
//! second scan by the same party adds a ROUND to it rather than starting a
//! second bill, which is how ordering at a table actually goes.
//!
//! That is the whole design decision, and everything else follows from it. The
//! order is `dine_in` at settle, so it carries the service charge the owner
//! ruled is dine-in only. The floor shows the table occupied, because it is.
//! The void, refund and kitchen paths are the ones that already exist.
//!
//! ## What this endpoint does NOT trust
//!
//! Everything about money. The items name menu items and quantities; the SERVER
//! prices them, through `resolve_ticket_lines` — the same single pricing path
//! the till and a waiter's fire both walk, and which reads no price off any
//! request from anyone. A customer cannot send a price, a discount or a total,
//! and there is nothing in the request that would be believed if they did.
//!
//! It is unauthenticated by necessity — the customer is a stranger with a
//! phone — and rate-limited per IP like the rest of the public intake. Anyone
//! who can read the code on a table can order onto it, which is a property of
//! QR ordering itself and not something an endpoint can check: the mitigation
//! is that the order appears on the floor and on the Bills list immediately,
//! under a badge saying it was scanned, where a person can void it.

use actix_web::{HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::{AppError, AppErrorResponse};
use crate::orders::handlers::OrderItemInput;
use crate::realtime::hub::BranchEventHub;
use crate::sync::ActingContext;
use crate::tickets::handlers::{AddRoundRequest, CreateOpenTicketRequest};

/// A table, as the page that opened from its code needs to know it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PublicTable {
    pub table_id: Uuid,
    pub branch_id: Uuid,
    pub org_id: Uuid,
    /// What the table is called in the room — "7", "T7", "Terrace 2".
    pub label: String,
    pub branch_name: String,
    /// The meal in progress, when there is one. `None` means the table is
    /// free and this scan will start the bill.
    pub bill: Option<PublicTableBill>,
    /// The branch is not serving right now — no till is open. The page says so
    /// instead of letting someone build a basket the kitchen will refuse.
    pub accepting: bool,
}

/// What the table has ordered so far.
///
/// The page draws this above the menu, so a customer who scans halfway through
/// a meal sees what is already on their bill rather than an empty basket that
/// looks like a fresh start. It is also how the second, third and fourth
/// person at the table see each other's rounds.
///
/// Public and unauthenticated, like everything else here. Whoever can read the
/// code stuck to the table can see what that table ordered — which is the
/// people sitting at it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PublicTableBill {
    pub ticket_id: Uuid,
    /// When the party's bill was opened. The page counts up from this; a
    /// duration computed here would be wrong by the time it arrived.
    pub opened_at: chrono::DateTime<chrono::Utc>,
    /// The kitchen has finished everything fired so far.
    pub ready: bool,
    /// Lines as charged, before discount — the bill's first line, not the bill.
    pub subtotal: i32,
    /// What the table will be asked to pay, as the SERVER prices it: the
    /// discount, the service charge and the tax are all in here, and none of
    /// them is something this page should be recomputing.
    pub total: i32,
    /// Every round fired, oldest first, with what went to the kitchen in each.
    pub rounds: Vec<PublicTableRound>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PublicTableRound {
    pub number: i32,
    pub fired_at: chrono::DateTime<chrono::Utc>,
    pub items: Vec<PublicTableLine>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PublicTableLine {
    pub name: String,
    pub quantity: i32,
    pub line_total: i32,
    /// Taken off the bill after it was ordered — struck through rather than
    /// hidden, so a customer who watches a plate go back sees it go.
    pub voided: bool,
}

/// One scan's worth of order.
#[derive(Deserialize, ToSchema)]
pub struct TableOrderRequest {
    pub table_id: Uuid,
    /// What they want. Named, never priced — see the module docs.
    pub items: Vec<OrderItemInput>,
    /// Who is at the table, if they offered a name. Shown on the bill so the
    /// waiter can find them.
    #[serde(default)]
    pub customer_name: Option<String>,
    /// Client-minted, so a phone that resends on a flaky connection does not
    /// order twice. This is the ONLY protection against a double-send, because
    /// a customer's browser has no outbox to dedup against.
    #[serde(default)]
    pub idempotency_key: Option<Uuid>,
}

#[utoipa::path(get, path = "/public/tables/{id}", tag = "open_tickets",
    operation_id = "public_table",
    params(("id" = Uuid, Path, description = "Table ID, from the QR")),
    responses((status = 200, body = PublicTable), AppErrorResponse))]
pub async fn table(pool: web::Data<PgPool>, id: web::Path<Uuid>) -> Result<HttpResponse, AppError> {
    let row: Option<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "SELECT t.branch_id, b.org_id, t.label, b.name \
           FROM branch_tables t JOIN branches b ON b.id = t.branch_id \
          WHERE t.id = $1 AND t.is_active \
            AND b.is_active AND b.deleted_at IS NULL",
    )
    .bind(*id)
    .fetch_optional(pool.get_ref())
    .await?;
    // A code for a table that has been removed, or whose branch is closed for
    // good, is a code on a wall somewhere. It gets the same nothing as a made-up
    // id rather than an explanation.
    let (branch_id, org_id, label, branch_name) =
        row.ok_or_else(|| AppError::NotFound("No table at that code".into()))?;

    let accepting = crate::shifts::handlers::branch_has_open_shift(pool.get_ref(), branch_id)
        .await
        .unwrap_or(false);

    Ok(HttpResponse::Ok().json(PublicTable {
        table_id: *id,
        branch_id,
        org_id,
        label,
        branch_name,
        bill: live_bill(pool.get_ref(), *id).await?,
        accepting,
    }))
}

/// The meal in progress on `table_id`, if there is one.
///
/// Priced through `open_ticket_view`, which is the same projection the till and
/// the floor read — so the figure a customer sees on their phone and the figure
/// the teller collects are the same number from the same place, not two
/// arithmetics that agree until a rate changes.
async fn live_bill(pool: &PgPool, table_id: Uuid) -> Result<Option<PublicTableBill>, AppError> {
    let Some(ticket_id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM open_tickets WHERE table_id = $1 AND status = 'open' \
         ORDER BY opened_at DESC LIMIT 1",
    )
    .bind(table_id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let Some(view) = crate::tickets::open_ticket_view(pool, ticket_id).await? else {
        return Ok(None);
    };

    // The rounds, oldest first, each with what went to the kitchen in it. The
    // ORDER is the one the ticket's own items already carry (`round_number`
    // then fired time), so the customer's list reads in the order they ordered.
    let mut rounds: Vec<PublicTableRound> = Vec::new();
    for it in &view.items {
        let name = it
            .line
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let entry = match rounds.last_mut() {
            Some(r) if r.number == it.round_number => r,
            _ => {
                rounds.push(PublicTableRound {
                    number: it.round_number,
                    fired_at: it.round_fired_at,
                    items: Vec::new(),
                });
                rounds.last_mut().expect("just pushed")
            }
        };
        entry.items.push(PublicTableLine {
            name,
            quantity: it
                .line
                .get("quantity")
                .or_else(|| it.line.get("qty"))
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(1) as i32,
            line_total: it.line_total,
            voided: it.voided,
        });
    }

    Ok(Some(PublicTableBill {
        ticket_id,
        opened_at: view.opened_at,
        ready: view.ready,
        subtotal: view.subtotal,
        // The server's own total. A page that summed the lines itself would be
        // short by the service charge and the tax on every branch that charges
        // either, which is the exact bug the till had before `bill` existed.
        total: view.bill.total,
        rounds,
    }))
}

/// The menu at this table.
///
/// The DINE-IN menu — branch prices, the whole catalogue, no channel discount
/// — because a table's order settles as a dine-in bill. Quoting a customer the
/// in-mall delivery menu and then charging them the till's prices is the same
/// class of mistake as letting the till price its own sales, and it would be
/// invisible until someone compared a receipt to a phone.
#[utoipa::path(get, path = "/public/tables/{id}/menu", tag = "open_tickets",
    operation_id = "public_table_menu",
    params(("id" = Uuid, Path, description = "Table ID, from the QR")),
    responses((status = 200, body = crate::delivery::public::DeliveryMenu), AppErrorResponse))]
pub async fn table_menu(
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT t.branch_id, b.org_id FROM branch_tables t JOIN branches b ON b.id = t.branch_id \
          WHERE t.id = $1 AND t.is_active AND b.is_active AND b.deleted_at IS NULL",
    )
    .bind(*id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (branch_id, org_id) =
        row.ok_or_else(|| AppError::NotFound("No table at that code".into()))?;
    let menu =
        crate::delivery::public::load_public_menu(pool.get_ref(), org_id, branch_id, None).await?;
    Ok(HttpResponse::Ok().json(menu))
}

/// Send this table's order to the kitchen.
///
/// Opens the bill if the table has none, adds a round if it does. Both answer
/// with the bill as it now stands, so the page can show what the table has
/// ordered so far — including the rounds somebody else at the table sent.
#[utoipa::path(post, path = "/public/table-orders", tag = "open_tickets",
    operation_id = "public_table_order", request_body = TableOrderRequest,
    responses((status = 200, description = "The bill as it now stands",
               body = crate::tickets::OpenTicketView), AppErrorResponse))]
pub async fn create_table_order(
    // `web::Data<PgPool>`, NOT `Db`. `Db` is the TENANT-SCOPED pool and it
    // extracts the org from verified claims — a public endpoint has none, so
    // taking it here 401s before the handler runs. The org is established
    // below, from the table, and the scoped handle is built from that.
    base: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    body: web::Json<TableOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let body = body.into_inner();
    if body.items.is_empty() {
        return Err(AppError::BadRequest("Nothing to send".into()));
    }

    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT t.branch_id, b.org_id FROM branch_tables t JOIN branches b ON b.id = t.branch_id \
          WHERE t.id = $1 AND t.is_active AND b.is_active AND b.deleted_at IS NULL",
    )
    .bind(body.table_id)
    .fetch_optional(base.get_ref())
    .await?;
    let (branch_id, org_id) =
        row.ok_or_else(|| AppError::NotFound("No table at that code".into()))?;
    // The tenant scope comes from the TABLE — the only thing the customer
    // supplied that the database can vouch for — and every write below runs
    // inside it, so a table id cannot reach another shop's rows.
    let pool = crate::db::Db::for_org(base.get_ref(), org_id).await;

    // The customer is the actor. Not a placeholder for one: something opened
    // this bill and it was not a member of staff, so it is named as what it
    // was. See the migration for why this is a row rather than a NULL.
    let guest = guest_principal(base.get_ref(), org_id).await?;
    let actor = ActingContext::guest(guest, org_id);
    // The branch-open gate, explicitly. `ActingContext::guest` is not a replay
    // — see its note — so nothing is skipping this on our behalf, and a scan
    // while the shop is shut must be refused before anything is written.
    if !crate::shifts::handlers::branch_has_open_shift(pool.get_ref(), branch_id).await? {
        return Err(AppError::Conflict("The kitchen is closed right now".into()));
    }

    // Joining a meal in progress, or starting one.
    let live: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM open_tickets WHERE table_id = $1 AND status = 'open' \
         ORDER BY opened_at DESC LIMIT 1",
    )
    .bind(body.table_id)
    .fetch_optional(pool.get_ref())
    .await?;

    match live {
        Some(ticket_id) => {
            crate::tickets::handlers::add_round_inner(
                pool.clone(),
                ticket_id,
                web::Json(AddRoundRequest {
                    idempotency_key: body.idempotency_key,
                    items: body.items,
                }),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        None => {
            let mut req = CreateOpenTicketRequest {
                branch_id,
                table_id: Some(body.table_id),
                customer_name: body
                    .customer_name
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty()),
                notes: None,
                guest_count: None,
                booking_id: None,
                idempotency_key: body.idempotency_key,
                round_idempotency_key: body.idempotency_key,
                items: body.items,
                discount_id: None,
                discount_type: None,
                discount_value: None,
            };
            // A customer cannot discount their own bill. Stated by construction
            // rather than by trusting the request not to carry one: the fields
            // are not on `TableOrderRequest` at all, and this is where that
            // becomes a fact about the ticket.
            req.discount_id = None;
            let resp = crate::tickets::handlers::create_open_ticket_inner(
                pool.clone(),
                web::Json(req),
                actor,
                Some(hub.get_ref()),
            )
            .await?;
            // Stamp how it was started, after the insert rather than through
            // it: `create_open_ticket_inner` is shared with the till and the
            // replay path, and neither should grow a parameter for a column
            // only this caller ever sets.
            //
            // Addressed by TABLE, not by parsing the id back out of the
            // response: there was no live bill on this table a moment ago —
            // that is the branch we are in — so the live one now is the one
            // just opened. `opened_via IS NULL` keeps it to the row that has
            // not been stamped, so a retry cannot relabel somebody else's.
            sqlx::query(
                "UPDATE open_tickets SET opened_via = 'qr_table' \
                  WHERE table_id = $1 AND status = 'open' AND opened_via IS NULL",
            )
            .bind(body.table_id)
            .execute(pool.get_ref())
            .await?;
            Ok(resp)
        }
    }
}

/// The organisation's guest principal, created on first use.
///
/// One row per org, and the unique partial index is what makes this safe when
/// two people scan two tables in the same second: the loser's insert does
/// nothing and it reads back the winner's.
async fn guest_principal(pool: &PgPool, org_id: Uuid) -> Result<Uuid, AppError> {
    if let Some(id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE org_id = $1 AND is_guest_principal AND deleted_at IS NULL",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    {
        return Ok(id);
    }
    sqlx::query(
        "INSERT INTO users (org_id, name, role, is_active, is_guest_principal) \
         VALUES ($1, 'Scanned', 'waiter'::user_role, false, true) ON CONFLICT DO NOTHING",
    )
    .bind(org_id)
    .execute(pool)
    .await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE org_id = $1 AND is_guest_principal AND deleted_at IS NULL",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or(AppError::Internal)
}
