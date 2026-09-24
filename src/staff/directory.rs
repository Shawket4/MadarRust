//! The employee directory: departments, employees, documents.
//!
//! AN EMPLOYEE IS ITS OWN ENTITY (Dawam Phase A, PHASE_A_DESIGN.md §1),
//! optionally linked to a Madar user:
//!
//! - **linked** — an existing user (a cashier, a manager, the owner) who is also
//!   on payroll. Their POS role is untouched; "make this user an employee" is
//!   `POST /staff/employees` with `user_id`.
//! - **app** — no user: signs in to the staff app with a WhatsApp code.
//! - **manual** — no user, no app: payroll and attendance records only.
//!
//! Adding an employee never creates a login, a till PIN or a POS teller, and
//! creating a login never makes anyone an employee. Removing an employee
//! terminates them: their attendance and payslips are records of what
//! happened and outlive the employment (AT-6).
//!
//! SALARY VISIBILITY: `base_salary_piastres` is nulled out unless the caller
//! may read payroll for that employee's branches (`hr.payroll.read`). It is
//! redacted in the *response*, not the query, so there is exactly one place to
//! get this wrong.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    authz::Cap,
    errors::{AppError, AppErrorResponse},
    staff::{
        access::{self, Subject},
        principal::caller,
        require_user_in_org, scope_org,
    },
};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Department {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub manager_user_id: Option<Uuid>,
    /// Denormalised for the dashboard list; not stored.
    #[sqlx(default)]
    pub manager_name: Option<String>,
    /// Live employees currently assigned. Not stored.
    #[sqlx(default)]
    pub employee_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Employee {
    pub id: Uuid,
    pub org_id: Uuid,
    /// The linked Madar user, when this employee is one (a cashier, a manager,
    /// the owner). Null for someone who is only on payroll.
    pub user_id: Option<Uuid>,
    /// `linked` · `app` (signs in to the staff app, no Madar account) ·
    /// `manual` (records only, no app).
    pub kind: String,
    pub name: String,
    pub phone: Option<String>,
    /// May sign in to the staff app with a WhatsApp code.
    pub app_access: bool,
    /// The linked user's POS role; null for an unlinked employee.
    pub role: Option<String>,
    pub email: Option<String>,
    pub department_id: Option<Uuid>,
    #[sqlx(default)]
    pub department_name: Option<String>,
    pub employee_code: Option<String>,
    pub job_title: Option<String>,
    pub hire_date: Option<NaiveDate>,
    pub termination_date: Option<NaiveDate>,
    /// `active` · `suspended` · `terminated`
    pub employment_status: String,
    /// `None` when the caller may not read this person's pay — see the module docs.
    pub base_salary_piastres: Option<i64>,
    pub national_id: Option<String>,
    pub photo_url: Option<String>,
    pub emergency_contact_name: Option<String>,
    pub emergency_contact_phone: Option<String>,
    pub notes: Option<String>,
    /// `m` · `f` · null — only ever a soft default for late shifts (SC-13).
    pub gender: Option<String>,
    /// `cash` · `bank` · `wallet`
    pub pay_method: String,
    pub pay_account: Option<String>,
    /// Paid through Dawam (the default). Off for someone who uses the app
    /// and is rostered but is not paid here (an owner, say): the payroll
    /// run, the estimate and the payslips skip them.
    #[sqlx(default)]
    pub on_payroll: bool,
    /// The owner's cap on what this person may owe in salary advances, in
    /// piastres (AV-5): the server's figure, so no client recomputes it.
    /// Hidden with the salary.
    #[sqlx(default)]
    pub advance_cap_piastres: Option<i64>,
    /// `morning` · `evening` · null
    pub pref_time: Option<String>,
    /// Days they can't work: 0 = Sunday … 6 = Saturday.
    pub cant_work_days: Vec<i16>,
    /// Where they work; managers see the people of their branches (RO-6).
    pub branch_ids: Vec<Uuid>,
    /// The live phone signed in to the staff app, if any.
    pub device_model: Option<String>,
    pub device_since: Option<DateTime<Utc>>,
    pub device_last_seen: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Employee {
    /// Strip the salary unless the caller may see it. Called on every path
    /// that returns an `Employee`.
    fn redact_salary(mut self, pay_scope: &Option<Vec<Uuid>>, may_read_pay: bool) -> Self {
        let visible = may_read_pay
            && match pay_scope {
                None => true,
                Some(at) => self.branch_ids.iter().any(|b| at.contains(b)),
            };
        if !visible {
            self.base_salary_piastres = None;
            self.advance_cap_piastres = None;
        }
        self
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct StaffDocument {
    pub id: Uuid,
    pub org_id: Uuid,
    pub employee_id: Uuid,
    pub kind: String,
    pub title: String,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub file_url: Option<String>,
    pub expires_on: Option<NaiveDate>,
    pub uploaded_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

const DOCUMENT_COLS: &str =
    "id, org_id, employee_id, kind, title, file_url, expires_on, uploaded_by, created_at";

// ── Requests ──────────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertDepartmentRequest {
    pub name: String,
    #[serde(default)]
    pub manager_user_id: Option<Uuid>,
}

/// Add an employee of any kind (see the module docs).
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateEmployeeRequest {
    /// Make this existing Madar user an employee (kind `linked`). Their name
    /// and number are the defaults for the employee's.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Required unless `user_id` is given.
    #[serde(default)]
    pub name: Option<String>,
    /// Their WhatsApp number: how they sign in to the staff app.
    #[serde(default)]
    pub phone: Option<String>,
    /// May sign in to the staff app. Defaults to "has a phone".
    #[serde(default)]
    pub app_access: Option<bool>,
    /// Where they work (at least one). `branch_id` is the older one-branch form.
    #[serde(default)]
    pub branch_ids: Vec<Uuid>,
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    /// Piastres. Ignored without `hr.payroll.edit` for every branch.
    #[serde(default)]
    pub base_salary_piastres: Option<i64>,
    #[serde(default)]
    pub job_title: Option<String>,
    /// `m` · `f`
    #[serde(default)]
    pub gender: Option<String>,
    /// Defaults to today.
    #[serde(default)]
    pub hire_date: Option<NaiveDate>,
    #[serde(default)]
    pub department_id: Option<Uuid>,
    #[serde(default)]
    pub employee_code: Option<String>,
}

/// Replace an employee's HR profile. Profile fields are a full replace (null
/// clears them); `name`, `phone`, `app_access` and `branch_ids` are kept when
/// omitted.
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PutEmployeeRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// A new number signs the old phone out (RO-10). Empty clears it.
    #[serde(default)]
    pub phone: Option<String>,
    /// Turning it off signs the phone out.
    #[serde(default)]
    pub app_access: Option<bool>,
    /// The whole set of branches.
    #[serde(default)]
    pub branch_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub department_id: Option<Uuid>,
    #[serde(default)]
    pub employee_code: Option<String>,
    #[serde(default)]
    pub job_title: Option<String>,
    #[serde(default)]
    pub hire_date: Option<NaiveDate>,
    #[serde(default)]
    pub termination_date: Option<NaiveDate>,
    /// `active` | `suspended` | `terminated`. Defaults to `active`. Anything
    /// but `active` signs the phone out (RO-10).
    #[serde(default)]
    pub employment_status: Option<String>,
    /// Piastres. Ignored unless the caller has `hr.payroll.edit` for every
    /// branch — a branch manager editing a job title must not award a raise.
    #[serde(default)]
    pub base_salary_piastres: Option<i64>,
    #[serde(default)]
    pub national_id: Option<String>,
    #[serde(default)]
    pub photo_url: Option<String>,
    #[serde(default)]
    pub emergency_contact_name: Option<String>,
    #[serde(default)]
    pub emergency_contact_phone: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// `m` · `f`; `null` or empty = not set; omitted keeps what is there.
    #[serde(
        default,
        deserialize_with = "crate::menu::handlers::deserialize_double_option"
    )]
    #[schema(nullable, value_type = Option<String>)]
    pub gender: Option<Option<String>>,
    /// `cash` · `bank` · `wallet`; omitted keeps what is there. Cash clears
    /// the account.
    #[serde(default)]
    pub pay_method: Option<String>,
    /// The IBAN or wallet number; `null` or empty clears it; omitted keeps
    /// it. Always cleared when the method is (or stays) `cash`.
    #[serde(
        default,
        deserialize_with = "crate::menu::handlers::deserialize_double_option"
    )]
    #[schema(nullable, value_type = Option<String>)]
    pub pay_account: Option<Option<String>>,
    /// Paid through Dawam. Like the salary, ignored unless the caller has
    /// `hr.payroll.edit` for every branch.
    #[serde(default)]
    pub on_payroll: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateDocumentRequest {
    #[serde(default)]
    pub kind: Option<String>,
    pub title: String,
    /// A path returned by the existing `/uploads` endpoints.
    pub file_url: String,
    #[serde(default)]
    pub expires_on: Option<NaiveDate>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct EmployeeListQuery {
    #[serde(default)]
    pub department_id: Option<Uuid>,
    /// `active` | `suspended` | `terminated`. Omitted = every status.
    #[serde(default)]
    pub employment_status: Option<String>,
    /// Case-insensitive substring over name, employee code, and job title.
    #[serde(default)]
    pub search: Option<String>,
    /// Only the people of this branch.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    /// `linked` · `app` · `manual`.
    #[serde(default)]
    pub kind: Option<String>,
}

fn validate_employment_status(status: &str) -> Result<&str, AppError> {
    match status {
        "active" | "suspended" | "terminated" => Ok(status),
        other => Err(AppError::BadRequest(format!(
            "Unknown employment status '{other}' — expected active, suspended, or terminated"
        ))),
    }
}

fn trimmed_required(value: &str, field: &str) -> Result<String, AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest(format!("{field} is required")));
    }
    Ok(trimmed.to_string())
}

/// Normalise an optional free-text field: blank becomes `NULL` rather than an
/// empty string, so "cleared" and "never set" are the same state in the database.
fn blank_to_none(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// A phone as stored: `+` and the canonical digits. Blank is none.
fn clean_phone(raw: Option<&str>) -> Result<Option<String>, AppError> {
    match raw.map(str::trim).filter(|p| !p.is_empty()) {
        None => Ok(None),
        Some(p) => Ok(Some(format!("+{}", crate::phone::normalize_phone(p)?))),
    }
}

// ── Departments ───────────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/departments", tag = "staff",
    responses((status = 200, description = "Departments in the org", body = Vec<Department>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_departments(
    req: HttpRequest,
    pool: crate::db::Db,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::scope(pool.get_ref(), &claims, org_id, Cap::HrStaffRead).await?;

    let rows = sqlx::query_as::<_, Department>(
        r#"
        SELECT d.id, d.org_id, d.name, d.manager_user_id,
               m.name AS manager_name,
               COUNT(e.id) AS employee_count,
               d.created_at, d.updated_at
          FROM departments d
          LEFT JOIN users m ON m.id = d.manager_user_id AND m.deleted_at IS NULL
          LEFT JOIN employees e ON e.department_id = d.id
                               AND e.employment_status <> 'terminated'
         WHERE d.org_id = $1
         GROUP BY d.id, m.name
         ORDER BY lower(d.name)
        "#,
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/staff/departments", tag = "staff",
    request_body = UpsertDepartmentRequest,
    responses((status = 201, description = "Department created", body = Department), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_department(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<UpsertDepartmentRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // A department spans the business: an org-wide act.
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrStaffCreate).await?;

    let name = trimmed_required(&body.name, "Department name")?;
    if let Some(manager) = body.manager_user_id {
        require_user_in_org(pool.get_ref(), org_id, manager).await?;
    }

    let row = sqlx::query_as::<_, Department>(
        "INSERT INTO departments (org_id, name, manager_user_id) VALUES ($1, $2, $3) \
         RETURNING id, org_id, name, manager_user_id, \
                   NULL::text AS manager_name, 0::bigint AS employee_count, \
                   created_at, updated_at",
    )
    .bind(org_id)
    .bind(&name)
    .bind(body.manager_user_id)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    patch, path = "/staff/departments/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Department ID")),
    request_body = UpsertDepartmentRequest,
    responses((status = 200, description = "Department updated", body = Department), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_department(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpsertDepartmentRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrStaffEdit).await?;

    let name = trimmed_required(&body.name, "Department name")?;
    if let Some(manager) = body.manager_user_id {
        require_user_in_org(pool.get_ref(), org_id, manager).await?;
    }

    let row = sqlx::query_as::<_, Department>(
        "UPDATE departments SET name = $3, manager_user_id = $4, updated_at = now() \
         WHERE id = $1 AND org_id = $2 \
         RETURNING id, org_id, name, manager_user_id, \
                   NULL::text AS manager_name, 0::bigint AS employee_count, \
                   created_at, updated_at",
    )
    .bind(*id)
    .bind(org_id)
    .bind(&name)
    .bind(body.manager_user_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Department not found".into()))?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete, path = "/staff/departments/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Department ID")),
    responses((status = 204, description = "Department deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_department(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrStaffDelete).await?;

    // Employees point at departments with ON DELETE SET NULL, so deleting one
    // orphans rather than cascades. Refuse anyway while anyone is still in it:
    // silently unfiling twenty people is not what "delete department" means.
    let occupied: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM employees WHERE department_id = $1 AND org_id = $2 \
            AND employment_status <> 'terminated'",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_one(pool.get_ref())
    .await?;
    if occupied > 0 {
        return Err(AppError::BadRequest(format!(
            "{occupied} employee(s) are still in this department — move them first"
        )));
    }

    let deleted = sqlx::query("DELETE FROM departments WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(pool.get_ref())
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Department not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}

// ── Employees ─────────────────────────────────────────────────

/// Every column of the employee projection, in `Employee` field order. Shared by
/// the list and single-row queries so the two can never drift.
const EMPLOYEE_SELECT: &str = r#"
    SELECT e.id, e.org_id, e.user_id,
           CASE WHEN e.user_id IS NOT NULL THEN 'linked'
                WHEN e.app_access THEN 'app' ELSE 'manual' END AS kind,
           e.name, e.phone, e.app_access, u.role::text AS role, u.email,
           e.department_id, d.name AS department_name, e.employee_code, e.job_title,
           e.hire_date, e.termination_date, e.employment_status, e.base_salary_piastres,
           e.national_id, e.photo_url, e.emergency_contact_name, e.emergency_contact_phone,
           e.notes, e.gender, e.pay_method, e.pay_account, e.pref_time, e.cant_work_days,
           e.on_payroll,
           dawam_advance_cap(e.org_id, e.base_salary_piastres) AS advance_cap_piastres,
           COALESCE(ARRAY(SELECT eb.branch_id FROM employee_branches eb
                           WHERE eb.employee_id = e.id ORDER BY eb.assigned_at, eb.branch_id),
                    '{}') AS branch_ids,
           dv.model AS device_model, dv.first_seen_at AS device_since,
           dv.last_seen_at AS device_last_seen,
           e.created_at, e.updated_at
      FROM employees e
      LEFT JOIN users u ON u.id = e.user_id
      LEFT JOIN departments d ON d.id = e.department_id
      LEFT JOIN staff_devices dv ON dv.employee_id = e.id AND dv.revoked_at IS NULL
"#;

/// Where the caller may read pay: `(scope, may at all)`.
async fn pay_scope(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
) -> Result<(Option<Vec<Uuid>>, bool), AppError> {
    match access::scope(pool, claims, org_id, Cap::HrPayrollRead).await {
        Ok(s) => Ok((s, true)),
        Err(AppError::Forbidden(_)) => Ok((Some(vec![]), false)),
        Err(e) => Err(e),
    }
}

#[utoipa::path(
    get, path = "/staff/employees", tag = "staff",
    params(EmployeeListQuery),
    responses((status = 200, description = "Employees at the caller's branches", body = Vec<Employee>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_employees(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<EmployeeListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    let scope = access::scope_at(pool, &claims, org_id, Cap::HrStaffRead, query.branch_id).await?;
    let (pay, may_pay) = pay_scope(pool, &claims, org_id).await?;

    if let Some(status) = query.employment_status.as_deref() {
        validate_employment_status(status)?;
    }
    if let Some(kind) = query.kind.as_deref()
        && !matches!(kind, "linked" | "app" | "manual")
    {
        return Err(AppError::BadRequest("kind is linked, app or manual".into()));
    }
    let search = query
        .search
        .as_deref()
        .map(|s| format!("%{}%", s.trim().to_lowercase()));

    let rows = sqlx::query_as::<_, Employee>(&format!(
        "{EMPLOYEE_SELECT} \
          WHERE e.org_id = $1 \
            AND ($2::uuid IS NULL OR e.department_id = $2) \
            AND ($3::text IS NULL OR e.employment_status = $3) \
            AND ($4::text IS NULL \
                 OR lower(e.name) LIKE $4 \
                 OR lower(COALESCE(e.employee_code, '')) LIKE $4 \
                 OR lower(COALESCE(e.job_title, '')) LIKE $4) \
            AND {} \
            AND ($6::text IS NULL OR $6 = CASE WHEN e.user_id IS NOT NULL THEN 'linked' \
                 WHEN e.app_access THEN 'app' ELSE 'manual' END) \
          ORDER BY lower(e.name)",
        access::in_scope("e.id", 5)
    ))
    .bind(org_id)
    .bind(query.department_id)
    .bind(query.employment_status.as_deref())
    .bind(search)
    .bind(scope.as_deref())
    .bind(query.kind.as_deref())
    .fetch_all(pool)
    .await?;

    let rows: Vec<Employee> = rows
        .into_iter()
        .map(|e| e.redact_salary(&pay, may_pay))
        .collect();
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    get, path = "/staff/employees/{employee_id}", tag = "staff",
    params(("employee_id" = Uuid, Path, description = "The employee's id")),
    responses((status = 200, description = "The employee", body = Employee), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_employee(
    req: HttpRequest,
    pool: crate::db::Db,
    employee_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrStaffRead).await?;
    let subject = access::subject(pool, org_id, *employee_id).await?;
    access::require_for(pool, &claims, Cap::HrStaffRead, &subject).await?;
    let (pay, may_pay) = pay_scope(pool, &claims, org_id).await?;
    let row = load_employee(pool, org_id, *employee_id).await?;
    Ok(HttpResponse::Ok().json(row.redact_salary(&pay, may_pay)))
}

pub(crate) async fn load_employee(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
) -> Result<Employee, AppError> {
    sqlx::query_as::<_, Employee>(&format!(
        "{EMPLOYEE_SELECT} WHERE e.id = $1 AND e.org_id = $2"
    ))
    .bind(employee_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Employee not found".into()))
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct BranchPerson {
    pub employee_id: Uuid,
    pub name: String,
}

/// Active employees at a branch, names only: what a till shows to tag a
/// pay-out as someone's expense advance (AV-8). Anyone who works the branch
/// may read it; nothing about pay is in it.
#[utoipa::path(
    get, path = "/staff/branches/{branch_id}/people", tag = "staff",
    params(("branch_id" = Uuid, Path)),
    responses((status = 200, body = Vec<BranchPerson>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_people(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    crate::authz::scope::org_read_branches(pool, &claims, org_id, Some(*branch_id)).await?;
    let rows: Vec<BranchPerson> = sqlx::query_as(
        "SELECT e.id AS employee_id, e.name FROM employee_branches eb \
           JOIN employees e ON e.id = eb.employee_id AND e.org_id = $2 \
                           AND e.employment_status = 'active' \
          WHERE eb.branch_id = $1 ORDER BY lower(e.name)",
    )
    .bind(*branch_id)
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// A Madar user who can be made an employee.
#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct LinkableUser {
    pub user_id: Uuid,
    pub name: String,
    /// Their POS role (`org_admin`, `branch_manager`, `teller`, …).
    pub role: String,
    pub phone: Option<String>,
    pub email: Option<String>,
}

/// The org's users who are not employees yet: the "make this user an
/// employee" picker.
#[utoipa::path(
    get, path = "/staff/employees/linkable", tag = "staff",
    responses((status = 200, body = Vec<LinkableUser>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn linkable_users(
    req: HttpRequest,
    pool: crate::db::Db,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    let scope = access::scope(pool, &claims, org_id, Cap::HrStaffCreate).await?;
    // A branch manager sees the users of their branches; an owner, everyone.
    let rows: Vec<LinkableUser> = sqlx::query_as(
        "SELECT u.id AS user_id, u.name, u.role::text AS role, u.phone, u.email \
           FROM users u \
          WHERE u.org_id = $1 AND u.deleted_at IS NULL AND u.is_active \
            AND u.role <> 'super_admin' \
            AND NOT EXISTS (SELECT 1 FROM employees e WHERE e.user_id = u.id) \
            AND ($2::uuid[] IS NULL OR EXISTS (SELECT 1 FROM user_branch_assignments a \
                                                WHERE a.user_id = u.id AND a.branch_id = ANY($2))) \
          ORDER BY lower(u.name)",
    )
    .bind(org_id)
    .bind(scope.as_deref())
    .fetch_all(pool)
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// Linking (or changing the number of) a user who is not the caller gives
/// them a phone sign-in with their own powers: the caller must dominate them
/// (the same anti-escalation rule as editing their account), so a manager can
/// never route the owner's staff-app code to their own phone.
async fn guard_linked_user(
    pool: &PgPool,
    claims: &Claims,
    user_id: Uuid,
    authority: Cap,
) -> Result<(), AppError> {
    if claims.sub == user_id.to_string() {
        return Ok(());
    }
    crate::permissions::guard::require_dominance(pool, claims, user_id, authority).await
}

async fn check_branches(pool: &PgPool, org_id: Uuid, branches: &[Uuid]) -> Result<(), AppError> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM branches WHERE id = ANY($1) AND org_id = $2 AND deleted_at IS NULL",
    )
    .bind(branches)
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    if n as usize != branches.len() {
        return Err(AppError::NotFound("Branch not found".into()));
    }
    Ok(())
}

async fn phone_taken(
    pool: &PgPool,
    org_id: Uuid,
    phone: &str,
    except: Option<Uuid>,
) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM employees WHERE org_id = $1 AND app_access \
            AND employment_status <> 'terminated' AND phone_key = phone_canonical($2) \
            AND ($3::uuid IS NULL OR id <> $3))",
    )
    .bind(org_id)
    .bind(phone)
    .bind(except)
    .fetch_one(pool)
    .await?)
}

fn dedup(mut v: Vec<Uuid>) -> Vec<Uuid> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|b| seen.insert(*b));
    v
}

/// Add an employee: linked to an existing user, or without one (with or
/// without the staff app). Used by the Employees page, the set-up wizard and
/// the spreadsheet import (DSH-7). Never creates a login.
#[utoipa::path(
    post, path = "/staff/employees", tag = "staff",
    request_body = CreateEmployeeRequest,
    responses((status = 201, description = "Employee added", body = Employee), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_employee(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateEmployeeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrStaffCreate).await?;
    let mut branches = body.branch_ids.clone();
    if let Some(b) = body.branch_id {
        branches.insert(0, b);
    }
    let branches = dedup(branches);
    if branches.is_empty() {
        return Err(AppError::BadRequest("Pick the branch they work at".into()));
    }
    check_branches(pool, org_id, &branches).await?;
    // Adding someone to a branch is that branch's manager's call (RO-1, RO-6).
    for b in &branches {
        access::require_at(pool, &claims, org_id, Cap::HrStaffCreate, *b).await?;
    }

    // The linked user, when this makes an existing user an employee.
    let linked: Option<(String, Option<String>)> = match body.user_id {
        Some(user) => {
            let row: Option<(String, Option<String>)> = sqlx::query_as(
                "SELECT name, phone FROM users WHERE id = $1 AND org_id = $2 \
                    AND deleted_at IS NULL AND role <> 'super_admin'",
            )
            .bind(user)
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
            let row = row.ok_or_else(|| AppError::NotFound("User not found".into()))?;
            let already: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM employees WHERE user_id = $1)")
                    .bind(user)
                    .fetch_one(pool)
                    .await?;
            if already {
                return Err(AppError::Conflict(
                    "That user is already an employee.".into(),
                ));
            }
            guard_linked_user(pool, &claims, user, Cap::HrStaffCreate).await?;
            Some(row)
        }
        None => None,
    };

    let name = match (body.name.as_deref(), &linked) {
        (Some(n), _) if !n.trim().is_empty() => n.trim().to_string(),
        (_, Some((user_name, _))) => user_name.clone(),
        _ => return Err(AppError::BadRequest("name is required".into())),
    };
    // A linked user's number is the default; a number that is not a phone is
    // dropped rather than refused, since it came from their account.
    let phone = match (body.phone.as_deref(), &linked) {
        (Some(p), _) if !p.trim().is_empty() => clean_phone(Some(p))?,
        (_, Some((_, user_phone))) => clean_phone(user_phone.as_deref()).unwrap_or(None),
        _ => None,
    };
    let app_access = body.app_access.unwrap_or(phone.is_some());
    if app_access && phone.is_none() {
        return Err(AppError::BadRequest(
            "The staff app needs their WhatsApp number.".into(),
        ));
    }
    if app_access
        && let Some(p) = &phone
        && phone_taken(pool, org_id, p, None).await?
    {
        return Err(AppError::Conflict(format!(
            "Someone here already signs in with {p}."
        )));
    }
    if body.base_salary_piastres.is_some_and(|s| s < 0) {
        return Err(AppError::BadRequest("Salary cannot be negative".into()));
    }
    let may_edit_pay = access::can_everywhere(pool, &claims, org_id, Cap::HrPayrollEdit).await?;
    let salary = if may_edit_pay {
        body.base_salary_piastres
    } else {
        None
    };
    if let Some(dept) = body.department_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM departments WHERE id = $1 AND org_id = $2)",
        )
        .bind(dept)
        .bind(org_id)
        .fetch_one(pool)
        .await?;
        if !ok {
            return Err(AppError::NotFound("Department not found".into()));
        }
    }

    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO employees (org_id, user_id, name, phone, app_access, job_title, hire_date, \
             base_salary_piastres, gender, department_id, employee_code) \
         VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, CURRENT_DATE), COALESCE($8, 0), $9, $10, $11) \
         RETURNING id",
    )
    .bind(org_id)
    .bind(body.user_id)
    .bind(&name)
    .bind(&phone)
    .bind(app_access)
    .bind(blank_to_none(body.job_title.clone()))
    .bind(body.hire_date)
    .bind(salary)
    .bind(blank_to_none(body.gender.clone()))
    .bind(body.department_id)
    .bind(blank_to_none(body.employee_code.clone()))
    .fetch_one(&mut *tx)
    .await?;
    for b in &branches {
        sqlx::query(
            "INSERT INTO employee_branches (employee_id, branch_id, org_id, assigned_by) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(b)
        .bind(org_id)
        .bind(claims.user_id_safe().ok())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let (pay, may_pay) = pay_scope(pool, &claims, org_id).await?;
    let row = load_employee(pool, org_id, id).await?;
    Ok(HttpResponse::Created().json(row.redact_salary(&pay, may_pay)))
}

#[utoipa::path(
    put, path = "/staff/employees/{employee_id}", tag = "staff",
    params(("employee_id" = Uuid, Path, description = "The employee's id")),
    request_body = PutEmployeeRequest,
    responses((status = 200, description = "Employee saved", body = Employee), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_employee(
    req: HttpRequest,
    pool: crate::db::Db,
    employee_id: web::Path<Uuid>,
    body: web::Json<PutEmployeeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrStaffEdit).await?;
    let subject: Subject = access::subject(pool, org_id, *employee_id).await?;
    access::require_for(pool, &claims, Cap::HrStaffEdit, &subject).await?;

    let status = match body.employment_status.as_deref() {
        Some(s) => validate_employment_status(s)?.to_string(),
        None => "active".to_string(),
    };
    if status == "terminated" && body.termination_date.is_none() {
        return Err(AppError::BadRequest(
            "A terminated employee needs a termination date".into(),
        ));
    }
    if status != "terminated" && body.termination_date.is_some() {
        return Err(AppError::BadRequest(
            "Only a terminated employee may carry a termination date".into(),
        ));
    }
    if let Some(dept) = body.department_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM departments WHERE id = $1 AND org_id = $2)",
        )
        .bind(dept)
        .bind(org_id)
        .fetch_one(pool)
        .await?;
        if !ok {
            return Err(AppError::NotFound("Department not found".into()));
        }
    }

    // The current sign-in facts, to see what changes.
    let (old_phone_key, old_app): (Option<String>, bool) =
        sqlx::query_as("SELECT phone_key, app_access FROM employees WHERE id = $1")
            .bind(*employee_id)
            .fetch_one(pool)
            .await?;
    let phone = match body.phone.as_deref() {
        None => None,
        Some(p) => Some(clean_phone(Some(p))?),
    };
    let new_key = match &phone {
        Some(p) => p.as_deref().map(|p| p.trim_start_matches('+').to_string()),
        None => old_phone_key.clone(),
    };
    let app_access = body.app_access.unwrap_or(old_app) && status != "terminated";
    if app_access && new_key.is_none() {
        return Err(AppError::BadRequest(
            "The staff app needs their WhatsApp number.".into(),
        ));
    }
    let phone_changed = new_key != old_phone_key;
    let sign_in_changed = phone_changed || app_access != old_app;
    // Handing a linked user's phone sign-in to a new number is an access
    // change on that user.
    if let Some(user) = subject.user_id
        && (sign_in_changed && app_access)
    {
        guard_linked_user(pool, &claims, user, Cap::HrStaffEdit).await?;
    }
    if app_access
        && let Some(Some(p)) = &phone
        && phone_taken(pool, org_id, p, Some(*employee_id)).await?
    {
        return Err(AppError::Conflict(format!(
            "Someone here already signs in with {p}."
        )));
    }

    // Branches: the caller must run every branch they add or take away.
    if let Some(wanted) = &body.branch_ids {
        let wanted = dedup(wanted.clone());
        if wanted.is_empty() {
            return Err(AppError::BadRequest(
                "An employee works at one branch at least".into(),
            ));
        }
        check_branches(pool, org_id, &wanted).await?;
        for b in wanted.iter().filter(|b| !subject.branches.contains(b)) {
            access::require_at(pool, &claims, org_id, Cap::HrStaffEdit, *b).await?;
        }
        for b in subject.branches.iter().filter(|b| !wanted.contains(b)) {
            access::require_at(pool, &claims, org_id, Cap::HrStaffEdit, *b).await?;
        }
    }

    // Salary is a payroll write, not a directory write: without
    // `hr.payroll.edit` for every branch the figure is ignored and the stored
    // one kept, so a branch manager saving a job title cannot award a raise.
    if body.base_salary_piastres.is_some_and(|s| s < 0) {
        return Err(AppError::BadRequest("Salary cannot be negative".into()));
    }
    let may_edit_pay = access::can_everywhere(pool, &claims, org_id, Cap::HrPayrollEdit).await?;
    let salary = if may_edit_pay {
        body.base_salary_piastres
    } else {
        None
    };
    let on_payroll = if may_edit_pay { body.on_payroll } else { None };
    let name = match body.name.as_deref() {
        Some(n) => Some(trimmed_required(n, "name")?),
        None => None,
    };

    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"
        UPDATE employees SET
            name                    = COALESCE($3, name),
            phone                   = CASE WHEN $4 THEN $5 ELSE phone END,
            app_access              = $6,
            department_id           = $7,
            employee_code           = $8,
            job_title               = $9,
            hire_date               = $10,
            termination_date        = $11,
            employment_status       = $12,
            -- NULL here means "not permitted to change it", not "set to zero".
            base_salary_piastres    = COALESCE($13, base_salary_piastres),
            national_id             = $14,
            photo_url               = $15,
            emergency_contact_name  = $16,
            emergency_contact_phone = $17,
            notes                   = $18,
            -- Sent (even null) replaces; omitted keeps (E2E B-SETUP-2).
            gender                  = CASE WHEN $23 THEN $19 ELSE gender END,
            pay_method              = COALESCE($20, pay_method),
            -- A cash payee has no account: a stale IBAN or wallet would
            -- feed the bank and wallet lists (PAY-7, PAY-8).
            pay_account             = CASE WHEN COALESCE($20, pay_method) = 'cash' THEN NULL
                                           WHEN $24 THEN $21 ELSE pay_account END,
            on_payroll              = COALESCE($22, on_payroll),
            updated_at              = now()
        WHERE id = $1 AND org_id = $2
        "#,
    )
    .bind(*employee_id)
    .bind(org_id)
    .bind(&name)
    .bind(phone.is_some())
    .bind(phone.clone().flatten())
    .bind(app_access)
    .bind(body.department_id)
    .bind(blank_to_none(body.employee_code.clone()))
    .bind(blank_to_none(body.job_title.clone()))
    .bind(body.hire_date)
    .bind(body.termination_date)
    .bind(&status)
    .bind(salary)
    .bind(blank_to_none(body.national_id.clone()))
    .bind(blank_to_none(body.photo_url.clone()))
    .bind(blank_to_none(body.emergency_contact_name.clone()))
    .bind(blank_to_none(body.emergency_contact_phone.clone()))
    .bind(blank_to_none(body.notes.clone()))
    .bind(blank_to_none(body.gender.clone().flatten()))
    .bind(blank_to_none(body.pay_method.clone()))
    .bind(blank_to_none(body.pay_account.clone().flatten()))
    .bind(on_payroll)
    .bind(body.gender.is_some())
    .bind(body.pay_account.is_some())
    .execute(&mut *tx)
    .await?;
    if let Some(wanted) = &body.branch_ids {
        let wanted = dedup(wanted.clone());
        sqlx::query(
            "DELETE FROM employee_branches WHERE employee_id = $1 AND NOT (branch_id = ANY($2))",
        )
        .bind(*employee_id)
        .bind(&wanted)
        .execute(&mut *tx)
        .await?;
        for b in &wanted {
            sqlx::query(
                "INSERT INTO employee_branches (employee_id, branch_id, org_id, assigned_by) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
            )
            .bind(*employee_id)
            .bind(b)
            .bind(org_id)
            .bind(claims.user_id_safe().ok())
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    // RO-10: a new number, the app switched off, or someone who is no longer
    // active loses the app on every phone at once.
    if sign_in_changed || status != "active" {
        crate::staff::dawam::revoke_devices(pool, *employee_id).await?;
    }

    let (pay, may_pay) = pay_scope(pool, &claims, org_id).await?;
    let row = load_employee(pool, org_id, *employee_id).await?;
    Ok(HttpResponse::Ok().json(row.redact_salary(&pay, may_pay)))
}

#[utoipa::path(
    delete, path = "/staff/employees/{employee_id}", tag = "staff",
    params(("employee_id" = Uuid, Path, description = "The employee's id")),
    responses((status = 204, description = "Employee terminated; their records stay"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_employee(
    req: HttpRequest,
    pool: crate::db::Db,
    employee_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrStaffDelete).await?;
    let subject = access::subject(pool, org_id, *employee_id).await?;
    access::require_for(pool, &claims, Cap::HrStaffDelete, &subject).await?;

    // Deliberately NOT a delete: attendance and payslips are records of what
    // happened and outlive the employment (AT-6). The person is terminated
    // today, loses the app, and drops out of rosters and payroll from here.
    sqlx::query(
        "UPDATE employees SET employment_status = 'terminated', app_access = false, \
             termination_date = GREATEST(CURRENT_DATE, COALESCE(hire_date, CURRENT_DATE)), \
             updated_at = now() \
          WHERE id = $1 AND org_id = $2 AND employment_status <> 'terminated'",
    )
    .bind(*employee_id)
    .bind(org_id)
    .execute(pool)
    .await?;
    crate::staff::dawam::revoke_devices(pool, *employee_id).await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Documents ─────────────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/employees/{employee_id}/documents", tag = "staff",
    params(("employee_id" = Uuid, Path, description = "The employee's id")),
    responses((status = 200, description = "The employee's documents", body = Vec<StaffDocument>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_documents(
    req: HttpRequest,
    pool: crate::db::Db,
    employee_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrStaffRead).await?;
    let subject = access::subject(pool.get_ref(), org_id, *employee_id).await?;
    access::require_for(pool.get_ref(), &claims, Cap::HrStaffRead, &subject).await?;

    let rows = sqlx::query_as::<_, StaffDocument>(&format!(
        "SELECT {DOCUMENT_COLS} FROM staff_documents \
         WHERE employee_id = $1 AND org_id = $2 ORDER BY created_at DESC"
    ))
    .bind(*employee_id)
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/staff/employees/{employee_id}/documents", tag = "staff",
    params(("employee_id" = Uuid, Path, description = "The employee's id")),
    request_body = CreateDocumentRequest,
    responses((status = 201, description = "Document attached", body = StaffDocument), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_document(
    req: HttpRequest,
    pool: crate::db::Db,
    employee_id: web::Path<Uuid>,
    body: web::Json<CreateDocumentRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrStaffEdit).await?;
    let subject = access::subject(pool.get_ref(), org_id, *employee_id).await?;
    access::require_for(pool.get_ref(), &claims, Cap::HrStaffEdit, &subject).await?;

    let title = trimmed_required(&body.title, "Document title")?;
    let file_url = trimmed_required(&body.file_url, "File URL")?;
    let kind = blank_to_none(body.kind.clone()).unwrap_or_else(|| "other".to_string());

    let row = sqlx::query_as::<_, StaffDocument>(&format!(
        "INSERT INTO staff_documents (org_id, employee_id, kind, title, file_url, expires_on, uploaded_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING {DOCUMENT_COLS}"
    ))
    .bind(org_id)
    .bind(*employee_id)
    .bind(&kind)
    .bind(&title)
    .bind(&file_url)
    .bind(body.expires_on)
    .bind(claims.user_id_safe().ok())
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    delete, path = "/staff/documents/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Document ID")),
    responses((status = 204, description = "Document deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_document(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrStaffDelete).await?;
    let owner: Uuid =
        sqlx::query_scalar("SELECT employee_id FROM staff_documents WHERE id = $1 AND org_id = $2")
            .bind(*id)
            .bind(org_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Document not found".into()))?;
    let subject = access::subject(pool.get_ref(), org_id, owner).await?;
    access::require_for(pool.get_ref(), &claims, Cap::HrStaffDelete, &subject).await?;

    sqlx::query("DELETE FROM staff_documents WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}
