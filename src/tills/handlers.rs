//! Tills: a PERSON's sales session with its own cash drawer, opened on a device
//! and bound to it (TILLS_DECISIONS 1–9, TILLS_CONTRACT §2.2 / §3).
//!
//! Many tills may be open at one branch. At most one open till per person is
//! enforced only by the live server check here (and the LAN peer check on the
//! device); replay ALWAYS accepts an open and flags a duplicate instead.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    devices::DeviceHeader,
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
    realtime::{
        event::{BranchEvent, Topic},
        hub::BranchEventHub,
    },
    sync::ActingContext,
    tills::reconcile::{self, ReconciliationInput, TillReconciliationLine},
};
use utoipa::{IntoParams, ToSchema};

const DEFAULT_TILLS_PER_PAGE: i64 = 20;
const MAX_TILLS_PER_PAGE: i64 = 200;

// ── Models ────────────────────────────────────────────────────

/// OpenAPI-only vocabulary for `Till.status` (the `till_status` DB enum). The
/// struct fields stay `String`, so the wire strings are exactly the DB values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TillStatus {
    Open,
    Closed,
    ForceClosed,
}

/// OpenAPI-only vocabulary for `Till.verification` (`tills_verification` CHECK):
/// how the one-open-till-per-person rule was checked when the till opened.
/// `legacy` marks tills opened by pre-rework clients / before the rework.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TillVerification {
    Server,
    Lan,
    Unverified,
    Legacy,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Till {
    pub id: Uuid,
    pub branch_id: Uuid,
    /// Branch label (populated by reads; may be null on some write responses).
    #[serde(default)]
    #[sqlx(default)]
    pub branch_name: Option<String>,
    pub teller_id: Uuid,
    pub teller_name: String,
    /// `open` | `closed` | `force_closed`
    #[schema(value_type = TillStatus)]
    pub status: String,
    pub opening_cash: i32,
    pub opening_cash_original: Option<i32>,
    pub opening_cash_was_edited: bool,
    pub opening_cash_edit_reason: Option<String>,
    pub closing_cash_declared: Option<i32>,
    pub closing_cash_system: Option<i32>,
    pub cash_discrepancy: Option<i32>,
    pub opened_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub closed_by: Option<Uuid>,
    pub force_closed_by: Option<Uuid>,
    pub force_closed_at: Option<DateTime<Utc>>,
    pub force_close_reason: Option<String>,
    pub notes: Option<String>,
    #[serde(default)]
    #[sqlx(default)]
    pub timezone: Option<String>,
    pub device_id: Option<Uuid>,
    pub device_code: Option<String>,
    pub device_label: Option<String>,
    /// `server` | `lan` | `unverified` | `legacy`
    #[schema(value_type = TillVerification)]
    pub verification: String,
    pub opened_while_another_open: bool,
    pub other_till_id: Option<Uuid>,
    pub flagged_at: Option<DateTime<Utc>>,
    /// `clean` | `disagreed` | `unreviewed` | null (open, or closed before reconciliation existed)
    pub reconciliation_status: Option<String>,
    pub disagreement_count: i64,
    pub open_bills_at_close: Option<i32>,
    pub old_bills_at_close: Option<i32>,
}

/// Every column of [`Till`], from `tills s` joined to `users u`, `branches b`.
pub(crate) const TILL_COLUMNS: &str = r#"
    s.id, s.branch_id, b.name AS branch_name, s.teller_id, u.name AS teller_name,
    s.status::text AS status,
    s.opening_cash, s.opening_cash_original, s.opening_cash_was_edited, s.opening_cash_edit_reason,
    s.closing_cash_declared, s.closing_cash_system, s.cash_discrepancy,
    s.opened_at, s.closed_at, s.closed_by,
    s.force_closed_by, s.force_closed_at, s.force_close_reason, s.notes,
    effective_timezone(s.branch_id) AS timezone,
    s.device_id, s.device_code, s.device_label, s.verification,
    s.opened_while_another_open, s.other_till_id, s.flagged_at, s.reconciliation_status,
    (SELECT COUNT(*) FROM till_reconciliations r WHERE r.till_id = s.id AND r.status = 'disagreed') AS disagreement_count,
    s.open_bills_at_close, s.old_bills_at_close
"#;
pub(crate) const TILL_FROM: &str =
    "FROM tills s JOIN users u ON u.id = s.teller_id JOIN branches b ON b.id = s.branch_id";

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct TillBrief {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub teller_id: Uuid,
    pub teller_name: String,
    #[schema(value_type = TillStatus)]
    pub status: String,
    pub opened_at: DateTime<Utc>,
    pub device_id: Option<Uuid>,
    pub device_code: Option<String>,
    pub device_label: Option<String>,
    #[schema(value_type = TillVerification)]
    pub verification: String,
    pub opened_while_another_open: bool,
}

impl From<&Till> for TillBrief {
    fn from(t: &Till) -> Self {
        Self {
            id: t.id,
            branch_id: t.branch_id,
            teller_id: t.teller_id,
            teller_name: t.teller_name.clone(),
            status: t.status.clone(),
            opened_at: t.opened_at,
            device_id: t.device_id,
            device_code: t.device_code.clone(),
            device_label: t.device_label.clone(),
            verification: t.verification.clone(),
            opened_while_another_open: t.opened_while_another_open,
        }
    }
}

/// What a cash movement IS, which fixes its sign.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CashMovementKind {
    PayIn,
    PayOut,
    SafeDrop,
    Correction,
}

impl CashMovementKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PayIn => "pay_in",
            Self::PayOut => "pay_out",
            Self::SafeDrop => "safe_drop",
            Self::Correction => "correction",
        }
    }

    pub fn from_sign(amount: i32) -> Self {
        if amount < 0 {
            Self::PayOut
        } else {
            Self::PayIn
        }
    }

    fn allows_sign(self, amount: i32) -> bool {
        match self {
            Self::PayIn => amount > 0,
            Self::PayOut | Self::SafeDrop => amount < 0,
            Self::Correction => amount != 0,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct CashMovement {
    pub id: Uuid,
    pub till_id: Uuid,
    /// DEPRECATED: same value as `till_id` (kept for POS v0.5.1/v0.6.0).
    pub shift_id: Uuid,
    pub amount: i32,
    pub kind: String,
    #[serde(default)]
    pub corrects_id: Option<Uuid>,
    pub note: String,
    pub moved_by: Uuid,
    pub moved_by_name: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub client_ref: Option<Uuid>,
    #[serde(default)]
    pub device_id: Option<Uuid>,
}

const CASH_MOVEMENT_COLUMNS: &str = "m.id, m.till_id, m.till_id AS shift_id, m.amount, m.kind, \
    m.corrects_id, m.note, m.moved_by, (SELECT name FROM users WHERE id = m.moved_by) AS moved_by_name, \
    m.created_at, m.client_ref, m.device_id";

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct OpenBillsNotice {
    pub open_bills_count: i64,
    pub open_bills_amount: i64,
    pub oldest_opened_at: Option<DateTime<Utc>>,
    pub old_bills_count: i64,
    pub old_bill_hours: i32,
    pub seated_tables_count: i64,
    /// `closed_at` of the most recent closed till at the branch.
    pub since: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct LastTillWarning {
    pub is_last_open_till: bool,
    pub open_bills_count: i64,
    pub open_bills_amount: i64,
    pub seated_tables_count: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct TillPreFill {
    pub has_open_till: bool,
    pub open_till: Option<Till>,
    pub open_elsewhere: Vec<TillBrief>,
    /// EVERY open till of the person at THIS branch, newest first (whatever the
    /// device). Normally zero or one; two or more only after an offline open
    /// was replayed while another was open — the newer is flagged
    /// (`opened_while_another_open`) and both stay open, so both are listed.
    #[serde(default)]
    pub open_at_branch: Vec<TillBrief>,
    pub suggested_opening_cash: i32,
    pub last_close_declared: Option<i32>,
    pub open_bills_notice: OpenBillsNotice,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PaymentSummaryRow {
    pub payment_method: String,
    pub is_cash: bool,
    pub total: i64,
    pub order_count: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct CashMovementSummaryRow {
    pub id: Uuid,
    pub amount: i32,
    pub kind: String,
    #[serde(default)]
    pub corrects_id: Option<Uuid>,
    #[serde(default)]
    pub corrects_kind: Option<String>,
    pub note: String,
    pub moved_by_name: String,
    pub created_at: DateTime<Utc>,
}

impl CashMovementSummaryRow {
    fn bucket(&self) -> &str {
        match (self.kind.as_str(), self.corrects_kind.as_deref()) {
            ("correction", Some(corrected)) => corrected,
            (kind, _) => kind,
        }
    }
}

/// The report figures shared by the new `TillReportResponse` and the legacy
/// `ShiftReportResponse` (flattened into both).
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct TillReportFigures {
    pub payment_summary: Vec<PaymentSummaryRow>,
    pub total_payments: i64,
    pub voided_amount: i64,
    pub net_payments: i64,
    pub total_tips: i64,
    pub cash_tips: i64,
    pub non_cash_tips: i64,
    pub cash_movements: Vec<CashMovementSummaryRow>,
    pub cash_movements_in: i64,
    pub cash_movements_out: i64,
    pub safe_drops: i64,
    pub cash_adjustments: i64,
    pub cash_movements_net: i64,
    #[serde(default)]
    pub refunds_issued_count: i64,
    #[serde(default)]
    pub refunds_issued_amount: i64,
    #[serde(default)]
    pub refunds_issued_cash: i64,
    #[serde(default)]
    pub cash_in_refunded_sales: i64,
    /// Tax on this till's sales, less the tax their refunds took back (a
    /// partial refund takes back its pro-rata share; a voided or fully
    /// refunded sale is out altogether). Additive.
    #[serde(default)]
    pub total_tax: i64,
    /// Service charge on this till's sales, less what their refunds took back.
    #[serde(default)]
    pub total_service_charge: i64,
    /// The tax and service charge inside the refunds issued FROM this till's
    /// drawer (`refunds_issued_amount`'s split).
    #[serde(default)]
    pub refunds_issued_tax: i64,
    #[serde(default)]
    pub refunds_issued_service_charge: i64,
    /// Table bills whose service charge was removed (`orders:waive_service`),
    /// and what those charges came to. Not part of any total.
    #[serde(default)]
    pub service_charge_waived_count: i64,
    #[serde(default)]
    pub service_charge_waived_amount: i64,
    /// `branches.standard_float`.
    pub standard_float: Option<i64>,
    pub suggested_safe_drop: Option<i64>,
    pub expected_cash: i64,
    /// Cash spot checks taken on this till, oldest first. Additive.
    #[serde(default)]
    pub spot_checks: Vec<crate::tills::spot_checks::TillSpotCheck>,
    pub printed_at: DateTime<Utc>,
    #[serde(default)]
    pub timezone: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct OrderNumberRange {
    pub device_code: Option<String>,
    pub first: Option<i32>,
    pub last: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct TillReportResponse {
    pub till: Till,
    #[serde(flatten)]
    pub figures: TillReportFigures,
    pub reconciliation: Vec<TillReconciliationLine>,
    pub old_bills_at_close: Option<i32>,
    pub open_bills_at_close: Option<i32>,
    pub order_number_range: OrderNumberRange,
    /// The branch changefeed horizon read BEFORE the figures (OFFLINE_B_DESIGN
    /// §7): every change with `seq <= as_of_seq` is in this report. A device
    /// whose cursor has reached it, with nothing of the till still on its way,
    /// can take these figures as the authority. `0` when no horizon was
    /// available (then it is never newer than any cursor). Additive.
    #[serde(default)]
    pub as_of_seq: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct PaginatedTills {
    pub data: Vec<Till>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
    pub total_pages: i64,
}

#[derive(Deserialize, IntoParams, Default)]
#[into_params(parameter_in = Query)]
pub struct ListTillsQuery {
    pub status: Option<String>,
    pub teller_id: Option<Uuid>,
    pub device_id: Option<Uuid>,
    /// Only tills opened while another was open, or with a disagreed reconciliation.
    pub flagged: Option<bool>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CurrentTillQuery {
    /// Non-teller roles may ask about another person.
    #[serde(default)]
    pub teller_id: Option<Uuid>,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct OpenTillRequest {
    #[serde(default)]
    pub id: Option<Uuid>,
    pub opening_cash: i32,
    /// Ignored. The server decides whether the opening was an edit, from its
    /// own expected carryover — a stale device computes this against a figure
    /// that has since moved on. Kept so older tablets keep parsing.
    #[serde(default)]
    pub opening_cash_edited: Option<bool>,
    #[serde(default)]
    pub edit_reason: Option<String>,
    #[serde(default)]
    pub opened_at: Option<DateTime<Utc>>,
    /// Else the `X-Madar-Device` header.
    #[serde(default)]
    pub device_id: Option<Uuid>,
    /// Ignored on the live route (live writes `server`).
    #[serde(default)]
    #[schema(value_type = Option<TillVerification>)]
    pub verification: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CashMovementRequest {
    pub amount: i32,
    #[serde(default)]
    pub kind: Option<CashMovementKind>,
    #[serde(default)]
    pub corrects_id: Option<Uuid>,
    pub note: String,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub client_ref: Option<Uuid>,
    #[serde(default)]
    pub device_id: Option<Uuid>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CloseTillRequest {
    pub closing_cash_declared: i32,
    #[serde(default)]
    pub cash_note: Option<String>,
    #[serde(default)]
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub device_id: Option<Uuid>,
    /// Absent (old clients) → every used method is stored `unreviewed`.
    #[serde(default)]
    pub reconciliation: Option<Vec<ReconciliationInput>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct ForceCloseRequest {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub device_id: Option<Uuid>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct CloseTillResponse {
    pub till: Till,
    pub reconciliation: Vec<TillReconciliationLine>,
    pub last_till_warning: Option<LastTillWarning>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct CloseTillMethod {
    pub method: String,
    pub payment_method_id: Option<Uuid>,
    pub is_cash: bool,
    pub system_total: i64,
    pub order_count: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct CloseTillPreview {
    pub till: Till,
    pub expected_cash: i64,
    pub methods: Vec<CloseTillMethod>,
    pub last_till_warning: Option<LastTillWarning>,
}

// ── Shared queries ─────────────────────────────────────────────

pub(crate) async fn fetch_till<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    till_id: Uuid,
) -> Result<Option<Till>, sqlx::Error> {
    sqlx::query_as::<_, Till>(&format!(
        "SELECT {TILL_COLUMNS} {TILL_FROM} WHERE s.id = $1"
    ))
    .bind(till_id)
    .fetch_optional(exec)
    .await
}

pub(crate) async fn fetch_till_or_404(pool: &PgPool, till_id: Uuid) -> Result<Till, AppError> {
    fetch_till(pool, till_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Till not found".into()))
}

/// Any open till at this branch (the "branch is operating" gate a waiter fire checks).
pub(crate) async fn branch_has_open_till<'e, E>(
    executor: E,
    branch_id: Uuid,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM tills WHERE branch_id = $1 AND status = 'open')",
    )
    .bind(branch_id)
    .fetch_one(executor)
    .await
}

/// The DRAWER's most recent declared close — the carryover the next opening is
/// compared with, whoever closed it.
///
/// It used to be the person's own last close, which is not where the money is:
/// cash stays in the drawer when a shift changes. Ahmed closing at 500 and Sara
/// opening the same drawer means 500 is in front of her, but she was offered
/// her OWN last close — possibly from last week — or the branch float.
///
/// A drawer is a physical box, so it is identified by DEVICE where one is
/// known, falling back to the branch. Device-first keeps two tablets side by
/// side at one branch from contaminating each other's carryover; the branch
/// fallback matters because 926 of the 932 tills in production carry no
/// device_id at all, and those branches each ran a single drawer.
async fn last_close_declared<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    branch_id: Uuid,
    device_id: Option<Uuid>,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<i32>>(
        "SELECT closing_cash_declared FROM tills \
          WHERE branch_id = $1 AND status IN ('closed','force_closed') \
            AND closing_cash_declared IS NOT NULL \
          ORDER BY ($2::uuid IS NOT NULL AND device_id = $2) DESC, opened_at DESC \
          LIMIT 1",
    )
    .bind(branch_id)
    .bind(device_id)
    .fetch_optional(exec)
    .await
    .map(Option::flatten)
}

/// Expected cash in a till's drawer: float + cash tenders + cash tips (not
/// voided) + movements − cash refunds issued from this till.
pub(crate) async fn compute_system_cash<'e, E>(
    executor: E,
    till_id: Uuid,
) -> Result<i64, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let sql = format!(
        r#"
        SELECT (
            (SELECT opening_cash FROM tills WHERE id = $1)
          + COALESCE((SELECT SUM(op.amount) FROM order_payments op JOIN orders o ON o.id = op.order_id
                WHERE o.till_id = $1 AND COALESCE(op.is_cash, op.method = 'cash') = true AND o.{TENDERED}), 0)
          + COALESCE((SELECT SUM(o.tip_amount) FROM orders o
                WHERE o.till_id = $1
                  AND COALESCE(o.tip_is_cash, COALESCE(o.tip_payment_method, o.payment_method) = 'cash') = true
                  AND o.{TENDERED}), 0)
          + COALESCE((SELECT SUM(amount) FROM till_cash_movements WHERE till_id = $1), 0)
          - COALESCE((SELECT SUM(r.amount) FROM order_refunds r WHERE r.till_id = $1 AND r.is_cash), 0)
        )::bigint
        "#,
        TENDERED = crate::orders::TENDERED
    );
    sqlx::query_scalar::<_, i64>(&sql)
        .bind(till_id)
        .fetch_one(executor)
        .await
}

/// Branch bill counts (open bills notice / last-till warning / close snapshot).
pub(crate) async fn open_bills_notice<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    branch_id: Uuid,
) -> Result<OpenBillsNotice, sqlx::Error> {
    sqlx::query_as::<_, OpenBillsNotice>(
        r#"
        SELECT
          (SELECT COUNT(*) FROM open_tickets WHERE branch_id = $1 AND status = 'open') AS open_bills_count,
          (SELECT COALESCE(SUM(subtotal), 0)::bigint FROM open_tickets WHERE branch_id = $1 AND status = 'open') AS open_bills_amount,
          (SELECT MIN(opened_at) FROM open_tickets WHERE branch_id = $1 AND status = 'open') AS oldest_opened_at,
          (SELECT COUNT(*) FROM open_tickets t WHERE t.branch_id = $1 AND t.status = 'open'
              AND t.opened_at < now() - make_interval(hours => b.old_bill_hours)) AS old_bills_count,
          b.old_bill_hours::int AS old_bill_hours,
          (SELECT COUNT(DISTINCT table_id) FROM table_occupancies WHERE branch_id = $1 AND ended_at IS NULL) AS seated_tables_count,
          (SELECT MAX(closed_at) FROM tills WHERE branch_id = $1 AND status IN ('closed','force_closed')) AS since
        FROM branches b WHERE b.id = $1
        "#,
    )
    .bind(branch_id)
    .fetch_one(exec)
    .await
}

fn last_till_warning(notice: &OpenBillsNotice, other_open: bool) -> Option<LastTillWarning> {
    (!other_open && (notice.open_bills_count > 0 || notice.seated_tables_count > 0)).then(|| {
        LastTillWarning {
            is_last_open_till: true,
            open_bills_count: notice.open_bills_count,
            open_bills_amount: notice.open_bills_amount,
            seated_tables_count: notice.seated_tables_count,
        }
    })
}

pub(crate) fn publish(
    hub: Option<&BranchEventHub>,
    branch_id: Uuid,
    event: &str,
    payload: serde_json::Value,
) {
    if let Some(hub) = hub {
        hub.publish(branch_id, BranchEvent::new(Topic::Tills, event, &payload));
    }
}

fn till_bound_elsewhere(till: &Till, device: Option<Uuid>) -> bool {
    matches!((till.device_id, device), (Some(bound), Some(dev)) if bound != dev)
}

/// Live guard shared by create-order / settle / refund / cash: a till bound to
/// a device refuses writes from another device.
pub(crate) async fn guard_till_device<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    till_id: Uuid,
    device: Option<Uuid>,
) -> Result<(), AppError> {
    let Some(dev) = device else { return Ok(()) };
    let bound: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT device_id FROM tills WHERE id = $1")
            .bind(till_id)
            .fetch_optional(exec)
            .await?;
    match bound.flatten() {
        Some(b) if b != dev => Err(AppError::Refused {
            code: "TILL_BOUND_TO_OTHER_DEVICE",
            reason: "This till is open on another device".into(),
        }),
        _ => Ok(()),
    }
}

// ── T1 GET /tills/branches/{b}/current ─────────────────────────

#[utoipa::path(get, path = "/tills/branches/{branch_id}/current", tag = "tills",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), CurrentTillQuery),
    responses((status = 200, description = "The person's till state at this branch", body = TillPreFill), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_current_till(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    query: web::Query<CurrentTillQuery>,
    device: DeviceHeader,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
    // Someone else's till only for people who may see every till at the branch.
    let sees_all = crate::authz::require::can(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::TillReadBranch,
        Some(*branch_id),
    )
    .await?;
    let person = match (sees_all, query.teller_id) {
        (true, Some(t)) => t,
        _ => claims.user_id(),
    };
    Ok(HttpResponse::Ok().json(current_till(pool.get_ref(), *branch_id, person, device.0).await?))
}

pub(crate) async fn current_till(
    pool: &PgPool,
    branch_id: Uuid,
    person: Uuid,
    device: Option<Uuid>,
) -> Result<TillPreFill, AppError> {
    let open: Vec<Till> = sqlx::query_as::<_, Till>(&format!(
        "SELECT {TILL_COLUMNS} {TILL_FROM} WHERE s.teller_id = $1 AND s.status = 'open' ORDER BY s.opened_at DESC"
    ))
    .bind(person)
    .fetch_all(pool)
    .await?;
    let here = open
        .iter()
        .find(|t| device.is_some() && t.device_id == device && t.branch_id == branch_id);
    let open_till = here
        .or_else(|| open.iter().find(|t| t.branch_id == branch_id))
        .cloned();
    let open_elsewhere = open
        .iter()
        .filter(|t| device.is_none() || t.device_id != device)
        .map(TillBrief::from)
        .collect();
    let open_at_branch = open
        .iter()
        .filter(|t| t.branch_id == branch_id)
        .map(TillBrief::from)
        .collect();
    let last = last_close_declared(pool, branch_id, None).await?;
    let float: Option<i32> =
        sqlx::query_scalar("SELECT standard_float FROM branches WHERE id = $1")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(TillPreFill {
        has_open_till: open_till.is_some(),
        open_till,
        open_elsewhere,
        open_at_branch,
        suggested_opening_cash: last.or(float).unwrap_or(0),
        last_close_declared: last,
        open_bills_notice: open_bills_notice(pool, branch_id).await?,
    })
}

// ── T2 POST /tills/branches/{b}/open ───────────────────────────

#[utoipa::path(post, path = "/tills/branches/{branch_id}/open", tag = "tills",
    params(("branch_id" = Uuid, Path, description = "Branch ID")), request_body = OpenTillRequest,
    responses((status = 201, description = "Till opened", body = Till),
              (status = 200, description = "Already open on this device (resumed)", body = Till), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn open_till(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    branch_id: web::Path<Uuid>,
    body: web::Json<OpenTillRequest>,
    device: DeviceHeader,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "create").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
    let (till, created) = open_till_inner(
        pool.get_ref(),
        hub.as_ref().map(|h| h.get_ref()),
        branch_id.into_inner(),
        body.into_inner(),
        ActingContext::live(&claims)?.scoped(pool.get_ref()).await?,
        OpenMeta {
            device_id: device.0,
            device_code: None,
            verification: None,
        },
    )
    .await?;
    Ok(if created {
        HttpResponse::Created()
    } else {
        HttpResponse::Ok()
    }
    .json(till))
}

/// Who/where an open came from (replay envelope fields; live: the header).
#[derive(Debug, Clone, Default)]
pub struct OpenMeta {
    pub device_id: Option<Uuid>,
    pub device_code: Option<String>,
    pub verification: Option<String>,
}

/// Open core. Returns `(till, created)`.
/// Live: one-open-per-person check (409 `TILL_OPEN_AT_OTHER_BRANCH` /
/// `TILL_OPEN_ELSEWHERE`, resume on the same device), carryover reason.
/// Replay: always accepts; a second open till of the same person is flagged.
pub(crate) async fn open_till_inner(
    pool: &PgPool,
    hub: Option<&BranchEventHub>,
    branch_id: Uuid,
    body: OpenTillRequest,
    actor: ActingContext,
    meta: OpenMeta,
) -> Result<(Till, bool), AppError> {
    if !actor.replay {
        crate::authz::require::require_for(
            pool,
            actor.teller_id,
            crate::authz::Cap::TillOpen,
            Some(branch_id),
        )
        .await?;
    }
    if let Some(id) = body.id
        && let Some(existing) = fetch_till(pool, id).await?
    {
        if existing.branch_id != branch_id {
            return Err(AppError::Conflict(
                "That till id belongs to another branch".into(),
            ));
        }
        return Ok((existing, false));
    }
    let till_id = body.id.unwrap_or_else(Uuid::new_v4);
    let device_id = body.device_id.or(meta.device_id);
    let opened_at = body.opened_at.unwrap_or_else(Utc::now);
    crate::clock::reject_if_future(opened_at, "opened_at")?;

    let mut tx = pool.begin().await?;
    // Serialise opens of one person (two live opens racing).
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(format!("till-open:{}", actor.teller_id))
        .execute(&mut *tx)
        .await?;

    let others: Vec<Till> = sqlx::query_as::<_, Till>(&format!(
        "SELECT {TILL_COLUMNS} {TILL_FROM} WHERE s.teller_id = $1 AND s.status = 'open' ORDER BY s.opened_at DESC"
    ))
    .bind(actor.teller_id)
    .fetch_all(&mut *tx)
    .await?;

    let verification = if actor.replay {
        match (
            meta.verification
                .as_deref()
                .or(body.verification.as_deref()),
            device_id,
        ) {
            (Some(v @ ("server" | "lan" | "unverified")), _) => v.to_string(),
            (_, Some(_)) => "unverified".into(),
            (_, None) => "legacy".into(),
        }
    } else {
        if let Some(other) = others.iter().find(|t| t.branch_id != branch_id) {
            return Err(AppError::RefusedWith {
                code: "TILL_OPEN_AT_OTHER_BRANCH",
                reason: "You already have an open till at another branch. Close it before opening a new one.".into(),
                till: serde_json::to_value(TillBrief::from(other)).unwrap_or_default(),
            });
        }
        if let Some(here) = others
            .iter()
            .find(|t| device_id.is_some() && t.device_id == device_id)
        {
            return Ok((here.clone(), false));
        }
        if let Some(other) = others.first() {
            return Err(AppError::RefusedWith {
                code: "TILL_OPEN_ELSEWHERE",
                reason: "You already have an open till on another device. Close it there first."
                    .into(),
                till: serde_json::to_value(TillBrief::from(other)).unwrap_or_default(),
            });
        }
        "server".into()
    };

    let snapshot = match device_id {
        Some(d) => {
            crate::devices::ensure_registered(
                &mut tx,
                actor.org_id,
                d,
                Some(branch_id),
                meta.device_code.as_deref(),
            )
            .await?
        }
        None => None,
    };
    let device_id = device_id.filter(|_| snapshot.is_some());

    let expected_opening = last_close_declared(&mut *tx, branch_id, device_id).await?;
    // The SERVER decides whether this was an edit, from its own expected figure
    // — never from the client's `opening_cash_edited`, which a stale device
    // computes against a carryover that has since moved on. And a till the
    // server considers un-edited carries no reason: a reason stored against no
    // discrepancy reads as tampering, and the reverse (flagged with a NULL
    // reason, which is what a stale client produced) reads as a blank note.
    let was_edited = expected_opening.is_some_and(|exp| exp != body.opening_cash);
    let edit_reason = body.edit_reason.as_deref().filter(|_| was_edited);
    if !actor.replay && was_edited && body.edit_reason.as_deref().unwrap_or("").trim().is_empty() {
        return Err(AppError::BadRequest(
            "Opening cash differs from your last declared closing cash; edit_reason is required."
                .into(),
        ));
    }
    let edit_reason = if was_edited {
        body.edit_reason.as_deref()
    } else {
        None
    };
    let other = if actor.replay { others.first() } else { None };

    let inserted = sqlx::query(
        r#"INSERT INTO tills (id, branch_id, teller_id, opening_cash, opening_cash_original,
               opening_cash_was_edited, opening_cash_edit_reason, opened_at,
               device_id, device_code, device_label, verification,
               opened_while_another_open, other_till_id, flagged_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14, CASE WHEN $13 THEN now() END)
           ON CONFLICT (id) DO NOTHING"#,
    )
    .bind(till_id)
    .bind(branch_id)
    .bind(actor.teller_id)
    .bind(body.opening_cash)
    .bind(expected_opening)
    .bind(was_edited)
    .bind(edit_reason)
    .bind(opened_at)
    .bind(device_id)
    .bind(snapshot.as_ref().map(|s| s.code.clone()))
    .bind(snapshot.as_ref().and_then(|s| s.label.clone()))
    .bind(&verification)
    .bind(other.is_some())
    .bind(other.map(|o| o.id))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let till = fetch_till_or_404(pool, till_id).await?;
    if inserted.rows_affected() == 1 {
        publish(
            hub,
            branch_id,
            "till.opened",
            serde_json::json!({ "till": TillBrief::from(&till) }),
        );
        if let Some(o) = other {
            publish(
                hub,
                branch_id,
                "till.flagged",
                serde_json::json!({
                    "till_id": till.id, "other_till_id": o.id, "branch_id": branch_id, "teller_id": till.teller_id,
                }),
            );
        }
    }
    Ok((till, inserted.rows_affected() == 1))
}

// ── T3 / T4 lists ──────────────────────────────────────────────

#[utoipa::path(get, path = "/tills/branches/{branch_id}", tag = "tills",
    params(("branch_id" = Uuid, Path, description = "Branch ID (nil UUID = all branches in org)"), ListTillsQuery),
    responses((status = 200, description = "Tills, newest first", body = PaginatedTills), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_tills(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    query: web::Query<ListTillsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "read").await?;
    Ok(HttpResponse::Ok()
        .json(list_tills_core(&req, pool.get_ref(), &claims, *branch_id, &query).await?))
}

pub(crate) async fn list_tills_core(
    req: &HttpRequest,
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
    query: &ListTillsQuery,
) -> Result<PaginatedTills, AppError> {
    let (scope, scope_id): (&str, Uuid) = if branch_id.is_nil() {
        let org = claims
            .scope_org(crate::auth::middleware::header_org_id(req))
            .ok_or_else(|| AppError::Forbidden("No organization in scope".into()))?;
        (
            "s.branch_id IN (SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL)",
            org,
        )
    } else {
        require_branch_access(pool, claims, branch_id).await?;
        ("s.branch_id = $1", branch_id)
    };
    if let Some(st) = &query.status
        && !matches!(st.as_str(), "open" | "closed" | "force_closed")
    {
        return Err(AppError::BadRequest(
            "status must be open, closed or force_closed".into(),
        ));
    }
    let filter = format!(
        "{scope} AND ($2::text IS NULL OR s.status::text = $2) AND ($3::uuid IS NULL OR s.teller_id = $3) \
         AND ($4::uuid IS NULL OR s.device_id = $4) \
         AND (NOT COALESCE($5::bool, false) OR s.opened_while_another_open OR s.reconciliation_status = 'disagreed') \
         AND ($6::timestamptz IS NULL OR s.opened_at >= $6) AND ($7::timestamptz IS NULL OR s.opened_at < $7)"
    );
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM tills s WHERE {filter}"))
        .bind(scope_id)
        .bind(&query.status)
        .bind(query.teller_id)
        .bind(query.device_id)
        .bind(query.flagged)
        .bind(query.from)
        .bind(query.to)
        .fetch_one(pool)
        .await?;
    let paginate = query.page.is_some() || query.per_page.is_some();
    let (page, per_page) = if paginate {
        (
            query.page.unwrap_or(1).max(1),
            query
                .per_page
                .unwrap_or(DEFAULT_TILLS_PER_PAGE)
                .clamp(1, MAX_TILLS_PER_PAGE),
        )
    } else {
        (1, total.max(1))
    };
    let data = sqlx::query_as::<_, Till>(&format!(
        "SELECT {TILL_COLUMNS} {TILL_FROM} WHERE {filter} ORDER BY s.opened_at DESC LIMIT $8 OFFSET $9"
    ))
    .bind(scope_id)
    .bind(&query.status)
    .bind(query.teller_id)
    .bind(query.device_id)
    .bind(query.flagged)
    .bind(query.from)
    .bind(query.to)
    .bind(per_page)
    .bind((page - 1) * per_page)
    .fetch_all(pool)
    .await?;
    Ok(PaginatedTills {
        data,
        total,
        page,
        per_page,
        total_pages: (total + per_page - 1) / per_page,
    })
}

#[utoipa::path(get, path = "/tills/branches/{branch_id}/open", tag = "tills",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "Every open till at the branch, newest first", body = Vec<Till>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_open_tills(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
    let rows = sqlx::query_as::<_, Till>(&format!(
        "SELECT {TILL_COLUMNS} {TILL_FROM} WHERE s.branch_id = $1 AND s.status = 'open' ORDER BY s.opened_at DESC"
    ))
    .bind(*branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(get, path = "/tills/branches/{branch_id}/open-bills-notice", tag = "tills",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "Bills left open at the branch", body = OpenBillsNotice), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_open_bills_notice(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
    Ok(HttpResponse::Ok().json(open_bills_notice(pool.get_ref(), *branch_id).await?))
}

// ── T6 / T7 ────────────────────────────────────────────────────

#[utoipa::path(get, path = "/tills/{till_id}", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")),
    responses((status = 200, description = "Till", body = Till), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_till(
    req: HttpRequest,
    pool: crate::db::Db,
    till_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "read").await?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    Ok(HttpResponse::Ok().json(till))
}

#[utoipa::path(get, path = "/tills/{till_id}/report", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")),
    responses((status = 200, description = "Till (Z) report", body = TillReportResponse), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_till_report(
    req: HttpRequest,
    pool: crate::db::Db,
    till_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "read").await?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    // Horizon first, figures after: the figures then include at least everything
    // up to it (READ COMMITTED; the horizon waits out uncommitted emitters).
    let as_of_seq: i64 = sqlx::query_scalar("SELECT sync_safe_horizon($1, 0, 200)")
        .bind(till.branch_id)
        .fetch_one(pool.get_ref())
        .await
        .unwrap_or(0);
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    let figures = report_figures(pool.get_ref(), &till).await?;
    let reconciliation = reconcile::lines_for_till(pool.get_ref(), till.id).await?;
    let (device_code, first, last): (Option<String>, Option<i32>, Option<i32>) = sqlx::query_as(
        "SELECT MAX(device_code), MIN(order_number), MAX(order_number) FROM orders WHERE till_id = $1",
    )
    .bind(till.id)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(TillReportResponse {
        old_bills_at_close: till.old_bills_at_close,
        open_bills_at_close: till.open_bills_at_close,
        order_number_range: OrderNumberRange {
            device_code: device_code.or(till.device_code.clone()),
            first,
            last,
        },
        figures,
        reconciliation,
        till,
        as_of_seq,
    }))
}

pub(crate) async fn report_figures(
    pool: &PgPool,
    till: &Till,
) -> Result<TillReportFigures, AppError> {
    let till_id = till.id;
    let payment_summary = sqlx::query_as::<_, PaymentSummaryRow>(
        r#"SELECT op.method::text AS payment_method,
                  bool_or(COALESCE(op.is_cash, op.method = 'cash')) AS is_cash,
                  COALESCE(SUM(op.amount), 0)::bigint AS total,
                  COUNT(DISTINCT op.order_id)::bigint AS order_count
           FROM order_payments op JOIN orders o ON o.id = op.order_id
           WHERE o.till_id = $1 AND o.status NOT IN ('voided', 'refunded')
           GROUP BY op.method
           -- "C" collation: byte order, so "Cash" vs "cash" sorts the same on
           -- every server regardless of its default locale (case-varying
           -- payment method names exist — e.g. seeded test data — and the
           -- till-report test vectors pin this exact order).
           ORDER BY op.method COLLATE "C""#,
    )
    .bind(till_id)
    .fetch_all(pool)
    .await?;
    let (total_tips, cash_tips): (i64, i64) = sqlx::query_as(
        r#"SELECT COALESCE(SUM(o.tip_amount), 0)::bigint,
                  COALESCE(SUM(o.tip_amount) FILTER (WHERE COALESCE(o.tip_is_cash,
                       COALESCE(o.tip_payment_method, o.payment_method) = 'cash')), 0)::bigint
           FROM orders o WHERE o.till_id = $1 AND o.status NOT IN ('voided', 'refunded')"#,
    )
    .bind(till_id)
    .fetch_one(pool)
    .await?;
    let (total_tax, total_service_charge, service_charge_waived_count, service_charge_waived_amount): (i64, i64, i64, i64) =
        sqlx::query_as(
            r#"SELECT COALESCE(SUM(o.tax_amount - COALESCE(rf.refunded_tax, 0)), 0)::bigint,
                      COALESCE(SUM(o.service_charge_amount - COALESCE(rf.refunded_service_charge, 0)), 0)::bigint,
                      COUNT(*) FILTER (WHERE o.service_charge_waived_by IS NOT NULL)::bigint,
                      COALESCE(SUM(o.service_charge_waived_amount), 0)::bigint
               FROM orders o LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
               WHERE o.till_id = $1 AND o.status NOT IN ('voided', 'refunded')"#,
        )
        .bind(till_id)
        .fetch_one(pool)
        .await?;
    let (refunds_issued_tax, refunds_issued_service_charge): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(tax_amount), 0)::bigint, COALESCE(SUM(service_charge_amount), 0)::bigint \
         FROM order_refunds WHERE till_id = $1",
    )
    .bind(till_id)
    .fetch_one(pool)
    .await?;
    let voided_amount: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(total_amount), 0)::bigint FROM orders WHERE till_id = $1 AND status = 'voided'",
    )
    .bind(till_id)
    .fetch_one(pool)
    .await?;
    let cash_movements = sqlx::query_as::<_, CashMovementSummaryRow>(
        r#"SELECT m.id, m.amount, m.kind, m.corrects_id, c.kind AS corrects_kind, m.note,
                  u.name AS moved_by_name, m.created_at
           FROM till_cash_movements m JOIN users u ON u.id = m.moved_by
           LEFT JOIN till_cash_movements c ON c.id = m.corrects_id
           WHERE m.till_id = $1 ORDER BY m.created_at ASC"#,
    )
    .bind(till_id)
    .fetch_all(pool)
    .await?;
    let bucket_total = |bucket: &str| -> i64 {
        cash_movements
            .iter()
            .filter(|m| m.bucket() == bucket)
            .map(|m| m.amount as i64)
            .sum()
    };
    let cash_movements_in = bucket_total("pay_in");
    let cash_movements_out = -bucket_total("pay_out");
    let safe_drops = -bucket_total("safe_drop");
    let cash_adjustments = bucket_total("correction");
    let total_payments: i64 = payment_summary.iter().map(|r| r.total).sum();
    let cash_movements_net: i64 = cash_movements.iter().map(|m| m.amount as i64).sum();
    let refund_totals = {
        let mut conn = pool.acquire().await?;
        crate::refunds::handlers::till_refund_totals(&mut conn, till_id).await?
    };
    let cash_in_refunded_sales: i64 = sqlx::query_scalar(
        r#"SELECT (
             COALESCE((SELECT SUM(op.amount) FROM order_payments op JOIN orders o ON o.id = op.order_id
                 WHERE o.till_id = $1 AND o.status = 'refunded' AND COALESCE(op.is_cash, op.method = 'cash') = true), 0)
           + COALESCE((SELECT SUM(o.tip_amount) FROM orders o WHERE o.till_id = $1 AND o.status = 'refunded'
                 AND COALESCE(o.tip_is_cash, COALESCE(o.tip_payment_method, o.payment_method) = 'cash')), 0)
           )::bigint"#,
    )
    .bind(till_id)
    .fetch_one(pool)
    .await?;
    let expected_cash = match till.closing_cash_system {
        Some(v) => v as i64,
        None => compute_system_cash(pool, till_id).await?,
    };
    let standard_float: Option<i64> =
        sqlx::query_scalar::<_, Option<i32>>("SELECT standard_float FROM branches WHERE id = $1")
            .bind(till.branch_id)
            .fetch_optional(pool)
            .await?
            .flatten()
            .map(i64::from);
    let suggested_safe_drop = match (till.status.as_str(), standard_float) {
        ("open", Some(float)) => Some((expected_cash - float).max(0)),
        _ => None,
    };
    let spot_checks = crate::tills::spot_checks::spot_checks_for_till(pool, till_id).await?;
    Ok(TillReportFigures {
        spot_checks,
        payment_summary,
        total_payments,
        voided_amount,
        net_payments: total_payments,
        total_tips,
        cash_tips,
        non_cash_tips: total_tips - cash_tips,
        cash_movements,
        cash_movements_in,
        cash_movements_out,
        safe_drops,
        cash_adjustments,
        cash_movements_net,
        refunds_issued_count: refund_totals.refund_count,
        refunds_issued_amount: refund_totals.refunded_amount,
        refunds_issued_cash: refund_totals.refunded_cash,
        cash_in_refunded_sales,
        total_tax,
        total_service_charge,
        refunds_issued_tax,
        refunds_issued_service_charge,
        service_charge_waived_count,
        service_charge_waived_amount,
        standard_float,
        suggested_safe_drop,
        expected_cash,
        printed_at: Utc::now(),
        timezone: till.timezone.clone(),
    })
}

// ── T11 / T12 cash movements ───────────────────────────────────

#[utoipa::path(post, path = "/tills/{till_id}/cash-movements", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")), request_body = CashMovementRequest,
    responses((status = 201, description = "Cash movement added", body = CashMovement), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn add_cash_movement(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    till_id: web::Path<Uuid>,
    body: web::Json<CashMovementRequest>,
    device: DeviceHeader,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "update").await?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    let mut body = body.into_inner();
    body.device_id = body.device_id.or(device.0);
    if till_bound_elsewhere(&till, device.0) {
        guard_till_device(pool.get_ref(), till.id, device.0).await?;
    }
    add_cash_movement_inner(
        pool.get_ref(),
        hub.as_ref().map(|h| h.get_ref()),
        till_id.into_inner(),
        body,
        ActingContext::live(&claims)?.scoped(pool.get_ref()).await?,
    )
    .await
}

async fn fetch_cash_movement_by_client_ref(
    pool: &PgPool,
    client_ref: Uuid,
    org_id: Uuid,
) -> Result<Option<CashMovement>, AppError> {
    Ok(sqlx::query_as::<_, CashMovement>(&format!(
        "SELECT {CASH_MOVEMENT_COLUMNS} FROM till_cash_movements m WHERE m.client_ref = $1 \
           AND m.till_id IN (SELECT s.id FROM tills s JOIN branches b ON b.id = s.branch_id WHERE b.org_id = $2)"
    ))
    .bind(client_ref)
    .bind(org_id)
    .fetch_optional(pool)
    .await?)
}

pub(crate) async fn add_cash_movement_inner(
    pool: &PgPool,
    hub: Option<&BranchEventHub>,
    till_id: Uuid,
    body: CashMovementRequest,
    actor: ActingContext,
) -> Result<HttpResponse, AppError> {
    let till = fetch_till_or_404(pool, till_id).await?;
    if !actor.replay
        && till.teller_id != actor.teller_id
        && !crate::authz::require::effective(pool, actor.teller_id, Some(till.branch_id))
            .await?
            .can(crate::authz::Cap::TillForceClose)
    {
        return Err(AppError::Forbidden(
            "You can only add cash movements to your own till".into(),
        ));
    }
    if body.amount == 0 {
        return Err(AppError::BadRequest("Amount cannot be zero".into()));
    }
    if body.note.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Note is required for cash movements".into(),
        ));
    }
    if let Some(ts) = body.created_at {
        crate::clock::reject_if_future(ts, "created_at")?;
    }
    let kind = body
        .kind
        .unwrap_or_else(|| CashMovementKind::from_sign(body.amount));
    if !kind.allows_sign(body.amount) {
        return Err(AppError::BadRequest(match kind {
            CashMovementKind::PayIn => "A pay-in must be a positive amount".into(),
            CashMovementKind::PayOut => "A pay-out must be a negative amount".into(),
            CashMovementKind::SafeDrop => "A safe drop must be a negative amount".into(),
            CashMovementKind::Correction => "Amount cannot be zero".into(),
        }));
    }
    if body.corrects_id.is_some() && kind != CashMovementKind::Correction {
        return Err(AppError::BadRequest(
            "Only a correction can name the movement it corrects".into(),
        ));
    }
    if let Some(cref) = body.client_ref
        && let Some(existing) = fetch_cash_movement_by_client_ref(pool, cref, actor.org_id).await?
    {
        return Ok(HttpResponse::Ok().json(existing));
    }

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(till_id.to_string())
        .execute(&mut *tx)
        .await?;
    let still_open: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tills WHERE id = $1 AND status = 'open')")
            .bind(till_id)
            .fetch_one(&mut *tx)
            .await?;
    if !still_open {
        return Err(AppError::Coded {
            status: 400,
            code: "TILL_NOT_OPEN",
            reason: "Cash movements can only be added to an open till".into(),
        });
    }
    if let Some(corrects_id) = body.corrects_id {
        let corrected: Option<(Uuid, i32, bool)> = sqlx::query_as(
            "SELECT m.till_id, m.amount, EXISTS(SELECT 1 FROM till_cash_movements x WHERE x.corrects_id = m.id) \
             FROM till_cash_movements m WHERE m.id = $1",
        )
        .bind(corrects_id)
        .fetch_optional(&mut *tx)
        .await?;
        match corrected {
            None => {
                return Err(AppError::NotFound(
                    "The movement to correct was not found".into(),
                ));
            }
            Some((other, _, _)) if other != till_id => {
                return Err(AppError::BadRequest(
                    "A correction must undo a movement on the same till".into(),
                ));
            }
            Some((_, _, true)) => {
                return Err(AppError::Conflict(
                    "That movement has already been corrected".into(),
                ));
            }
            Some((_, original, _)) if original.checked_neg() != Some(body.amount) => {
                return Err(AppError::BadRequest(format!(
                    "A correction must reverse the movement exactly: expected {}",
                    -(original as i64)
                )));
            }
            Some(_) => {}
        }
    }
    let device_id = match body.device_id {
        Some(d) => {
            crate::devices::ensure_registered(&mut tx, actor.org_id, d, Some(till.branch_id), None)
                .await?
                .map(|_| d)
        }
        None => None,
    };
    let inserted = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO till_cash_movements (till_id, amount, kind, corrects_id, note, moved_by, created_at, client_ref, device_id) \
         VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, now()), $8, $9) RETURNING id",
    )
    .bind(till_id)
    .bind(body.amount)
    .bind(kind.as_str())
    .bind(body.corrects_id)
    .bind(&body.note)
    .bind(actor.teller_id)
    .bind(body.created_at)
    .bind(body.client_ref)
    .bind(device_id)
    .fetch_one(&mut *tx)
    .await;
    let id = match inserted {
        Ok(id) => id,
        Err(sqlx::Error::Database(db))
            if db.code().as_deref() == Some("23505")
                && db.constraint().is_some_and(|c| c.contains("client_ref")) =>
        {
            drop(tx);
            if let Some(cref) = body.client_ref
                && let Some(existing) =
                    fetch_cash_movement_by_client_ref(pool, cref, actor.org_id).await?
            {
                return Ok(HttpResponse::Ok().json(existing));
            }
            return Err(AppError::Conflict("Duplicate cash movement".into()));
        }
        Err(e) => return Err(e.into()),
    };
    let movement = sqlx::query_as::<_, CashMovement>(&format!(
        "SELECT {CASH_MOVEMENT_COLUMNS} FROM till_cash_movements m WHERE m.id = $1"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    publish(
        hub,
        till.branch_id,
        "till.cash_movement",
        serde_json::json!({
            "till_id": till_id, "branch_id": till.branch_id, "movement_id": movement.id,
            "amount": movement.amount, "kind": movement.kind,
        }),
    );
    Ok(HttpResponse::Created().json(movement))
}

#[utoipa::path(get, path = "/tills/{till_id}/cash-movements", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")),
    responses((status = 200, description = "Cash movements", body = Vec<CashMovement>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_cash_movements(
    req: HttpRequest,
    pool: crate::db::Db,
    till_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "read").await?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    let rows = sqlx::query_as::<_, CashMovement>(&format!(
        "SELECT {CASH_MOVEMENT_COLUMNS} FROM till_cash_movements m WHERE m.till_id = $1 ORDER BY m.created_at ASC"
    ))
    .bind(*till_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

// ── T8 close preview / T9 close / T10 force close ──────────────

#[utoipa::path(get, path = "/tills/{till_id}/close-preview", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")),
    responses((status = 200, description = "What the close screen shows", body = CloseTillPreview), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn close_preview(
    req: HttpRequest,
    pool: crate::db::Db,
    till_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "update").await?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    let expected_cash = match till.closing_cash_system {
        Some(v) => v as i64,
        None => compute_system_cash(pool.get_ref(), till.id).await?,
    };
    let mut conn = pool.acquire().await?;
    let methods = reconcile::system_totals_by_method(&mut conn, till.id, expected_cash)
        .await?
        .into_iter()
        .map(|m| CloseTillMethod {
            method: m.method,
            payment_method_id: m.payment_method_id,
            is_cash: m.is_cash,
            system_total: m.system_total,
            order_count: m.order_count,
        })
        .collect();
    let other_open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tills WHERE branch_id = $1 AND status = 'open' AND id <> $2)",
    )
    .bind(till.branch_id)
    .bind(till.id)
    .fetch_one(&mut *conn)
    .await?;
    let notice = open_bills_notice(&mut *conn, till.branch_id).await?;
    Ok(HttpResponse::Ok().json(CloseTillPreview {
        last_till_warning: last_till_warning(&notice, other_open),
        till,
        expected_cash,
        methods,
    }))
}

#[utoipa::path(post, path = "/tills/{till_id}/close", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")), request_body = CloseTillRequest,
    responses((status = 200, description = "Till closed", body = CloseTillResponse), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn close_till(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    till_id: web::Path<Uuid>,
    body: web::Json<CloseTillRequest>,
    device: DeviceHeader,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "update").await?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    let mut body = body.into_inner();
    body.device_id = body.device_id.or(device.0);
    let out = close_till_inner(
        pool.get_ref(),
        hub.as_ref().map(|h| h.get_ref()),
        till_id.into_inner(),
        body,
        ActingContext::live(&claims)?.scoped(pool.get_ref()).await?,
    )
    .await?;
    Ok(HttpResponse::Ok().json(out))
}

pub(crate) async fn close_till_inner(
    pool: &PgPool,
    hub: Option<&BranchEventHub>,
    till_id: Uuid,
    body: CloseTillRequest,
    actor: ActingContext,
) -> Result<CloseTillResponse, AppError> {
    let till = fetch_till_or_404(pool, till_id).await?;
    if !actor.replay
        && till.teller_id != actor.teller_id
        && !crate::authz::require::effective(pool, actor.teller_id, Some(till.branch_id))
            .await?
            .can(crate::authz::Cap::TillForceClose)
    {
        return Err(AppError::Forbidden(
            "You can only close your own till".into(),
        ));
    }
    if till.status != "open" {
        let reconciliation = reconcile::lines_for_till(pool, till_id).await?;
        return Ok(CloseTillResponse {
            till,
            reconciliation,
            last_till_warning: None,
        });
    }
    let closed_at = body.closed_at.unwrap_or_else(Utc::now);
    crate::clock::reject_if_future(closed_at, "closed_at")?;
    if closed_at < till.opened_at {
        return Err(AppError::BadRequest(
            "closed_at cannot be before the till was opened".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(till_id.to_string())
        .execute(&mut *tx)
        .await?;
    let still_open: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tills WHERE id = $1 AND status = 'open')")
            .bind(till_id)
            .fetch_one(&mut *tx)
            .await?;
    if !still_open {
        tx.rollback().await?;
        let till = fetch_till_or_404(pool, till_id).await?;
        let reconciliation = reconcile::lines_for_till(pool, till_id).await?;
        return Ok(CloseTillResponse {
            till,
            reconciliation,
            last_till_warning: None,
        });
    }
    let closing_cash_system = cash_to_i32(compute_system_cash(&mut *tx, till_id).await?)?;
    let notice = open_bills_notice(&mut *tx, till.branch_id).await?;
    let device_id = match body.device_id {
        Some(d) => {
            crate::devices::ensure_registered(&mut tx, actor.org_id, d, Some(till.branch_id), None)
                .await?
                .map(|_| d)
        }
        None => None,
    };
    sqlx::query(
        "UPDATE tills SET status = 'closed', closing_cash_declared = $2, closing_cash_system = $3, \
            closed_at = $4, closed_by = $5, notes = COALESCE($6, notes), closed_device_id = $7, \
            open_bills_at_close = $8, old_bills_at_close = $9 WHERE id = $1",
    )
    .bind(till_id)
    .bind(body.closing_cash_declared)
    .bind(closing_cash_system)
    .bind(closed_at)
    .bind(actor.teller_id)
    .bind(&body.cash_note)
    .bind(device_id)
    .bind(notice.open_bills_count as i32)
    .bind(notice.old_bills_count as i32)
    .execute(&mut *tx)
    .await?;
    let (reconciliation, rollup) = reconcile::write_close_reconciliation(
        &mut tx,
        till_id,
        actor.teller_id,
        body.closing_cash_declared,
        closing_cash_system,
        body.cash_note.as_deref(),
        body.reconciliation.as_deref().unwrap_or(&[]),
        actor.replay,
    )
    .await?;
    let other_open = branch_has_open_till(&mut *tx, till.branch_id).await?;
    // Unbumped kitchen tickets retire only when the LAST till at the branch closes.
    if !other_open {
        crate::kitchen::retire_unbumped_at_till_close(
            &mut tx,
            till.branch_id,
            Some(actor.teller_id),
        )
        .await?;
    }
    tx.commit().await?;

    let closed = fetch_till_or_404(pool, till_id).await?;
    publish(
        hub,
        closed.branch_id,
        "till.closed",
        serde_json::json!({
            "till_id": till_id, "branch_id": closed.branch_id, "teller_id": closed.teller_id, "status": "closed",
            "device_id": device_id, "reconciliation_status": rollup, "last_till": !other_open,
        }),
    );
    Ok(CloseTillResponse {
        till: closed,
        reconciliation,
        last_till_warning: last_till_warning(&notice, other_open),
    })
}

#[utoipa::path(post, path = "/tills/{till_id}/force-close", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")), request_body = ForceCloseRequest,
    responses((status = 200, description = "Till force-closed", body = Till), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn force_close_till(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    till_id: web::Path<Uuid>,
    body: web::Json<ForceCloseRequest>,
    device: DeviceHeader,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "update").await?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::TillForceClose,
        Some(till.branch_id),
    )
    .await?;
    if till.status != "open" {
        return Ok(HttpResponse::Ok().json(till));
    }
    let org = claims.org_id().unwrap_or_default();
    let mut tx = pool.get_ref().begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(till_id.to_string())
        .execute(&mut *tx)
        .await?;
    let still_open: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tills WHERE id = $1 AND status = 'open')")
            .bind(*till_id)
            .fetch_one(&mut *tx)
            .await?;
    if !still_open {
        tx.rollback().await?;
        return Ok(HttpResponse::Ok().json(fetch_till_or_404(pool.get_ref(), *till_id).await?));
    }
    let closing_cash_system = cash_to_i32(compute_system_cash(&mut *tx, *till_id).await?)?;
    let notice = open_bills_notice(&mut *tx, till.branch_id).await?;
    let device_id = match body.device_id.or(device.0) {
        Some(d) => crate::devices::ensure_registered(&mut tx, org, d, Some(till.branch_id), None)
            .await?
            .map(|_| d),
        None => None,
    };
    sqlx::query(
        "UPDATE tills SET status = 'force_closed', closing_cash_system = $4, closed_at = now(), closed_by = $2, \
            force_closed_by = $2, force_closed_at = now(), force_close_reason = $3, closed_device_id = $5, \
            open_bills_at_close = $6, old_bills_at_close = $7 WHERE id = $1",
    )
    .bind(*till_id)
    .bind(claims.user_id())
    .bind(&body.reason)
    .bind(closing_cash_system)
    .bind(device_id)
    .bind(notice.open_bills_count as i32)
    .bind(notice.old_bills_count as i32)
    .execute(&mut *tx)
    .await?;
    let (_, rollup) = reconcile::write_force_close_reconciliation(
        &mut tx,
        *till_id,
        claims.user_id(),
        closing_cash_system,
    )
    .await?;
    let other_open = branch_has_open_till(&mut *tx, till.branch_id).await?;
    if !other_open {
        crate::kitchen::retire_unbumped_at_till_close(
            &mut tx,
            till.branch_id,
            Some(claims.user_id()),
        )
        .await?;
    }
    tx.commit().await?;
    let closed = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    publish(
        hub.as_ref().map(|h| h.get_ref()),
        closed.branch_id,
        "till.closed",
        serde_json::json!({
            "till_id": closed.id, "branch_id": closed.branch_id, "teller_id": closed.teller_id, "status": "force_closed",
            "device_id": device_id, "reconciliation_status": rollup, "last_till": !other_open,
        }),
    );
    Ok(HttpResponse::Ok().json(closed))
}

// ── T13 DELETE ─────────────────────────────────────────────────

#[utoipa::path(delete, path = "/tills/{till_id}", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")),
    responses((status = 204, description = "Till deleted"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn delete_till(
    req: HttpRequest,
    pool: crate::db::Db,
    till_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    crate::authz::require::require(pool.get_ref(), &claims, crate::authz::Cap::TillDelete, None)
        .await?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    if till.status == "open" {
        return Err(AppError::Conflict(
            "Cannot delete an open till — force-close it first.".into(),
        ));
    }
    let has_real_orders: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM orders WHERE till_id = $1 AND status <> 'voided')",
    )
    .bind(*till_id)
    .fetch_one(pool.get_ref())
    .await?;
    if has_real_orders {
        return Err(AppError::Conflict(
            "Cannot delete a till that has recorded (non-voided) orders — its sales are part of the financial record.".into(),
        ));
    }
    let mut tx = pool.get_ref().begin().await?;
    sqlx::query("DELETE FROM orders WHERE till_id = $1")
        .bind(*till_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM tills WHERE id = $1")
        .bind(*till_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Helpers ───────────────────────────────────────────────────

pub(crate) fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

fn cash_to_i32(v: i64) -> Result<i32, AppError> {
    i32::try_from(v).map_err(|_| AppError::Internal)
}

pub(crate) async fn require_branch_access(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    // Architecture E: the branches a person may work a till at come from their
    // live role assignments, not from their role name (see `authz::scope`).
    crate::authz::scope::require_branch_access(pool, claims, branch_id).await
}
