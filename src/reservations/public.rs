//! Public, unauthenticated self-booking.
//!
//! Reuses delivery's identity stack: the guest gets a phone-bound device-trust
//! token from the existing delivery OTP endpoints (`whatsapp::verify_device_token`)
//! and the browser supplies geolocation. We never trust client-supplied identity
//! beyond the verified token. These routes are mounted WITHOUT `JwtMiddleware`
//! (like `delivery::public`), so they carry no permission checks — the device
//! token is the gate.

use actix_web::{HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::delivery::{normalize_phone, validate_coords, whatsapp};
use crate::errors::{AppError, AppErrorResponse};
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct PublicBranch {
    pub id: Uuid,
    pub name: String,
    pub accepting_reservations: bool,
    pub accepting_waitlist: bool,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PublicBranchesQuery {
    pub org_id: Uuid,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PublicCreateBookingRequest {
    pub branch_id: Uuid,
    /// `reservation` or `walk_in`; defaults from whether `reserved_for` is set.
    #[serde(default)]
    pub kind: Option<String>,
    pub customer_name: String,
    pub customer_phone: String,
    /// Device-trust token from the delivery OTP flow, proving this phone is verified.
    pub device_token: String,
    #[serde(default)]
    pub party_size: Option<i16>,
    #[serde(default)]
    pub reserved_for: Option<DateTime<Utc>>,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lng: Option<f64>,
}

/// Slim, guest-safe view (no org/internal columns).
#[derive(Debug, Serialize, ToSchema)]
pub struct PublicBooking {
    pub id: Uuid,
    pub status: String,
    pub kind: String,
    pub party_size: i16,
    pub reserved_for: Option<DateTime<Utc>>,
    pub table_count: i64,
    /// OSRM drive estimate from the guest's saved location, when available.
    pub eta_minutes: Option<i64>,
}

// ── GET /public/reservations/branches?org_id ──────────────────

#[utoipa::path(
    get, path = "/public/reservations/branches", tag = "delivery-public",
    operation_id = "list_reservation_public_branches",
    params(PublicBranchesQuery),
    responses((status = 200, description = "Branches accepting reservations/waitlist", body = Vec<PublicBranch>), AppErrorResponse),
)]
pub async fn public_branches(
    pool: web::Data<PgPool>,
    query: web::Query<PublicBranchesQuery>,
) -> Result<HttpResponse, AppError> {
    let rows = sqlx::query_as::<_, PublicBranch>(
        "SELECT b.id, b.name, \
                COALESCE(s.accepting_reservations, false) AS accepting_reservations, \
                COALESCE(s.accepting_waitlist, false)     AS accepting_waitlist \
         FROM branches b \
         LEFT JOIN branch_reservation_settings s ON s.branch_id = b.id \
         WHERE b.org_id = $1 AND b.deleted_at IS NULL \
           AND (COALESCE(s.accepting_reservations, false) OR COALESCE(s.accepting_waitlist, false)) \
         ORDER BY lower(b.name)",
    )
    .bind(query.org_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

// ── POST /public/reservations ─────────────────────────────────

#[utoipa::path(
    post, path = "/public/reservations", tag = "delivery-public",
    request_body = PublicCreateBookingRequest,
    responses((status = 201, description = "Booking created", body = PublicBooking), AppErrorResponse),
)]
pub async fn create_public_booking(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    hub: web::Data<BranchEventHub>,
    body: web::Json<PublicCreateBookingRequest>,
) -> Result<HttpResponse, AppError> {
    let name = body.customer_name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Customer name is required".into()));
    }
    let phone = normalize_phone(&body.customer_phone)?;
    if !whatsapp::verify_device_token(&secret.0, &phone, &body.device_token) {
        return Err(AppError::Unauthorized(
            "Phone not verified for this device".into(),
        ));
    }

    let party = body.party_size.unwrap_or(1);
    if party <= 0 {
        return Err(AppError::BadRequest("party_size must be positive".into()));
    }
    if let (Some(lat), Some(lng)) = (body.lat, body.lng) {
        validate_coords(lat, lng)?;
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

    // Resolve org + enforce the branch is actually accepting this booking kind.
    let row: Option<(Uuid, bool, bool)> = sqlx::query_as(
        "SELECT b.org_id, \
                COALESCE(s.accepting_reservations, false), COALESCE(s.accepting_waitlist, false) \
         FROM branches b \
         LEFT JOIN branch_reservation_settings s ON s.branch_id = b.id \
         WHERE b.id = $1 AND b.deleted_at IS NULL",
    )
    .bind(body.branch_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (org_id, acc_res, acc_wait) =
        row.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    let wants_reservation = body.reserved_for.is_some();
    if (wants_reservation && !acc_res) || (!wants_reservation && !acc_wait) {
        return Err(AppError::Conflict(
            "This branch is not accepting that booking type right now.".into(),
        ));
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO bookings \
             (org_id, branch_id, kind, customer_name, customer_phone, party_size, \
              reserved_for, customer_lat, customer_lng, otp_verified, source, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, true, 'public', 'confirmed') \
         RETURNING id",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(&kind)
    .bind(name)
    .bind(&phone)
    .bind(party)
    .bind(body.reserved_for)
    .bind(body.lat)
    .bind(body.lng)
    .fetch_one(pool.get_ref())
    .await?;

    if let Ok(view) = super::bookings::fetch_view(pool.get_ref(), id).await {
        hub.publish(
            body.branch_id,
            BranchEvent::new(Topic::Reservations, "booking.created", &view),
        );
    }
    let out = load_public(pool.get_ref(), id).await?;
    Ok(HttpResponse::Created().json(out))
}

// ── GET /public/reservations/{id} (track) ─────────────────────

#[utoipa::path(
    get, path = "/public/reservations/{id}", tag = "delivery-public",
    params(("id" = Uuid, Path, description = "Booking ID")),
    responses((status = 200, description = "Booking status for the guest tracker", body = PublicBooking), AppErrorResponse),
)]
pub async fn track_public_booking(
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let out = load_public(pool.get_ref(), *id).await?;
    Ok(HttpResponse::Ok().json(out))
}

async fn load_public(pool: &PgPool, id: Uuid) -> Result<PublicBooking, AppError> {
    let row: Option<(
        Uuid,
        String,
        String,
        i16,
        Option<DateTime<Utc>>,
        Uuid,
        Option<f64>,
        Option<f64>,
    )> = sqlx::query_as(
        "SELECT id, status::text, kind, party_size, reserved_for, branch_id, customer_lat, customer_lng \
         FROM bookings WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let (id, status, kind, party_size, reserved_for, branch_id, lat, lng) =
        row.ok_or_else(|| AppError::NotFound("Booking not found".into()))?;

    let table_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM booking_tables WHERE booking_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?;

    // Waitlist guests get a live drive estimate; reservations don't need one.
    let eta_minutes = match (reserved_for, lat, lng) {
        (None, Some(la), Some(ln)) => {
            super::bookings::branch_eta_minutes(pool, branch_id, la, ln).await
        }
        _ => None,
    };

    Ok(PublicBooking {
        id,
        status,
        kind,
        party_size,
        reserved_for,
        table_count,
        eta_minutes,
    })
}
