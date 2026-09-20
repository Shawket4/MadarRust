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

/// The lookup key: the canonical phone (`crate::phone`), or `None` when the
/// text is not a phone number. Mirrors the SQL `customers_phone_key`, which
/// delegates to `phone_canonical` the same way.
pub fn phone_key(phone: &str) -> Option<String> {
    crate::phone::canonical(phone)
}

fn clean_name(name: &str) -> Result<String, AppError> {
    let n = name.trim();
    if n.is_empty() {
        return Err(AppError::BadRequest("A customer needs a name".into()));
    }
    Ok(n.chars().take(MAX_NAME).collect())
}

/// The phone as typed, kept for display. Text with no digit in it is nothing;
/// text that has digits but is not a valid number is kept WITHOUT a key (a
/// till's queued create cannot be refused over a typo), so it simply cannot be
/// looked up or collide.
fn clean_phone(phone: Option<&str>) -> Option<String> {
    phone
        .map(str::trim)
        .filter(|p| !crate::phone::digits(p).is_empty())
        .map(|p| p.chars().take(40).collect())
}

/// Where a customer first came from — `customers.source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomerSource {
    Pos,
    Online,
    Loyalty,
    Booking,
    TableQr,
    Aggregator,
    Dashboard,
}

impl CustomerSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pos => "pos",
            Self::Online => "online",
            Self::Loyalty => "loyalty",
            Self::Booking => "booking",
            Self::TableQr => "table_qr",
            Self::Aggregator => "aggregator",
            Self::Dashboard => "dashboard",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pos" => Self::Pos,
            "online" => Self::Online,
            "loyalty" => Self::Loyalty,
            "booking" => Self::Booking,
            "table_qr" => Self::TableQr,
            "aggregator" => Self::Aggregator,
            "dashboard" => Self::Dashboard,
            _ => return None,
        })
    }
}

/// The partial unique index that makes one live customer per phone a fact.
const PHONE_LIVE_KEY: &str = "customers_org_phone_live_key";

fn is_phone_taken(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.constraint() == Some(PHONE_LIVE_KEY))
}

fn phone_exists(holder: Option<Uuid>) -> AppError {
    AppError::Coded {
        status: 409,
        code: "CUSTOMER_PHONE_EXISTS",
        reason: match holder {
            Some(h) => format!("A customer with this phone already exists ({h})"),
            None => "A customer with this phone already exists".into(),
        },
    }
}

fn clean_notes(notes: Option<&str>) -> Option<String> {
    notes
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| n.chars().take(MAX_NOTES).collect())
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema, sqlx::FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub phone: Option<String>,
    pub notes: Option<String>,
    /// DEPRECATED, kept for one release: a loyalty membership now shares the
    /// customer's id, so this is `id` when `is_member` and null otherwise.
    pub loyalty_customer_id: Option<Uuid>,
    /// A live loyalty membership exists for this customer (same id).
    #[serde(default)]
    pub is_member: bool,
    /// Null when not a member.
    #[serde(default)]
    pub points_balance: Option<i32>,
    /// Null when not a member.
    #[serde(default)]
    pub visits_balance: Option<i32>,
    /// Where the customer first came from: `pos`, `online`, `loyalty`,
    /// `booking`, `table_qr`, `aggregator` or `dashboard`.
    #[serde(default)]
    pub source: String,
    /// `en` or `ar`; null when never asked.
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub marketing_opt_out: bool,
    #[serde(default)]
    pub birth_month: Option<i16>,
    #[serde(default)]
    pub birth_day: Option<i16>,
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
    /// `true` = loyalty members only, `false` = non-members only.
    pub member: Option<bool>,
    /// Only customers that first came from this source (`pos`, `online`,
    /// `loyalty`, `booking`, `table_qr`, `aggregator`, `dashboard`).
    pub source: Option<String>,
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
    /// DEPRECATED and ignored: a membership shares the customer's id, so there
    /// is nothing to link. Accepted so deployed tills keep working.
    #[serde(default)]
    pub loyalty_customer_id: Option<Uuid>,
    /// The branch where the customer was added (a till sends its own).
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    /// Where the customer came from. Defaults to `pos` when a branch is named
    /// (a till) and `dashboard` otherwise.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default, ToSchema)]
pub struct UpdateCustomerRequest {
    pub name: Option<String>,
    /// Absent = unchanged; `""` clears.
    pub phone: Option<String>,
    /// Absent = unchanged; `""` clears.
    pub notes: Option<String>,
    /// DEPRECATED and ignored (see `CreateCustomerRequest`).
    #[serde(default)]
    pub loyalty_customer_id: Option<Uuid>,
    /// DEPRECATED and ignored: leaving the programme is
    /// `DELETE /loyalty/members/{id}`.
    #[serde(default)]
    pub unlink_loyalty: bool,
    /// `en` or `ar`. Absent = unchanged.
    #[serde(default)]
    pub locale: Option<String>,
    /// Absent = unchanged.
    #[serde(default)]
    pub marketing_opt_out: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct MergeCustomerRequest {
    /// The customer that stays.
    pub into: Uuid,
}

const SELECT_CUSTOMER: &str = "SELECT c.id, c.name, c.phone, c.notes,
        CASE WHEN m.id IS NOT NULL THEN c.id END AS loyalty_customer_id,
        (m.id IS NOT NULL) AS is_member, m.points_balance, m.visits_balance,
        c.source, c.locale, c.marketing_opt_out, c.birth_month, c.birth_day,
        COALESCE(s.n, 0) AS orders_count, COALESCE(s.spent, 0) AS total_spent,
        s.last_at AS last_order_at, c.created_at, c.updated_at
   FROM customers c
   LEFT JOIN loyalty_customers m ON m.id = c.id AND m.deleted_at IS NULL
   LEFT JOIN LATERAL (
        SELECT count(*) AS n,
               COALESCE(sum(o.total_amount) FILTER (WHERE o.status::text = 'completed'), 0)::bigint AS spent,
               max(o.created_at) AS last_at
          FROM orders o WHERE o.customer_id = c.id) s ON true";

async fn fetch_live(
    conn: &mut PgConnection,
    org: Uuid,
    id: Uuid,
) -> Result<Option<Customer>, AppError> {
    Ok(sqlx::query_as(&format!(
        "{SELECT_CUSTOMER} WHERE c.id = $1 AND c.org_id = $2 AND c.merged_into IS NULL AND c.erased_at IS NULL"
    ))
    .bind(id)
    .bind(org)
    .fetch_optional(&mut *conn)
    .await?)
}

/// The live customer holding `key` in `org`, if any.
async fn live_with_phone(
    conn: &mut PgConnection,
    org: Uuid,
    key: &str,
) -> Result<Option<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM customers WHERE org_id = $1 AND phone_key = $2
            AND merged_into IS NULL AND erased_at IS NULL",
    )
    .bind(org)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?)
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

/// What [`resolve_or_create`] did to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nobody held the phone: a new customer.
    Created,
    /// A live customer already held the phone; nothing about them changed.
    Matched,
    /// The caller brought its own id (an offline till) and the phone was taken:
    /// its row is stored merged into the holder, whose id is returned.
    MergedInto,
}

struct NewCustomer<'a> {
    id: Uuid,
    name: &'a str,
    phone: Option<&'a str>,
    key: Option<&'a str>,
    notes: Option<String>,
    source: CustomerSource,
    branch: Option<Uuid>,
    actor: Option<Uuid>,
}

enum Landed {
    Inserted,
    /// The id was already stored (a replay, or a second flush racing the first).
    IdExists,
    /// Another live customer of the org holds the phone.
    PhoneHeldBy(Uuid),
}

/// THE insert. Every customer row is written here, so the one rule that
/// matters — one live customer per phone per tenant — has one writer.
///
/// Race-free by construction: `ON CONFLICT DO NOTHING` waits for a concurrent
/// inserter of the same phone (or id) to finish and then does nothing, and the
/// read that follows sees what the winner committed. No pre-check is involved.
async fn insert_row(
    conn: &mut PgConnection,
    org: Uuid,
    c: &NewCustomer<'_>,
) -> Result<Landed, AppError> {
    // A holder can be erased or merged away between our lost insert and our
    // read of who won; then the phone is free again and the insert is retried.
    for _ in 0..3 {
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO customers (id, org_id, name, phone, phone_key, notes, source,
                                    created_by, created_branch_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                     (SELECT b.id FROM branches b WHERE b.id = $9 AND b.org_id = $2))
             ON CONFLICT DO NOTHING
             RETURNING id",
        )
        .bind(c.id)
        .bind(org)
        .bind(c.name)
        .bind(c.phone)
        .bind(c.key)
        .bind(&c.notes)
        .bind(c.source.as_str())
        .bind(c.actor)
        .bind(c.branch)
        .fetch_optional(&mut *conn)
        .await?;
        if inserted.is_some() {
            return Ok(Landed::Inserted);
        }
        let prior: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM customers WHERE id = $1")
            .bind(c.id)
            .fetch_optional(&mut *conn)
            .await?;
        match prior {
            Some(o) if o == org => return Ok(Landed::IdExists),
            Some(_) => return Err(AppError::Conflict("That customer id is taken".into())),
            None => {}
        }
        if let Some(k) = c.key
            && let Some(h) = live_with_phone(conn, org, k).await?
        {
            return Ok(Landed::PhoneHeldBy(h));
        }
    }
    Err(AppError::Conflict("Please try again".into()))
}

/// A till's row for a phone someone else holds: stored already merged, so the
/// id the till has attached to its sales resolves to the holder.
async fn insert_merged(
    conn: &mut PgConnection,
    org: Uuid,
    c: &NewCustomer<'_>,
    into: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO customers (id, org_id, name, phone, phone_key, notes, source,
                                merged_into, merged_at, created_by, created_branch_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now(), $9,
                 (SELECT b.id FROM branches b WHERE b.id = $10 AND b.org_id = $2))
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(c.id)
    .bind(org)
    .bind(c.name)
    .bind(c.phone)
    .bind(c.key)
    .bind(&c.notes)
    .bind(c.source.as_str())
    .bind(into)
    .bind(c.actor)
    .bind(c.branch)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The only way a customer comes into being from a transactional flow (design
/// §2.4): the live customer holding `phone` in `org`, created if there is none.
///
/// * No valid phone, no customer: the caller gets a 400 and keeps the typed
///   name as the order's snapshot.
/// * A matched customer is returned AS IS. The name a guest types on a join or
///   an order never overwrites the one on file; names change only by an
///   explicit edit.
/// * `client_id` is an id minted by an offline till. If the phone is taken, a
///   row with that id is stored merged into the holder, so the id still
///   resolves; the HOLDER's id is what comes back.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_or_create(
    tx: &mut PgConnection,
    org: Uuid,
    phone: &str,
    name: &str,
    source: CustomerSource,
    branch: Option<Uuid>,
    actor: Option<Uuid>,
    client_id: Option<Uuid>,
) -> Result<(Uuid, Outcome), AppError> {
    let key = crate::phone::normalize_phone(phone)?;
    let name = clean_name(name)?;
    let typed = clean_phone(Some(phone));
    let c = NewCustomer {
        id: client_id.unwrap_or_else(Uuid::new_v4),
        name: &name,
        phone: typed.as_deref(),
        key: Some(&key),
        notes: None,
        source,
        branch,
        actor,
    };
    match insert_row(tx, org, &c).await? {
        Landed::Inserted => Ok((c.id, Outcome::Created)),
        Landed::IdExists => {
            // A replay of a create we already stored: whoever that id is now.
            let live: Option<Uuid> = sqlx::query_scalar("SELECT customers_resolve($1, $2)")
                .bind(org)
                .bind(c.id)
                .fetch_one(&mut *tx)
                .await?;
            Ok((live.unwrap_or(c.id), Outcome::Matched))
        }
        Landed::PhoneHeldBy(holder) => {
            if client_id.is_some() {
                insert_merged(tx, org, &c, holder).await?;
                Ok((holder, Outcome::MergedInto))
            } else {
                Ok((holder, Outcome::Matched))
            }
        }
    }
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
    // Before validating: a replay of a stored create is a success even if the
    // same body would no longer pass.
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
    let source = match body.source.as_deref() {
        Some(s) => CustomerSource::parse(s)
            .ok_or_else(|| AppError::BadRequest(format!("Unknown customer source `{s}`")))?,
        None if replay || body.branch_id.is_some() => CustomerSource::Pos,
        None => CustomerSource::Dashboard,
    };
    let c = NewCustomer {
        id,
        name: &name,
        phone: phone.as_deref(),
        key: key.as_deref(),
        notes: clean_notes(body.notes.as_deref()),
        source,
        branch: body.branch_id,
        actor: Some(actor),
    };
    match insert_row(conn, org, &c).await? {
        Landed::Inserted => Ok(Created::New(id)),
        Landed::IdExists => Ok(Created::Existing(id)),
        Landed::PhoneHeldBy(h) if !replay => Err(phone_exists(Some(h))),
        Landed::PhoneHeldBy(into) => {
            insert_merged(conn, org, &c, into).await?;
            Ok(Created::MergedInto { id, into })
        }
    }
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
    let digits = text.and_then(crate::phone::search_digits);
    let source = match q.source.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(src) => Some(
            CustomerSource::parse(src)
                .ok_or_else(|| AppError::BadRequest(format!("Unknown customer source `{src}`")))?
                .as_str(),
        ),
        None => None,
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let rows: Vec<Customer> = sqlx::query_as(&format!(
        "{SELECT_CUSTOMER}
          WHERE c.org_id = $1 AND c.merged_into IS NULL AND c.erased_at IS NULL
            AND ($2::text IS NULL OR c.name ILIKE '%' || $2 || '%'
                 OR ($3::text IS NOT NULL AND c.phone_key LIKE '%' || $3 || '%'))
            AND ($6::boolean IS NULL OR (m.id IS NOT NULL) = $6)
            AND ($7::text IS NULL OR c.source = $7)
          ORDER BY COALESCE(s.last_at, c.created_at) DESC, c.id
          LIMIT $4 OFFSET $5"
    ))
    .bind(org)
    .bind(text)
    .bind(digits)
    .bind(limit)
    .bind(offset)
    .bind(q.member)
    .bind(source)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
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
    let notes = match &body.notes {
        Some(n) => clean_notes(Some(n)),
        None => cur.notes.clone(),
    };
    let locale = match body.locale.as_deref() {
        Some(l) if l.starts_with("ar") => Some("ar".to_string()),
        Some(_) => Some("en".to_string()),
        None => cur.locale.clone(),
    };
    let old_key = cur.phone.as_deref().and_then(phone_key);
    if cur.is_member && key.is_none() {
        // The card is found by this number, and the OTP goes to it.
        return Err(AppError::BadRequest(
            "A loyalty member needs a valid phone number".into(),
        ));
    }
    // The unique index decides whether the phone is free; there is no check
    // beforehand for a second editor to slip past.
    let saved = sqlx::query(
        "UPDATE customers SET name = $3, phone = $4, phone_key = $5, notes = $6,
                locale = $7, marketing_opt_out = COALESCE($8, marketing_opt_out),
                updated_at = now()
          WHERE id = $1 AND org_id = $2",
    )
    .bind(id)
    .bind(org)
    .bind(&name)
    .bind(&phone)
    .bind(&key)
    .bind(&notes)
    .bind(&locale)
    .bind(body.marketing_opt_out)
    .execute(&mut *tx)
    .await;
    if let Err(e) = saved {
        if is_phone_taken(&e) {
            drop(tx);
            let mut conn = pool.get_ref().acquire().await?;
            let holder = match &key {
                Some(k) => live_with_phone(&mut conn, org, k).await?,
                None => None,
            };
            return Err(phone_exists(holder));
        }
        return Err(e.into());
    }
    // One phone per customer; the number they had is remembered.
    if let Some(old) = &cur.phone
        && old_key != key
    {
        remember_phone(
            &mut tx,
            org,
            id,
            old,
            old_key.as_deref(),
            "edit",
            Some(claims.user_id()),
        )
        .await?;
    }
    tx.commit().await?;
    if cur.is_member && (cur.name != name || cur.locale != locale) {
        // The card carries the name and is written in the locale.
        crate::loyalty::wallet::push_update(pool.get_ref(), id);
    }
    Ok(HttpResponse::Ok().json(detail(pool.get_ref(), org, id).await?))
}

async fn remember_phone(
    conn: &mut PgConnection,
    org: Uuid,
    customer: Uuid,
    phone: &str,
    key: Option<&str>,
    reason: &str,
    by: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO customer_phone_history (org_id, customer_id, phone, phone_key, reason, replaced_by)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(org)
    .bind(customer)
    .bind(phone)
    .bind(key)
    .bind(reason)
    .bind(by)
    .execute(&mut *conn)
    .await?;
    Ok(())
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
    require(pool.get_ref(), &claims, Cap::CustomersMerge, None).await?;
    let from = path.into_inner();
    if from == body.into {
        return Err(AppError::BadRequest(
            "A customer cannot be merged into itself".into(),
        ));
    }
    let mut tx = pool.get_ref().begin().await?;
    let retired = merge_inner(&mut tx, org, from, body.into, Some(claims.user_id())).await?;
    tx.commit().await?;
    // Wallet calls are network calls: after the commit, never inside it.
    if let Some(loser) = retired {
        crate::loyalty::wallet::push_update(pool.get_ref(), body.into);
        tokio::spawn(async move {
            // Google: the loser's object stops rendering. Its barcode still
            // scans to the survivor through `loyalty_token_aliases`.
            // TODO(customers-unification §2.7): Apple has no equivalent hook
            // yet. The pass builder cannot mark a pass `voided`, so the
            // loser's Apple pass simply stops updating. Add a `voided` flag to
            // `wallet::apple::pass_json` and push it to the loser's devices.
            if let Err(e) = crate::loyalty::wallet::google::expire_object(&loser).await {
                use crate::observability::report::{Failure, report};
                report(Failure::new("loyalty", "expire_google_object"), &e);
            }
        });
    }
    Ok(HttpResponse::Ok().json(detail(pool.get_ref(), org, body.into).await?))
}

/// Fold `from` into `into`, memberships included (design §2.7). Returns the
/// retired membership when BOTH were members, so the caller can void its
/// passes after committing.
pub async fn merge_inner(
    tx: &mut PgConnection,
    org: Uuid,
    from: Uuid,
    into: Uuid,
    actor: Option<Uuid>,
) -> Result<Option<crate::loyalty::model::MemberRow>, AppError> {
    // Lock both rows in id order so two opposite merges cannot cross.
    let mut ids = [from, into];
    ids.sort();
    sqlx::query(
        "SELECT id FROM customers WHERE id = ANY($1) AND org_id = $2 ORDER BY id FOR UPDATE",
    )
    .bind(&ids[..])
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    let a = fetch_live(tx, org, from)
        .await?
        .ok_or_else(|| AppError::NotFound("Customer not found".into()))?;
    let b = fetch_live(tx, org, into)
        .await?
        .ok_or_else(|| AppError::NotFound("Customer to keep not found".into()))?;
    // The member side always survives: its id is printed in wallet passes and
    // held by the ledger, the pass devices and the greetings. Refused rather
    // than silently swapped, so the operator sees which record stays.
    if a.is_member && !b.is_member {
        return Err(AppError::Coded {
            status: 409,
            code: "CUSTOMER_MERGE_MEMBER_SURVIVES",
            reason: format!(
                "{} is a loyalty member and must be the customer that stays; merge the other way ({into} into {from})",
                a.name
            ),
        });
    }
    let retired = if a.is_member && b.is_member {
        Some(crate::loyalty::model::merge_memberships(tx, org, from, into, actor).await?)
    } else {
        None
    };
    // The kept customer takes what it lacks from the duplicate.
    let phone = b.phone.clone().or(a.phone.clone());
    let key = phone.as_deref().and_then(phone_key);
    let notes = match (&b.notes, &a.notes) {
        (Some(x), Some(y)) if x != y => Some(format!("{x}\n{y}")),
        (Some(x), _) => Some(x.clone()),
        (None, y) => y.clone(),
    };
    // Marked merged FIRST: that frees the duplicate's phone for the survivor.
    sqlx::query(
        "UPDATE customers SET merged_into = $2, merged_at = now(), updated_at = now()
          WHERE id = $1 AND org_id = $3",
    )
    .bind(from)
    .bind(into)
    .bind(org)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE customers SET phone = $2, phone_key = $3, notes = $4,
                locale = COALESCE(locale, $5),
                birth_month = COALESCE(birth_month, $6),
                birth_day = CASE WHEN birth_month IS NULL THEN $7 ELSE birth_day END,
                -- Consent: the stricter answer wins.
                marketing_opt_out = marketing_opt_out OR $8,
                first_seen_at = LEAST(first_seen_at,
                    (SELECT d.first_seen_at FROM customers d WHERE d.id = $10)),
                updated_at = now()
          WHERE id = $1 AND org_id = $9",
    )
    .bind(into)
    .bind(&phone)
    .bind(&key)
    .bind(&notes)
    .bind(&a.locale)
    .bind(a.birth_month)
    .bind(a.birth_day)
    .bind(a.marketing_opt_out)
    .bind(org)
    .bind(from)
    .execute(&mut *tx)
    .await?;
    // The duplicate's own number, when the survivor keeps a different one.
    let a_key = a.phone.as_deref().and_then(phone_key);
    if let Some(p) = &a.phone
        && (a_key != key || (a_key.is_none() && a.phone != phone))
    {
        remember_phone(tx, org, into, p, a_key.as_deref(), "merge", actor).await?;
    }
    sqlx::query(
        "UPDATE customer_phone_history SET customer_id = $2 WHERE customer_id = $1 AND org_id = $3",
    )
    .bind(from)
    .bind(into)
    .bind(org)
    .execute(&mut *tx)
    .await?;
    // Earlier merges into the duplicate now point at the kept customer too.
    sqlx::query("UPDATE customers SET merged_into = $2 WHERE merged_into = $1 AND org_id = $3")
        .bind(from)
        .bind(into)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE orders SET customer_id = $2 WHERE customer_id = $1")
        .bind(from)
        .bind(into)
        .execute(&mut *tx)
        .await?;
    Ok(retired)
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
                birth_month = NULL, birth_day = NULL, marketing_opt_out = true,
                erased_at = now(), updated_at = now()
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
