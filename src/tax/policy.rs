//! Which policy applies to a bill, and where it comes from.
//!
//! A branch may override its organisation, for an org trading in more than one
//! jurisdiction. The override is per FIELD and `NULL` means inherit — never
//! "zero" — so an org that changes its rate still moves every branch that
//! never asked to differ. A branch that genuinely charges no tax says so with
//! an explicit `0`.

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::tax::engine::TaxPolicy;

/// The policy in force at a branch, resolved branch-first then org.
pub async fn for_branch(pool: &PgPool, branch_id: Uuid) -> Result<TaxPolicy, AppError> {
    let row: Option<(
        Decimal,
        bool,
        Decimal,
        bool,
        Option<Decimal>,
        Option<bool>,
        Option<Decimal>,
        Option<bool>,
    )> = sqlx::query_as(
        "SELECT o.tax_rate, o.tax_inclusive, o.service_charge_rate, o.service_charge_taxable, \
                b.tax_rate, b.tax_inclusive, b.service_charge_rate, b.service_charge_taxable \
         FROM branches b JOIN organizations o ON o.id = b.org_id \
         WHERE b.id = $1 AND b.deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;

    let Some((o_rate, o_incl, o_sc, o_sc_tax, b_rate, b_incl, b_sc, b_sc_tax)) = row else {
        return Err(AppError::NotFound("Branch not found".into()));
    };

    let policy = TaxPolicy {
        tax_rate: b_rate.unwrap_or(o_rate),
        tax_inclusive: b_incl.unwrap_or(o_incl),
        service_charge_rate: b_sc.unwrap_or(o_sc),
        service_charge_taxable: b_sc_tax.unwrap_or(o_sc_tax),
    };
    guard(policy, "branch", branch_id)
}

/// The org's own policy, for surfaces with no branch in hand — the public
/// ordering page before a branch is chosen, and the login payload the till
/// caches.
pub async fn for_org(pool: &PgPool, org_id: Uuid) -> Result<TaxPolicy, AppError> {
    let row: Option<(Decimal, bool, Decimal, bool)> = sqlx::query_as(
        "SELECT tax_rate, tax_inclusive, service_charge_rate, service_charge_taxable \
         FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;

    let Some((tax_rate, tax_inclusive, service_charge_rate, service_charge_taxable)) = row else {
        return Err(AppError::NotFound("Organization not found".into()));
    };
    guard(
        TaxPolicy {
            tax_rate,
            tax_inclusive,
            service_charge_rate,
            service_charge_taxable,
        },
        "organization",
        org_id,
    )
}

/// Refuse to price anything under a policy that is not a pair of fractions.
///
/// The database now CHECKs this, so reaching here means a rate arrived from
/// somewhere the constraint does not cover — a restored dump, a manual UPDATE
/// on an older schema, a future column. Failing loudly is the point: the
/// alternative is multiplying a bill by fourteen and recording the result as
/// money.
fn guard(policy: TaxPolicy, kind: &str, id: Uuid) -> Result<TaxPolicy, AppError> {
    if !policy.is_sane() {
        tracing::error!(
            %id,
            kind,
            tax_rate = %policy.tax_rate,
            service_charge_rate = %policy.service_charge_rate,
            "tax policy is out of range — a rate is a FRACTION (0.14 = 14%), not a percentage"
        );
        // A 409 rather than a 500: nothing is broken on our side, the shop's
        // settings are, and the message says which and what to do.
        return Err(AppError::Conflict(format!(
            "This {kind}'s tax settings are invalid — a rate must be between 0 and 1 \
             (0.14 means 14%). Correct them in Settings before taking orders."
        )));
    }
    Ok(policy)
}
