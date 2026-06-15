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

pub mod hub;
pub mod public;
pub mod routes;
pub mod settings;
pub mod snapshot;
pub mod staff;
pub mod whatsapp;

#[cfg(test)]
mod tests;

pub const CHANNEL_IN_MALL: &str = "in_mall";
pub const CHANNEL_OUTSIDE: &str = "outside";

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
/// rewrites a leading `0` to `20`, and leaves an already-`20`-prefixed number
/// alone. Returns an error for anything implausibly short.
pub fn normalize_phone(raw: &str) -> Result<String, AppError> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let normalized = if let Some(rest) = digits.strip_prefix("00") {
        rest.to_string()
    } else if digits.starts_with("20") {
        digits.clone()
    } else if let Some(rest) = digits.strip_prefix('0') {
        format!("20{rest}")
    } else {
        digits.clone()
    };
    if normalized.len() < 10 {
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
    if claims.role == UserRole::Teller
        && let Some(token_branch) = claims.branch_id()
        && token_branch != branch_id
    {
        return Err(AppError::Forbidden(
            "This device is signed in to a different branch.".into(),
        ));
    }
    Ok(())
}
