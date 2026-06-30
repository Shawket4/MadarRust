//! Floor plan: sections, table geometry, and per-branch reservation settings.
//!
//! Authoring (sections + geometry) is gated by the `floor_plan` permission —
//! managers, dashboard-only. The live table `status` (free/held/seated/dirty) is
//! a host op under `reservations`. Tables are the same `branch_tables` rows the
//! QR-card module uses; we just read/write the geometry columns added in
//! `20260630120000_reservations_floor.sql`.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    delivery::require_branch_access,
    errors::{AppError, AppErrorResponse},
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
    reservations::resolve_branch_org,
};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct FloorSection {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub name: String,
    pub ordering: i32,
    pub canvas_w: i32,
    pub canvas_h: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const SECTION_COLS: &str =
    "id, org_id, branch_id, name, ordering, canvas_w, canvas_h, created_at, updated_at";

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct FloorTable {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub section_id: Option<Uuid>,
    pub label: String,
    pub seats: i16,
    pub shape: String,
    pub pos_x: f64,
    pub pos_y: f64,
    pub width: f64,
    pub height: f64,
    pub rotation: f64,
    pub status: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const TABLE_COLS: &str = "id, org_id, branch_id, section_id, label, seats, shape, \
     pos_x, pos_y, width, height, rotation, status, is_active, created_at, updated_at";

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct ReservationSettings {
    pub branch_id: Uuid,
    pub accepting_reservations: bool,
    pub accepting_waitlist: bool,
    pub lead_minutes: i32,
    pub hold_lead_minutes: i32,
    pub grace_minutes: i32,
    pub max_party_size: Option<i32>,
    pub slot_minutes: i32,
    pub updated_at: DateTime<Utc>,
}

const SETTINGS_COLS: &str = "branch_id, accepting_reservations, accepting_waitlist, \
     lead_minutes, hold_lead_minutes, grace_minutes, max_party_size, slot_minutes, updated_at";

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchQuery {
    pub branch_id: Uuid,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateSectionRequest {
    pub branch_id: Uuid,
    pub name: String,
    #[serde(default)]
    pub ordering: Option<i32>,
    #[serde(default)]
    pub canvas_w: Option<i32>,
    #[serde(default)]
    pub canvas_h: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateSectionRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub ordering: Option<i32>,
    #[serde(default)]
    pub canvas_w: Option<i32>,
    #[serde(default)]
    pub canvas_h: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateFloorTableRequest {
    pub branch_id: Uuid,
    pub label: String,
    #[serde(default)]
    pub section_id: Option<Uuid>,
    #[serde(default)]
    pub seats: Option<i16>,
    #[serde(default)]
    pub shape: Option<String>,
    #[serde(default)]
    pub pos_x: Option<f64>,
    #[serde(default)]
    pub pos_y: Option<f64>,
    #[serde(default)]
    pub width: Option<f64>,
    #[serde(default)]
    pub height: Option<f64>,
    #[serde(default)]
    pub rotation: Option<f64>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateFloorTableRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub section_id: Option<Uuid>,
    #[serde(default)]
    pub seats: Option<i16>,
    #[serde(default)]
    pub shape: Option<String>,
    #[serde(default)]
    pub pos_x: Option<f64>,
    #[serde(default)]
    pub pos_y: Option<f64>,
    #[serde(default)]
    pub width: Option<f64>,
    #[serde(default)]
    pub height: Option<f64>,
    #[serde(default)]
    pub rotation: Option<f64>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

/// One table's geometry in a bulk drag-save. `section_id` lets a drag move a
/// table between sections in the same save.
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct TablePosition {
    pub id: Uuid,
    #[serde(default)]
    pub section_id: Option<Uuid>,
    pub pos_x: f64,
    pub pos_y: f64,
    pub width: f64,
    pub height: f64,
    pub rotation: f64,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct SaveLayoutRequest {
    pub branch_id: Uuid,
    pub tables: Vec<TablePosition>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateSettingsRequest {
    #[serde(default)]
    pub accepting_reservations: Option<bool>,
    #[serde(default)]
    pub accepting_waitlist: Option<bool>,
    #[serde(default)]
    pub lead_minutes: Option<i32>,
    #[serde(default)]
    pub hold_lead_minutes: Option<i32>,
    #[serde(default)]
    pub grace_minutes: Option<i32>,
    #[serde(default)]
    pub max_party_size: Option<i32>,
    #[serde(default)]
    pub slot_minutes: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct SetTableStatusRequest {
    /// One of `free`, `held`, `seated`, `dirty`.
    pub status: String,
}

fn validate_shape(shape: &str) -> Result<(), AppError> {
    match shape {
        "rect" | "circle" => Ok(()),
        _ => Err(AppError::BadRequest(
            "shape must be 'rect' or 'circle'".into(),
        )),
    }
}

fn validate_table_status(status: &str) -> Result<(), AppError> {
    match status {
        "free" | "held" | "seated" | "dirty" => Ok(()),
        _ => Err(AppError::BadRequest(
            "status must be one of free, held, seated, dirty".into(),
        )),
    }
}

// ── Sections ──────────────────────────────────────────────────

#[utoipa::path(
    get, path = "/floor/sections", tag = "reservations",
    params(BranchQuery),
    responses((status = 200, description = "Sections for the branch", body = Vec<FloorSection>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_sections(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let rows = sqlx::query_as::<_, FloorSection>(&format!(
        "SELECT {SECTION_COLS} FROM floor_sections WHERE branch_id = $1 ORDER BY ordering, lower(name)"
    ))
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/floor/sections", tag = "reservations",
    request_body = CreateSectionRequest,
    responses((status = 201, description = "Section created", body = FloorSection), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_section(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateSectionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Section name is required".into()));
    }
    let org_id = resolve_branch_org(pool.get_ref(), body.branch_id).await?;

    let row = sqlx::query_as::<_, FloorSection>(&format!(
        "INSERT INTO floor_sections (org_id, branch_id, name, ordering, canvas_w, canvas_h) \
         VALUES ($1, $2, $3, COALESCE($4, 0), COALESCE($5, 1000), COALESCE($6, 700)) \
         RETURNING {SECTION_COLS}"
    ))
    .bind(org_id)
    .bind(body.branch_id)
    .bind(name)
    .bind(body.ordering)
    .bind(body.canvas_w)
    .bind(body.canvas_h)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    patch, path = "/floor/sections/{id}", tag = "reservations",
    params(("id" = Uuid, Path, description = "Section ID")),
    request_body = UpdateSectionRequest,
    responses((status = 200, description = "Section updated", body = FloorSection), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_section(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<UpdateSectionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "update").await?;

    let branch_id: Uuid = sqlx::query_scalar("SELECT branch_id FROM floor_sections WHERE id = $1")
        .bind(*id)
        .fetch_optional(pool.get_ref())
        .await?
        .ok_or_else(|| AppError::NotFound("Section not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if body.name.as_deref().is_some_and(|s| s.trim().is_empty()) {
        return Err(AppError::BadRequest("Section name cannot be empty".into()));
    }

    let row = sqlx::query_as::<_, FloorSection>(&format!(
        "UPDATE floor_sections SET \
             name = COALESCE($2, name), ordering = COALESCE($3, ordering), \
             canvas_w = COALESCE($4, canvas_w), canvas_h = COALESCE($5, canvas_h), \
             updated_at = now() \
         WHERE id = $1 RETURNING {SECTION_COLS}"
    ))
    .bind(*id)
    .bind(name)
    .bind(body.ordering)
    .bind(body.canvas_w)
    .bind(body.canvas_h)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete, path = "/floor/sections/{id}", tag = "reservations",
    params(("id" = Uuid, Path, description = "Section ID")),
    responses((status = 204, description = "Section deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_section(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "delete").await?;

    let branch_id: Uuid = sqlx::query_scalar("SELECT branch_id FROM floor_sections WHERE id = $1")
        .bind(*id)
        .fetch_optional(pool.get_ref())
        .await?
        .ok_or_else(|| AppError::NotFound("Section not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    // Tables in the section keep existing (section_id → NULL via FK) so we never
    // orphan an occupied table by deleting its section.
    sqlx::query("DELETE FROM floor_sections WHERE id = $1")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Tables (geometry) ─────────────────────────────────────────

#[utoipa::path(
    get, path = "/floor/tables", tag = "reservations", operation_id = "list_floor_tables",
    params(BranchQuery),
    responses((status = 200, description = "Tables for the branch", body = Vec<FloorTable>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_tables(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let rows = sqlx::query_as::<_, FloorTable>(&format!(
        "SELECT {TABLE_COLS} FROM branch_tables WHERE branch_id = $1 ORDER BY lower(label)"
    ))
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/floor/tables", tag = "reservations", operation_id = "create_floor_table",
    request_body = CreateFloorTableRequest,
    responses((status = 201, description = "Table created", body = FloorTable), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_table(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateFloorTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    let label = body.label.trim();
    if label.is_empty() {
        return Err(AppError::BadRequest("Table label is required".into()));
    }
    let shape = body.shape.as_deref().unwrap_or("rect");
    validate_shape(shape)?;
    let org_id = resolve_branch_org(pool.get_ref(), body.branch_id).await?;

    let row = sqlx::query_as::<_, FloorTable>(&format!(
        "INSERT INTO branch_tables \
             (org_id, branch_id, section_id, label, seats, shape, pos_x, pos_y, width, height, rotation) \
         VALUES ($1, $2, $3, $4, COALESCE($5, 2), $6, \
                 COALESCE($7, 0), COALESCE($8, 0), COALESCE($9, 80), COALESCE($10, 80), COALESCE($11, 0)) \
         RETURNING {TABLE_COLS}"
    ))
    .bind(org_id)
    .bind(body.branch_id)
    .bind(body.section_id)
    .bind(label)
    .bind(body.seats)
    .bind(shape)
    .bind(body.pos_x)
    .bind(body.pos_y)
    .bind(body.width)
    .bind(body.height)
    .bind(body.rotation)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    patch, path = "/floor/tables/{id}", tag = "reservations", operation_id = "update_floor_table",
    params(("id" = Uuid, Path, description = "Table ID")),
    request_body = UpdateFloorTableRequest,
    responses((status = 200, description = "Table updated", body = FloorTable), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_table(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<UpdateFloorTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "update").await?;

    let existing = fetch_table_branch(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, existing).await?;

    if let Some(s) = body.shape.as_deref() {
        validate_shape(s)?;
    }
    let label = body
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if body.label.as_deref().is_some_and(|s| s.trim().is_empty()) {
        return Err(AppError::BadRequest("Table label cannot be empty".into()));
    }

    let row = sqlx::query_as::<_, FloorTable>(&format!(
        "UPDATE branch_tables SET \
             label = COALESCE($2, label), section_id = COALESCE($3, section_id), \
             seats = COALESCE($4, seats), shape = COALESCE($5, shape), \
             pos_x = COALESCE($6, pos_x), pos_y = COALESCE($7, pos_y), \
             width = COALESCE($8, width), height = COALESCE($9, height), \
             rotation = COALESCE($10, rotation), is_active = COALESCE($11, is_active), \
             updated_at = now() \
         WHERE id = $1 RETURNING {TABLE_COLS}"
    ))
    .bind(*id)
    .bind(label)
    .bind(body.section_id)
    .bind(body.seats)
    .bind(body.shape.as_deref())
    .bind(body.pos_x)
    .bind(body.pos_y)
    .bind(body.width)
    .bind(body.height)
    .bind(body.rotation)
    .bind(body.is_active)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete, path = "/floor/tables/{id}", tag = "reservations", operation_id = "delete_floor_table",
    params(("id" = Uuid, Path, description = "Table ID")),
    responses((status = 204, description = "Table deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_table(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "delete").await?;

    let branch_id = fetch_table_branch(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    // A table backing a live open ticket can't be retired — settle/move it first.
    let has_open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM open_tickets WHERE table_id = $1 AND status IN ('open','ready'))",
    )
    .bind(*id)
    .fetch_one(pool.get_ref())
    .await?;
    if has_open {
        return Err(AppError::Conflict(
            "Cannot delete a table with a live open ticket — settle or move it first.".into(),
        ));
    }

    sqlx::query("DELETE FROM branch_tables WHERE id = $1")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    put, path = "/floor/layout", tag = "reservations",
    request_body = SaveLayoutRequest,
    responses((status = 200, description = "Layout saved", body = Vec<FloorTable>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn save_layout(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<SaveLayoutRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    let mut tx = pool.get_ref().begin().await?;
    for t in &body.tables {
        // Scoped to the branch so a forged id can't move another branch's table.
        sqlx::query(
            "UPDATE branch_tables SET \
                 section_id = $3, pos_x = $4, pos_y = $5, width = $6, height = $7, \
                 rotation = $8, updated_at = now() \
             WHERE id = $1 AND branch_id = $2",
        )
        .bind(t.id)
        .bind(body.branch_id)
        .bind(t.section_id)
        .bind(t.pos_x)
        .bind(t.pos_y)
        .bind(t.width)
        .bind(t.height)
        .bind(t.rotation)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let rows = sqlx::query_as::<_, FloorTable>(&format!(
        "SELECT {TABLE_COLS} FROM branch_tables WHERE branch_id = $1 ORDER BY lower(label)"
    ))
    .bind(body.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    patch, path = "/floor/tables/{id}/status", tag = "reservations",
    params(("id" = Uuid, Path, description = "Table ID")),
    request_body = SetTableStatusRequest,
    responses((status = 200, description = "Table status set", body = FloorTable), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn set_table_status(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<crate::realtime::hub::BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<SetTableStatusRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Live status is an operational host action, not geometry authoring.
    check_permission(pool.get_ref(), &claims, "reservations", "update").await?;

    let branch_id = fetch_table_branch(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;
    validate_table_status(&body.status)?;

    let row = sqlx::query_as::<_, FloorTable>(&format!(
        "UPDATE branch_tables SET status = $2, updated_at = now() WHERE id = $1 RETURNING {TABLE_COLS}"
    ))
    .bind(*id)
    .bind(&body.status)
    .fetch_one(pool.get_ref())
    .await?;

    hub.publish(
        branch_id,
        crate::realtime::event::BranchEvent::new(
            crate::realtime::event::Topic::Reservations,
            "table.status_changed",
            &row,
        ),
    );
    Ok(HttpResponse::Ok().json(row))
}

// ── Reservation settings ──────────────────────────────────────

#[utoipa::path(
    get, path = "/floor/reservation-settings", tag = "reservations",
    params(BranchQuery),
    responses((status = 200, description = "Effective settings (defaults if unset)", body = ReservationSettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_reservation_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "reservations", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let settings = load_settings(pool.get_ref(), query.branch_id).await?;
    Ok(HttpResponse::Ok().json(settings))
}

#[utoipa::path(
    put, path = "/floor/reservation-settings", tag = "reservations",
    params(BranchQuery),
    request_body = UpdateSettingsRequest,
    responses((status = 200, description = "Settings saved", body = ReservationSettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_reservation_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<BranchQuery>,
    body: web::Json<UpdateSettingsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "reservations", "update").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    // Upsert: seed defaults on first write, then COALESCE the partial update.
    let row = sqlx::query_as::<_, ReservationSettings>(&format!(
        "INSERT INTO branch_reservation_settings (branch_id) VALUES ($1) \
         ON CONFLICT (branch_id) DO UPDATE SET \
             accepting_reservations = COALESCE($2, branch_reservation_settings.accepting_reservations), \
             accepting_waitlist     = COALESCE($3, branch_reservation_settings.accepting_waitlist), \
             lead_minutes           = COALESCE($4, branch_reservation_settings.lead_minutes), \
             hold_lead_minutes      = COALESCE($5, branch_reservation_settings.hold_lead_minutes), \
             grace_minutes          = COALESCE($6, branch_reservation_settings.grace_minutes), \
             max_party_size         = COALESCE($7, branch_reservation_settings.max_party_size), \
             slot_minutes           = COALESCE($8, branch_reservation_settings.slot_minutes), \
             updated_at             = now() \
         RETURNING {SETTINGS_COLS}"
    ))
    .bind(query.branch_id)
    .bind(body.accepting_reservations)
    .bind(body.accepting_waitlist)
    .bind(body.lead_minutes)
    .bind(body.hold_lead_minutes)
    .bind(body.grace_minutes)
    .bind(body.max_party_size)
    .bind(body.slot_minutes)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── Shared helpers ────────────────────────────────────────────

pub(crate) async fn fetch_table_branch(pool: &PgPool, table_id: Uuid) -> Result<Uuid, AppError> {
    sqlx::query_scalar("SELECT branch_id FROM branch_tables WHERE id = $1")
        .bind(table_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Table not found".into()))
}

/// Load a branch's reservation settings, falling back to schema defaults when no
/// row exists yet (so the scheduler and reads never NULL-out on a fresh branch).
pub(crate) async fn load_settings(
    pool: &PgPool,
    branch_id: Uuid,
) -> Result<ReservationSettings, AppError> {
    if let Some(row) = sqlx::query_as::<_, ReservationSettings>(&format!(
        "SELECT {SETTINGS_COLS} FROM branch_reservation_settings WHERE branch_id = $1"
    ))
    .bind(branch_id)
    .fetch_optional(pool)
    .await?
    {
        return Ok(row);
    }
    Ok(ReservationSettings {
        branch_id,
        accepting_reservations: false,
        accepting_waitlist: false,
        lead_minutes: 30,
        hold_lead_minutes: 120,
        grace_minutes: 15,
        max_party_size: None,
        slot_minutes: 15,
        updated_at: Utc::now(),
    })
}
