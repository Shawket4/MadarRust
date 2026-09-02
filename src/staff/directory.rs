//! The employee directory: departments, staff profiles, documents.
//!
//! Creating a `staff_profiles` row is what makes an existing user an employee —
//! there is no separate "create employee" flow, because an employee has to be
//! able to log in, and logging in is what `users` is for. Deleting the profile
//! demotes them back to a plain login without touching their account, their
//! orders, or their history.
//!
//! SALARY VISIBILITY: `base_salary_piastres` is nulled out for any caller who
//! lacks `payroll:read`. Branch managers get `staff:read` (they need the roster)
//! but not `payroll:read`, so they see who works for them without seeing what
//! anyone earns. The field is redacted in the *response*, not the query, so there
//! is exactly one place to get this wrong.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    errors::{AppError, AppErrorResponse},
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
    staff::{require_user_in_org, scope_org},
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
    pub user_id: Uuid,
    pub org_id: Uuid,
    /// From `users` — the employee's name IS their user name; there is no
    /// second copy to drift.
    pub name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    /// The POS role. Orthogonal to employment: a cleaner is a `teller`-role user
    /// with the POS permissions revoked.
    pub role: String,
    pub is_active: bool,
    pub department_id: Option<Uuid>,
    #[sqlx(default)]
    pub department_name: Option<String>,
    pub employee_code: Option<String>,
    pub job_title: Option<String>,
    pub hire_date: Option<NaiveDate>,
    pub termination_date: Option<NaiveDate>,
    pub employment_status: String,
    /// `None` when the caller lacks `payroll:read` — see the module docs.
    pub base_salary_piastres: Option<i64>,
    pub national_id: Option<String>,
    pub photo_url: Option<String>,
    pub emergency_contact_name: Option<String>,
    pub emergency_contact_phone: Option<String>,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Employee {
    /// Strip the salary unless the caller may see payroll. Called on every path
    /// that returns an `Employee`.
    fn redact_salary(mut self, may_see_salary: bool) -> Self {
        if !may_see_salary {
            self.base_salary_piastres = None;
        }
        self
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct StaffDocument {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub kind: String,
    pub title: String,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub file_url: Option<String>,
    pub expires_on: Option<NaiveDate>,
    pub uploaded_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

const DOCUMENT_COLS: &str =
    "id, org_id, user_id, kind, title, file_url, expires_on, uploaded_by, created_at";

// ── Requests ──────────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertDepartmentRequest {
    pub name: String,
    #[serde(default)]
    pub manager_user_id: Option<Uuid>,
}

/// Full replace of an employee's HR profile. A PUT rather than a POST because
/// the key is the user id: writing a profile for a user who has none promotes
/// them to staff, and writing it again edits them.
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PutEmployeeRequest {
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
    /// `active` | `suspended` | `terminated`. Defaults to `active`.
    #[serde(default)]
    pub employment_status: Option<String>,
    /// Piastres. Ignored unless the caller has `payroll:update` — a branch
    /// manager editing a job title must not be able to award a raise.
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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "read").await?;
    let org_id = scope_org(&req, &claims)?;

    let rows = sqlx::query_as::<_, Department>(
        r#"
        SELECT d.id, d.org_id, d.name, d.manager_user_id,
               m.name AS manager_name,
               COUNT(p.user_id) AS employee_count,
               d.created_at, d.updated_at
          FROM departments d
          LEFT JOIN users m ON m.id = d.manager_user_id AND m.deleted_at IS NULL
          LEFT JOIN staff_profiles p ON p.department_id = d.id
                                    AND p.employment_status <> 'terminated'
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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "create").await?;
    let org_id = scope_org(&req, &claims)?;

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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "update").await?;
    let org_id = scope_org(&req, &claims)?;

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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "delete").await?;
    let org_id = scope_org(&req, &claims)?;

    // Profiles point at departments with ON DELETE SET NULL, so deleting one
    // orphans rather than cascades. Refuse anyway while anyone is still in it:
    // silently unfiling twenty people is not what "delete department" means.
    let occupied: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM staff_profiles WHERE department_id = $1 AND org_id = $2",
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
const EMPLOYEE_COLS: &str = r#"
    p.user_id, p.org_id, u.name, u.email, u.phone, u.role::text AS role, u.is_active,
    p.department_id, d.name AS department_name, p.employee_code, p.job_title,
    p.hire_date, p.termination_date, p.employment_status, p.base_salary_piastres,
    p.national_id, p.photo_url, p.emergency_contact_name, p.emergency_contact_phone,
    p.notes, p.created_at, p.updated_at
"#;

#[utoipa::path(
    get, path = "/staff/employees", tag = "staff",
    params(EmployeeListQuery),
    responses((status = 200, description = "Employees in the org", body = Vec<Employee>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_employees(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<EmployeeListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "read").await?;
    let org_id = scope_org(&req, &claims)?;
    let may_see_salary = check_permission(pool.get_ref(), &claims, "payroll", "read")
        .await
        .is_ok();

    if let Some(status) = query.employment_status.as_deref() {
        validate_employment_status(status)?;
    }
    let search = query
        .search
        .as_deref()
        .map(|s| format!("%{}%", s.trim().to_lowercase()));

    let rows = sqlx::query_as::<_, Employee>(&format!(
        r#"
        SELECT {EMPLOYEE_COLS}
          FROM staff_profiles p
          JOIN users u ON u.id = p.user_id AND u.deleted_at IS NULL
          LEFT JOIN departments d ON d.id = p.department_id
         WHERE p.org_id = $1
           AND ($2::uuid IS NULL OR p.department_id = $2)
           AND ($3::text IS NULL OR p.employment_status = $3)
           AND ($4::text IS NULL
                OR lower(u.name) LIKE $4
                OR lower(COALESCE(p.employee_code, '')) LIKE $4
                OR lower(COALESCE(p.job_title, '')) LIKE $4)
         ORDER BY lower(u.name)
        "#
    ))
    .bind(org_id)
    .bind(query.department_id)
    .bind(query.employment_status.as_deref())
    .bind(search)
    .fetch_all(pool.get_ref())
    .await?;

    let rows: Vec<Employee> = rows
        .into_iter()
        .map(|e| e.redact_salary(may_see_salary))
        .collect();
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    get, path = "/staff/employees/{user_id}", tag = "staff",
    params(("user_id" = Uuid, Path, description = "The employee's user ID")),
    responses((status = 200, description = "The employee", body = Employee), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_employee(
    req: HttpRequest,
    pool: crate::db::Db,
    user_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "read").await?;
    let org_id = scope_org(&req, &claims)?;
    let may_see_salary = check_permission(pool.get_ref(), &claims, "payroll", "read")
        .await
        .is_ok();

    let row = load_employee(pool.get_ref(), org_id, *user_id).await?;
    Ok(HttpResponse::Ok().json(row.redact_salary(may_see_salary)))
}

pub(crate) async fn load_employee(
    pool: &sqlx::PgPool,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<Employee, AppError> {
    sqlx::query_as::<_, Employee>(&format!(
        r#"
        SELECT {EMPLOYEE_COLS}
          FROM staff_profiles p
          JOIN users u ON u.id = p.user_id AND u.deleted_at IS NULL
          LEFT JOIN departments d ON d.id = p.department_id
         WHERE p.user_id = $1 AND p.org_id = $2
        "#
    ))
    .bind(user_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Employee not found".into()))
}

#[utoipa::path(
    put, path = "/staff/employees/{user_id}", tag = "staff",
    params(("user_id" = Uuid, Path, description = "The employee's user ID")),
    request_body = PutEmployeeRequest,
    responses((status = 200, description = "Employee profile saved", body = Employee), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_employee(
    req: HttpRequest,
    pool: crate::db::Db,
    user_id: web::Path<Uuid>,
    body: web::Json<PutEmployeeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "update").await?;
    let org_id = scope_org(&req, &claims)?;
    require_user_in_org(pool.get_ref(), org_id, *user_id).await?;

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
        .fetch_one(pool.get_ref())
        .await?;
        if !ok {
            return Err(AppError::NotFound("Department not found".into()));
        }
    }

    // Salary is a payroll write, not a directory write. Without `payroll:update`
    // the submitted figure is ignored and the stored one is kept, so a branch
    // manager saving a job title cannot hand out a raise as a side effect.
    let may_set_salary = check_permission(pool.get_ref(), &claims, "payroll", "update")
        .await
        .is_ok();
    if body.base_salary_piastres.is_some_and(|s| s < 0) {
        return Err(AppError::BadRequest("Salary cannot be negative".into()));
    }
    let salary = if may_set_salary {
        body.base_salary_piastres
    } else {
        None
    };

    sqlx::query(
        r#"
        INSERT INTO staff_profiles (
            user_id, org_id, department_id, employee_code, job_title, hire_date,
            termination_date, employment_status, base_salary_piastres, national_id,
            photo_url, emergency_contact_name, emergency_contact_phone, notes
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, COALESCE($9, 0), $10, $11, $12, $13, $14)
        ON CONFLICT (user_id) DO UPDATE SET
            department_id           = EXCLUDED.department_id,
            employee_code           = EXCLUDED.employee_code,
            job_title               = EXCLUDED.job_title,
            hire_date               = EXCLUDED.hire_date,
            termination_date        = EXCLUDED.termination_date,
            employment_status       = EXCLUDED.employment_status,
            -- NULL here means "not permitted to change it", not "set to zero".
            base_salary_piastres    = COALESCE($9, staff_profiles.base_salary_piastres),
            national_id             = EXCLUDED.national_id,
            photo_url               = EXCLUDED.photo_url,
            emergency_contact_name  = EXCLUDED.emergency_contact_name,
            emergency_contact_phone = EXCLUDED.emergency_contact_phone,
            notes                   = EXCLUDED.notes,
            updated_at              = now()
        "#,
    )
    .bind(*user_id)
    .bind(org_id)
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
    .execute(pool.get_ref())
    .await?;

    let row = load_employee(pool.get_ref(), org_id, *user_id).await?;
    let may_see_salary = check_permission(pool.get_ref(), &claims, "payroll", "read")
        .await
        .is_ok();
    Ok(HttpResponse::Ok().json(row.redact_salary(may_see_salary)))
}

#[utoipa::path(
    delete, path = "/staff/employees/{user_id}", tag = "staff",
    params(("user_id" = Uuid, Path, description = "The employee's user ID")),
    responses((status = 204, description = "Profile removed; the user account is untouched"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_employee(
    req: HttpRequest,
    pool: crate::db::Db,
    user_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "delete").await?;
    let org_id = scope_org(&req, &claims)?;

    // Deliberately NOT a cascade to attendance/payroll: the ledger and any
    // generated payslips are records of what happened and outlive the profile.
    let deleted = sqlx::query("DELETE FROM staff_profiles WHERE user_id = $1 AND org_id = $2")
        .bind(*user_id)
        .bind(org_id)
        .execute(pool.get_ref())
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Employee not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}

// ── Documents ─────────────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/employees/{user_id}/documents", tag = "staff",
    params(("user_id" = Uuid, Path, description = "The employee's user ID")),
    responses((status = 200, description = "The employee's documents", body = Vec<StaffDocument>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_documents(
    req: HttpRequest,
    pool: crate::db::Db,
    user_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "read").await?;
    let org_id = scope_org(&req, &claims)?;

    let rows = sqlx::query_as::<_, StaffDocument>(&format!(
        "SELECT {DOCUMENT_COLS} FROM staff_documents \
         WHERE user_id = $1 AND org_id = $2 ORDER BY created_at DESC"
    ))
    .bind(*user_id)
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/staff/employees/{user_id}/documents", tag = "staff",
    params(("user_id" = Uuid, Path, description = "The employee's user ID")),
    request_body = CreateDocumentRequest,
    responses((status = 201, description = "Document attached", body = StaffDocument), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_document(
    req: HttpRequest,
    pool: crate::db::Db,
    user_id: web::Path<Uuid>,
    body: web::Json<CreateDocumentRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "update").await?;
    let org_id = scope_org(&req, &claims)?;
    require_user_in_org(pool.get_ref(), org_id, *user_id).await?;

    let title = trimmed_required(&body.title, "Document title")?;
    let file_url = trimmed_required(&body.file_url, "File URL")?;
    let kind = blank_to_none(body.kind.clone()).unwrap_or_else(|| "other".to_string());

    let row = sqlx::query_as::<_, StaffDocument>(&format!(
        "INSERT INTO staff_documents (org_id, user_id, kind, title, file_url, expires_on, uploaded_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING {DOCUMENT_COLS}"
    ))
    .bind(org_id)
    .bind(*user_id)
    .bind(&kind)
    .bind(&title)
    .bind(&file_url)
    .bind(body.expires_on)
    .bind(claims.user_id())
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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "staff", "delete").await?;
    let org_id = scope_org(&req, &claims)?;

    let deleted = sqlx::query("DELETE FROM staff_documents WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(pool.get_ref())
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Document not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}
