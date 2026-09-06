//! Guest self-service: which branches take bookings, a branch's booking info,
//! open slots for a date and party, book (with WhatsApp OTP when the branch
//! requires it), and the manage link (view / change / cancel). Unauthenticated
//! and rate-limited like the ordering endpoints; nothing staff-only leaks
//! (no table ids, no phone numbers of other guests).

use actix_web::{HttpResponse, web};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::availability::slot_starts;
use super::handlers::{
    CreateBookingRequest, UpdateBookingRequest, cancel_inner, create_booking_inner,
    day_availability, service_today, update_booking_inner,
};
use super::model::{BookingView, booking_view, booking_view_by_token};
use super::settings::{HoursEntry, load_settings};
use super::whatsapp::{Kind, notify};
use super::{branch_tz, publish_booking};
use crate::auth::jwt::JwtSecret;
use crate::delivery::{normalize_phone, whatsapp};
use crate::errors::{AppError, AppErrorResponse};
use crate::realtime::hub::BranchEventHub;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OrgQuery {
    pub org_id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct PublicBookingBranch {
    pub id: Uuid,
    pub name: String,
    pub code: String,
}

#[utoipa::path(get, path = "/public/booking-branches", tag = "bookings-public", params(OrgQuery),
    responses((status = 200, body = Vec<PublicBookingBranch>), AppErrorResponse))]
pub async fn booking_branches(
    pool: web::Data<PgPool>,
    query: web::Query<OrgQuery>,
) -> Result<HttpResponse, AppError> {
    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT b.id, b.name, b.code FROM branches b \
         JOIN branch_booking_settings s ON s.branch_id = b.id AND s.enabled \
         WHERE b.org_id = $1 AND b.is_active AND b.deleted_at IS NULL ORDER BY b.name",
    )
    .bind(query.org_id)
    .fetch_all(pool.get_ref())
    .await?;
    let out: Vec<PublicBookingBranch> = rows
        .into_iter()
        .map(|(id, name, code)| PublicBookingBranch { id, name, code })
        .collect();
    Ok(HttpResponse::Ok().json(out))
}

#[derive(Serialize, ToSchema)]
pub struct PublicBookingInfo {
    pub branch_id: Uuid,
    pub branch_name: String,
    pub org_name: String,
    pub enabled: bool,
    pub timezone: String,
    pub slot_minutes: i16,
    pub default_duration_minutes: i16,
    pub min_party: i16,
    pub max_party: i16,
    pub lead_time_minutes: i32,
    pub horizon_days: i16,
    pub require_otp: bool,
    pub hours: Vec<HoursEntry>,
    pub blackout_dates: Vec<String>,
    /// Today's service date in the branch zone (the picker's floor).
    pub today: String,
}

async fn branch_names(pool: &PgPool, branch_id: Uuid) -> Result<(String, String), AppError> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT b.name, o.name FROM branches b JOIN organizations o ON o.id = b.org_id \
         WHERE b.id = $1 AND b.is_active AND b.deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;
    row.ok_or_else(|| AppError::NotFound("Branch not found".into()))
}

#[utoipa::path(get, path = "/public/branches/{id}/booking-info", tag = "bookings-public",
    params(("id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, body = PublicBookingInfo), AppErrorResponse))]
pub async fn booking_info(
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let (branch_name, org_name) = branch_names(pool.get_ref(), *id).await?;
    let s = load_settings(pool.get_ref(), *id).await?;
    let tz = branch_tz(pool.get_ref(), *id).await?;
    Ok(HttpResponse::Ok().json(PublicBookingInfo {
        branch_id: *id,
        branch_name,
        org_name,
        enabled: s.enabled,
        timezone: tz.name().to_string(),
        slot_minutes: s.slot_minutes,
        default_duration_minutes: s.default_duration_minutes,
        min_party: s.min_party,
        max_party: s.max_party,
        lead_time_minutes: s.lead_time_minutes,
        horizon_days: s.horizon_days,
        require_otp: s.require_otp,
        hours: s.hours,
        blackout_dates: s.blackout_dates,
        today: service_today(tz, Utc::now()).format("%Y-%m-%d").to_string(),
    }))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SlotsQuery {
    #[param(value_type = String)]
    pub date: NaiveDate,
    pub party_size: i32,
}

#[derive(Serialize, ToSchema)]
pub struct PublicSlot {
    pub starts_at: DateTime<Utc>,
    pub available: bool,
}

#[derive(Serialize, ToSchema)]
pub struct PublicSlots {
    #[schema(value_type = String)]
    pub date: NaiveDate,
    pub timezone: String,
    pub slots: Vec<PublicSlot>,
}

/// The branch must take online bookings; the date must sit inside the horizon.
async fn public_gate(
    pool: &PgPool,
    branch_id: Uuid,
    date: NaiveDate,
    party: i32,
) -> Result<(super::settings::BookingSettings, chrono_tz::Tz), AppError> {
    let s = load_settings(pool, branch_id).await?;
    if !s.enabled {
        return Err(AppError::NotFound(
            "This branch does not take online bookings".into(),
        ));
    }
    if party < s.min_party as i32 || party > s.max_party as i32 {
        return Err(AppError::BadRequest(format!(
            "We take parties of {} to {} online — please call for larger groups",
            s.min_party, s.max_party
        )));
    }
    let tz = branch_tz(pool, branch_id).await?;
    let today = service_today(tz, Utc::now());
    if date < today || date > today + Duration::days(s.horizon_days as i64) {
        return Err(AppError::BadRequest(format!(
            "Bookings open up to {} days ahead",
            s.horizon_days
        )));
    }
    Ok((s, tz))
}

#[utoipa::path(get, path = "/public/branches/{id}/booking-slots", tag = "bookings-public",
    params(("id" = Uuid, Path, description = "Branch ID"), SlotsQuery),
    responses((status = 200, body = PublicSlots), AppErrorResponse))]
pub async fn booking_slots(
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    query: web::Query<SlotsQuery>,
) -> Result<HttpResponse, AppError> {
    let (s, tz) = public_gate(pool.get_ref(), *id, query.date, query.party_size).await?;
    let slots = day_availability(
        pool.get_ref(),
        &s,
        tz,
        *id,
        query.date,
        query.party_size,
        None,
        None,
        true,
    )
    .await?;
    Ok(HttpResponse::Ok().json(PublicSlots {
        date: query.date,
        timezone: tz.name().to_string(),
        slots: slots
            .into_iter()
            .map(|x| PublicSlot {
                starts_at: x.starts_at,
                available: x.available,
            })
            .collect(),
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct PublicBookingInput {
    pub branch_id: Uuid,
    pub starts_at: DateTime<Utc>,
    pub party_size: i32,
    pub guest_name: String,
    pub phone: String,
    /// From `/public/otp/verify`; required when the branch requires OTP.
    #[serde(default)]
    pub device_token: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub locale: Option<String>,
}

/// What a guest sees on the manage page — no table ids, no staff fields.
#[derive(Serialize, ToSchema)]
pub struct PublicBookingView {
    pub id: Uuid,
    pub manage_token: String,
    pub status: String,
    pub branch_id: Uuid,
    pub branch_name: String,
    pub timezone: String,
    pub party_size: i32,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub guest_name: String,
    pub notes: Option<String>,
    /// Still confirmed and further away than the branch's lead time.
    pub can_modify: bool,
}

async fn public_view(pool: &PgPool, view: BookingView) -> Result<PublicBookingView, AppError> {
    let (branch_name, _) = branch_names(pool, view.branch_id).await?;
    let s = load_settings(pool, view.branch_id).await?;
    let tz = branch_tz(pool, view.branch_id).await?;
    let token: String = sqlx::query_scalar("SELECT manage_token FROM bookings WHERE id = $1")
        .bind(view.id)
        .fetch_one(pool)
        .await?;
    let can_modify = view.status == "confirmed"
        && view.starts_at > Utc::now() + Duration::minutes(s.lead_time_minutes as i64);
    Ok(PublicBookingView {
        id: view.id,
        manage_token: token,
        status: view.status,
        branch_id: view.branch_id,
        branch_name,
        timezone: tz.name().to_string(),
        party_size: view.party_size,
        starts_at: view.starts_at,
        ends_at: view.ends_at,
        guest_name: view.guest_name,
        notes: view.notes,
        can_modify,
    })
}

/// A public start must be one of the day's offered slots and beyond the lead.
fn check_slot(
    s: &super::settings::BookingSettings,
    tz: chrono_tz::Tz,
    starts_at: DateTime<Utc>,
) -> Result<(), AppError> {
    let local_date = service_today(tz, starts_at);
    let mut ok = slot_starts(s, tz, local_date).contains(&starts_at);
    if !ok {
        // A slot after midnight belongs to the previous service day's window.
        ok = slot_starts(s, tz, local_date - Duration::days(1)).contains(&starts_at)
            || slot_starts(s, tz, local_date + Duration::days(1)).contains(&starts_at);
    }
    if !ok {
        return Err(AppError::BadRequest(
            "That time is not offered for booking".into(),
        ));
    }
    if starts_at < Utc::now() + Duration::minutes(s.lead_time_minutes as i64) {
        return Err(AppError::BadRequest(
            "That time is too soon to book online".into(),
        ));
    }
    Ok(())
}

#[utoipa::path(post, path = "/public/bookings", tag = "bookings-public", request_body = PublicBookingInput,
    responses((status = 201, body = PublicBookingView), AppErrorResponse))]
pub async fn create_public_booking(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    hub: web::Data<BranchEventHub>,
    body: web::Json<PublicBookingInput>,
) -> Result<HttpResponse, AppError> {
    let tz = branch_tz(pool.get_ref(), body.branch_id).await?;
    let date = service_today(tz, body.starts_at);
    let (s, _) = public_gate(pool.get_ref(), body.branch_id, date, body.party_size).await?;
    check_slot(&s, tz, body.starts_at)?;
    let phone = normalize_phone(&body.phone)?;
    if s.require_otp {
        let ok = body
            .device_token
            .as_deref()
            .is_some_and(|t| whatsapp::verify_device_token(&secret.0, &phone, t));
        if !ok {
            return Err(AppError::Forbidden(
                "Please verify your phone number first".into(),
            ));
        }
    }
    let req = CreateBookingRequest {
        branch_id: body.branch_id,
        party_size: body.party_size,
        starts_at: body.starts_at,
        duration_minutes: None,
        guest_name: body.guest_name.clone(),
        guest_phone: phone,
        notes: body.notes.clone(),
        section_id: None,
        table_ids: None,
        locale: body.locale.clone(),
        force: Some(false),
        send_confirmation: Some(true),
    };
    let id =
        create_booking_inner(pool.get_ref(), &req, None, "public", s.require_otp, false).await?;
    publish_booking(pool.get_ref(), hub.get_ref(), "booking.created", id).await;
    let view = booking_view(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    notify(pool.get_ref(), &view, Kind::Confirmed).await;
    Ok(HttpResponse::Created().json(public_view(pool.get_ref(), view).await?))
}

async fn by_token(pool: &PgPool, token: &str) -> Result<BookingView, AppError> {
    if token.len() != 32 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::NotFound("Booking not found".into()));
    }
    booking_view_by_token(pool, token)
        .await?
        .ok_or_else(|| AppError::NotFound("Booking not found".into()))
}

#[utoipa::path(get, path = "/public/bookings/{token}", tag = "bookings-public",
    params(("token" = String, Path, description = "Manage token from the confirmation link")),
    responses((status = 200, body = PublicBookingView), AppErrorResponse))]
pub async fn get_public_booking(
    pool: web::Data<PgPool>,
    token: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let view = by_token(pool.get_ref(), &token).await?;
    Ok(HttpResponse::Ok().json(public_view(pool.get_ref(), view).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct PublicBookingChange {
    #[serde(default)]
    pub starts_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub party_size: Option<i32>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[utoipa::path(patch, path = "/public/bookings/{token}", tag = "bookings-public", request_body = PublicBookingChange,
    params(("token" = String, Path, description = "Manage token")),
    responses((status = 200, body = PublicBookingView), AppErrorResponse))]
pub async fn update_public_booking(
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    token: web::Path<String>,
    body: web::Json<PublicBookingChange>,
) -> Result<HttpResponse, AppError> {
    let cur = by_token(pool.get_ref(), &token).await?;
    let s = load_settings(pool.get_ref(), cur.branch_id).await?;
    let tz = branch_tz(pool.get_ref(), cur.branch_id).await?;
    let lead = Duration::minutes(s.lead_time_minutes as i64);
    if cur.status != "confirmed" || cur.starts_at <= Utc::now() + lead {
        return Err(AppError::Conflict(
            "This booking can no longer be changed online — please call the venue".into(),
        ));
    }
    if let Some(new_start) = body.starts_at {
        let date = service_today(tz, new_start);
        public_gate(
            pool.get_ref(),
            cur.branch_id,
            date,
            body.party_size.unwrap_or(cur.party_size),
        )
        .await?;
        check_slot(&s, tz, new_start)?;
    }
    let req = UpdateBookingRequest {
        party_size: body.party_size,
        starts_at: body.starts_at,
        notes: body.notes.clone(),
        force: Some(false),
        ..Default::default()
    };
    let material = update_booking_inner(pool.get_ref(), cur.id, &req, false).await?;
    publish_booking(pool.get_ref(), hub.get_ref(), "booking.changed", cur.id).await;
    let view = booking_view(pool.get_ref(), cur.id)
        .await?
        .ok_or(AppError::Internal)?;
    if material {
        notify(pool.get_ref(), &view, Kind::Changed).await;
    }
    Ok(HttpResponse::Ok().json(public_view(pool.get_ref(), view).await?))
}

#[utoipa::path(post, path = "/public/bookings/{token}/cancel", tag = "bookings-public",
    params(("token" = String, Path, description = "Manage token")),
    responses((status = 200, body = PublicBookingView), AppErrorResponse))]
pub async fn cancel_public_booking(
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    token: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let cur = by_token(pool.get_ref(), &token).await?;
    if cur.status != "confirmed" {
        return Err(AppError::Conflict(format!(
            "This booking is already {}",
            cur.status
        )));
    }
    cancel_inner(pool.get_ref(), cur.id, "guest", None).await?;
    publish_booking(pool.get_ref(), hub.get_ref(), "booking.changed", cur.id).await;
    let view = booking_view(pool.get_ref(), cur.id)
        .await?
        .ok_or(AppError::Internal)?;
    notify(pool.get_ref(), &view, Kind::Cancelled).await;
    Ok(HttpResponse::Ok().json(public_view(pool.get_ref(), view).await?))
}
