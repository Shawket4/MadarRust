use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::Claims;
use crate::errors::AppError;
use crate::models::UserRole;
use crate::realtime::hub::BranchEventHub;
use crate::sync::ActingContext;

use crate::floor_ops::handlers::{
    CreateFloorTransferRequest, FulfillTransferRequest, SwapTablesRequest,
};
use crate::orders::handlers::{CreateOrderRequest, VoidOrderRequest};
use crate::tickets::handlers::{
    AddRoundRequest, CreateOpenTicketRequest, SettleOpenTicketRequest, VoidOpenTicketRequest,
};
use crate::tills::handlers::{CashMovementRequest, CloseTillRequest, OpenTillRequest};

/// The release op's body. The branch is not on the wire (the table resolves it,
/// like a clear); `bus` is the one thing the till knows and the server cannot
/// derive — whether the party ate before the hold ended.
#[derive(Debug, Default, Deserialize)]
pub struct ReleaseReplay {
    #[serde(default)]
    pub bus: bool,
}

/// One queued op from a device, carrying its ORIGINAL actor (`teller_id` — a
/// teller, a waiter, a kitchen screen, or a branch manager at the till) so the
/// replay attributes the write to whoever rang it — not to whoever is signed in
/// when the backlog flushes. The `request` payloads are the SAME bodies the live
/// routes accept (idempotency keys ride inside them), so a replayed op dedups
/// server-side exactly like a lost-response retry on the live endpoint.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ReplayOp {
    /// Permanent alias `open_shift` (POS v0.5.1 / v0.6.0).
    #[serde(alias = "open_shift")]
    OpenTill {
        teller_id: Uuid,
        branch_id: Uuid,
        #[serde(default)]
        device_id: Option<Uuid>,
        #[serde(default)]
        device_code: Option<String>,
        #[serde(default)]
        verification: Option<String>,
        request: OpenTillRequest,
    },
    /// Permanent alias `close_shift`.
    #[serde(alias = "close_shift")]
    CloseTill {
        teller_id: Uuid,
        #[serde(alias = "shift_id")]
        till_id: Uuid,
        #[serde(default)]
        device_id: Option<Uuid>,
        request: CloseTillRequest,
    },
    CreateOrder {
        teller_id: Uuid,
        #[serde(default)]
        device_id: Option<Uuid>,
        #[serde(default)]
        device_code: Option<String>,
        request: CreateOrderRequest,
    },
    VoidOrder {
        teller_id: Uuid,
        order_id: Uuid,
        request: VoidOrderRequest,
    },
    // Money handed back against a settled sale, out of the till's drawer, while
    // the till was offline. The request names the order, the shift the money
    // left (required on replay — there is no "current" shift for a queued op),
    // the real `issued_at`, and a `client_ref` so a re-flushed queue returns
    // the original refund instead of paying out twice. Distinct from a void:
    // a void says the sale never happened; a refund says it did and some of
    // the money went back (owner ruling 4).
    RefundOrder {
        teller_id: Uuid,
        request: crate::refunds::handlers::CreateRefundRequest,
    },
    // Adding a sale's loyalty points — an explicit teller action, queued when
    // the till was offline. The request carries `requested_at` (the moment the
    // button was pressed), so a drain days later still credits an award made in
    // time; the server bounds that claim against the order's own timestamp, so
    // a queued op can never reach outside the 24-hour window.
    AwardLoyaltyPoints {
        teller_id: Uuid,
        request: crate::loyalty::award::AwardRequest,
    },
    CashMovement {
        teller_id: Uuid,
        #[serde(alias = "shift_id")]
        till_id: Uuid,
        #[serde(default)]
        device_id: Option<Uuid>,
        request: CashMovementRequest,
    },
    // Open-ticket ops. Typically a waiter fires and adds rounds, the cashier
    // settles, and either may void — but "typically" is not enforced here:
    // whoever holds the grant does the op, and a teller or manager seating a
    // party fires too. `teller_id` carries the acting user.
    // `origin_device_id`: the device that queued the op, stamped on the events
    // it publishes so that device skips its own ping (absent on older clients).
    FireOpenTicket {
        teller_id: Uuid,
        request: CreateOpenTicketRequest,
        #[serde(default)]
        origin_device_id: Option<String>,
    },
    AddTicketRound {
        teller_id: Uuid,
        ticket_id: Uuid,
        request: AddRoundRequest,
        #[serde(default)]
        origin_device_id: Option<String>,
    },
    SettleOpenTicket {
        teller_id: Uuid,
        ticket_id: Uuid,
        request: SettleOpenTicketRequest,
    },
    VoidOpenTicket {
        teller_id: Uuid,
        ticket_id: Uuid,
        request: VoidOpenTicketRequest,
    },
    // One line off a bill — "take the calamari off". Same request shape and
    // same rung as voiding the whole bill; `item_id` names the line and is the
    // idempotency key (a replayed void of a voided line is a no-op, which is
    // what stops a retried drain subtracting the price twice).
    VoidTicketLine {
        teller_id: Uuid,
        ticket_id: Uuid,
        item_id: Uuid,
        request: VoidOpenTicketRequest,
    },
    // KDS bump/unbump (kitchen device, or a teller on the till queue). `item_id`
    // is the kitchen line; it doubles as the idempotency key (re-bumping a bumped
    // line is a no-op, and a bump for a gone line replays as a clean no-op).
    BumpKitchenItem {
        teller_id: Uuid,
        item_id: Uuid,
    },
    UnbumpKitchenItem {
        teller_id: Uuid,
        item_id: Uuid,
    },
    // Held-order (teller parked-cart) ops. All idempotent on the CLIENT-minted
    // held-order id; a park that loses a table race applies WITHOUT the table
    // (never dead-letters — see held_orders::handlers).
    // Floor ops shared by tellers (held orders) and waiters (their tickets).
    // Per-occupant permissions are enforced inside the cores.
    SwapTables {
        teller_id: Uuid,
        request: SwapTablesRequest,
    },
    CreateTableTransfer {
        teller_id: Uuid,
        request: CreateFloorTransferRequest,
    },
    CancelTableTransfer {
        teller_id: Uuid,
        transfer_id: Uuid,
    },
    FulfillTableTransfer {
        teller_id: Uuid,
        transfer_id: Uuid,
        request: FulfillTransferRequest,
    },
    /// "The plates are gone" — the one table transition no server can observe.
    /// The POS has always queued this with an empty `request` (no branch), so
    /// the core reads the branch off the table; `request` is accepted and
    /// ignored purely so a body that IS sent doesn't fail the envelope.
    ClearTable {
        teller_id: Uuid,
        table_id: Uuid,
        #[serde(default)]
        request: serde_json::Value,
    },
    /// A till parked one of its own device-local orders on a table, or took it
    /// back off. Only the OCCUPANCY crosses the wire — the parked order itself
    /// never leaves the device — so the floor the dashboard sees matches the
    /// room, and the next party isn't seated on top of a held cart.
    HoldTable {
        teller_id: Uuid,
        table_id: Uuid,
        #[serde(default)]
        request: serde_json::Value,
    },
    ReleaseTable {
        teller_id: Uuid,
        table_id: Uuid,
        #[serde(default)]
        request: ReleaseReplay,
    },
    // Bookings at service time: a waiter/teller seats a booked party or marks
    // a no-show while the cloud is unreachable. Both idempotent on status.
    SeatBooking {
        teller_id: Uuid,
        booking_id: Uuid,
        #[serde(default)]
        request: crate::bookings::handlers::SeatBookingRequest,
    },
    NoShowBooking {
        teller_id: Uuid,
        booking_id: Uuid,
    },
}

impl ReplayOp {
    fn teller_id(&self) -> Uuid {
        match self {
            ReplayOp::OpenTill { teller_id, .. }
            | ReplayOp::CloseTill { teller_id, .. }
            | ReplayOp::CreateOrder { teller_id, .. }
            | ReplayOp::VoidOrder { teller_id, .. }
            | ReplayOp::RefundOrder { teller_id, .. }
            | ReplayOp::CashMovement { teller_id, .. }
            | ReplayOp::FireOpenTicket { teller_id, .. }
            | ReplayOp::AddTicketRound { teller_id, .. }
            | ReplayOp::SettleOpenTicket { teller_id, .. }
            | ReplayOp::VoidOpenTicket { teller_id, .. }
            | ReplayOp::VoidTicketLine { teller_id, .. }
            | ReplayOp::BumpKitchenItem { teller_id, .. }
            | ReplayOp::UnbumpKitchenItem { teller_id, .. }
            | ReplayOp::SwapTables { teller_id, .. }
            | ReplayOp::CreateTableTransfer { teller_id, .. }
            | ReplayOp::CancelTableTransfer { teller_id, .. }
            | ReplayOp::FulfillTableTransfer { teller_id, .. }
            | ReplayOp::ClearTable { teller_id, .. }
            | ReplayOp::HoldTable { teller_id, .. }
            | ReplayOp::ReleaseTable { teller_id, .. }
            | ReplayOp::SeatBooking { teller_id, .. }
            | ReplayOp::NoShowBooking { teller_id, .. }
            | ReplayOp::AwardLoyaltyPoints { teller_id, .. } => *teller_id,
        }
    }

    /// The variant's name, for a flag row the owner reads.
    fn variant_name(&self) -> &'static str {
        match self {
            ReplayOp::OpenTill { .. } => "OpenTill",
            ReplayOp::CloseTill { .. } => "CloseTill",
            ReplayOp::CashMovement { .. } => "CashMovement",
            ReplayOp::CreateOrder { .. } => "CreateOrder",
            ReplayOp::VoidOrder { .. } => "VoidOrder",
            ReplayOp::RefundOrder { .. } => "RefundOrder",
            ReplayOp::FireOpenTicket { .. } => "FireOpenTicket",
            ReplayOp::AddTicketRound { .. } => "AddTicketRound",
            ReplayOp::SettleOpenTicket { .. } => "SettleOpenTicket",
            ReplayOp::VoidOpenTicket { .. } => "VoidOpenTicket",
            ReplayOp::VoidTicketLine { .. } => "VoidTicketLine",
            ReplayOp::BumpKitchenItem { .. } => "BumpKitchenItem",
            ReplayOp::UnbumpKitchenItem { .. } => "UnbumpKitchenItem",
            ReplayOp::SwapTables { .. } => "SwapTables",
            ReplayOp::CreateTableTransfer { .. } => "CreateTableTransfer",
            ReplayOp::CancelTableTransfer { .. } => "CancelTableTransfer",
            ReplayOp::FulfillTableTransfer { .. } => "FulfillTableTransfer",
            ReplayOp::ClearTable { .. } => "ClearTable",
            ReplayOp::HoldTable { .. } => "HoldTable",
            ReplayOp::ReleaseTable { .. } => "ReleaseTable",
            ReplayOp::SeatBooking { .. } => "SeatBooking",
            ReplayOp::NoShowBooking { .. } => "NoShowBooking",
            ReplayOp::AwardLoyaltyPoints { .. } => "AwardLoyaltyPoints",
        }
    }

    /// Did real value change hands, so that refusing the op would lose a fact
    /// rather than prevent one? This is the accept-and-flag test (§4.4.5).
    ///
    /// The question is NOT "is this op important" — it is "did something
    /// already happen in the shop that the books must now agree with". Cash
    /// crossing the counter, a drawer opened or counted, a sale rung, taken off
    /// the books, or paid back: all of those are facts by the time they reach
    /// us, and points are value the customer was already promised.
    ///
    /// Everything else is a request about state we still control — a bump, a
    /// table move, a booking, tearing up an unpaid ticket. Nothing is lost by
    /// refusing those, so an actor who lacks the capability is refused, exactly
    /// as they would be live.
    ///
    /// **A void is deliberately NOT here.** `required_permissions` above defines
    /// it as "taking a sale off the books BEFORE any money moved" — that is the
    /// whole reason it sits on its own rung apart from `refunds`. Nothing has
    /// changed hands, so a revoked void stays refused, and
    /// `a_revoked_void_does_not_get_through_by_being_queued` still holds. A
    /// refund, where the money really did go back, is a different op and is
    /// flagged.
    fn money_moved(&self) -> bool {
        matches!(
            self,
            ReplayOp::OpenTill { .. }
                | ReplayOp::CloseTill { .. }
                | ReplayOp::CashMovement { .. }
                | ReplayOp::CreateOrder { .. }
                | ReplayOp::RefundOrder { .. }
                | ReplayOp::SettleOpenTicket { .. }
                | ReplayOp::AwardLoyaltyPoints { .. }
        )
    }

    /// The `(resource, action)` permission(s) the LIVE endpoint enforces for this
    /// op. Replay checks the SAME ones against the op's embedded actor, through
    /// the same resolver the live route uses (super_admin → per-user override →
    /// role default → deny), so a grant made in the dashboard works offline and
    /// a revocation stops a queued op the same as a live one.
    ///
    /// This is THE authority on what a replayed op may do. There is no role
    /// table beside it any more. There used to be — `actor_role_allowed`, a
    /// hard-coded role → op match — and it kept disagreeing with the table in
    /// both directions: a teller seating a party at a table queued a fire the
    /// role table refused while the live route (which checks
    /// `open_tickets:create` and no role at all) allowed it, and a waiter the
    /// dashboard had granted `kitchen_orders:update` could bump live but had
    /// the queued bump thrown out. Since the POS drains EVERY write through
    /// `/sync/replay`, "stricter than live" here never meant "safer"; it meant
    /// the feature did not work offline. The role's part is now attribution
    /// only — the embedded actor must hold `pos.sign_in` — and this list decides the rest.
    ///
    /// Kept in lock-step with the per-endpoint `check_permission` calls:
    ///   open=shifts/create; close+cash=shifts/update;
    ///   order create=orders/create; order VOID=orders/delete;
    ///   ticket fire=open_tickets/create; round=open_tickets/update;
    ///   ticket VOID=open_tickets/delete;
    ///   settle=open_tickets/update + orders/create + payments/create;
    ///   bump=kitchen_orders/update.
    /// A live route that changes its check without changing this list has
    /// re-created the drift this function exists to prevent.
    fn required_permissions(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            ReplayOp::OpenTill { .. } => &[("tills", "create")],
            ReplayOp::CloseTill { .. } => &[("tills", "update")],
            ReplayOp::CashMovement { .. } => &[("tills", "update")],
            ReplayOp::CreateOrder { .. } => &[("orders", "create")],
            // A void is its own rung. Ringing up is `create`; voiding is
            // `delete` — nothing hard-deletes an order, so the rung was free,
            // and it is what a void is: taking a sale off the books before any
            // money moved. Splitting it from `update` is what lets a shop hand
            // voiding to someone other than the person ringing up (the owner
            // ruled there is no approval FLOW, not that the grant is
            // indivisible). Refunds are a different event again, under the
            // `refunds` resource.
            ReplayOp::VoidOrder { .. } => &[("orders", "delete")],
            // Returning money is its own resource (`refunds:create`), held
            // apart from voiding so a shop may give the two to different
            // people. Same check as `POST /refunds`.
            ReplayOp::RefundOrder { .. } => &[("refunds", "create")],
            ReplayOp::FireOpenTicket { .. } => &[("open_tickets", "create")],
            ReplayOp::AddTicketRound { .. } => &[("open_tickets", "update")],
            // Settle writes a paid order AND its payment legs, and the live
            // route asks for all three — so does this, or a teller whose
            // `payments:create` was revoked could still take money by
            // queueing the settle.
            ReplayOp::SettleOpenTicket { .. } => &[
                ("open_tickets", "update"),
                ("orders", "create"),
                ("payments", "create"),
            ],
            // Tearing up a bill is the ticket's void rung, same reasoning as
            // the order's: separate from adding to it.
            ReplayOp::VoidOpenTicket { .. } | ReplayOp::VoidTicketLine { .. } => {
                &[("open_tickets", "delete")]
            }
            ReplayOp::BumpKitchenItem { .. } | ReplayOp::UnbumpKitchenItem { .. } => {
                &[("kitchen_orders", "update")]
            }
            // Swap checks the moved tickets inside the core, and so does clear.
            ReplayOp::SwapTables { .. } | ReplayOp::ClearTable { .. } => &[],
            // Parking an order on a table is the same authority as working the
            // ticket that would otherwise sit there.
            ReplayOp::HoldTable { .. } | ReplayOp::ReleaseTable { .. } => {
                &[("open_tickets", "update")]
            }
            ReplayOp::CreateTableTransfer { .. } => &[("table_transfers", "create")],
            ReplayOp::CancelTableTransfer { .. } => &[("table_transfers", "update")],
            // Fulfill also checks the occupant's kind inside the core.
            ReplayOp::FulfillTableTransfer { .. } => &[("table_transfers", "update")],
            ReplayOp::SeatBooking { .. } | ReplayOp::NoShowBooking { .. } => {
                &[("bookings", "update")]
            }
            // Lock-step with `loyalty::award::award`'s own check, so a teller
            // whose loyalty grant was revoked cannot get an award through by
            // having queued it offline.
            ReplayOp::AwardLoyaltyPoints { .. } => &[("loyalty", "update")],
        }
    }
}

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing authentication".into()))
}

/// An old envelope names the till `shift_id`: at the top (`close_shift`,
/// `cash_movement`) or inside the request (`create_order`, `settle_open_ticket`,
/// `refund_order`).
pub fn replay_names_shift_id(body: &serde_json::Value) -> bool {
    body.get("shift_id").is_some()
        || body
            .get("request")
            .and_then(|r| r.get("shift_id"))
            .is_some()
}

/// POST /sync/replay — flush ONE queued op, attributed to its embedded teller.
///
/// Authorization, in two parts that answer two different questions:
///
///   * ATTRIBUTION — may this write carry this actor's name? The bearer must be
///     a member of an org, and the op's embedded actor must be an ACTIVE TILL
///     USER OF THAT SAME ORG WHO HOLDS `pos.sign_in`. So any teller (or, later,
///     a device principal) may flush the whole device backlog — A's ops and B's
///     ops — each landing under its true author.
///   * PERMISSION — may this actor do this thing? Answered by the permission
///     tables, via `ReplayOp::required_permissions`, exactly as the live route
///     answers it. Not by role here, not anywhere else.
///
/// The op's target (branch / shift / order) must also belong to the bearer's
/// org, so a token can never replay across orgs.
///
/// One op per call keeps the proven client-side drain engine (FIFO, dependency
/// gating, backoff, idempotency, close-last) intact — the client just points each
/// op at this route instead of the per-resource live one.
pub async fn replay(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    body: web::Json<serde_json::Value>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Old POS (v0.5.1 / v0.6.0) queue `open_shift` / `close_shift`; their acks
    // keep the legacy `Shift` / `CloseShiftResponse` shapes.
    let legacy_op = matches!(
        body.get("op").and_then(|v| v.as_str()),
        Some("open_shift" | "close_shift")
    );
    if legacy_op {
        crate::client_seen::legacy_hit(crate::client_seen::KIND_REPLAY_LEGACY_OP);
    }
    if replay_names_shift_id(&body) {
        crate::client_seen::legacy_hit(crate::client_seen::KIND_REPLAY_SHIFT_ID_FIELD);
    }
    let token_org = claims
        .org_id()
        .ok_or_else(|| AppError::Unauthorized("Token has no organization".into()))?;

    let header_device = crate::devices::DeviceHeader::from_request_headers(&req);
    let body = body.into_inner();
    let occurred_at = replay_occurred_at(&body);
    let op: ReplayOp = serde_json::from_value(body)
        .map_err(|e| AppError::BadRequest(format!("Json deserialize error: {e}")))?;
    let teller_id = op.teller_id();

    // ATTRIBUTION. The embedded actor must be a real, active till user of the
    // bearer's org — someone who could have unlocked the device that queued this
    // op. That is all the role decides here: a write can never be replayed under
    // an actor from a different org, a disabled account, or a role that never
    // signs in at a till. It says nothing about what the op may DO; that is the
    // permission check below, and only that.
    let row: Option<(Option<Uuid>, bool, UserRole)> = sqlx::query_as(
        "SELECT org_id, is_active, role FROM users WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(teller_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (actor_org, is_active, actor_role) = match row {
        Some((Some(org), active, role)) => (org, active, role),
        _ => {
            return Err(AppError::Forbidden(
                "Replay actor is not a member of this organization".into(),
            ));
        }
    };
    let signs_in = crate::authz::require::effective(pool.get_ref(), teller_id, None)
        .await?
        .can(crate::authz::Cap::PosSignIn);
    if actor_org != token_org || !is_active || !signs_in {
        return Err(AppError::Forbidden(
            "Replay actor may not perform this operation for this organization".into(),
        ));
    }

    // PERMISSION. The SAME `(resource, action)` checks the LIVE endpoint makes,
    // resolved against the ACTOR (the op's embedded author) through the same
    // super_admin → per-user override → role default → deny chain. This is the
    // one place "may they" is answered for a replayed op: a teller whose void
    // was revoked in the dashboard cannot get it through by queueing it, and a
    // waiter the dashboard granted a bump to gets the bump through offline.
    //
    // ACCEPT AND FLAG (§4.4.5, and the owner's binding decision). A failure
    // here rejects a NON-money op — nothing irreversible happened, so refusing
    // it is honest. A MONEY op is accepted anyway and recorded in
    // `authz_replay_flags` for the owner, because the sale already happened:
    // the customer paid and left while the shop was offline. Dropping the op
    // does not un-take the money, it only loses the record and leaves the
    // drawer short at close.
    let mut flags: Vec<(&'static str, &'static str)> = Vec::new();
    for &(resource, action) in op.required_permissions() {
        match crate::permissions::checker::check_permission_for(
            pool.get_ref(),
            teller_id,
            &actor_role,
            resource,
            action,
        )
        .await
        {
            Ok(()) => {}
            Err(AppError::Forbidden(_)) if op.money_moved() => flags.push((resource, action)),
            Err(e) => return Err(e),
        }
    }

    // The target must belong to the bearer's org — block any cross-org replay.
    let op_branch = op_branch_must_be_in_org(pool.get_ref(), &op, token_org).await?;

    let actor = ActingContext::replay_with_role(teller_id, token_org, actor_role);
    let op_branch = match (op_branch, &op) {
        (Some(b), _) => Some(b),
        // A till opened by this very op, a sale on it: the branch is on the op.
        (None, ReplayOp::OpenTill { branch_id, .. }) => Some(*branch_id),
        (None, ReplayOp::CreateOrder { request, .. }) => Some(request.branch_id),
        _ => None,
    };
    let op_name = op.variant_name();
    let result = replay_dispatch(&req, &pool, &hub, op, actor, legacy_op, header_device).await;
    // Only once the op has really committed: a flag for an op that never
    // applied would send the owner looking for money that never moved.
    if result.is_ok() && !flags.is_empty() {
        record_replay_flags(
            pool.get_ref(),
            token_org,
            op_branch,
            op_name,
            teller_id,
            &flags,
            occurred_at,
        )
        .await;
    }
    stamp_sync_seq(pool.get_ref(), op_branch, result).await
}

/// When the act happened ON THE DEVICE, for the accept-and-flag reason (§4.4.5)
/// and the offline window a flag row shows.
///
/// Every field here is one clients ALREADY send, so no release in the field has
/// to change to be classified correctly: a refund names `issued_at`, a cash
/// movement and an order name `created_at`. A new top-level `occurred_at` is
/// read first so later clients can be explicit for ops that carry neither.
///
/// It matters because it decides which of the two reasons the owner sees. Read
/// as "now", a revocation made an hour ago always looks NEWER than the act, and
/// every routine stale-snapshot case would be reported as a tampered client.
fn replay_occurred_at(body: &serde_json::Value) -> chrono::DateTime<chrono::Utc> {
    let parse = |v: Option<&serde_json::Value>| {
        v.and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
    };
    let req = body.get("request");
    parse(body.get("occurred_at"))
        .or_else(|| parse(req.and_then(|r| r.get("issued_at"))))
        .or_else(|| parse(req.and_then(|r| r.get("created_at"))))
        .unwrap_or_else(chrono::Utc::now)
}

/// Record the accept-and-flag rows for one replayed op (§4.4.5).
///
/// Never fails the request: the op has already committed, and losing the
/// owner's notice is far better than 500-ing a sale that is now on the books
/// and making the tablet retry a write it has already applied.
async fn record_replay_flags(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Option<Uuid>,
    op: &'static str,
    author_id: Uuid,
    flags: &[(&'static str, &'static str)],
    occurred_at: chrono::DateTime<chrono::Utc>,
) {
    // Was this a revocation the device had not heard about yet? If anything
    // touching this person's grants was written AFTER the act, the device was
    // working from a snapshot that was true when it acted — routine, and a
    // different thing from a client that never had the grant at all.
    let revoked_after: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM authz_grant_events e
              WHERE e.occurred_at > $2
                AND (e.before->>'user_id' = $1::text OR e.after->>'user_id' = $1::text)
         )",
    )
    .bind(author_id)
    .bind(occurred_at)
    .fetch_one(pool)
    .await
    .unwrap_or(false);
    let reason = if revoked_after {
        "stale_snapshot"
    } else {
        "unauthorized_offline"
    };

    for (resource, action) in flags {
        let cap = format!("{resource}:{action}");
        if let Err(e) = sqlx::query(
            "INSERT INTO authz_replay_flags
                 (org_id, branch_id, op, author_id, capability, reason, occurred_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(org_id)
        .bind(branch_id)
        .bind(op)
        .bind(author_id)
        .bind(&cap)
        .bind(reason)
        .bind(occurred_at)
        .execute(pool)
        .await
        {
            tracing::error!(
                error = %e, %org_id, %author_id, op, cap,
                "could not record an authz replay flag; the op itself committed"
            );
        }
    }
}

/// `X-Madar-Sync-Seq` on a replay answer (OFFLINE_B_DESIGN §4): the branch
/// changefeed's committed horizon once the op has committed. Every change the op
/// made has a seq at or below it, so a device whose cursor has reached it and
/// whose feed still does not list the row knows the row is really gone — no
/// time-based grace needed. Absent when the branch cannot be resolved.
pub(crate) const SYNC_SEQ_HEADER: &str = "X-Madar-Sync-Seq";

async fn stamp_sync_seq(
    pool: &PgPool,
    branch: Option<Uuid>,
    result: Result<HttpResponse, AppError>,
) -> Result<HttpResponse, AppError> {
    let mut resp = result?;
    if let Some(branch) = branch {
        if resp.status().is_success() {
            let seq: Option<i64> = sqlx::query_scalar("SELECT sync_safe_horizon($1, 0, 200)")
                .bind(branch)
                .fetch_one(pool)
                .await
                .ok();
            if let Some(seq) = seq.filter(|s| *s > 0) {
                if let Ok(v) = actix_web::http::header::HeaderValue::from_str(&seq.to_string()) {
                    resp.headers_mut().insert(
                        actix_web::http::header::HeaderName::from_static("x-madar-sync-seq"),
                        v,
                    );
                }
            }
        }
    }
    Ok(resp)
}

async fn replay_dispatch(
    req: &HttpRequest,
    pool: &crate::db::Db,
    hub: &web::Data<BranchEventHub>,
    op: ReplayOp,
    actor: ActingContext,
    legacy_op: bool,
    header_device: Option<Uuid>,
) -> Result<HttpResponse, AppError> {
    let _ = req;
    match op {
        ReplayOp::OpenTill {
            branch_id,
            device_id,
            device_code,
            verification,
            request,
            ..
        } => {
            let (till, created) = crate::tills::handlers::open_till_inner(
                pool,
                Some(hub.get_ref()),
                branch_id,
                request,
                actor,
                crate::tills::handlers::OpenMeta {
                    device_id: device_id.or(header_device),
                    device_code,
                    verification,
                },
            )
            .await?;
            let mut out = if created {
                HttpResponse::Created()
            } else {
                HttpResponse::Ok()
            };
            if legacy_op {
                let shift = crate::tills::legacy::legacy_shift(
                    pool,
                    till,
                    crate::tills::legacy::LegacyJoins::Till,
                )
                .await?;
                return Ok(out.json(shift));
            }
            Ok(out.json(till))
        }
        ReplayOp::CloseTill {
            till_id,
            device_id,
            mut request,
            ..
        } => {
            request.device_id = request.device_id.or(device_id).or(header_device);
            // An already-closed till is returned as stored; the old backend read
            // it back WITH the drawer join, a fresh close without it.
            let already_closed = legacy_op
                && crate::tills::handlers::fetch_till_or_404(pool, till_id)
                    .await?
                    .status
                    != "open";
            let resp = crate::tills::handlers::close_till_inner(
                pool,
                Some(hub.get_ref()),
                till_id,
                request,
                actor,
            )
            .await?;
            if legacy_op {
                let shift = crate::tills::legacy::legacy_shift(
                    pool,
                    resp.till,
                    if already_closed {
                        crate::tills::legacy::LegacyJoins::Till
                    } else {
                        crate::tills::legacy::LegacyJoins::None
                    },
                )
                .await?;
                return Ok(
                    HttpResponse::Ok().json(crate::tills::legacy::CloseShiftResponse { shift })
                );
            }
            Ok(HttpResponse::Ok().json(resp))
        }
        ReplayOp::CreateOrder {
            device_id,
            device_code,
            mut request,
            ..
        } => {
            request.device_id = request.device_id.or(device_id).or(header_device);
            if request.device_code.is_none() {
                request.device_code = device_code;
            }
            // Replay never fires to the KDS (the order is historical) → hub = None.
            // A replayed direct sale has no waiter (only ticket settles do) → None.
            crate::orders::handlers::create_order_inner(
                pool.clone(),
                web::Json(request),
                actor,
                None,
                None,
            )
            .await
        }
        ReplayOp::VoidOrder {
            order_id, request, ..
        } => {
            crate::orders::handlers::void_order_inner(
                pool.clone(),
                order_id,
                web::Json(request),
                actor,
            )
            .await
        }
        ReplayOp::RefundOrder { request, .. } => {
            crate::refunds::handlers::create_refund_inner(pool.clone(), web::Json(request), actor)
                .await
        }
        ReplayOp::AwardLoyaltyPoints { request, .. } => {
            crate::loyalty::award::award_inner(
                pool,
                request,
                Some(actor.teller_id),
                Some(actor.org_id),
            )
            .await
        }
        ReplayOp::CashMovement {
            till_id,
            device_id,
            mut request,
            ..
        } => {
            request.device_id = request.device_id.or(device_id).or(header_device);
            crate::tills::handlers::add_cash_movement_inner(
                pool,
                Some(hub.get_ref()),
                till_id,
                request,
                actor,
            )
            .await
        }
        // Ticket ops: publish to the realtime bus (hub = Some). Waiter devices fire
        // offline-first — the fire/round/settle/void ALWAYS arrives here via the
        // outbox, even when the waiter is online — so a connected teller/KDS only
        // gets a live push (and the ping/notification) if replay publishes. The
        // inner handlers dedup on the idempotency key BEFORE publishing, so an
        // at-least-once retry re-applies as a no-op and emits nothing; only the
        // first apply fires the event. A consumer that was offline still re-seeds
        // via the realtime snapshot on reconnect.
        ReplayOp::FireOpenTicket {
            request,
            origin_device_id,
            ..
        } => {
            crate::tickets::handlers::create_open_ticket_inner(
                pool.clone(),
                web::Json(request),
                actor,
                Some(hub.get_ref()),
                crate::tickets::clean_device_id(origin_device_id.as_deref()),
            )
            .await
        }
        ReplayOp::AddTicketRound {
            ticket_id,
            request,
            origin_device_id,
            ..
        } => {
            crate::tickets::handlers::add_round_inner(
                pool.clone(),
                ticket_id,
                web::Json(request),
                actor,
                Some(hub.get_ref()),
                crate::tickets::clean_device_id(origin_device_id.as_deref()),
            )
            .await
        }
        ReplayOp::SettleOpenTicket {
            ticket_id, request, ..
        } => {
            crate::tickets::handlers::settle_open_ticket_inner(
                pool.clone(),
                ticket_id,
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::VoidOpenTicket {
            ticket_id, request, ..
        } => {
            crate::tickets::handlers::void_open_ticket_inner(
                pool.clone(),
                ticket_id,
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::VoidTicketLine {
            ticket_id,
            item_id,
            request,
            ..
        } => {
            crate::tickets::handlers::void_ticket_line_inner(
                pool.clone(),
                ticket_id,
                item_id,
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        // Bump/unbump: publish so other KDS/till devices reflect the bump live.
        ReplayOp::BumpKitchenItem { item_id, .. } => {
            crate::kitchen::kds::set_bump_inner(pool, Some(hub.get_ref()), &actor, item_id, true)
                .await
        }
        ReplayOp::UnbumpKitchenItem { item_id, .. } => {
            crate::kitchen::kds::set_bump_inner(pool, Some(hub.get_ref()), &actor, item_id, false)
                .await
        }
        // Cross-table floor ops: publish (hub = Some) so other tills' canvases
        // update live; the cores dedup before publishing, same as tickets.
        //
        // Parking an order is NOT here. A parked order is a client-local draft,
        // so it has nothing to replay -- which is the point: the offline path
        // for the commonest POS action is now no path at all.
        ReplayOp::SwapTables { request, .. } => {
            crate::floor_ops::handlers::swap_tables_inner(
                pool.clone(),
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::CreateTableTransfer { request, .. } => {
            crate::floor_ops::handlers::create_transfer_inner(
                pool.clone(),
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::CancelTableTransfer { transfer_id, .. } => {
            crate::floor_ops::handlers::cancel_transfer_inner(
                pool.clone(),
                transfer_id,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::ClearTable { table_id, .. } => {
            crate::floor_ops::handlers::clear_table_inner(
                pool.clone(),
                table_id,
                // No branch on the wire: the core reads it off the table.
                None,
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::HoldTable {
            table_id, request, ..
        } => {
            // The till stamps when the party sat; a malformed or absent stamp
            // just means "now", never a refused op.
            let seated_at = request
                .get("seated_at")
                .and_then(|v| v.as_str())
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                .map(|v| v.with_timezone(&chrono::Utc));
            // Covers, when the till counted them; anything unreadable is
            // simply not recorded.
            let party_size = request
                .get("party_size")
                .and_then(|v| v.as_i64())
                .and_then(|v| i32::try_from(v).ok());
            crate::floor_ops::handlers::hold_table_inner(
                pool.clone(),
                table_id,
                None,
                seated_at,
                party_size,
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::ReleaseTable {
            table_id, request, ..
        } => {
            crate::floor_ops::handlers::release_table_inner(
                pool.clone(),
                table_id,
                None,
                request.bus,
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::FulfillTableTransfer {
            transfer_id,
            request,
            ..
        } => {
            crate::floor_ops::handlers::fulfill_transfer_inner(
                pool.clone(),
                transfer_id,
                web::Json(request),
                actor,
                Some(hub.get_ref()),
            )
            .await
        }
        ReplayOp::SeatBooking {
            booking_id,
            request,
            ..
        } => {
            let resp =
                crate::bookings::handlers::seat_inner(pool, booking_id, &request, &actor).await?;
            crate::bookings::publish_booking(pool, hub.get_ref(), "booking.changed", booking_id)
                .await;
            Ok(resp)
        }
        ReplayOp::NoShowBooking { booking_id, .. } => {
            let resp = crate::bookings::handlers::no_show_inner(pool, booking_id).await?;
            crate::bookings::publish_booking(pool, hub.get_ref(), "booking.changed", booking_id)
                .await;
            Ok(resp)
        }
    }
}

/// Verify the op's effective branch belongs to `org` (resolving shift / order to
/// their branch first). A missing target is left to the inner handler (it will
/// 404/409 idempotently) — we only reject a target that exists in a DIFFERENT org.
async fn op_branch_must_be_in_org(
    pool: &PgPool,
    op: &ReplayOp,
    org: Uuid,
) -> Result<Option<Uuid>, AppError> {
    let branch_org: Option<(Uuid, Uuid)> = match op {
        ReplayOp::OpenTill { branch_id, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>("SELECT id, org_id FROM branches WHERE id = $1")
                .bind(branch_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::CreateOrder { request, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>("SELECT id, org_id FROM branches WHERE id = $1")
                .bind(request.branch_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::CloseTill { till_id, .. } | ReplayOp::CashMovement { till_id, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT b.id, b.org_id FROM tills s JOIN branches b ON b.id = s.branch_id WHERE s.id = $1",
            )
            .bind(till_id)
            .fetch_optional(pool)
            .await?
        }
        ReplayOp::VoidOrder { order_id, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT b.id, b.org_id FROM orders o JOIN branches b ON b.id = o.branch_id WHERE o.id = $1",
            )
            .bind(order_id)
            .fetch_optional(pool)
            .await?
        }
        // Resolved through the order the money goes back against; the
        // `order_refunds` trigger then refuses a shift at any other branch.
        ReplayOp::RefundOrder { request, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT b.id, b.org_id FROM orders o JOIN branches b ON b.id = o.branch_id WHERE o.id = $1",
            )
            .bind(request.order_id)
            .fetch_optional(pool)
            .await?
        }
        // Resolved through the branch the op names; `award_inner` then re-checks
        // it against the ORDER's own org, which is the boundary that counts.
        ReplayOp::AwardLoyaltyPoints { request, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>("SELECT id, org_id FROM branches WHERE id = $1")
                .bind(request.branch_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::FireOpenTicket { request, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>("SELECT id, org_id FROM branches WHERE id = $1")
                .bind(request.branch_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::AddTicketRound { ticket_id, .. }
        | ReplayOp::SettleOpenTicket { ticket_id, .. }
        | ReplayOp::VoidOpenTicket { ticket_id, .. }
        | ReplayOp::VoidTicketLine { ticket_id, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT b.id, b.org_id FROM open_tickets ot JOIN branches b ON b.id = ot.branch_id WHERE ot.id = $1",
            )
            .bind(ticket_id)
            .fetch_optional(pool)
            .await?
        }
        ReplayOp::BumpKitchenItem { item_id, .. } | ReplayOp::UnbumpKitchenItem { item_id, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT b.id, b.org_id FROM kitchen_ticket_items kti \
                 JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id \
                 JOIN branches b ON b.id = kt.branch_id WHERE kti.id = $1",
            )
            .bind(item_id)
            .fetch_optional(pool)
            .await?
        }
        ReplayOp::SwapTables { request, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>("SELECT id, org_id FROM branches WHERE id = $1")
                .bind(request.branch_id)
                .fetch_optional(pool)
                .await?
        }
        // The op carries no branch, so the table is the only thing to resolve
        // through — which is exactly the cross-org check this fn exists for.
        ReplayOp::ClearTable { table_id, .. }
        | ReplayOp::HoldTable { table_id, .. }
        | ReplayOp::ReleaseTable { table_id, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT b.id, b.org_id FROM branch_tables bt JOIN branches b ON b.id = bt.branch_id \
                 WHERE bt.id = $1",
            )
            .bind(table_id)
            .fetch_optional(pool)
            .await?
        }
        ReplayOp::CreateTableTransfer { request, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>("SELECT id, org_id FROM branches WHERE id = $1")
                .bind(request.branch_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::CancelTableTransfer { transfer_id, .. }
        | ReplayOp::FulfillTableTransfer { transfer_id, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>("SELECT branch_id, org_id FROM table_transfer_requests WHERE id = $1")
                .bind(transfer_id)
                .fetch_optional(pool)
                .await?
        }
        ReplayOp::SeatBooking { booking_id, .. } | ReplayOp::NoShowBooking { booking_id, .. } => {
            sqlx::query_as::<_, (Uuid, Uuid)>("SELECT branch_id, org_id FROM bookings WHERE id = $1")
                .bind(booking_id)
                .fetch_optional(pool)
                .await?
        }
    };
    match branch_org {
        Some((_, o)) if o != org => Err(AppError::Forbidden(
            "Replay target belongs to another organization".into(),
        )),
        // same org, or not-yet-present (inner handler resolves it)
        other => Ok(other.map(|(branch, _)| branch)),
    }
}
