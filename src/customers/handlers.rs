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
pub(crate) async fn live_with_phone(
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
    // The act, beside the trail: who renamed or re-numbered whom.
    if cur.name != name {
        audit_identity(
            &mut tx,
            org,
            id,
            "name",
            IdentityActor::Staff(claims.user_id()),
            Some(&cur.name),
            Some(&name),
        )
        .await?;
    }
    if old_key != key {
        audit_identity(
            &mut tx,
            org,
            id,
            "phone",
            IdentityActor::Staff(claims.user_id()),
            old_key.as_deref(),
            key.as_deref(),
        )
        .await?;
    }
    tx.commit().await?;
    // The back of the card prints the number too, so a phone change refreshes
    // the pass exactly as a rename does.
    if cur.is_member && (cur.name != name || cur.locale != locale || old_key != key) {
        // The card carries the name and is written in the locale. The stored
        // pass is dropped HERE as well as inside `push_update`: that path is
        // skipped when no wallet is configured or the member has no Apple
        // pass yet, and the bytes must not outlive the name either way.
        crate::loyalty::wallet::store::invalidate(pool.get_ref(), id).await;
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
        after_merge(pool.get_ref(), body.into, loser);
    }
    Ok(HttpResponse::Ok().json(detail(pool.get_ref(), org, body.into).await?))
}

/// The wallet side of a both-members merge, AFTER the commit (network calls
/// never run inside the transaction): the survivor's card shows the combined
/// balance; the loser's Apple pass is pushed and served `voided`, its Google
/// object goes inactive.
pub fn after_merge(pool: &sqlx::PgPool, survivor: Uuid, loser: crate::loyalty::model::MemberRow) {
    crate::loyalty::wallet::push_update(pool, survivor);
    crate::loyalty::wallet::apple::void_pass(pool, &loser);
    {
        tokio::spawn(async move {
            // Google: the loser's object stops rendering. Its barcode still
            // scans to the survivor through `loyalty_token_aliases`.
            if let Err(e) = crate::loyalty::wallet::google::expire_object(&loser).await {
                use crate::observability::report::{Failure, report};
                report(Failure::new("loyalty", "expire_google_object"), &e);
            }
        });
    }
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
    // Everything else that names the duplicate (design §2.7). The snapshots on
    // those rows are history and are not touched.
    for table in ["delivery_orders", "bookings", "open_tickets", "customer_identity_audit"] {
        sqlx::query(&format!(
            "UPDATE {table} SET customer_id = $2 WHERE customer_id = $1 AND org_id = $3"
        ))
        .bind(from)
        .bind(into)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    }
    repoint_addresses(tx, org, from, into).await?;
    Ok(retired)
}

/// The duplicate's saved addresses become the survivor's, folded by the same
/// rule a write uses: an address the survivor already has absorbs the
/// duplicate's uses; orders that were sent to the folded row follow it.
async fn repoint_addresses(
    tx: &mut PgConnection,
    org: Uuid,
    from: Uuid,
    into: Uuid,
) -> Result<(), AppError> {
    let theirs: Vec<(Uuid, String, String, Option<String>, Option<f64>, Option<f64>, i32, DateTime<Utc>)> =
        sqlx::query_as(
            "SELECT id, channel, norm_key, unit_number, lat, lng, use_count, last_used_at
               FROM customer_addresses
              WHERE customer_id = $1 AND org_id = $2 AND erased_at IS NULL ORDER BY created_at",
        )
        .bind(from)
        .bind(org)
        .fetch_all(&mut *tx)
        .await?;
    for (id, channel, norm, unit, lat, lng, uses, used_at) in theirs {
        let twin: Option<Uuid> = sqlx::query_scalar(
            "SELECT a.id FROM customer_addresses a
              WHERE a.customer_id = $1 AND a.erased_at IS NULL AND a.channel = $2
                AND (a.norm_key = $3
                     OR ($5::float8 IS NOT NULL AND a.lat IS NOT NULL
                         AND lower(btrim(COALESCE(a.unit_number, ''))) = lower(btrim(COALESCE($4, '')))
                         AND geo_distance_m(a.lat, a.lng, $5, $6) <= 30.0))
              ORDER BY (a.norm_key = $3) DESC, a.last_used_at DESC LIMIT 1",
        )
        .bind(into)
        .bind(&channel)
        .bind(&norm)
        .bind(&unit)
        .bind(lat)
        .bind(lng)
        .fetch_optional(&mut *tx)
        .await?;
        match twin {
            Some(keep) => {
                sqlx::query(
                    "UPDATE customer_addresses SET use_count = use_count + $2,
                            last_used_at = GREATEST(last_used_at, $3) WHERE id = $1",
                )
                .bind(keep)
                .bind(uses)
                .bind(used_at)
                .execute(&mut *tx)
                .await?;
                sqlx::query("UPDATE delivery_orders SET address_id = $2 WHERE address_id = $1")
                    .bind(id)
                    .bind(keep)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM customer_addresses WHERE id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            None => {
                sqlx::query("UPDATE customer_addresses SET customer_id = $2 WHERE id = $1")
                    .bind(id)
                    .bind(into)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    // Erased rows carry no text; they simply follow.
    sqlx::query(
        "UPDATE customer_addresses SET customer_id = $2 WHERE customer_id = $1 AND org_id = $3",
    )
    .bind(from)
    .bind(into)
    .bind(org)
    .execute(&mut *tx)
    .await?;
    Ok(())
}


// ── erase (PDPL) ────────────────────────────────────────────────────────────

/// `id` plus every customer merged into it, transitively. A reference written
/// before a merge was re-pointed at merge time, but a till that was offline can
/// still land a row under a merged id afterwards — so everything that acts on
/// "this person's rows" goes through the chain, not the one id.
pub async fn chain_ids(conn: &mut PgConnection, org: Uuid, id: Uuid) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "WITH RECURSIVE chain(id, depth) AS (
             SELECT $2::uuid, 0
             UNION ALL
             SELECT c.id, chain.depth + 1 FROM customers c JOIN chain ON c.merged_into = chain.id
              WHERE c.org_id = $1 AND chain.depth < 32)
         SELECT DISTINCT id FROM chain",
    )
    .bind(org)
    .bind(id)
    .fetch_all(&mut *conn)
    .await?)
}

/// Erase one person everywhere, inside the caller's transaction (design §2.8).
/// `None` when there is no live customer under `id`. Returns the memberships
/// as they were, so the caller can expire their Google objects after commit.
///
/// Goes: the customer's name, phone, notes, birthday (and those of every row
/// merged into it); the membership (`loyalty::model::forget_in`) with its
/// token, devices, aliases and stored pass; the contact snapshots on delivery
/// orders, bookings, bills and sales; saved addresses; phone history; the
/// identity audit; OTP rows for every number they held.
/// Stays: every money figure, the order lines, the loyalty ledger — the shop's
/// books, which now say nothing about anyone.
pub async fn erase_inner(
    tx: &mut PgConnection,
    org: Uuid,
    id: Uuid,
) -> Result<Option<Vec<crate::loyalty::model::MemberRow>>, AppError> {
    let live: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM customers WHERE id = $1 AND org_id = $2
            AND erased_at IS NULL AND merged_into IS NULL FOR UPDATE",
    )
    .bind(id)
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?;
    if live.is_none() {
        return Ok(None);
    }
    let chain = chain_ids(tx, org, id).await?;

    // Every number they held, BEFORE it is blanked: the OTP table is keyed by
    // phone alone.
    let phones: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT k FROM (
             SELECT phone_key AS k FROM customers WHERE id = ANY($1)
             UNION ALL
             SELECT phone_key FROM customer_phone_history WHERE customer_id = ANY($1)
             UNION ALL
             SELECT phone_canonical(customer_phone) FROM delivery_orders
              WHERE customer_id = ANY($1) AND org_id = $2) x
          WHERE k IS NOT NULL",
    )
    .bind(&chain[..])
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;

    // The card(s). A live one is forgotten the way loyalty forgets; a retired
    // one (merge loser, or left the programme) loses what `leave` kept alive
    // for the voided pass.
    let mut cards = Vec::new();
    for member in &chain {
        if let Some(before) = crate::loyalty::model::forget_in(tx, *member).await? {
            cards.push(before);
        }
    }
    sqlx::query(
        "UPDATE loyalty_customers SET apple_auth_token = NULL, pass_voided_at = NULL, updated_at = now()
          WHERE id = ANY($1) AND org_id = $2",
    )
    .bind(&chain[..])
    .bind(org)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM loyalty_pass_devices WHERE customer_id = ANY($1)")
        .bind(&chain[..])
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM loyalty_pass_cache WHERE customer_id = ANY($1)")
        .bind(&chain[..])
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM loyalty_token_aliases WHERE customer_id = ANY($1) OR was_customer_id = ANY($1)",
    )
    .bind(&chain[..])
    .execute(&mut *tx)
    .await?;

    // The person — and the rows merged into them, which still carry what each
    // duplicate was called.
    sqlx::query(
        "UPDATE customers SET name = '', phone = NULL, phone_key = NULL, notes = NULL,
                birth_month = NULL, birth_day = NULL, marketing_opt_out = true,
                erased_at = COALESCE(erased_at, now()), updated_at = now()
          WHERE id = ANY($1) AND org_id = $2",
    )
    .bind(&chain[..])
    .bind(org)
    .execute(&mut *tx)
    .await?;

    // What was typed on their rows. Status, money, items and timestamps stay.
    sqlx::query(
        "UPDATE orders o SET customer_name = NULL,
                notes = CASE WHEN o.delivery_order_id IS NOT NULL THEN NULL ELSE o.notes END
           FROM branches b
          WHERE b.id = o.branch_id AND b.org_id = $2
            AND (o.customer_id = ANY($1)
                 OR o.delivery_order_id IN (SELECT d.id FROM delivery_orders d
                                             WHERE d.customer_id = ANY($1) AND d.org_id = $2))",
    )
    .bind(&chain[..])
    .bind(org)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE delivery_orders SET customer_name = '', customer_phone = '',
                place_name = NULL, floor = NULL, unit_number = NULL, landmark = NULL,
                address_line = NULL, delivery_notes = NULL,
                customer_lat = NULL, customer_lng = NULL, updated_at = now()
          WHERE customer_id = ANY($1) AND org_id = $2",
    )
    .bind(&chain[..])
    .bind(org)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE bookings SET guest_name = '', guest_phone = '', notes = NULL, updated_at = now()
          WHERE customer_id = ANY($1) AND org_id = $2",
    )
    .bind(&chain[..])
    .bind(org)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE open_tickets SET customer_name = NULL, updated_at = now()
          WHERE customer_id = ANY($1) AND org_id = $2",
    )
    .bind(&chain[..])
    .bind(org)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE customer_addresses SET label = NULL, place_name = NULL, floor = NULL,
                unit_number = NULL, landmark = NULL, address_line = NULL, delivery_notes = NULL,
                lat = NULL, lng = NULL, norm_key = '', erased_at = COALESCE(erased_at, now())
          WHERE customer_id = ANY($1) AND org_id = $2",
    )
    .bind(&chain[..])
    .bind(org)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM customer_phone_history WHERE customer_id = ANY($1) AND org_id = $2")
        .bind(&chain[..])
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM customer_identity_audit WHERE customer_id = ANY($1) AND org_id = $2")
        .bind(&chain[..])
        .bind(org)
        .execute(&mut *tx)
        .await?;
    if !phones.is_empty() {
        // Through the definer function: the tenant role cannot see
        // `delivery_otp` rows (row security, no policy), so a plain DELETE
        // here would succeed and remove nothing.
        sqlx::query("SELECT customers_purge_otp($1)")
            .bind(&phones[..])
            .execute(&mut *tx)
            .await?;
    }
    Ok(Some(cards))
}

#[utoipa::path(post, path = "/customers/{id}/erase", tag = "customers",
    params(("id" = Uuid, Path, description = "Customer id")),
    responses((status = 204, description = "Personal data erased everywhere: the customer, their loyalty card and passes, the contact snapshots on their orders, delivery orders, bookings and bills, saved addresses, phone history and OTP rows. Money and ledger rows stay."), AppErrorResponse),
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
    let mut tx = pool.get_ref().begin().await?;
    let Some(cards) = erase_inner(&mut tx, org, id).await? else {
        return Err(AppError::NotFound("Customer not found".into()));
    };
    tx.commit().await?;
    // Network calls: after the commit, never inside it.
    for before in cards {
        tokio::spawn(async move {
            if let Err(e) = crate::loyalty::wallet::google::expire_object(&before).await {
                use crate::observability::report::{Failure, report};
                report(Failure::new("loyalty", "expire_google_object"), &e);
            }
        });
    }
    Ok(HttpResponse::NoContent().finish())
}

// ── addresses ───────────────────────────────────────────────────────────────

/// A place a customer has had an order sent to.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema, sqlx::FromRow)]
pub struct CustomerAddress {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub label: Option<String>,
    pub place_name: Option<String>,
    pub floor: Option<String>,
    pub unit_number: Option<String>,
    pub landmark: Option<String>,
    pub address_line: Option<String>,
    pub delivery_notes: Option<String>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    pub delivery_zone_id: Option<Uuid>,
    /// The branch it was last ordered from.
    pub branch_id: Option<Uuid>,
    /// The channel it was last used with: `in_mall`, `outside` or `umbrella`.
    pub channel: String,
    pub use_count: i32,
    pub last_used_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

pub const ADDRESS_COLS: &str = "id, customer_id, label, place_name, floor, unit_number, landmark, \
    address_line, delivery_notes, lat, lng, delivery_zone_id, branch_id, channel, use_count, \
    last_used_at, created_at";

/// A customer's live addresses, most recently used first.
pub async fn addresses_of(
    conn: &mut PgConnection,
    org: Uuid,
    customer: Uuid,
) -> Result<Vec<CustomerAddress>, AppError> {
    Ok(sqlx::query_as(&format!(
        "SELECT {ADDRESS_COLS} FROM customer_addresses
          WHERE customer_id = $1 AND org_id = $2 AND erased_at IS NULL
          ORDER BY last_used_at DESC, id"
    ))
    .bind(customer)
    .bind(org)
    .fetch_all(&mut *conn)
    .await?)
}

/// What an order was sent to. The fields of a delivery order, borrowed.
pub struct AddressInput<'a> {
    pub branch_id: Uuid,
    pub channel: &'a str,
    pub place_name: Option<&'a str>,
    pub floor: Option<&'a str>,
    pub unit_number: Option<&'a str>,
    pub landmark: Option<&'a str>,
    pub address_line: Option<&'a str>,
    pub delivery_notes: Option<&'a str>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    pub zone_id: Option<Uuid>,
}

/// Remember where a PLACED order went (design §2.6, §4.3): bump the address the
/// customer already has, or add it. The rule lives in the database
/// (`customer_address_upsert`), so the backfill and this agree by construction.
/// `None` when there is nothing to keep (a pickup, or no text at all).
pub async fn save_address(
    tx: &mut PgConnection,
    org: Uuid,
    customer: Uuid,
    a: &AddressInput<'_>,
) -> Result<Option<Uuid>, AppError> {
    if a.channel == crate::delivery::CHANNEL_PICKUP {
        return Ok(None);
    }
    Ok(sqlx::query_scalar(
        "SELECT customer_address_upsert($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, now())",
    )
    .bind(org)
    .bind(customer)
    .bind(a.branch_id)
    .bind(a.channel)
    .bind(a.place_name)
    .bind(a.floor)
    .bind(a.unit_number)
    .bind(a.landmark)
    .bind(a.address_line)
    .bind(a.delivery_notes)
    .bind(a.lat)
    .bind(a.lng)
    .bind(a.zone_id)
    .fetch_one(&mut *tx)
    .await?)
}

#[utoipa::path(get, path = "/customers/{id}/addresses", tag = "customers",
    params(("id" = Uuid, Path, description = "Customer id (a merged id resolves)")),
    responses((status = 200, description = "Saved addresses, most recently used first", body = Vec<CustomerAddress>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_customer_addresses(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    require(pool.get_ref(), &claims, Cap::CustomersAddressesView, None).await?;
    let mut conn = pool.get_ref().acquire().await?;
    let id: Uuid = sqlx::query_scalar("SELECT customers_resolve($1, $2)")
        .bind(org)
        .bind(path.into_inner())
        .fetch_one(&mut *conn)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| AppError::NotFound("Customer not found".into()))?;
    Ok(HttpResponse::Ok().json(addresses_of(&mut conn, org, id).await?))
}

// ── identity changes ────────────────────────────────────────────────────────

/// Who is changing a customer's identity.
#[derive(Debug, Clone, Copy)]
pub enum IdentityActor {
    /// The customer, from a device verified for their phone (design §4.4).
    CustomerSelf,
    Staff(Uuid),
}

pub async fn audit_identity(
    conn: &mut PgConnection,
    org: Uuid,
    customer: Uuid,
    kind: &str,
    actor: IdentityActor,
    old: Option<&str>,
    new: Option<&str>,
) -> Result<(), AppError> {
    let (actor_kind, actor_user) = match actor {
        IdentityActor::CustomerSelf => ("customer", None),
        IdentityActor::Staff(u) => ("staff", Some(u)),
    };
    sqlx::query(
        "INSERT INTO customer_identity_audit (org_id, customer_id, kind, actor_kind, actor_user, old_value, new_value)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(org)
    .bind(customer)
    .bind(kind)
    .bind(actor_kind)
    .bind(actor_user)
    .bind(old)
    .bind(new)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Two names are the same name: trimmed, inner whitespace collapsed,
/// case-folded, Unicode NFC (design §4.4 — "Ali" typed as "ali " is no edit).
pub fn same_name(a: &str, b: &str) -> bool {
    fn fold(s: &str) -> String {
        use unicode_normalization::UnicodeNormalization;
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .nfc()
            .collect::<String>()
            .to_lowercase()
    }
    fold(a) == fold(b)
}

/// An EXPLICIT rename — the only way a stored name changes (§2.4). Audited.
/// The caller refreshes the passes after its commit ([`after_identity_change`]);
/// the changefeed row is the `customers` sync trigger's.
pub async fn rename(
    tx: &mut PgConnection,
    org: Uuid,
    customer: Uuid,
    new_name: &str,
    actor: IdentityActor,
) -> Result<(), AppError> {
    let name = clean_name(new_name)?;
    let old: Option<String> = sqlx::query_scalar(
        "SELECT name FROM customers WHERE id = $1 AND org_id = $2
            AND merged_into IS NULL AND erased_at IS NULL FOR UPDATE",
    )
    .bind(customer)
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?;
    let old = old.ok_or_else(|| AppError::NotFound("Customer not found".into()))?;
    if old == name {
        return Ok(());
    }
    sqlx::query("UPDATE customers SET name = $3, updated_at = now() WHERE id = $1 AND org_id = $2")
        .bind(customer)
        .bind(org)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    audit_identity(tx, org, customer, "name", actor, Some(&old), Some(&name)).await
}

/// What stopped a phone replacement.
pub enum ReplaceRefusal {
    /// Another live customer of the org holds the new number.
    BelongsTo(Uuid),
}

/// Give a customer a new phone: same id, the old number goes to
/// `customer_phone_history`, the act is audited. The unique index decides
/// whether the number is free. `Ok(Err(..))` is a refusal the caller turns into
/// its own error; the transaction is still usable ONLY when `Ok(Ok(()))`.
pub async fn replace_phone(
    tx: &mut PgConnection,
    org: Uuid,
    customer: Uuid,
    new_phone: &str,
    actor: IdentityActor,
) -> Result<Result<(), ReplaceRefusal>, AppError> {
    let key = crate::phone::normalize_phone(new_phone)?;
    let typed = clean_phone(Some(new_phone));
    let cur: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT phone, phone_key FROM customers WHERE id = $1 AND org_id = $2
            AND merged_into IS NULL AND erased_at IS NULL FOR UPDATE",
    )
    .bind(customer)
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?;
    let (old_phone, old_key) = cur.ok_or_else(|| AppError::NotFound("Customer not found".into()))?;
    if old_key.as_deref() == Some(key.as_str()) {
        return Ok(Ok(()));
    }
    // Looked up first so the refusal can name the holder without poisoning the
    // transaction; the index below still has the last word under a race.
    if let Some(holder) = live_with_phone(tx, org, &key).await?
        && holder != customer
    {
        return Ok(Err(ReplaceRefusal::BelongsTo(holder)));
    }
    let saved = sqlx::query(
        "UPDATE customers SET phone = $3, phone_key = $4, updated_at = now() WHERE id = $1 AND org_id = $2",
    )
    .bind(customer)
    .bind(org)
    .bind(&typed)
    .bind(&key)
    .execute(&mut *tx)
    .await;
    if let Err(e) = saved {
        if is_phone_taken(&e) {
            return Err(phone_exists(None));
        }
        return Err(e.into());
    }
    if let Some(old) = &old_phone {
        let (reason, by) = match actor {
            IdentityActor::CustomerSelf => ("self", None),
            IdentityActor::Staff(u) => ("edit", Some(u)),
        };
        remember_phone(tx, org, customer, old, old_key.as_deref(), reason, by).await?;
    }
    audit_identity(tx, org, customer, "phone", actor, old_key.as_deref(), Some(&key)).await?;
    Ok(Ok(()))
}

/// After an identity change has COMMITTED: the card carries the name and the
/// number, so the stored pass is dropped and the wallets are told. A no-op for
/// a customer without a card.
pub async fn after_identity_change(pool: &sqlx::PgPool, customer: Uuid) {
    crate::loyalty::wallet::store::invalidate(pool, customer).await;
    crate::loyalty::wallet::push_update(pool, customer);
}
