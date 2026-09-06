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
    /// The next active booking claiming this table (today's service, or the
    /// one in progress). The floor renders "held" from `held_from` by its own
    /// clock; nothing here is written to `status`. Only the list endpoint fills
    /// it — single-row writes return `null`.
    #[sqlx(skip)]
    #[serde(default)]
    pub next_booking: Option<TableBookingHint>,
}

/// The slice of a booking the floor needs to show a held/reserved table.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct TableBookingHint {
    pub booking_id: Uuid,
    /// `confirmed` | `seated`.
    pub status: String,
    pub guest_name: String,
    pub party_size: i32,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    /// `starts_at - hold_minutes`: from here the table reads as held.
    pub held_from: DateTime<Utc>,
}

/// Attach each table's next booking (the earliest active claim that has not
/// ended and starts within the next 24 hours).
pub(crate) async fn attach_next_bookings(
    pool: &PgPool,
    tables: &mut [FloorTable],
) -> Result<(), AppError> {
    if tables.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = tables.iter().map(|t| t.id).collect();
    /// `(table_id, booking_id, status, guest_name, party_size, starts_at, ends_at, hold_minutes)`.
    type HintRow = (
        Uuid,
        Uuid,
        String,
        String,
        i16,
        DateTime<Utc>,
        DateTime<Utc>,
        i16,
    );
    let rows: Vec<HintRow> = sqlx::query_as(
        "SELECT DISTINCT ON (bt.table_id) bt.table_id, b.id, b.status::text, b.guest_name, \
                b.party_size, b.starts_at, b.ends_at, COALESCE(s.hold_minutes, 15)::smallint \
         FROM booking_tables bt \
         JOIN bookings b ON b.id = bt.booking_id \
         LEFT JOIN branch_booking_settings s ON s.branch_id = b.branch_id \
         WHERE bt.table_id = ANY($1) AND bt.active \
           AND b.ends_at > now() AND b.starts_at < now() + interval '24 hours' \
         ORDER BY bt.table_id, b.starts_at",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    for (table_id, id, status, guest_name, party, starts_at, ends_at, hold) in rows {
        if let Some(t) = tables.iter_mut().find(|t| t.id == table_id) {
            t.next_booking = Some(TableBookingHint {
                booking_id: id,
                status,
                guest_name,
                party_size: party as i32,
                starts_at,
                ends_at,
                held_from: starts_at - chrono::Duration::minutes(hold as i64),
            });
        }
    }
    Ok(())
}

const TABLE_COLS: &str = "id, org_id, branch_id, section_id, label, seats, shape, \
     pos_x, pos_y, width, height, rotation, status, is_active, created_at, updated_at";

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
    /// Optimistic-concurrency token: the `updated_at` the client last saw for
    /// this table.
    ///
    /// The dashboard autosaves every gesture, so two managers arranging the
    /// same room no longer collide rarely and visibly -- they collide often and
    /// silently, each overwriting the other's last drag. When this is sent, the
    /// write only lands if the row has not moved since; otherwise the whole
    /// request is rejected and the caller is told exactly which tables changed.
    ///
    /// Optional so existing clients (and the POS) keep working unchanged: absent
    /// means "no guard", which is the previous last-write-wins behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_updated_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct SaveLayoutRequest {
    pub branch_id: Uuid,
    pub tables: Vec<TablePosition>,
}

fn validate_shape(shape: &str) -> Result<(), AppError> {
    match shape {
        "rect" | "circle" => Ok(()),
        _ => Err(AppError::BadRequest(
            "shape must be 'rect' or 'circle'".into(),
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
    pool: crate::db::Db,
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

/// Tell every device on the branch that the AUTHORED layout changed (sections
/// or table geometry), so POS canvases re-pull instead of showing yesterday's
/// room until the next manual sync. Lean signal — the client re-fetches
/// `/floor/sections` + `/floor/tables` itself.
/// Build the 409 for a layout save that lost a race.
///
/// Deliberately names the tables. "Someone else changed the layout" tells a
/// manager nothing they can act on -- they cannot tell whether their whole
/// arrangement is lost or one table moved a centimetre. Naming the labels lets
/// the client say which tables to look at, and reload just those.
///
/// Best-effort by construction: if the lookup itself fails we still return the
/// conflict, because reporting a vaguer 409 is right and reporting a 500 is not.
async fn stale_layout_error(
    pool: &sqlx::PgPool,
    branch_id: Uuid,
    stale: &[Uuid],
) -> crate::errors::AppError {
    let labels: Vec<String> = sqlx::query_scalar(
        "SELECT label FROM branch_tables \
         WHERE branch_id = $1 AND id = ANY($2) ORDER BY lower(label)",
    )
    .bind(branch_id)
    .bind(stale)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let named = if labels.is_empty() {
        format!("{} table(s)", stale.len())
    } else {
        labels.join(", ")
    };
    crate::errors::AppError::Conflict(format!(
        "The layout changed while you were editing: {named}. \
         Nothing was saved. Reload the floor to pick up the current positions, \
         then reapply your changes."
    ))
}

fn publish_layout_changed(hub: &crate::realtime::hub::BranchEventHub, branch_id: Uuid) {
    hub.publish(
        branch_id,
        crate::realtime::event::BranchEvent::new(
            crate::realtime::event::Topic::Floor,
            "floor.layout_changed",
            &serde_json::json!({ "branch_id": branch_id }),
        ),
    );
}

#[utoipa::path(
    post, path = "/floor/sections", tag = "reservations",
    request_body = CreateSectionRequest,
    responses((status = 201, description = "Section created", body = FloorSection), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_section(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<crate::realtime::hub::BranchEventHub>,
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
    publish_layout_changed(hub.get_ref(), body.branch_id);
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
    pool: crate::db::Db,
    hub: web::Data<crate::realtime::hub::BranchEventHub>,
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
    publish_layout_changed(hub.get_ref(), branch_id);
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
    pool: crate::db::Db,
    hub: web::Data<crate::realtime::hub::BranchEventHub>,
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
    publish_layout_changed(hub.get_ref(), branch_id);
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
    pool: crate::db::Db,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let mut rows = sqlx::query_as::<_, FloorTable>(&format!(
        "SELECT {TABLE_COLS} FROM branch_tables WHERE branch_id = $1 ORDER BY lower(label)"
    ))
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    attach_next_bookings(pool.get_ref(), &mut rows).await?;
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
    pool: crate::db::Db,
    hub: web::Data<crate::realtime::hub::BranchEventHub>,
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
    publish_layout_changed(hub.get_ref(), body.branch_id);
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
    pool: crate::db::Db,
    hub: web::Data<crate::realtime::hub::BranchEventHub>,
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
    publish_layout_changed(hub.get_ref(), existing);
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
    pool: crate::db::Db,
    hub: web::Data<crate::realtime::hub::BranchEventHub>,
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
    publish_layout_changed(hub.get_ref(), branch_id);
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
    pool: crate::db::Db,
    hub: web::Data<crate::realtime::hub::BranchEventHub>,
    body: web::Json<SaveLayoutRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "floor_plan", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    let mut tx = pool.get_ref().begin().await?;
    // Tables whose guard did not match. Collected rather than returned on the
    // first miss so the caller learns everything that moved under them in one
    // round trip, instead of rediscovering it one table at a time.
    let mut stale: Vec<Uuid> = Vec::new();
    for t in &body.tables {
        // Scoped to the branch so a forged id can't move another branch's table.
        // The `expected_updated_at` clause is what makes an autosaving client
        // safe: no match means someone else wrote this row first.
        let affected = sqlx::query(
            "UPDATE branch_tables SET \
                 section_id = $3, pos_x = $4, pos_y = $5, width = $6, height = $7, \
                 rotation = $8, updated_at = now() \
             WHERE id = $1 AND branch_id = $2 \
               AND ($9::timestamptz IS NULL OR updated_at = $9)",
        )
        .bind(t.id)
        .bind(body.branch_id)
        .bind(t.section_id)
        .bind(t.pos_x)
        .bind(t.pos_y)
        .bind(t.width)
        .bind(t.height)
        .bind(t.rotation)
        .bind(t.expected_updated_at)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        // Only a supplied guard can fail this way. Without one the clause is a
        // no-op, and zero rows just means the id is not this branch's -- which
        // this endpoint has always ignored rather than treated as an error.
        if affected == 0 && t.expected_updated_at.is_some() {
            stale.push(t.id);
        }
    }

    if !stale.is_empty() {
        // All or nothing. A partial layout save would leave the room in a state
        // neither manager arranged, which is worse than refusing.
        tx.rollback().await?;
        return Err(stale_layout_error(pool.get_ref(), body.branch_id, &stale).await);
    }
    tx.commit().await?;

    let rows = sqlx::query_as::<_, FloorTable>(&format!(
        "SELECT {TABLE_COLS} FROM branch_tables WHERE branch_id = $1 ORDER BY lower(label)"
    ))
    .bind(body.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    publish_layout_changed(hub.get_ref(), body.branch_id);
    Ok(HttpResponse::Ok().json(rows))
}

pub(crate) async fn fetch_table_branch(pool: &PgPool, table_id: Uuid) -> Result<Uuid, AppError> {
    sqlx::query_scalar("SELECT branch_id FROM branch_tables WHERE id = $1")
        .bind(table_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Table not found".into()))
}
