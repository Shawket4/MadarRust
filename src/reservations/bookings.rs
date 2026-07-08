//! Bookings — the unified reservation + waitlist entity, host operations.
//!
//! A reservation has `reserved_for`; a waitlist entry has none. One status
//! machine: requested → confirmed → notified → arrived → seated → completed,
//! with cancelled / no_show as terminals. Seating assigns table(s) and
//! auto-opens an `open_ticket` (booking_id linked) so the party flows into the
//! existing dine-in/KDS/settle path. Gated by the `reservations` permission.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    delivery::require_branch_access,
    errors::{AppError, AppErrorResponse},
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
    realtime::event::{BranchEvent, Topic},
    realtime::hub::BranchEventHub,
    reservations::resolve_branch_org,
};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct BookingView {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub kind: String,
    pub customer_name: String,
    pub customer_phone: String,
    pub party_size: i16,
    pub reserved_for: Option<DateTime<Utc>>,
    pub quoted_ready_at: Option<DateTime<Utc>>,
    pub customer_lat: Option<f64>,
    pub customer_lng: Option<f64>,
    pub otp_verified: bool,
    pub source: String,
    pub status: String,
    pub notes: Option<String>,
    pub created_by: Option<Uuid>,
    pub notified_at: Option<DateTime<Utc>>,
    pub arrived_at: Option<DateTime<Utc>>,
    pub seated_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub no_show_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Assigned table ids (multiple ⇒ merged tables).
    pub table_ids: Vec<Uuid>,
}

/// `b.*` plus an aggregated `table_ids`. `GROUP BY b.id` is valid because `id`
/// is the PK (functional dependency covers the other `b.*` columns).
const VIEW_SELECT: &str = "SELECT b.id, b.org_id, b.branch_id, b.kind, b.customer_name, \
     b.customer_phone, b.party_size, b.reserved_for, b.quoted_ready_at, b.customer_lat, \
     b.customer_lng, b.otp_verified, b.source, b.status::text AS status, b.notes, b.created_by, \
     b.notified_at, b.arrived_at, b.seated_at, b.completed_at, b.cancelled_at, b.no_show_at, \
     b.created_at, b.updated_at, \
     COALESCE(array_remove(array_agg(bt.table_id), NULL), '{}') AS table_ids \
     FROM bookings b LEFT JOIN booking_tables bt ON bt.booking_id = b.id ";

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListBookingsQuery {
    pub branch_id: Uuid,
    #[serde(default)]
    pub status: Option<String>,
    /// Filter reservations to this calendar date (YYYY-MM-DD). Omit for the live
    /// board (everything not yet completed/cancelled/no_show).
    #[serde(default)]
    pub date: Option<NaiveDate>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateBookingRequest {
    pub branch_id: Uuid,
    /// `reservation` or `walk_in`. Defaults from whether `reserved_for` is set.
    #[serde(default)]
    pub kind: Option<String>,
    pub customer_name: String,
    pub customer_phone: String,
    #[serde(default)]
    pub party_size: Option<i16>,
    #[serde(default)]
    pub reserved_for: Option<DateTime<Utc>>,
    #[serde(default)]
    pub quoted_ready_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateBookingRequest {
    /// Drive the status machine: confirmed / notified / arrived / seated /
    /// completed / no_show / cancelled. The matching timestamp is stamped and,
    /// for terminals, assigned tables are freed.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub party_size: Option<i16>,
    #[serde(default)]
    pub customer_name: Option<String>,
    #[serde(default)]
    pub reserved_for: Option<DateTime<Utc>>,
    #[serde(default)]
    pub quoted_ready_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct AssignTablesRequest {
    pub table_ids: Vec<Uuid>,
}

const VALID_STATUSES: &[&str] = &[
    "requested",
    "confirmed",
    "notified",
    "arrived",
    "seated",
    "completed",
    "no_show",
    "cancelled",
];

fn validate_status(s: &str) -> Result<(), AppError> {
    if VALID_STATUSES.contains(&s) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("invalid status '{s}'")))
    }
}

/// Column stamped when a booking enters this status (None ⇒ no timestamp).
fn stamp_col(status: &str) -> Option<&'static str> {
    match status {
        "notified" => Some("notified_at"),
        "arrived" => Some("arrived_at"),
        "seated" => Some("seated_at"),
        "completed" => Some("completed_at"),
        "cancelled" => Some("cancelled_at"),
        "no_show" => Some("no_show_at"),
        _ => None,
    }
}

fn user_id(claims: &crate::auth::jwt::Claims) -> Result<Uuid, AppError> {
    Uuid::parse_str(&claims.sub).map_err(|_| AppError::Unauthorized("Invalid subject".into()))
}

// ── GET /reservations ─────────────────────────────────────────

#[utoipa::path(
    get, path = "/reservations", tag = "reservations",
    params(ListBookingsQuery),
    responses((status = 200, description = "Bookings (host board + waitlist)", body = Vec<BookingView>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_bookings(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ListBookingsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "reservations", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let mut sql = format!("{VIEW_SELECT} WHERE b.branch_id = $1");
    if let Some(s) = query.status.as_deref() {
        validate_status(s)?;
    } else {
        // Live board: hide finished bookings unless a status is requested.
        sql.push_str(" AND b.status NOT IN ('completed','cancelled','no_show')");
    }
    if query.status.is_some() {
        sql.push_str(" AND b.status = $2::booking_status");
    }
    if query.date.is_some() {
        sql.push_str(" AND b.reserved_for::date = $3");
    }
    sql.push_str(" GROUP BY b.id ORDER BY COALESCE(b.reserved_for, b.created_at)");

    let rows = sqlx::query_as::<_, BookingView>(&sql)
        .bind(query.branch_id)
        .bind(query.status.as_deref())
        .bind(query.date)
        .fetch_all(pool.get_ref())
        .await?;
    Ok(HttpResponse::Ok().json(rows))
}

// ── POST /reservations (staff) ────────────────────────────────

#[utoipa::path(
    post, path = "/reservations", tag = "reservations",
    request_body = CreateBookingRequest,
    responses((status = 201, description = "Booking created", body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_booking(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    body: web::Json<CreateBookingRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "reservations", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    let name = body.customer_name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Customer name is required".into()));
    }
    let phone = crate::delivery::normalize_phone(&body.customer_phone)?;
    let party = body.party_size.unwrap_or(1);
    if party <= 0 {
        return Err(AppError::BadRequest("party_size must be positive".into()));
    }
    let kind = match body.kind.as_deref() {
        Some(k @ ("reservation" | "walk_in")) => k.to_string(),
        Some(_) => {
            return Err(AppError::BadRequest(
                "kind must be 'reservation' or 'walk_in'".into(),
            ));
        }
        None if body.reserved_for.is_some() => "reservation".into(),
        None => "walk_in".into(),
    };
    let org_id = resolve_branch_org(pool.get_ref(), body.branch_id).await?;

    // Staff-created bookings start confirmed (no public OTP step).
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO bookings \
             (org_id, branch_id, kind, customer_name, customer_phone, party_size, \
              reserved_for, quoted_ready_at, source, status, notes, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'staff', 'confirmed', $9, $10) \
         RETURNING id",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(&kind)
    .bind(name)
    .bind(&phone)
    .bind(party)
    .bind(body.reserved_for)
    .bind(body.quoted_ready_at)
    .bind(body.notes.as_deref())
    .bind(user_id(&claims)?)
    .fetch_one(pool.get_ref())
    .await?;

    let view = fetch_view(pool.get_ref(), id).await?;
    hub.publish(
        body.branch_id,
        BranchEvent::new(Topic::Reservations, "booking.created", &view),
    );
    Ok(HttpResponse::Created().json(view))
}

// ── PATCH /reservations/{id} ──────────────────────────────────

#[utoipa::path(
    patch, path = "/reservations/{id}", tag = "reservations",
    params(("id" = Uuid, Path, description = "Booking ID")),
    request_body = UpdateBookingRequest,
    responses((status = 200, description = "Booking updated", body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_booking(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<UpdateBookingRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "reservations", "update").await?;

    let branch_id = fetch_booking_branch(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    if let Some(s) = body.status.as_deref() {
        validate_status(s)?;
    }
    let name = body
        .customer_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut tx = pool.get_ref().begin().await?;

    // Build the status-stamp fragment safely (status comes from a validated set).
    let stamp = body
        .status
        .as_deref()
        .and_then(stamp_col)
        .map(|col| format!(", {col} = now()"))
        .unwrap_or_default();

    sqlx::query(&format!(
        "UPDATE bookings SET \
             status = COALESCE($2::booking_status, status), \
             notes = COALESCE($3, notes), party_size = COALESCE($4, party_size), \
             customer_name = COALESCE($5, customer_name), \
             reserved_for = COALESCE($6, reserved_for), \
             quoted_ready_at = COALESCE($7, quoted_ready_at), \
             updated_at = now(){stamp} \
         WHERE id = $1"
    ))
    .bind(*id)
    .bind(body.status.as_deref())
    .bind(body.notes.as_deref())
    .bind(body.party_size)
    .bind(name)
    .bind(body.reserved_for)
    .bind(body.quoted_ready_at)
    .execute(&mut *tx)
    .await?;

    // Terminal transitions free the assigned tables: completed ⇒ needs bussing
    // (dirty), no_show/cancelled ⇒ straight back to free.
    if let Some(s) = body.status.as_deref() {
        let new_table_status = match s {
            "completed" => Some("dirty"),
            "no_show" | "cancelled" => Some("free"),
            _ => None,
        };
        if let Some(ts) = new_table_status {
            sqlx::query(
                "UPDATE branch_tables SET status = $2, updated_at = now() \
                 WHERE id IN (SELECT table_id FROM booking_tables WHERE booking_id = $1)",
            )
            .bind(*id)
            .bind(ts)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;

    let view = fetch_view(pool.get_ref(), *id).await?;
    hub.publish(
        branch_id,
        BranchEvent::new(Topic::Reservations, "booking.updated", &view),
    );
    Ok(HttpResponse::Ok().json(view))
}

// ── POST /reservations/{id}/assign (seat) ─────────────────────

#[utoipa::path(
    post, path = "/reservations/{id}/assign", tag = "reservations",
    params(("id" = Uuid, Path, description = "Booking ID")),
    request_body = AssignTablesRequest,
    responses((status = 200, description = "Party seated; tables assigned + ticket opened", body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn assign_tables(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<AssignTablesRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "reservations", "update").await?;

    let branch_id = fetch_booking_branch(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    if body.table_ids.is_empty() {
        return Err(AppError::BadRequest(
            "At least one table is required".into(),
        ));
    }
    // Every table must belong to this branch (guards forged ids / cross-branch).
    let valid: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM branch_tables WHERE branch_id = $1 AND id = ANY($2)",
    )
    .bind(branch_id)
    .bind(&body.table_ids)
    .fetch_one(pool.get_ref())
    .await?;
    if valid as usize != body.table_ids.len() {
        return Err(AppError::BadRequest(
            "One or more tables are not in this branch".into(),
        ));
    }

    let (org_id, name, party): (Uuid, String, i16) =
        sqlx::query_as("SELECT org_id, customer_name, party_size FROM bookings WHERE id = $1")
            .bind(*id)
            .fetch_one(pool.get_ref())
            .await?;

    let mut tx = pool.get_ref().begin().await?;

    // Replace any prior assignment, occupy the tables, move the booking to seated.
    sqlx::query("DELETE FROM booking_tables WHERE booking_id = $1")
        .bind(*id)
        .execute(&mut *tx)
        .await?;
    for tid in &body.table_ids {
        sqlx::query("INSERT INTO booking_tables (booking_id, table_id) VALUES ($1, $2)")
            .bind(*id)
            .bind(tid)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "UPDATE branch_tables SET status = 'seated', updated_at = now() WHERE id = ANY($1)",
    )
    .bind(&body.table_ids)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE bookings SET status = 'seated', seated_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(*id)
    .execute(&mut *tx)
    .await?;

    // Auto-open a dine-in ticket on the primary table (idempotent: skip if this
    // booking already has a live ticket — e.g. a re-seat after a move).
    let has_ticket: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM open_tickets WHERE booking_id = $1 AND status IN ('open','ready'))",
    )
    .bind(*id)
    .fetch_one(&mut *tx)
    .await?;
    if !has_ticket {
        sqlx::query(
            "INSERT INTO open_tickets \
                 (org_id, branch_id, table_id, status, opened_by, customer_name, guest_count, booking_id) \
             VALUES ($1, $2, $3, 'open', $4, $5, $6, $7)",
        )
        .bind(org_id)
        .bind(branch_id)
        .bind(body.table_ids[0])
        .bind(user_id(&claims)?)
        .bind(&name)
        .bind(party as i32)
        .bind(*id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let view = fetch_view(pool.get_ref(), *id).await?;
    hub.publish(
        branch_id,
        BranchEvent::new(Topic::Reservations, "booking.seated", &view),
    );
    Ok(HttpResponse::Ok().json(view))
}

// ── POST /reservations/{id}/notify ────────────────────────────

#[utoipa::path(
    post, path = "/reservations/{id}/notify", tag = "reservations",
    params(("id" = Uuid, Path, description = "Booking ID")),
    responses((status = 200, description = "Nudge sent (reservation departure / waitlist ready)", body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn notify_booking(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "reservations", "update").await?;

    let view = fetch_view(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, view.branch_id).await?;

    let (msg, nudge_kind) = if view.reserved_for.is_some() {
        // Reservation: manual departure nudge.
        let when = view
            .reserved_for
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_default();
        (
            crate::delivery::whatsapp::build_reservation_departure_message(
                &view.customer_name,
                &when,
            ),
            "departure",
        )
    } else if let (Some(lat), Some(lng)) = (view.customer_lat, view.customer_lng) {
        // Waitlist with a known location: include the OSRM drive estimate.
        match branch_eta_minutes(pool.get_ref(), view.branch_id, lat, lng).await {
            Some(mins) => (
                crate::delivery::whatsapp::build_waitlist_headout_message(
                    &view.customer_name,
                    mins,
                ),
                "waitlist_headout",
            ),
            None => (
                crate::delivery::whatsapp::build_waitlist_ready_message(&view.customer_name),
                "table_ready",
            ),
        }
    } else {
        // Waitlist, no location: plain "table ready".
        (
            crate::delivery::whatsapp::build_waitlist_ready_message(&view.customer_name),
            "table_ready",
        )
    };

    crate::delivery::whatsapp::send_message(
        pool.get_ref().clone(),
        view.customer_phone.clone(),
        msg,
    );
    // Idempotent: a re-tap of "notify" won't duplicate the nudge log.
    sqlx::query(
        "INSERT INTO booking_nudges (booking_id, kind) VALUES ($1, $2) \
         ON CONFLICT (booking_id, kind) DO NOTHING",
    )
    .bind(*id)
    .bind(nudge_kind)
    .execute(pool.get_ref())
    .await?;
    sqlx::query(
        "UPDATE bookings SET status = 'notified', notified_at = now(), updated_at = now() \
         WHERE id = $1 AND status NOT IN ('arrived','seated','completed','cancelled','no_show')",
    )
    .bind(*id)
    .execute(pool.get_ref())
    .await?;

    let view = fetch_view(pool.get_ref(), *id).await?;
    hub.publish(
        view.branch_id,
        BranchEvent::new(Topic::Reservations, "booking.updated", &view),
    );
    Ok(HttpResponse::Ok().json(view))
}

// ── Shared helpers ────────────────────────────────────────────

pub(crate) async fn fetch_view(pool: &PgPool, id: Uuid) -> Result<BookingView, AppError> {
    sqlx::query_as::<_, BookingView>(&format!("{VIEW_SELECT} WHERE b.id = $1 GROUP BY b.id"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Booking not found".into()))
}

pub(crate) async fn fetch_booking_branch(pool: &PgPool, id: Uuid) -> Result<Uuid, AppError> {
    sqlx::query_scalar("SELECT branch_id FROM bookings WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Booking not found".into()))
}

/// OSRM free-flow drive time (minutes) from a customer's coords to the branch,
/// or `None` when OSRM is unset/unreachable or the branch has no coordinates.
pub(crate) async fn branch_eta_minutes(
    pool: &PgPool,
    branch_id: Uuid,
    cust_lat: f64,
    cust_lng: f64,
) -> Option<i64> {
    let coords: Option<(Option<f64>, Option<f64>)> =
        sqlx::query_as("SELECT latitude, longitude FROM branches WHERE id = $1")
            .bind(branch_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let (blat, blng) = coords?;
    let (blat, blng) = (blat?, blng?);
    use crate::geo::osrm::{LatLng, road_eta_seconds};
    road_eta_seconds(
        LatLng {
            lat: blat,
            lng: blng,
        },
        LatLng {
            lat: cust_lat,
            lng: cust_lng,
        },
    )
    .await
    .ok()
    .map(|secs| (secs / 60.0).round() as i64)
}
