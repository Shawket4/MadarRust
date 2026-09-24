//! Loyalty / points program.
//!
//! Customers carry no app. Their identity and balance live in an Apple Wallet /
//! Google Wallet pass whose barcode encodes an opaque member token; that token
//! is what a teller scans at checkout. See `LOYALTY_V1.md` for the decisions.
//!
//! ## Shape
//! - **One balance per org.** A member earns and spends at every branch of a
//!   tenant; the branch is recorded on each ledger row.
//! - **Points are never typed by a cashier.** The scan attaches a member to the
//!   order being tendered and the SERVER computes the award from the order's own
//!   amounts when it settles ([`earn::points_for`]). A till cannot inflate an
//!   award because it does not decide one.
//! - **Settings are org defaults with per-branch overrides**, scoped exactly like
//!   `attendance_settings`: `branch_id IS NULL` is the org default and a branch
//!   row replaces it wholesale for that branch.
//! - **Redeeming subtracts the threshold and keeps the remainder.** The database
//!   maintains the balance from the ledger by trigger, so it cannot drift.

pub mod award;
pub mod birthdays;
pub mod earn;
pub mod handlers;
pub mod model;
pub mod public;
pub mod redeem;
pub mod routes;
pub mod settings;
pub mod wallet;
pub mod winback;

use uuid::Uuid;

use crate::errors::AppError;

/// Mint the opaque token a member's pass barcode encodes.
///
/// Deliberately NOT the customer's id: a member QR must not be guessable from
/// another's, and ids leak elsewhere. 122 bits from the OS CSPRNG (`uuid` v4),
/// base64url'd to 22 characters — short enough that the QR stays sparse and
/// scans off a phone screen on a cheap counter imager.
pub fn mint_member_token() -> String {
    // The format is madar-shared's (`madar_ids::member`), the shape the till
    // recognises in a scan; the randomness is this server's.
    madar_ids::member::member_token(Uuid::new_v4().as_bytes())
}

/// The org a branch belongs to, or 404. Mirrors `reservations::resolve_branch_org`.
pub async fn resolve_branch_org<'e, E>(exec: E, branch_id: Uuid) -> Result<Uuid, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT org_id FROM branches WHERE id = $1 AND is_active AND deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(exec)
    .await?;
    row.map(|r| r.0)
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}
