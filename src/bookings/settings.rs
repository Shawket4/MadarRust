//! Per-branch booking settings: opening windows, slot/duration, party limits,
//! hold and no-show grace, the OTP switch. A branch with no row behaves as the
//! defaults with online booking OFF; hosts can still book by hand.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::NaiveTime;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::delivery::require_branch_access;
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::permissions::checker::check_permission;
use crate::reservations::resolve_branch_org;

/// One weekly booking window. `dow`: 0 = Sunday … 6 = Saturday. `open`/`close`
/// are `HH:MM` local; a close at or before open means "closes after midnight".
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct HoursEntry {
    pub dow: u8,
    pub open: String,
    pub close: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BookingSettings {
    pub branch_id: Uuid,
    /// Online (public) booking switch. Host bookings work regardless.
    pub enabled: bool,
    pub hours: Vec<HoursEntry>,
    pub slot_minutes: i16,
    pub default_duration_minutes: i16,
    pub min_party: i16,
    pub max_party: i16,
    pub lead_time_minutes: i32,
    pub horizon_days: i16,
    /// The floor shows the table as held from `starts_at - hold_minutes`.
    pub hold_minutes: i16,
    /// Unseated this long after `starts_at` → `no_show` automatically. `null`
    /// = only when the window ends.
    pub auto_no_show_minutes: Option<i16>,
    /// WhatsApp reminder lead. `null` = no reminder.
    pub reminder_lead_minutes: Option<i32>,
    /// Online guests must verify their phone by WhatsApp code.
    pub require_otp: bool,
    /// Optional ceiling on guests whose bookings start in one slot.
    pub max_covers_per_slot: Option<i32>,
    /// ISO dates (`YYYY-MM-DD`) with no online slots.
    pub blackout_dates: Vec<String>,
}

impl BookingSettings {
    pub fn defaults(branch_id: Uuid) -> Self {
        Self {
            branch_id,
            enabled: false,
            hours: (0..7)
                .map(|dow| HoursEntry {
                    dow,
                    open: "12:00".into(),
                    close: "23:00".into(),
                })
                .collect(),
            slot_minutes: 30,
            default_duration_minutes: 90,
            min_party: 1,
            max_party: 12,
            lead_time_minutes: 60,
            horizon_days: 30,
            hold_minutes: 15,
            auto_no_show_minutes: Some(30),
            reminder_lead_minutes: Some(120),
            require_otp: true,
            max_covers_per_slot: None,
            blackout_dates: Vec::new(),
        }
    }

    /// The window for `dow`, parsed. `None` when the day takes no bookings.
    pub fn window_for(&self, dow: u8) -> Option<(NaiveTime, NaiveTime)> {
        let e = self.hours.iter().find(|h| h.dow == dow)?;
        let open = parse_hhmm(&e.open)?;
        let close = parse_hhmm(&e.close)?;
        Some((open, close))
    }

    pub fn is_blackout(&self, date: chrono::NaiveDate) -> bool {
        let s = date.format("%Y-%m-%d").to_string();
        self.blackout_dates.iter().any(|d| d == &s)
    }

    fn validate(&self) -> Result<(), AppError> {
        if ![15i16, 30, 60].contains(&self.slot_minutes) {
            return Err(AppError::BadRequest(
                "slot_minutes must be 15, 30 or 60".into(),
            ));
        }
        if !(15..=600).contains(&self.default_duration_minutes) {
            return Err(AppError::BadRequest(
                "default_duration_minutes must be between 15 and 600".into(),
            ));
        }
        if self.min_party < 1 || self.max_party < self.min_party {
            return Err(AppError::BadRequest(
                "party limits must satisfy 1 <= min_party <= max_party".into(),
            ));
        }
        if self.lead_time_minutes < 0 {
            return Err(AppError::BadRequest(
                "lead_time_minutes must be >= 0".into(),
            ));
        }
        if !(1..=365).contains(&self.horizon_days) {
            return Err(AppError::BadRequest(
                "horizon_days must be between 1 and 365".into(),
            ));
        }
        if !(0..=180).contains(&self.hold_minutes) {
            return Err(AppError::BadRequest(
                "hold_minutes must be between 0 and 180".into(),
            ));
        }
        if self
            .auto_no_show_minutes
            .is_some_and(|m| !(5..=240).contains(&m))
        {
            return Err(AppError::BadRequest(
                "auto_no_show_minutes must be between 5 and 240".into(),
            ));
        }
        if self
            .reminder_lead_minutes
            .is_some_and(|m| !(15..=2880).contains(&m))
        {
            return Err(AppError::BadRequest(
                "reminder_lead_minutes must be between 15 and 2880".into(),
            ));
        }
        if self.max_covers_per_slot.is_some_and(|c| c <= 0) {
            return Err(AppError::BadRequest(
                "max_covers_per_slot must be > 0".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for h in &self.hours {
            if h.dow > 6 {
                return Err(AppError::BadRequest("hours.dow must be 0..6".into()));
            }
            if !seen.insert(h.dow) {
                return Err(AppError::BadRequest(format!(
                    "hours: day {} listed twice",
                    h.dow
                )));
            }
            if parse_hhmm(&h.open).is_none() || parse_hhmm(&h.close).is_none() {
                return Err(AppError::BadRequest(
                    "hours.open / hours.close must be HH:MM".into(),
                ));
            }
        }
        for d in &self.blackout_dates {
            if chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_err() {
                return Err(AppError::BadRequest(format!(
                    "blackout_dates: '{d}' is not YYYY-MM-DD"
                )));
            }
        }
        Ok(())
    }
}

pub(crate) fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

#[derive(sqlx::FromRow)]
struct Row {
    branch_id: Uuid,
    enabled: bool,
    hours: serde_json::Value,
    slot_minutes: i16,
    default_duration_minutes: i16,
    min_party: i16,
    max_party: i16,
    lead_time_minutes: i32,
    horizon_days: i16,
    hold_minutes: i16,
    auto_no_show_minutes: Option<i16>,
    reminder_lead_minutes: Option<i32>,
    require_otp: bool,
    max_covers_per_slot: Option<i32>,
    blackout_dates: serde_json::Value,
}

const COLS: &str = "branch_id, enabled, hours, slot_minutes, default_duration_minutes, min_party, \
    max_party, lead_time_minutes, horizon_days, hold_minutes, auto_no_show_minutes, \
    reminder_lead_minutes, require_otp, max_covers_per_slot, blackout_dates";

impl From<Row> for BookingSettings {
    fn from(r: Row) -> Self {
        Self {
            branch_id: r.branch_id,
            enabled: r.enabled,
            hours: serde_json::from_value(r.hours).unwrap_or_default(),
            slot_minutes: r.slot_minutes,
            default_duration_minutes: r.default_duration_minutes,
            min_party: r.min_party,
            max_party: r.max_party,
            lead_time_minutes: r.lead_time_minutes,
            horizon_days: r.horizon_days,
            hold_minutes: r.hold_minutes,
            auto_no_show_minutes: r.auto_no_show_minutes,
            reminder_lead_minutes: r.reminder_lead_minutes,
            require_otp: r.require_otp,
            max_covers_per_slot: r.max_covers_per_slot,
            blackout_dates: serde_json::from_value(r.blackout_dates).unwrap_or_default(),
        }
    }
}

/// The branch's settings, or the defaults when it has never saved any.
pub async fn load_settings<'e, E>(exec: E, branch_id: Uuid) -> Result<BookingSettings, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM branch_booking_settings WHERE branch_id = $1"
    ))
    .bind(branch_id)
    .fetch_optional(exec)
    .await?;
    Ok(row
        .map(BookingSettings::from)
        .unwrap_or_else(|| BookingSettings::defaults(branch_id)))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchQuery {
    pub branch_id: Uuid,
}

#[utoipa::path(get, path = "/bookings/settings", tag = "bookings", operation_id = "get_booking_settings", params(BranchQuery),
    responses((status = 200, body = BookingSettings), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    Ok(HttpResponse::Ok().json(load_settings(pool.get_ref(), query.branch_id).await?))
}

#[utoipa::path(put, path = "/bookings/settings", tag = "bookings", operation_id = "put_booking_settings", request_body = BookingSettings,
    responses((status = 200, body = BookingSettings), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn put_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<BookingSettings>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    body.validate()?;
    let org_id = resolve_branch_org(pool.get_ref(), body.branch_id).await?;
    let s = body.into_inner();
    sqlx::query(
        "INSERT INTO branch_booking_settings \
            (branch_id, org_id, enabled, hours, slot_minutes, default_duration_minutes, min_party, \
             max_party, lead_time_minutes, horizon_days, hold_minutes, auto_no_show_minutes, \
             reminder_lead_minutes, require_otp, max_covers_per_slot, blackout_dates) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) \
         ON CONFLICT (branch_id) DO UPDATE SET \
            enabled = EXCLUDED.enabled, hours = EXCLUDED.hours, slot_minutes = EXCLUDED.slot_minutes, \
            default_duration_minutes = EXCLUDED.default_duration_minutes, min_party = EXCLUDED.min_party, \
            max_party = EXCLUDED.max_party, lead_time_minutes = EXCLUDED.lead_time_minutes, \
            horizon_days = EXCLUDED.horizon_days, hold_minutes = EXCLUDED.hold_minutes, \
            auto_no_show_minutes = EXCLUDED.auto_no_show_minutes, \
            reminder_lead_minutes = EXCLUDED.reminder_lead_minutes, require_otp = EXCLUDED.require_otp, \
            max_covers_per_slot = EXCLUDED.max_covers_per_slot, blackout_dates = EXCLUDED.blackout_dates, \
            updated_at = now()",
    )
    .bind(s.branch_id)
    .bind(org_id)
    .bind(s.enabled)
    .bind(serde_json::to_value(&s.hours).unwrap_or(serde_json::Value::Array(vec![])))
    .bind(s.slot_minutes)
    .bind(s.default_duration_minutes)
    .bind(s.min_party)
    .bind(s.max_party)
    .bind(s.lead_time_minutes)
    .bind(s.horizon_days)
    .bind(s.hold_minutes)
    .bind(s.auto_no_show_minutes)
    .bind(s.reminder_lead_minutes)
    .bind(s.require_otp)
    .bind(s.max_covers_per_slot)
    .bind(serde_json::to_value(&s.blackout_dates).unwrap_or(serde_json::Value::Array(vec![])))
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(load_settings(pool.get_ref(), s.branch_id).await?))
}
