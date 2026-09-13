//! Payment method availability: org ∩ branch ∩ teller ∩ device.
//!
//! Contract: TILLS_CONTRACT.md §1.2 "Payment method availability", §2.3, §9.
//! "No rows = no restriction"; rows present = allow-list. Effective = the org's
//! ACTIVE methods ∩ every list that exists. Runtime queries only (the tables
//! come from B1's migration `20260914090100_payment_method_availability.sql`).

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    payment_methods::handlers::OrgPaymentMethod,
    permissions::checker::check_permission,
    realtime::{
        event::{BranchEvent, Topic},
        hub::BranchEventHub,
    },
};

pub const CODE_EMPTY_ALLOW_LIST: &str = "EMPTY_ALLOW_LIST";
pub const EVENT_AVAILABILITY_CHANGED: &str = "payment_methods.availability_changed";

/// The one predicate every reader shares. `$1` org, `$2` branch, `$3` user
/// (nullable), `$4` device (nullable); aliases the method table as `m`.
const EFFECTIVE_WHERE: &str = r#"
    m.org_id = $1 AND m.is_active
    AND (NOT EXISTS (SELECT 1 FROM branch_payment_methods b WHERE b.branch_id = $2)
         OR EXISTS (SELECT 1 FROM branch_payment_methods b WHERE b.branch_id = $2 AND b.payment_method_id = m.id))
    AND ($3::uuid IS NULL
         OR NOT EXISTS (SELECT 1 FROM user_payment_methods u WHERE u.user_id = $3)
         OR EXISTS (SELECT 1 FROM user_payment_methods u WHERE u.user_id = $3 AND u.payment_method_id = m.id))
    AND ($4::uuid IS NULL
         OR NOT EXISTS (SELECT 1 FROM device_payment_methods d WHERE d.device_id = $4)
         OR EXISTS (SELECT 1 FROM device_payment_methods d WHERE d.device_id = $4 AND d.payment_method_id = m.id))
"#;

/// Names (the string stored in `order_payments.method`) of the effective set.
pub async fn effective_method_names(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    user_id: Option<Uuid>,
    device_id: Option<Uuid>,
) -> Result<Vec<String>, AppError> {
    let sql = format!(
        "SELECT m.name FROM org_payment_methods m WHERE {EFFECTIVE_WHERE} ORDER BY m.created_at, m.name"
    );
    Ok(sqlx::query_scalar::<_, String>(&sql)
        .bind(org_id)
        .bind(branch_id)
        .bind(user_id)
        .bind(device_id)
        .fetch_all(pool)
        .await?)
}

/// Is `method` (by name) in the effective set? Used by live create_order (B2).
pub async fn is_method_available(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    user_id: Option<Uuid>,
    device_id: Option<Uuid>,
    method: &str,
) -> Result<bool, AppError> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM org_payment_methods m WHERE m.name = $5 AND {EFFECTIVE_WHERE})"
    );
    Ok(sqlx::query_scalar::<_, bool>(&sql)
        .bind(org_id)
        .bind(branch_id)
        .bind(user_id)
        .bind(device_id)
        .bind(method)
        .fetch_one(pool)
        .await?)
}

async fn effective_methods(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    user_id: Option<Uuid>,
    device_id: Option<Uuid>,
) -> Result<Vec<OrgPaymentMethod>, AppError> {
    let sql = format!(
        "SELECT m.id, m.org_id, m.name, m.label_translations, m.color, m.icon, m.is_cash, m.is_active,
                m.visible_in_integrations, m.created_at, m.updated_at
         FROM org_payment_methods m WHERE {EFFECTIVE_WHERE} ORDER BY m.created_at, m.name"
    );
    Ok(sqlx::query_as::<_, OrgPaymentMethod>(&sql)
        .bind(org_id)
        .bind(branch_id)
        .bind(user_id)
        .bind(device_id)
        .fetch_all(pool)
        .await?)
}

// ── Wire types ───────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, ToSchema)]
pub struct AllowList {
    pub restricted: bool,
    pub payment_method_ids: Vec<Uuid>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, ToSchema)]
pub struct UserAllowList {
    pub user_id: Uuid,
    pub payment_method_ids: Vec<Uuid>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, ToSchema)]
pub struct DeviceAllowList {
    pub device_id: Uuid,
    pub payment_method_ids: Vec<Uuid>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, ToSchema)]
pub struct PaymentMethodAvailability {
    pub branch_id: Uuid,
    pub branch: AllowList,
    pub users: Vec<UserAllowList>,
    pub devices: Vec<DeviceAllowList>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AvailabilityQuery {
    pub branch_id: Uuid,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EffectiveQuery {
    pub branch_id: Uuid,
    #[serde(default)]
    pub user_id: Option<Uuid>,
    #[serde(default)]
    pub device_id: Option<Uuid>,
}

// ── Helpers ──────────────────────────────────────────────────────

fn claims_of(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

fn org_of(claims: &Claims) -> Result<Uuid, AppError> {
    claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("No org id".into()))
}

#[derive(Clone, Copy)]
pub enum Owner {
    Branch,
    User,
    Device,
}

impl Owner {
    fn table(self) -> &'static str {
        match self {
            Owner::Branch => "branch_payment_methods",
            Owner::User => "user_payment_methods",
            Owner::Device => "device_payment_methods",
        }
    }
    fn column(self) -> &'static str {
        match self {
            Owner::Branch => "branch_id",
            Owner::User => "user_id",
            Owner::Device => "device_id",
        }
    }
    /// The owner row must exist in the caller's org (404 otherwise — no leak).
    fn owner_sql(self) -> &'static str {
        match self {
            Owner::Branch => "SELECT EXISTS (SELECT 1 FROM branches WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL)",
            Owner::User => "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL)",
            Owner::Device => "SELECT EXISTS (SELECT 1 FROM devices WHERE id = $1 AND org_id = $2)",
        }
    }
}

async fn ensure_branch_in_org(pool: &PgPool, org_id: Uuid, branch_id: Uuid) -> Result<(), AppError> {
    let ok: bool = sqlx::query_scalar(Owner::Branch.owner_sql())
        .bind(branch_id)
        .bind(org_id)
        .fetch_one(pool)
        .await?;
    if ok { Ok(()) } else { Err(AppError::NotFound("Branch not found".into())) }
}

async fn ids_for(pool: &PgPool, owner: Owner, id: Uuid) -> Result<Vec<Uuid>, AppError> {
    let sql = format!(
        "SELECT t.payment_method_id FROM {} t JOIN org_payment_methods m ON m.id = t.payment_method_id
         WHERE t.{} = $1 ORDER BY m.created_at, m.name",
        owner.table(),
        owner.column()
    );
    Ok(sqlx::query_scalar::<_, Uuid>(&sql).bind(id).fetch_all(pool).await?)
}

// ── GET /payment-methods/availability ────────────────────────────

#[utoipa::path(
    get,
    path = "/payment-methods/availability",
    tag = "payment_methods",
    params(AvailabilityQuery),
    responses((status = 200, description = "Branch, teller and device allow-lists", body = PaymentMethodAvailability), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_availability(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<AvailabilityQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    check_permission(pool.get_ref(), &claims, "payment_methods", "read").await?;
    let org_id = org_of(&claims)?;
    Ok(HttpResponse::Ok().json(load_availability(pool.get_ref(), org_id, q.branch_id).await?))
}

pub async fn load_availability(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
) -> Result<PaymentMethodAvailability, AppError> {
    ensure_branch_in_org(pool, org_id, branch_id).await?;
    let branch_ids = ids_for(pool, Owner::Branch, branch_id).await?;

    let user_rows: Vec<(Uuid, Vec<Uuid>)> = sqlx::query_as(
        "SELECT u.user_id, array_agg(u.payment_method_id ORDER BY m.created_at, m.name)
         FROM user_payment_methods u JOIN org_payment_methods m ON m.id = u.payment_method_id
         WHERE u.org_id = $1 GROUP BY u.user_id ORDER BY u.user_id",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;

    let device_rows: Vec<(Uuid, Vec<Uuid>)> = sqlx::query_as(
        "SELECT d.device_id, array_agg(d.payment_method_id ORDER BY m.created_at, m.name)
         FROM device_payment_methods d
         JOIN org_payment_methods m ON m.id = d.payment_method_id
         JOIN devices dv ON dv.id = d.device_id
         WHERE d.org_id = $1 AND dv.branch_id = $2
         GROUP BY d.device_id ORDER BY d.device_id",
    )
    .bind(org_id)
    .bind(branch_id)
    .fetch_all(pool)
    .await?;

    Ok(PaymentMethodAvailability {
        branch_id,
        branch: AllowList { restricted: !branch_ids.is_empty(), payment_method_ids: branch_ids },
        users: user_rows
            .into_iter()
            .map(|(user_id, payment_method_ids)| UserAllowList { user_id, payment_method_ids })
            .collect(),
        devices: device_rows
            .into_iter()
            .map(|(device_id, payment_method_ids)| DeviceAllowList { device_id, payment_method_ids })
            .collect(),
    })
}

// ── PUT /payment-methods/availability/{branches|users|devices}/{id} ──

async fn put_list(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    owner: Owner,
    owner_id: Uuid,
    body: AllowList,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    check_permission(pool.get_ref(), &claims, "payment_methods", "update").await?;
    let org_id = org_of(&claims)?;
    let stored = replace_list(pool.get_ref(), org_id, owner, owner_id, &body).await?;

    // Realtime after commit (§0.6). A user's list applies at every branch.
    if let Some(hub) = hub {
        let branches: Vec<Uuid> = match owner {
            Owner::Branch => vec![owner_id],
            Owner::Device => sqlx::query_scalar("SELECT branch_id FROM devices WHERE id = $1 AND branch_id IS NOT NULL")
                .bind(owner_id)
                .fetch_all(pool.get_ref())
                .await?,
            Owner::User => sqlx::query_scalar("SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL")
                .bind(org_id)
                .fetch_all(pool.get_ref())
                .await?,
        };
        for branch_id in branches {
            hub.publish(
                branch_id,
                BranchEvent::new(
                    AVAILABILITY_TOPIC,
                    EVENT_AVAILABILITY_CHANGED,
                    &serde_json::json!({ "branch_id": branch_id }),
                ),
            );
        }
    }
    Ok(HttpResponse::Ok().json(stored))
}

/// Topic for `payment_methods.availability_changed`. Contract §2.3 says `tills`;
/// `Topic::Tills` is added by B2 — switch this constant when it lands.
pub const AVAILABILITY_TOPIC: Topic = Topic::Orders;

/// Validate and replace one owner's allow-list in a transaction. Idempotent:
/// the same body twice leaves the same rows.
pub async fn replace_list(
    pool: &PgPool,
    org_id: Uuid,
    owner: Owner,
    owner_id: Uuid,
    body: &AllowList,
) -> Result<AllowList, AppError> {
    if body.restricted && body.payment_method_ids.is_empty() {
        return Err(AppError::BadRequest(format!(
            "{CODE_EMPTY_ALLOW_LIST}: a restricted list needs at least one payment method"
        )));
    }
    let exists: bool = sqlx::query_scalar(owner.owner_sql())
        .bind(owner_id)
        .bind(org_id)
        .fetch_one(pool)
        .await?;
    if !exists {
        return Err(AppError::NotFound("Not found".into()));
    }
    let mut ids = body.payment_method_ids.clone();
    ids.sort();
    ids.dedup();
    if body.restricted {
        let known: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM org_payment_methods WHERE org_id = $1 AND id = ANY($2)",
        )
        .bind(org_id)
        .bind(&ids)
        .fetch_one(pool)
        .await?;
        if known as usize != ids.len() {
            return Err(AppError::BadRequest("Unknown payment method id".into()));
        }
    }

    let mut tx = pool.begin().await?;
    // Serialize concurrent PUTs for the same owner.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(format!("pm-availability:{owner_id}"))
        .execute(&mut *tx)
        .await?;
    sqlx::query(&format!("DELETE FROM {} WHERE {} = $1", owner.table(), owner.column()))
        .bind(owner_id)
        .execute(&mut *tx)
        .await?;
    if body.restricted {
        sqlx::query(&format!(
            "INSERT INTO {} ({}, payment_method_id, org_id) SELECT $1, unnest($2::uuid[]), $3",
            owner.table(),
            owner.column()
        ))
        .bind(owner_id)
        .bind(&ids)
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let stored = ids_for(pool, owner, owner_id).await?;
    Ok(AllowList { restricted: !stored.is_empty(), payment_method_ids: stored })
}

#[utoipa::path(
    put,
    path = "/payment-methods/availability/branches/{branch_id}",
    tag = "payment_methods",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = AllowList,
    responses((status = 200, description = "Stored branch allow-list", body = AllowList), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_branch_availability(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    id: web::Path<Uuid>,
    body: web::Json<AllowList>,
) -> Result<HttpResponse, AppError> {
    put_list(req, pool, hub, Owner::Branch, *id, body.into_inner()).await
}

#[utoipa::path(
    put,
    path = "/payment-methods/availability/users/{user_id}",
    tag = "payment_methods",
    params(("user_id" = Uuid, Path, description = "User (teller) ID")),
    request_body = AllowList,
    responses((status = 200, description = "Stored teller allow-list", body = AllowList), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_user_availability(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    id: web::Path<Uuid>,
    body: web::Json<AllowList>,
) -> Result<HttpResponse, AppError> {
    put_list(req, pool, hub, Owner::User, *id, body.into_inner()).await
}

#[utoipa::path(
    put,
    path = "/payment-methods/availability/devices/{device_id}",
    tag = "payment_methods",
    params(("device_id" = Uuid, Path, description = "Device ID")),
    request_body = AllowList,
    responses((status = 200, description = "Stored device allow-list", body = AllowList), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_device_availability(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    id: web::Path<Uuid>,
    body: web::Json<AllowList>,
) -> Result<HttpResponse, AppError> {
    put_list(req, pool, hub, Owner::Device, *id, body.into_inner()).await
}

// ── GET /payment-methods/effective ───────────────────────────────

#[utoipa::path(
    get,
    path = "/payment-methods/effective",
    tag = "payment_methods",
    params(EffectiveQuery),
    responses((status = 200, description = "Active methods allowed for branch ∩ teller ∩ device", body = Vec<OrgPaymentMethod>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_effective(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<EffectiveQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    check_permission(pool.get_ref(), &claims, "payment_methods", "read").await?;
    let org_id = org_of(&claims)?;
    ensure_branch_in_org(pool.get_ref(), org_id, q.branch_id).await?;
    let rows = effective_methods(pool.get_ref(), org_id, q.branch_id, q.user_id, q.device_id).await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        get_availability,
        put_branch_availability,
        put_user_availability,
        put_device_availability,
        get_effective,
    ),
    components(schemas(
        AllowList,
        UserAllowList,
        DeviceAllowList,
        PaymentMethodAvailability,
        crate::tills::reconcile::MethodTotal,
        crate::tills::reconcile::ReconciliationInput,
        crate::tills::reconcile::TillReconciliationLine,
    ))
)]
pub struct AvailabilityApiDoc;
