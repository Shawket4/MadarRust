//! Provisioning a new organization from a template (PERMISSIONS_ARCHITECTURE
//! phase 7): the org, its tenders, its roles and their default limits, the
//! ingredient categories the menu swaps key on, the first branch and the owner,
//! in ONE transaction.
//!
//! Roles come from `madar_authz::TEMPLATES` (the spec's `[templates.*]`): every
//! system role kind gets the template's grants (spec defaults, plus the
//! template's additions, minus its removals; core is implied, never stored) and
//! the template's default limits. The limits are the locked decisions for NEW
//! orgs only: a teller voids their own sale within 10 minutes, any refund goes
//! to a manager, a waiter holds no refund. Existing orgs are never touched.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::authz::{ROLE_LABELS, RoleKind, TEMPLATES, core_set, template_grants, template_limits};
use crate::errors::{AppError, AppErrorResponse};

use super::handlers::{Org, extract_claims};

/// Every role kind's system role for `template`, with its grants and limits.
/// Idempotent per kind: an existing live role with the kind's key is left as it
/// is (the owner may have edited it).
pub async fn provision_roles(
    conn: &mut PgConnection,
    org: Uuid,
    template: &str,
) -> Result<(), AppError> {
    let t = TEMPLATES
        .iter()
        .find(|t| t.key == template)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown template {template}")))?;
    for kind in RoleKind::ALL {
        let (en, ar) = ROLE_LABELS
            .iter()
            .find(|(k, _, _)| *k == kind)
            .map(|(_, en, ar)| (*en, *ar))
            .unwrap_or((kind.as_str(), kind.as_str()));
        let role: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO org_roles (org_id, key, name_en, name_ar, kind, template_key, template_version, is_system)
             SELECT $1, $2, $3, $4, $2::user_role, $5, $6, true
              WHERE NOT EXISTS (SELECT 1 FROM org_roles
                                 WHERE org_id = $1 AND key = $2 AND deleted_at IS NULL)
             RETURNING id",
        )
        .bind(org)
        .bind(kind.as_str())
        .bind(en)
        .bind(ar)
        .bind(t.key)
        .bind(t.version as i32)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(role) = role else { continue };
        let grants = template_grants(t.key, kind)
            .unwrap_or_default()
            .minus(&core_set(kind));
        let limits = template_limits(t.key, kind);
        for cap in grants.iter() {
            let l = limits
                .iter()
                .find(|(c, _)| *c == cap)
                .map(|(_, l)| serde_json::to_value(l).unwrap_or_default())
                .unwrap_or_else(|| serde_json::json!({}));
            sqlx::query(
                "INSERT INTO org_role_grants (org_role_id, org_id, capability_id, limits, source, template_version)
                 VALUES ($1, $2, $3, $4, 'template', $5)",
            )
            .bind(role)
            .bind(org)
            .bind(cap.id() as i16)
            .bind(l)
            .bind(t.version as i32)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// What a new org is born with besides its row: tenders, roles from the
/// template, and the `milk` / `coffee_bean` categories the menu's swap add-ons
/// key on (`general` is seeded by a trigger).
pub async fn provision_org_defaults(
    conn: &mut PgConnection,
    org: Uuid,
    template: &str,
) -> Result<(), AppError> {
    super::handlers::seed_payment_methods(conn, org).await?;
    provision_roles(conn, org, template).await?;
    for slug in ["milk", "coffee_bean"] {
        sqlx::query("SELECT ingredient_category_id($1, $2)")
            .bind(org)
            .bind(slug)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ProvisionBranch {
    #[schema(example = "Zamalek")]
    pub name: String,
    pub address: Option<String>,
    pub phone: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ProvisionOwner {
    pub name: String,
    pub email: String,
    /// At least 8 characters.
    pub password: String,
    /// Optional six-digit PIN so the owner can also work a till.
    pub pin: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ProvisionOrgRequest {
    #[schema(example = "The Rue")]
    pub name: String,
    #[schema(example = "the-rue")]
    pub slug: String,
    /// `restaurant` or `cafe`.
    #[schema(example = "cafe")]
    pub template: String,
    #[schema(example = "EGP")]
    pub currency_code: Option<String>,
    #[schema(example = "Africa/Cairo")]
    pub timezone: Option<String>,
    /// A FRACTION (0.14 = 14%). Default 0 (locked decision).
    pub tax_rate: Option<f64>,
    pub branch: ProvisionBranch,
    pub owner: ProvisionOwner,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ProvisionedOrg {
    pub org: Org,
    pub branch_id: Uuid,
    pub owner_id: Uuid,
    pub template: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrgTemplate {
    pub key: String,
    pub version: u32,
    pub name_en: String,
    pub name_ar: String,
    /// Role kinds the template is meant to use.
    pub roles: Vec<String>,
}

#[utoipa::path(get, path = "/orgs/templates", tag = "orgs",
    responses((status = 200, description = "Templates a new org can start from", body = Vec<OrgTemplate>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_templates(req: HttpRequest) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    crate::auth::guards::require_super_admin(&claims)?;
    let out: Vec<OrgTemplate> = TEMPLATES
        .iter()
        .map(|t| OrgTemplate {
            key: t.key.into(),
            version: t.version,
            name_en: t.en.into(),
            name_ar: t.ar.into(),
            roles: t.roles.iter().map(|r| r.as_str().to_string()).collect(),
        })
        .collect();
    Ok(HttpResponse::Ok().json(out))
}

#[utoipa::path(post, path = "/orgs/provision", tag = "orgs", request_body = ProvisionOrgRequest,
    responses((status = 201, description = "Organization, first branch and owner created", body = ProvisionedOrg), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn provision_org(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<ProvisionOrgRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    crate::auth::guards::require_super_admin(&claims)?;
    let b = body.into_inner();

    let name = b.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest("name is required".into()));
    }
    super::slugs::validate(&b.slug)?;
    if !TEMPLATES.iter().any(|t| t.key == b.template) {
        return Err(AppError::BadRequest(format!(
            "template must be one of: {}",
            TEMPLATES
                .iter()
                .map(|t| t.key)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let tax_rate = b.tax_rate.unwrap_or(0.0);
    if !(0.0..=1.0).contains(&tax_rate) {
        return Err(AppError::BadRequest(
            "tax_rate is a fraction between 0 and 1, not a percentage — 0.14 means 14%".into(),
        ));
    }
    let timezone = b
        .timezone
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("Africa/Cairo")
        .to_string();
    crate::branches::handlers::validate_timezone(pool.get_ref(), &timezone).await?;
    let branch_name = b.branch.name.trim().to_string();
    if branch_name.is_empty() {
        return Err(AppError::BadRequest("branch.name is required".into()));
    }
    let owner_name = b.owner.name.trim().to_string();
    let email = b.owner.email.trim().to_lowercase();
    if owner_name.is_empty() || !email.contains('@') {
        return Err(AppError::BadRequest(
            "owner.name and a valid owner.email are required".into(),
        ));
    }
    if b.owner.password.chars().count() < 8 {
        return Err(AppError::BadRequest(
            "owner.password must be at least 8 characters".into(),
        ));
    }
    if let Some(pin) = &b.owner.pin {
        crate::users::handlers::check_new_pin(pin)?;
    }

    let slug_taken: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations WHERE slug = $1)")
            .bind(&b.slug)
            .fetch_one(pool.get_ref())
            .await?;
    if slug_taken {
        return Err(AppError::Conflict(format!(
            "Slug '{}' is already taken",
            b.slug
        )));
    }
    let email_taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE lower(email) = $1 AND deleted_at IS NULL)",
    )
    .bind(&email)
    .fetch_one(pool.get_ref())
    .await?;
    if email_taken {
        return Err(AppError::Conflict("Email already in use".into()));
    }
    // Hashed before the transaction opens: bcrypt is slow and a transaction
    // should not wait on it.
    let password_hash =
        bcrypt::hash(&b.owner.password, crate::auth::hashing::bcrypt_cost()).map_err(|_| AppError::Internal)?;
    let pin_hash = b
        .owner
        .pin
        .as_deref()
        .map(|p| bcrypt::hash(p, crate::auth::hashing::bcrypt_cost()))
        .transpose()
        .map_err(|_| AppError::Internal)?;

    let mut tx = pool.get_ref().begin().await?;
    let org = sqlx::query_as::<_, Org>(
        r#"
        INSERT INTO organizations (name, slug, currency_code, tax_rate, timezone)
        VALUES ($1, $2, $3, $4, $5::timezone_name)
        RETURNING id, name, slug, logo_url, currency_code, tax_rate, tax_inclusive, service_charge_rate, service_charge_taxable, require_table_for_orders, receipt_footer, brand_background, brand_foreground, brand_accent, brand_logo_is_mark, brand_card_image, custom_branding, social_links, is_active, timezone::text AS timezone
        "#,
    )
    .bind(&name)
    .bind(&b.slug)
    .bind(b.currency_code.as_deref().unwrap_or("EGP"))
    .bind(tax_rate)
    .bind(&timezone)
    .fetch_one(&mut *tx)
    .await?;

    provision_org_defaults(&mut tx, org.id, &b.template).await?;

    let branch_id: Uuid = sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, address, phone, timezone)
         VALUES ($1, $2, $3, $4, $5::timezone_name) RETURNING id",
    )
    .bind(org.id)
    .bind(&branch_name)
    .bind(
        b.branch
            .address
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
    .bind(
        b.branch
            .phone
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
    .bind(&timezone)
    .fetch_one(&mut *tx)
    .await?;

    let owner_id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, role, password_hash, pin_hash, pin_fingerprint)
         VALUES ($1, $2, $3, 'org_admin', $4, $5, $6) RETURNING id",
    )
    .bind(org.id)
    .bind(&owner_name)
    .bind(&email)
    .bind(&password_hash)
    .bind(&pin_hash)
    .bind(
        b.owner
            .pin
            .as_deref()
            .map(|p| crate::auth::pin_fingerprint::fingerprint(org.id, p)),
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(HttpResponse::Created().json(ProvisionedOrg {
        org,
        branch_id,
        owner_id,
        template: b.template,
    }))
}
