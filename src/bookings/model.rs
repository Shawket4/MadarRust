//! The booking read model shared by every surface (host list, POS arrivals,
//! realtime payloads, the public manage page's staff-free subset).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgExecutor;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::AppError;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BookingView {
    pub id: Uuid,
    pub branch_id: Uuid,
    /// `confirmed` | `seated` | `completed` | `no_show` | `cancelled`.
    pub status: String,
    pub party_size: i32,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    /// The floor shows the claimed tables as held from here (branch
    /// `hold_minutes` before the start). Clients compare with their clock.
    pub held_from: DateTime<Utc>,
    pub guest_name: String,
    pub guest_phone: String,
    pub phone_verified: bool,
    pub notes: Option<String>,
    /// `public` | `host`.
    pub source: String,
    pub locale: String,
    pub section_id: Option<Uuid>,
    pub open_ticket_id: Option<Uuid>,
    pub table_ids: Vec<Uuid>,
    pub table_labels: Vec<String>,
    /// Active but holding no table: the host must assign one.
    pub needs_table: bool,
    pub created_by: Option<Uuid>,
    pub cancel_reason: Option<String>,
    pub cancelled_by: Option<String>,
    pub seated_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub no_show_at: Option<DateTime<Utc>>,
    pub reminder_sent_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl BookingView {
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "confirmed" | "seated")
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    branch_id: Uuid,
    status: String,
    party_size: i16,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    hold_minutes: i16,
    guest_name: String,
    guest_phone: String,
    phone_verified: bool,
    notes: Option<String>,
    source: String,
    locale: String,
    section_id: Option<Uuid>,
    open_ticket_id: Option<Uuid>,
    table_ids: Vec<Uuid>,
    table_labels: Vec<String>,
    created_by: Option<Uuid>,
    cancel_reason: Option<String>,
    cancelled_by: Option<String>,
    seated_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    cancelled_at: Option<DateTime<Utc>>,
    no_show_at: Option<DateTime<Utc>>,
    reminder_sent_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<Row> for BookingView {
    fn from(r: Row) -> Self {
        let active = matches!(r.status.as_str(), "confirmed" | "seated");
        Self {
            id: r.id,
            branch_id: r.branch_id,
            status: r.status,
            party_size: r.party_size as i32,
            starts_at: r.starts_at,
            ends_at: r.ends_at,
            held_from: r.starts_at - Duration::minutes(r.hold_minutes as i64),
            guest_name: r.guest_name,
            guest_phone: r.guest_phone,
            phone_verified: r.phone_verified,
            notes: r.notes,
            source: r.source,
            locale: r.locale,
            section_id: r.section_id,
            open_ticket_id: r.open_ticket_id,
            needs_table: active && r.table_ids.is_empty(),
            table_ids: r.table_ids,
            table_labels: r.table_labels,
            created_by: r.created_by,
            cancel_reason: r.cancel_reason,
            cancelled_by: r.cancelled_by,
            seated_at: r.seated_at,
            completed_at: r.completed_at,
            cancelled_at: r.cancelled_at,
            no_show_at: r.no_show_at,
            reminder_sent_at: r.reminder_sent_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

const VIEW_SELECT: &str = "SELECT b.id, b.branch_id, b.status::text AS status, b.party_size, \
    b.starts_at, b.ends_at, COALESCE(s.hold_minutes, 15)::smallint AS hold_minutes, \
    b.guest_name, b.guest_phone, b.phone_verified, b.notes, b.source, b.locale, b.section_id, \
    b.open_ticket_id, \
    ARRAY(SELECT bt.table_id FROM booking_tables bt JOIN branch_tables t ON t.id = bt.table_id \
          WHERE bt.booking_id = b.id ORDER BY lower(t.label)) AS table_ids, \
    ARRAY(SELECT t.label FROM booking_tables bt JOIN branch_tables t ON t.id = bt.table_id \
          WHERE bt.booking_id = b.id ORDER BY lower(t.label)) AS table_labels, \
    b.created_by, b.cancel_reason, b.cancelled_by, b.seated_at, b.completed_at, b.cancelled_at, \
    b.no_show_at, b.reminder_sent_at, b.created_at, b.updated_at \
    FROM bookings b LEFT JOIN branch_booking_settings s ON s.branch_id = b.branch_id";

pub async fn booking_view<'e, E>(exec: E, id: Uuid) -> Result<Option<BookingView>, AppError>
where
    E: PgExecutor<'e>,
{
    let row: Option<Row> = sqlx::query_as(&format!("{VIEW_SELECT} WHERE b.id = $1"))
        .bind(id)
        .fetch_optional(exec)
        .await?;
    Ok(row.map(BookingView::from))
}

pub async fn booking_view_by_token<'e, E>(
    exec: E,
    token: &str,
) -> Result<Option<BookingView>, AppError>
where
    E: PgExecutor<'e>,
{
    let row: Option<Row> = sqlx::query_as(&format!("{VIEW_SELECT} WHERE b.manage_token = $1"))
        .bind(token)
        .fetch_optional(exec)
        .await?;
    Ok(row.map(BookingView::from))
}

/// Bookings on a branch whose window overlaps `[from, to)`, optionally only the
/// active ones (confirmed/seated), ordered by start.
pub async fn list_views<'e, E>(
    exec: E,
    branch_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    active_only: bool,
    status: Option<&str>,
) -> Result<Vec<BookingView>, AppError>
where
    E: PgExecutor<'e>,
{
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "{VIEW_SELECT} WHERE b.branch_id = $1 AND b.starts_at < $3 AND b.ends_at > $2 \
           AND (NOT $4 OR b.status IN ('confirmed', 'seated')) \
           AND ($5::text IS NULL OR b.status::text = $5) \
         ORDER BY b.starts_at, b.created_at"
    ))
    .bind(branch_id)
    .bind(from)
    .bind(to)
    .bind(active_only)
    .bind(status)
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(BookingView::from).collect())
}
