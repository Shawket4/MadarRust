//! Manual customers (phase 6): separate from loyalty but linkable, attached at
//! the till (online or queued offline), managed and merged in the dashboard.
//!
//! PDPL: every read here needs `customers.view` (the capability that says
//! "see customers and their phone numbers"); a till that may only attach
//! reads names from the feed and hides the phone itself. Erasing wipes the
//! personal fields but keeps the row so past orders stay consistent.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::auth::jwt::Claims;
use crate::authz::Cap;
use crate::authz::require::require;
use crate::errors::{AppError, AppErrorResponse};

const MAX_NAME: usize = 120;
const MAX_NOTES: usize = 2000;

fn claims_of(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

fn org_of(req: &HttpRequest, claims: &Claims) -> Result<Uuid, AppError> {
    claims
        .scope_org(crate::auth::middleware::header_org_id(req))
        .ok_or_else(|| AppError::Forbidden("No organization selected".into()))
}

/// Digits only; a leading Egyptian country code folds to the local `0`.
/// Mirrors the SQL `customers_phone_key`.
pub fn phone_key(phone: &str) -> Option<String> {
    let d: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if d.is_empty() {
        None
    } else if let Some(rest) = d.strip_prefix("0020") {
        Some(format!("0{rest}"))
    } else if d.len() == 12 && d.starts_with("20") {
        Some(format!("0{}", &d[2..]))
    } else {
        Some(d)
    }
}

fn clean_name(name: &str) -> Result<String, AppError> {
    let n = name.trim();
    if n.is_empty() {
        return Err(AppError::BadRequest("A customer needs a name".into()));
    }
    Ok(n.chars().take(MAX_NAME).collect())
}

fn clean_phone(phone: Option<&str>) -> Option<String> {
    phone
        .map(str::trim)
        .filter(|p| phone_key(p).is_some())
        .map(|p| p.chars().take(40).collect())
}

fn clean_notes(notes: Option<&str>) -> Option<String> {
    notes
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| n.chars().take(MAX_NOTES).collect())
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub phone: Option<String>,
    pub notes: Option<String>,
    pub loyalty_customer_id: Option<Uuid>,
    pub orders_count: i64,
    /// Sum of completed sales, minor units.
    pub total_spent: i64,
    pub last_order_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct CustomerOrder {
    pub id: Uuid,
    pub order_ref: Option<String>,
    pub branch_id: Uuid,
    pub branch_name: Option<String>,
    pub status: String,
    pub total_amount: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct CustomerDetail {
    pub customer: Customer,
    /// Set when the id asked for was merged: the id that was asked for.
    pub resolved_from: Option<Uuid>,
    /// Customers merged into this one.
    pub merged_from: Vec<Uuid>,
    pub recent_orders: Vec<CustomerOrder>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CustomerQuery {
    /// Matches name (contains) or phone (digits).
    pub q: Option<String>,
    /// Default 100, at most 500.
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct CreateCustomerRequest {
    /// Client-minted id; a repeat with the same id returns the stored customer.
    #[serde(default)]
    pub id: Option<Uuid>,
    pub name: String,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub loyalty_customer_id: Option<Uuid>,
    /// The branch where the customer was added (a till sends its own).
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default, ToSchema)]
pub struct UpdateCustomerRequest {
    pub name: Option<String>,
    /// Absent = unchanged; `""` clears.
    pub phone: Option<String>,
    /// Absent = unchanged; `""` clears.
    pub notes: Option<String>,
    /// Absent = unchanged.
    #[serde(default)]
    pub loyalty_customer_id: Option<Uuid>,
    /// `true` unlinks the loyalty member.
    #[serde(default)]
    pub unlink_loyalty: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct MergeCustomerRequest {
    /// The customer that stays.
    pub into: Uuid,
}

const SELECT_CUSTOMER: &str = "SELECT c.id, c.name, c.phone, c.notes, c.loyalty_customer_id,
        COALESCE(s.n, 0) AS orders_count, COALESCE(s.spent, 0) AS total_spent, s.last_at,
        c.created_at, c.updated_at
   FROM customers c
   LEFT JOIN LATERAL (
        SELECT count(*) AS n,
               COALESCE(sum(o.total_amount) FILTER (WHERE o.status::text = 'completed'), 0)::bigint AS spent,
               max(o.created_at) AS last_at
          FROM orders o WHERE o.customer_id = c.id) s ON true";

type Row = (
    Uuid,
    String,
    Option<String>,
    Option<String>,
    Option<Uuid>,
    i64,
    i64,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
    DateTime<Utc>,
);

fn to_customer(r: Row) -> Customer {
    Customer {
        id: r.0,
        name: r.1,
        phone: r.2,
        notes: r.3,
        loyalty_customer_id: r.4,
        orders_count: r.5,
        total_spent: r.6,
        last_order_at: r.7,
        created_at: r.8,
        updated_at: r.9,
    }
}

async fn fetch_live(
    conn: &mut PgConnection,
    org: Uuid,
    id: Uuid,
) -> Result<Option<Customer>, AppError> {
    let row: Option<Row> = sqlx::query_as(&format!(
        "{SELECT_CUSTOMER} WHERE c.id = $1 AND c.org_id = $2 AND c.merged_into IS NULL AND c.erased_at IS NULL"
    ))
    .bind(id)
    .bind(org)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(to_customer))
}

async fn live_with_phone(
    conn: &mut PgConnection,
    org: Uuid,
    key: &str,
    except: Option<Uuid>,
) -> Result<Option<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM customers WHERE org_id = $1 AND phone_key = $2
            AND merged_into IS NULL AND erased_at IS NULL AND id IS DISTINCT FROM $3
          ORDER BY created_at LIMIT 1",
    )
    .bind(org)
    .bind(key)
    .bind(except)
    .fetch_optional(&mut *conn)
    .await?)
}

async fn loyalty_in_org(
    conn: &mut PgConnection,
    org: Uuid,
    id: Option<Uuid>,
) -> Result<(), AppError> {
    let Some(id) = id else { return Ok(()) };
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM loyalty_customers WHERE id = $1 AND org_id = $2)",
    )
    .bind(id)
    .bind(org)
    .fetch_one(&mut *conn)
    .await?;
    if ok {
        Ok(())
    } else {
        Err(AppError::NotFound("No such loyalty member".into()))
    }
}

/// What a create did.
pub enum Created {
    New(Uuid),
    /// Same id seen before.
    Existing(Uuid),
    /// A till added a phone another live customer already holds: the new row
    /// is stored merged into that one.
    MergedInto {
        id: Uuid,
        into: Uuid,
    },
}

/// Shared by the dashboard route and replay. `replay = false` refuses a phone
/// that is already taken; a replayed create cannot be refused (the till has
/// already attached the id), so it is stored merged into the holder.
pub async fn insert_customer(
    conn: &mut PgConnection,
    org: Uuid,
    actor: Uuid,
    body: &CreateCustomerRequest,
    replay: bool,
) -> Result<Created, AppError> {
    let id = body.id.unwrap_or_else(Uuid::new_v4);
    let prior: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM customers WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    match prior {
        Some(o) if o == org => return Ok(Created::Existing(id)),
        Some(_) => return Err(AppError::Conflict("That customer id is taken".into())),
        None => {}
    }
    let name = clean_name(&body.name)?;
    let phone = clean_phone(body.phone.as_deref());
    let key = phone.as_deref().and_then(phone_key);
    loyalty_in_org(conn, org, body.loyalty_customer_id).await?;
    let holder = match &key {
        Some(k) => live_with_phone(conn, org, k, None).await?,
        None => None,
    };
    if let (Some(h), false) = (holder, replay) {
        return Err(AppError::Coded {
            status: 409,
            code: "CUSTOMER_PHONE_EXISTS",
            reason: format!("A customer with this phone already exists ({h})"),
        });
    }
    let branch: Option<Uuid> = match body.branch_id {
        Some(b) => {
            sqlx::query_scalar("SELECT id FROM branches WHERE id = $1 AND org_id = $2")
                .bind(b)
                .bind(org)
                .fetch_optional(&mut *conn)
                .await?
        }
        None => None,
    };
    sqlx::query(
        "INSERT INTO customers (id, org_id, name, phone, phone_key, notes, loyalty_customer_id,
                                merged_into, merged_at, created_by, created_branch_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CASE WHEN $8::uuid IS NULL THEN NULL ELSE now() END, $9, $10)",
    )
    .bind(id)
    .bind(org)
    .bind(&name)
    .bind(&phone)
    .bind(&key)
    .bind(clean_notes(body.notes.as_deref()))
    .bind(body.loyalty_customer_id)
    .bind(holder)
    .bind(actor)
    .bind(branch)
    .execute(&mut *conn)
    .await?;
    Ok(match holder {
        Some(into) => Created::MergedInto { id, into },
        None => Created::New(id),
    })
}

/// Point an order at a customer (or clear it). Unknown customers are ignored
/// rather than refused: a sale is never lost over its customer reference.
pub async fn attach_to_order(
    conn: &mut PgConnection,
    org: Uuid,
    order_id: Uuid,
    customer_id: Option<Uuid>,
) -> Result<Option<Uuid>, AppError> {
    let resolved: Option<Uuid> = match customer_id {
        Some(c) => {
            sqlx::query_scalar("SELECT customers_resolve($1, $2)")
                .bind(org)
                .bind(c)
                .fetch_one(&mut *conn)
                .await?
        }
        None => None,
    };
    if customer_id.is_some() && resolved.is_none() {
        return Ok(None);
    }
    sqlx::query(
        "UPDATE orders o SET customer_id = $2 FROM branches b
          WHERE o.id = $1 AND b.id = o.branch_id AND b.org_id = $3",
    )
    .bind(order_id)
    .bind(resolved)
    .bind(org)
    .execute(&mut *conn)
    .await?;
    Ok(resolved)
}

// ── routes ──────────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/customers", tag = "customers", params(CustomerQuery),
    responses((status = 200, description = "Customers", body = Vec<Customer>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_customers(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<CustomerQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    require(pool.get_ref(), &claims, Cap::CustomersView, None).await?;
    let text = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let digits = text.and_then(phone_key);
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "{SELECT_CUSTOMER}
          WHERE c.org_id = $1 AND c.merged_into IS NULL AND c.erased_at IS NULL
            AND ($2::text IS NULL OR c.name ILIKE '%' || $2 || '%'
                 OR ($3::text IS NOT NULL AND c.phone_key LIKE '%' || $3 || '%'))
          ORDER BY COALESCE(s.last_at, c.created_at) DESC, c.id
          LIMIT $4 OFFSET $5"
    ))
    .bind(org)
    .bind(text)
    .bind(digits)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows.into_iter().map(to_customer).collect::<Vec<_>>()))
}

#[utoipa::path(get, path = "/customers/{id}", tag = "customers",
    params(("id" = Uuid, Path, description = "Customer id (a merged id resolves)")),
    responses((status = 200, description = "Customer with history", body = CustomerDetail), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_customer(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    require(pool.get_ref(), &claims, Cap::CustomersView, None).await?;
    let asked = path.into_inner();
    Ok(HttpResponse::Ok().json(detail(pool.get_ref(), org, asked).await?))
}

async fn detail(pool: &sqlx::PgPool, org: Uuid, asked: Uuid) -> Result<CustomerDetail, AppError> {
    let mut conn = pool.acquire().await?;
    let id: Uuid = sqlx::query_scalar("SELECT customers_resolve($1, $2)")
        .bind(org)
        .bind(asked)
        .fetch_one(&mut *conn)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| AppError::NotFound("Customer not found".into()))?;
    let customer = fetch_live(&mut conn, org, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Customer not found".into()))?;
    let merged_from: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM customers WHERE org_id = $1 AND merged_into = $2 ORDER BY merged_at",
    )
    .bind(org)
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let recent_orders: Vec<CustomerOrder> = sqlx::query_as::<
        _,
        (Uuid, Option<String>, Uuid, Option<String>, String, i64, DateTime<Utc>),
    >(
        "SELECT o.id, o.order_ref, o.branch_id, b.name, o.status::text, o.total_amount::bigint, o.created_at
           FROM orders o JOIN branches b ON b.id = o.branch_id
          WHERE o.customer_id = $1 AND b.org_id = $2
          ORDER BY o.created_at DESC LIMIT 50",
    )
    .bind(id)
    .bind(org)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(id, order_ref, branch_id, branch_name, status, total_amount, created_at)| CustomerOrder {
        id,
        order_ref,
        branch_id,
        branch_name,
        status,
        total_amount,
        created_at,
    })
    .collect();
    Ok(CustomerDetail {
        customer,
        resolved_from: (asked != id).then_some(asked),
        merged_from,
        recent_orders,
    })
}

#[utoipa::path(post, path = "/customers", tag = "customers", request_body = CreateCustomerRequest,
    responses((status = 201, description = "Created", body = CustomerDetail),
              (status = 200, description = "Already stored under this id", body = CustomerDetail),
              AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn create_customer(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateCustomerRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    require(
        pool.get_ref(),
        &claims,
        Cap::CustomersCreate,
        body.branch_id,
    )
    .await?;
    let mut conn = pool.get_ref().acquire().await?;
    let created = insert_customer(&mut conn, org, claims.user_id(), &body, false).await?;
    drop(conn);
    let (status, id) = match created {
        Created::New(id) => (actix_web::http::StatusCode::CREATED, id),
        Created::Existing(id) | Created::MergedInto { id, .. } => {
            (actix_web::http::StatusCode::OK, id)
        }
    };
    let d = detail(pool.get_ref(), org, id).await?;
    Ok(HttpResponse::build(status).json(d))
}

#[utoipa::path(patch, path = "/customers/{id}", tag = "customers", request_body = UpdateCustomerRequest,
    params(("id" = Uuid, Path, description = "Customer id")),
    responses((status = 200, description = "Updated", body = CustomerDetail), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn update_customer(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<Uuid>,
    body: web::Json<UpdateCustomerRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    require(pool.get_ref(), &claims, Cap::CustomersEdit, None).await?;
    let id = path.into_inner();
    let mut tx = pool.get_ref().begin().await?;
    let cur = fetch_live(&mut tx, org, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Customer not found".into()))?;
    let name = match &body.name {
        Some(n) => clean_name(n)?,
        None => cur.name.clone(),
    };
    let phone = match &body.phone {
        Some(p) => clean_phone(Some(p)),
        None => cur.phone.clone(),
    };
    let key = phone.as_deref().and_then(phone_key);
    if let Some(k) = &key
        && let Some(h) = live_with_phone(&mut tx, org, k, Some(id)).await?
    {
        return Err(AppError::Coded {
            status: 409,
            code: "CUSTOMER_PHONE_EXISTS",
            reason: format!("A customer with this phone already exists ({h})"),
        });
    }
    let notes = match &body.notes {
        Some(n) => clean_notes(Some(n)),
        None => cur.notes.clone(),
    };
    let loyalty = if body.unlink_loyalty {
        None
    } else {
        body.loyalty_customer_id.or(cur.loyalty_customer_id)
    };
    if body.loyalty_customer_id.is_some() {
        loyalty_in_org(&mut tx, org, body.loyalty_customer_id).await?;
    }
    sqlx::query(
        "UPDATE customers SET name = $3, phone = $4, phone_key = $5, notes = $6,
                loyalty_customer_id = $7, updated_at = now()
          WHERE id = $1 AND org_id = $2",
    )
    .bind(id)
    .bind(org)
    .bind(&name)
    .bind(&phone)
    .bind(&key)
    .bind(&notes)
    .bind(loyalty)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(detail(pool.get_ref(), org, id).await?))
}

#[utoipa::path(post, path = "/customers/{id}/merge", tag = "customers", request_body = MergeCustomerRequest,
    params(("id" = Uuid, Path, description = "The duplicate, which stops being listed")),
    responses((status = 200, description = "The customer that stays", body = CustomerDetail), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn merge_customer(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<Uuid>,
    body: web::Json<MergeCustomerRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    require(pool.get_ref(), &claims, Cap::CustomersEdit, None).await?;
    let from = path.into_inner();
    if from == body.into {
        return Err(AppError::BadRequest(
            "A customer cannot be merged into itself".into(),
        ));
    }
    let mut tx = pool.get_ref().begin().await?;
    // Lock both rows in id order so two opposite merges cannot cross.
    let mut ids = [from, body.into];
    ids.sort();
    sqlx::query(
        "SELECT id FROM customers WHERE id = ANY($1) AND org_id = $2 ORDER BY id FOR UPDATE",
    )
    .bind(&ids[..])
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    let a = fetch_live(&mut tx, org, from)
        .await?
        .ok_or_else(|| AppError::NotFound("Customer not found".into()))?;
    let b = fetch_live(&mut tx, org, body.into)
        .await?
        .ok_or_else(|| AppError::NotFound("Customer to keep not found".into()))?;
    // The kept customer takes what it lacks from the duplicate.
    let phone = b.phone.clone().or(a.phone.clone());
    let notes = match (&b.notes, &a.notes) {
        (Some(x), Some(y)) if x != y => Some(format!("{x}\n{y}")),
        (Some(x), _) => Some(x.clone()),
        (None, y) => y.clone(),
    };
    let loyalty = b.loyalty_customer_id.or(a.loyalty_customer_id);
    sqlx::query(
        "UPDATE customers SET merged_into = $2, merged_at = now(), updated_at = now()
          WHERE id = $1 AND org_id = $3",
    )
    .bind(from)
    .bind(body.into)
    .bind(org)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE customers SET phone = $2, phone_key = $3, notes = $4, loyalty_customer_id = $5,
                updated_at = now()
          WHERE id = $1 AND org_id = $6",
    )
    .bind(body.into)
    .bind(&phone)
    .bind(phone.as_deref().and_then(phone_key))
    .bind(&notes)
    .bind(loyalty)
    .bind(org)
    .execute(&mut *tx)
    .await?;
    // Earlier merges into the duplicate now point at the kept customer too.
    sqlx::query("UPDATE customers SET merged_into = $2 WHERE merged_into = $1 AND org_id = $3")
        .bind(from)
        .bind(body.into)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE orders SET customer_id = $2 WHERE customer_id = $1")
        .bind(from)
        .bind(body.into)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(detail(pool.get_ref(), org, body.into).await?))
}

#[utoipa::path(post, path = "/customers/{id}/erase", tag = "customers",
    params(("id" = Uuid, Path, description = "Customer id")),
    responses((status = 204, description = "Personal data erased"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn erase_customer(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    require(pool.get_ref(), &claims, Cap::CustomersErase, None).await?;
    let id = path.into_inner();
    let done = sqlx::query(
        "UPDATE customers SET name = '', phone = NULL, phone_key = NULL, notes = NULL,
                loyalty_customer_id = NULL, erased_at = now(), updated_at = now()
          WHERE id = $1 AND org_id = $2 AND erased_at IS NULL AND merged_into IS NULL",
    )
    .bind(id)
    .bind(org)
    .execute(pool.get_ref())
    .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound("Customer not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}
