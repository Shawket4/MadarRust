//! Online ordering & delivery.
//!
//! `delivery_orders` is a standalone, shift-independent, branch-scoped entity.
//! Its whole lifecycle (received → confirmed → preparing → ready →
//! out_for_delivery → delivered, plus cancelled/rejected) lives on the
//! `delivery_orders` row. NO `orders` row exists until **finalize**, when the
//! frozen snapshot is replayed through [`snapshot::apply_snapshot`] to produce a
//! normal completed sale (inventory + COGS + order_ref + shift/teller binding).
//!
//! Pricing for public orders is 100% server-side (untrusted browser, live menu)
//! and frozen at intake, so dashboard edits between order and delivery can never
//! change an in-flight order — the opposite of the POS pricing-integrity model.

use chrono::NaiveTime;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::Claims;
use crate::errors::AppError;
use crate::models::UserRole;

pub(crate) use crate::orgs::handlers::extract_claims;

pub mod public;
pub mod gateway;
pub mod routes;
pub mod settings;
pub mod snapshot;
pub mod staff;
pub mod whatsapp;

#[cfg(test)]
mod tests;

pub const CHANNEL_IN_MALL: &str = "in_mall";
pub const CHANNEL_OUTSIDE: &str = "outside";

// ── Public-surface input limits ───────────────────────────────
// The public ordering endpoints are unauthenticated and rate-limited; every
// free-text field is bounded (chars, not bytes — names/addresses are bilingual)
// and every count/quantity is capped so an untrusted client can neither store
// unbounded blobs nor overflow the integer-piastre money math.
pub const MAX_NAME_LEN: usize = 120;
pub const MAX_SHORT_TEXT_LEN: usize = 120;
pub const MAX_ADDRESS_LEN: usize = 500;
pub const MAX_NOTES_LEN: usize = 1000;
pub const MAX_LINE_NOTES_LEN: usize = 500;
pub const MAX_SIZE_LABEL_LEN: usize = 40;
pub const MAX_CART_LINES: usize = 100;
pub const MAX_LINE_QTY: i32 = 999;
pub const MAX_ADDON_QTY: i32 = 99;
pub const MAX_PHONE_RAW_LEN: usize = 32;
pub const MAX_OTP_CODE_LEN: usize = 10;

/// A required free-text field: non-empty after trimming, at most `max` chars.
pub fn validate_required_text(field: &str, value: &str, max: usize) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest(format!("{field} is required")));
    }
    if trimmed.chars().count() > max {
        return Err(AppError::BadRequest(format!(
            "{field} must be at most {max} characters"
        )));
    }
    Ok(())
}

/// An optional free-text field: when present, at most `max` chars.
pub fn validate_optional_text(field: &str, value: Option<&str>, max: usize) -> Result<(), AppError> {
    if let Some(v) = value
        && v.chars().count() > max
    {
        return Err(AppError::BadRequest(format!(
            "{field} must be at most {max} characters"
        )));
    }
    Ok(())
}

/// Geographic coordinates must be finite and within WGS84 bounds.
pub fn validate_coords(lat: f64, lng: f64) -> Result<(), AppError> {
    if !lat.is_finite()
        || !lng.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lng)
    {
        return Err(AppError::BadRequest("Invalid delivery coordinates".into()));
    }
    Ok(())
}

/// The customer's payment-method hint is display-only (the teller picks the real,
/// org-validated method at finalize) but is still constrained to the documented set.
pub fn validate_payment_hint(value: &str) -> Result<(), AppError> {
    match value {
        "cash" | "card" => Ok(()),
        _ => Err(AppError::BadRequest(
            "payment_method_hint must be 'cash' or 'card'".into(),
        )),
    }
}

/// Reject anything that isn't a known channel before it reaches a `::delivery_channel` cast.
pub fn validate_channel(channel: &str) -> Result<(), AppError> {
    match channel {
        CHANNEL_IN_MALL | CHANNEL_OUTSIDE => Ok(()),
        _ => Err(AppError::BadRequest(
            "channel must be 'in_mall' or 'outside'".into(),
        )),
    }
}

/// A channel's POS open/close override. `auto` follows the daily window, `open`
/// force-accepts (ignores the window), `closed` pauses. Stored as text on
/// `branch_delivery_settings.{in_mall,outside}_override`.
pub fn validate_override(value: &str) -> Result<(), AppError> {
    match value {
        "auto" | "open" | "closed" => Ok(()),
        _ => Err(AppError::BadRequest(
            "override must be 'auto', 'open' or 'closed'".into(),
        )),
    }
}

/// Is `now` inside the daily window? A `None` on either bound means "no time
/// restriction". A window whose close is < open is treated as spanning midnight
/// (e.g. 18:00 → 02:00).
pub fn within_window(open: Option<NaiveTime>, close: Option<NaiveTime>, now: NaiveTime) -> bool {
    match (open, close) {
        (None, _) | (_, None) => true,
        (Some(o), Some(c)) if o == c => true, // degenerate: treat as always open
        (Some(o), Some(c)) if o < c => now >= o && now < c,
        // Overnight window: open in the evening through to the morning close.
        (Some(o), Some(c)) => now >= o || now < c,
    }
}

/// Derive effective-open for one channel (derived live, no cron):
/// `enabled AND has_open_shift AND override != 'closed' AND (override == 'open' OR within window)`.
pub fn channel_open(
    enabled: bool,
    override_mode: &str,
    open: Option<NaiveTime>,
    close: Option<NaiveTime>,
    now_local: NaiveTime,
    has_open_shift: bool,
) -> bool {
    if !enabled || !has_open_shift || override_mode == "closed" {
        return false;
    }
    override_mode == "open" || within_window(open, close, now_local)
}

/// Normalise an Egyptian phone number to digits with a `20` country code, used
/// as the OTP key and the WhatsApp recipient. Best-effort: keeps only digits,
/// rewrites a leading `0` to `20`, prefixes `20` onto a bare national mobile
/// typed without the leading `0` (e.g. `1012345678`), and leaves an
/// already-`20`-prefixed number alone. Returns an error for anything
/// implausibly short.
pub fn normalize_phone(raw: &str) -> Result<String, AppError> {
    if raw.chars().count() > MAX_PHONE_RAW_LEN {
        return Err(AppError::BadRequest("phone number looks invalid".into()));
    }
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let normalized = if let Some(rest) = digits.strip_prefix("00") {
        rest.to_string()
    } else if digits.starts_with("20") {
        digits.clone()
    } else if let Some(rest) = digits.strip_prefix('0') {
        format!("20{rest}")
    } else if digits.len() == 10 && digits.starts_with('1') {
        // Bare national mobile with no leading `0` (`1XXXXXXXXX`) — prefix `20`.
        format!("20{digits}")
    } else {
        digits.clone()
    };
    // E.164 caps real numbers at 15 digits; a normalised value outside [10, 15]
    // is not a phone number we can route an OTP / WhatsApp message to.
    if normalized.len() < 10 || normalized.len() > 15 {
        return Err(AppError::BadRequest("phone number looks invalid".into()));
    }
    Ok(normalized)
}

/// Branch-scoped access check, mirroring the orders module: super_admin bypass,
/// org match for everyone else, branch assignment for non-org-admins, and the
/// teller token-branch binding.
pub(crate) async fn require_branch_access(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }
    let branch_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;
    let branch_org = branch_org.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden("Branch belongs to a different org".into()));
    }
    if claims.role == UserRole::OrgAdmin {
        return Ok(());
    }
    // D13: tellers are ORG-scoped, not branch-scoped — the org check above is the
    // boundary; any active org teller may act on this branch's deliveries.
    // Waiters and kitchen users are org-scoped the same way (device-bound, no
    // branch assignment).
    if matches!(claims.role, UserRole::Teller | UserRole::Waiter | UserRole::Kitchen) {
        return Ok(());
    }
    // Branch managers stay branch-scoped via their explicit assignments.
    let assigned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_branch_assignments WHERE user_id = $1 AND branch_id = $2)",
    )
    .bind(claims.user_id())
    .bind(branch_id)
    .fetch_one(pool)
    .await?;
    if !assigned {
        return Err(AppError::Forbidden("Not assigned to this branch".into()));
    }
    Ok(())
}
