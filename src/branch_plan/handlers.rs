//! `GET /branch-plan`, `PUT /branch-plan`, `GET /branch-plan/versions`.
//!
//! Permission: the kitchen-setup cells (`kitchen_stations` read / update, i.e.
//! `kitchen.stations.edit`). A plan writes stations and category routing, which
//! that capability already governs; a capability of its own would need a
//! madar-shared release and is left for when devices and printers need a
//! separate grant.

use std::collections::{HashMap, HashSet};

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{BranchPlan, PlanDevice, PlanPrinter, PlanSection, check_plan, routing_mode_for};
use crate::branches::handlers::PrinterBrand;
use crate::delivery::require_branch_access;
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::permissions::checker::check_permission;
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

// ── Views ─────────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PlanQuery {
    pub branch_id: Uuid,
}

/// A registered install at the branch, for showing which slot it fills and
/// when it was last heard from.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct PlanRegisteredDevice {
    pub id: Uuid,
    pub code: String,
    pub label: Option<String>,
    /// `pos` | `kds` | `waiter`
    pub kind: String,
    pub platform: Option<String>,
    pub app_version: Option<String>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct PlanCategory {
    pub id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
}

/// A count per section: open kitchen items, or items routed there one by one.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct SectionCount {
    pub section_id: Uuid,
    pub count: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BranchPlanView {
    pub branch_id: Uuid,
    /// Bumped by every save; send it back as `expected_version`.
    pub version: i32,
    /// False until the plan is first saved. Until then it is assembled from the
    /// branch's existing stations, printers and registered devices.
    pub saved: bool,
    pub plan: BranchPlan,
    /// The branch's kitchen routing mode as stored now.
    pub routing_mode: String,
    pub devices: Vec<PlanRegisteredDevice>,
    pub categories: Vec<PlanCategory>,
    /// Kitchen items not yet bumped, per section. A section with any can't be
    /// removed (spec KS-9, CH-4).
    pub open_items: Vec<SectionCount>,
    /// Items routed to a section one by one, bypassing their category.
    pub item_overrides: Vec<SectionCount>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SavePlanRequest {
    pub branch_id: Uuid,
    /// The `version` the plan was loaded at. A save over a newer version is
    /// refused (`PLAN_CHANGED`), so two people editing one branch never
    /// silently overwrite each other.
    pub expected_version: i32,
    pub plan: BranchPlan,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct PlanVersionView {
    pub version: i32,
    pub saved_at: DateTime<Utc>,
    pub saved_by_name: Option<String>,
    #[schema(value_type = BranchPlan)]
    pub plan: sqlx::types::Json<BranchPlan>,
}

// ── Layout for pieces never placed ────────────────────────────

const COL_FRONT: f64 = 0.0;
const COL_SECTIONS: f64 = 340.0;
const COL_OUTPUTS: f64 = 680.0;
const ROW: f64 = 120.0;

fn stable_id(branch_id: Uuid, what: &str) -> Uuid {
    Uuid::new_v5(&branch_id, what.as_bytes())
}

// ── Reading ───────────────────────────────────────────────────

struct BranchRow {
    org_id: Uuid,
    version: i32,
    till_prints_kitchen: bool,
    routing_mode: Option<String>,
    printer_ip: Option<String>,
    printer_port: Option<i32>,
    printer_brand: Option<PrinterBrand>,
}

#[allow(clippy::type_complexity)]
async fn branch_row(pool: &PgPool, branch_id: Uuid) -> Result<BranchRow, AppError> {
    let row: Option<(
        Uuid,
        i32,
        bool,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<PrinterBrand>,
    )> = sqlx::query_as(
        "SELECT org_id, hardware_plan_version, till_prints_kitchen, kitchen_routing_mode::text,
                    host(printer_ip), printer_port, printer_brand
               FROM branches WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;
    let r = row.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    Ok(BranchRow {
        org_id: r.0,
        version: r.1,
        till_prints_kitchen: r.2,
        routing_mode: r.3,
        printer_ip: r.4,
        printer_port: r.5,
        printer_brand: r.6,
    })
}

#[derive(sqlx::FromRow)]
struct StationRow {
    id: Uuid,
    name: String,
    is_default: bool,
    printer_brand: Option<PrinterBrand>,
    printer_ip: Option<String>,
    printer_port: Option<i32>,
    canvas_x: Option<f64>,
    canvas_y: Option<f64>,
}

async fn category_routes(
    pool: &PgPool,
    branch_id: Uuid,
) -> Result<HashMap<Uuid, Vec<Uuid>>, AppError> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT r.station_id, r.category_id
           FROM category_station_routes r JOIN categories c ON c.id = r.category_id
          WHERE r.branch_id = $1 AND c.deleted_at IS NULL
          ORDER BY c.display_order NULLS LAST, lower(c.name)",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (station, cat) in rows {
        map.entry(station).or_default().push(cat);
    }
    Ok(map)
}

async fn registered_devices(
    pool: &PgPool,
    branch_id: Uuid,
) -> Result<Vec<PlanRegisteredDevice>, AppError> {
    Ok(sqlx::query_as::<_, PlanRegisteredDevice>(
        "SELECT id, code, label, kind, platform, app_version, last_seen_at
           FROM devices WHERE branch_id = $1 AND retired_at IS NULL
          ORDER BY kind, code",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?)
}

/// The plan as saved, or — for a branch never saved — assembled from what the
/// branch already has, so the builder opens on the real branch rather than an
/// empty canvas.
async fn load_plan(pool: &PgPool, branch_id: Uuid, b: &BranchRow) -> Result<BranchPlan, AppError> {
    let stations = sqlx::query_as::<_, StationRow>(
        "SELECT id, name, is_default, printer_brand, printer_ip, printer_port, canvas_x, canvas_y
           FROM kitchen_stations
          WHERE branch_id = $1 AND deleted_at IS NULL AND is_active
          ORDER BY sort_order, lower(name)",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?;
    let cats = category_routes(pool, branch_id).await?;

    if b.version > 0 {
        #[allow(clippy::type_complexity)]
        let devices: Vec<(Uuid, String, String, Option<Uuid>, Option<Uuid>, f64, f64)> =
            sqlx::query_as(
                "SELECT id, kind, name, receipt_printer_id, device_id, canvas_x, canvas_y
               FROM branch_device_slots WHERE branch_id = $1 ORDER BY sort_order, created_at, id",
            )
            .bind(branch_id)
            .fetch_all(pool)
            .await?;
        #[allow(clippy::type_complexity)]
        let printers: Vec<(Uuid, String, String, String, Option<PrinterBrand>, Option<String>, Option<i32>, i32, Option<Uuid>, f64, f64)> =
            sqlx::query_as(
                "SELECT id, role, name, connection, brand, ip, port, paper_mm, host_device_id, canvas_x, canvas_y
                   FROM branch_printers WHERE branch_id = $1 ORDER BY sort_order, created_at, id",
            )
            .bind(branch_id)
            .fetch_all(pool)
            .await?;
        let screens: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT s.station_id, s.slot_id FROM kitchen_station_screens s
               JOIN branch_device_slots d ON d.id = s.slot_id WHERE d.branch_id = $1
              ORDER BY d.sort_order, d.created_at, d.id",
        )
        .bind(branch_id)
        .fetch_all(pool)
        .await?;
        let station_printers: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT s.station_id, s.printer_id FROM kitchen_station_printers s
               JOIN branch_printers p ON p.id = s.printer_id WHERE p.branch_id = $1
              ORDER BY p.sort_order, p.created_at, p.id",
        )
        .bind(branch_id)
        .fetch_all(pool)
        .await?;

        let sections = stations
            .iter()
            .enumerate()
            .map(|(i, s)| PlanSection {
                id: s.id,
                name: s.name.clone(),
                is_default: s.is_default,
                category_ids: cats.get(&s.id).cloned().unwrap_or_default(),
                screen_ids: screens
                    .iter()
                    .filter(|(st, _)| *st == s.id)
                    .map(|(_, d)| *d)
                    .collect(),
                printer_ids: station_printers
                    .iter()
                    .filter(|(st, _)| *st == s.id)
                    .map(|(_, p)| *p)
                    .collect(),
                x: s.canvas_x.unwrap_or(COL_SECTIONS),
                y: s.canvas_y.unwrap_or(i as f64 * ROW * 1.5),
            })
            .collect();
        return Ok(BranchPlan {
            devices: devices
                .into_iter()
                .map(|d| PlanDevice {
                    id: d.0,
                    kind: d.1,
                    name: d.2,
                    receipt_printer_id: d.3,
                    device_id: d.4,
                    x: d.5,
                    y: d.6,
                })
                .collect(),
            printers: printers
                .into_iter()
                .map(|p| PlanPrinter {
                    id: p.0,
                    role: p.1,
                    name: p.2,
                    connection: p.3,
                    brand: p.4,
                    ip: p.5,
                    port: p.6,
                    paper_mm: p.7,
                    host_device_id: p.8,
                    x: p.9,
                    y: p.10,
                })
                .collect(),
            sections,
            till_prints_kitchen: b.till_prints_kitchen,
        });
    }

    // Never saved: assemble from the branch as it is.
    let registered = registered_devices(pool, branch_id).await?;
    let receipt_id = b
        .printer_ip
        .as_ref()
        .map(|_| stable_id(branch_id, "receipt-printer"));
    let mut devices = Vec::new();
    let (mut front_row, mut out_row) = (0.0, 0.0);
    for d in &registered {
        let kind = match d.kind.as_str() {
            "kds" => "kitchen",
            "waiter" => "waiter",
            _ => "pos",
        };
        let (x, y) = if kind == "kitchen" {
            out_row += 1.0;
            (COL_OUTPUTS, (out_row - 1.0) * ROW)
        } else {
            front_row += 1.0;
            (COL_FRONT, (front_row - 1.0) * ROW)
        };
        devices.push(PlanDevice {
            id: stable_id(branch_id, &format!("device:{}", d.id)),
            kind: kind.into(),
            name: d
                .label
                .clone()
                .filter(|l| !l.trim().is_empty())
                .unwrap_or_else(|| d.code.clone()),
            receipt_printer_id: if kind == "kitchen" { None } else { receipt_id },
            device_id: Some(d.id),
            x,
            y,
        });
    }
    let mut printers = Vec::new();
    if let (Some(id), Some(ip)) = (receipt_id, b.printer_ip.clone()) {
        printers.push(PlanPrinter {
            id,
            role: "receipt".into(),
            name: "Receipt printer".into(),
            connection: "network".into(),
            brand: b.printer_brand.clone(),
            ip: Some(ip),
            port: b.printer_port.or(Some(9100)),
            paper_mm: 80,
            host_device_id: None,
            x: COL_FRONT,
            y: front_row * ROW + ROW * 0.5,
        });
    }
    let mut sections = Vec::new();
    for (i, s) in stations.iter().enumerate() {
        let mut printer_ids = Vec::new();
        if let Some(ip) = s.printer_ip.clone().filter(|ip| !ip.trim().is_empty()) {
            let id = stable_id(branch_id, &format!("station-printer:{}", s.id));
            out_row += 1.0;
            printers.push(PlanPrinter {
                id,
                role: "kitchen".into(),
                name: format!("{} printer", s.name),
                connection: "network".into(),
                brand: s.printer_brand.clone(),
                ip: Some(ip),
                port: s.printer_port.or(Some(9100)),
                paper_mm: 80,
                host_device_id: None,
                x: COL_OUTPUTS,
                y: (out_row - 1.0) * ROW,
            });
            printer_ids.push(id);
        }
        sections.push(PlanSection {
            id: s.id,
            name: s.name.clone(),
            is_default: s.is_default,
            category_ids: cats.get(&s.id).cloned().unwrap_or_default(),
            screen_ids: vec![],
            printer_ids,
            x: s.canvas_x.unwrap_or(COL_SECTIONS),
            y: s.canvas_y.unwrap_or(i as f64 * ROW * 1.5),
        });
    }
    // A branch with sections but no default gets the first, which is where
    // unrouted items would have gone had one been set.
    if !sections.is_empty() && !sections.iter().any(|s| s.is_default) {
        sections[0].is_default = true;
    }
    Ok(BranchPlan {
        devices,
        printers,
        sections,
        till_prints_kitchen: matches!(
            b.routing_mode.as_deref(),
            Some("till") | Some("both") | None
        ),
    })
}

async fn plan_view(pool: &PgPool, branch_id: Uuid) -> Result<BranchPlanView, AppError> {
    let b = branch_row(pool, branch_id).await?;
    let plan = load_plan(pool, branch_id, &b).await?;
    let routing_mode = crate::kitchen::effective_routing_mode(pool, branch_id).await?;
    let categories = sqlx::query_as::<_, PlanCategory>(
        "SELECT id, name, name_translations FROM categories
          WHERE org_id = $1 AND deleted_at IS NULL AND is_active
          ORDER BY display_order NULLS LAST, lower(name)",
    )
    .bind(b.org_id)
    .fetch_all(pool)
    .await?;
    let open_items = sqlx::query_as::<_, SectionCount>(
        "SELECT kti.station_id AS section_id, count(*) AS count
           FROM kitchen_ticket_items kti JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id
          WHERE kt.branch_id = $1 AND kt.closed_at IS NULL AND kt.status = 'firing'
            AND kti.bumped_at IS NULL AND kti.voided_at IS NULL AND kti.station_id IS NOT NULL
          GROUP BY kti.station_id",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?;
    let item_overrides = sqlx::query_as::<_, SectionCount>(
        "SELECT station_id AS section_id, count(*) AS count
           FROM menu_item_station_routes WHERE branch_id = $1 GROUP BY station_id",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?;
    Ok(BranchPlanView {
        branch_id,
        version: b.version,
        saved: b.version > 0,
        plan,
        routing_mode,
        devices: registered_devices(pool, branch_id).await?,
        categories,
        open_items,
        item_overrides,
    })
}

#[utoipa::path(get, path = "/branch-plan", tag = "branch_plan", params(PlanQuery),
    responses((status = 200, body = BranchPlanView), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn get_plan(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<PlanQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    Ok(HttpResponse::Ok().json(plan_view(pool.get_ref(), query.branch_id).await?))
}

#[utoipa::path(get, path = "/branch-plan/versions", tag = "branch_plan", params(PlanQuery),
    responses((status = 200, description = "The last 20 saved plans, newest first", body = Vec<PlanVersionView>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_versions(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<PlanQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    let rows = sqlx::query_as::<_, PlanVersionView>(
        "SELECT v.version, v.saved_at, u.name AS saved_by_name, v.plan
           FROM branch_plan_versions v LEFT JOIN users u ON u.id = v.saved_by
          WHERE v.branch_id = $1 ORDER BY v.version DESC LIMIT 20",
    )
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

// ── Saving ────────────────────────────────────────────────────

#[utoipa::path(put, path = "/branch-plan", tag = "branch_plan", request_body = SavePlanRequest,
    responses(
        (status = 200, description = "Saved; the plan as now stored", body = BranchPlanView),
        (status = 409, description = "PLAN_CHANGED: saved by someone else since loading; SECTION_HAS_OPEN_TICKETS: a removed section still has items cooking"),
        AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn save_plan(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    body: web::Json<SavePlanRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    let SavePlanRequest {
        branch_id,
        expected_version,
        plan,
    } = body.into_inner();
    let problems = check_plan(&plan);
    if !problems.is_empty() {
        return Err(AppError::BadRequest(problems.join("; ")));
    }

    let mut tx = pool.get_ref().begin().await?;
    // Lock the branch row: the version check and the writes are one decision.
    let (org_id, version): (Uuid, i32) = sqlx::query_as(
        "SELECT org_id, hardware_plan_version FROM branches
          WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(branch_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    if version != expected_version {
        return Err(AppError::Refused {
            code: "PLAN_CHANGED",
            reason: "Someone saved this branch's plan while you were editing. Reload to see their version.".into(),
        });
    }

    check_references(&mut tx, org_id, branch_id, &plan).await?;
    remove_sections(&mut tx, branch_id, &plan).await?;
    write_pieces(&mut tx, org_id, branch_id, &plan).await?;
    write_sections(&mut tx, org_id, branch_id, &plan).await?;
    write_branch(&mut tx, branch_id, &plan).await?;

    sqlx::query(
        "INSERT INTO branch_plan_versions (org_id, branch_id, version, plan, saved_by)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(version + 1)
    .bind(sqlx::types::Json(&plan))
    .bind(claims.user_id_safe().ok())
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE branches SET hardware_plan_version = $2 WHERE id = $1")
        .bind(branch_id)
        .bind(version + 1)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    hub.publish(
        branch_id,
        BranchEvent::new(
            Topic::Kitchen,
            "branch.plan_changed",
            &serde_json::json!({ "branch_id": branch_id, "version": version + 1 }),
        ),
    );
    Ok(HttpResponse::Ok().json(plan_view(pool.get_ref(), branch_id).await?))
}

/// Categories and registered devices named by the plan must be this org's and
/// this branch's; a plan can't route another org's category or claim another
/// branch's tablet.
async fn check_references(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    branch_id: Uuid,
    plan: &BranchPlan,
) -> Result<(), AppError> {
    let cats: Vec<Uuid> = plan
        .sections
        .iter()
        .flat_map(|s| s.category_ids.iter().copied())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if !cats.is_empty() {
        let found: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM categories WHERE id = ANY($1) AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(&cats)
        .bind(org_id)
        .fetch_one(&mut **tx)
        .await?;
        if found != cats.len() as i64 {
            return Err(AppError::BadRequest(
                "The plan routes a category that doesn't exist".into(),
            ));
        }
    }
    let devs: Vec<Uuid> = plan.devices.iter().filter_map(|d| d.device_id).collect();
    if !devs.is_empty() {
        let found: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM devices WHERE id = ANY($1) AND branch_id = $2 AND retired_at IS NULL",
        )
        .bind(&devs)
        .bind(branch_id)
        .fetch_one(&mut **tx)
        .await?;
        if found != devs.len() as i64 {
            return Err(AppError::BadRequest(
                "The plan assigns a device that isn't registered at this branch".into(),
            ));
        }
    }
    Ok(())
}

/// Sections dropped from the plan are soft-deleted, with their routes, unless
/// they still have items cooking (KS-9): those orders would lose their screen.
async fn remove_sections(
    tx: &mut Transaction<'_, Postgres>,
    branch_id: Uuid,
    plan: &BranchPlan,
) -> Result<(), AppError> {
    let keep: Vec<Uuid> = plan.sections.iter().map(|s| s.id).collect();
    let busy: Vec<(String, i64)> = sqlx::query_as(
        "SELECT s.name, count(*) FROM kitchen_stations s
           JOIN kitchen_ticket_items kti ON kti.station_id = s.id
           JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id
          WHERE s.branch_id = $1 AND s.deleted_at IS NULL AND s.is_active AND NOT (s.id = ANY($2))
            AND kt.closed_at IS NULL AND kt.status = 'firing'
            AND kti.bumped_at IS NULL AND kti.voided_at IS NULL
          GROUP BY s.name ORDER BY s.name",
    )
    .bind(branch_id)
    .bind(&keep)
    .fetch_all(&mut **tx)
    .await?;
    if !busy.is_empty() {
        let list = busy
            .iter()
            .map(|(n, c)| format!("'{n}' ({c})"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(AppError::Refused {
            code: "SECTION_HAS_OPEN_TICKETS",
            reason: format!(
                "Items are still cooking in {list}. Finish or move them before removing the section."
            ),
        });
    }
    let removed: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE kitchen_stations SET deleted_at = now(), is_default = false, updated_at = now()
          WHERE branch_id = $1 AND deleted_at IS NULL AND is_active AND NOT (id = ANY($2))
          RETURNING id",
    )
    .bind(branch_id)
    .bind(&keep)
    .fetch_all(&mut **tx)
    .await?;
    if !removed.is_empty() {
        // Their item routes would point at a section that no longer exists;
        // those items fall back to their category, then the default section.
        sqlx::query(
            "DELETE FROM menu_item_station_routes WHERE branch_id = $1 AND station_id = ANY($2)",
        )
        .bind(branch_id)
        .bind(&removed)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Device slots and printers: delete what the plan dropped, upsert the rest.
/// Links between them are cleared first and written last, so the order of
/// inserts never trips a foreign key.
async fn write_pieces(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    branch_id: Uuid,
    plan: &BranchPlan,
) -> Result<(), AppError> {
    let slot_ids: Vec<Uuid> = plan.devices.iter().map(|d| d.id).collect();
    let printer_ids: Vec<Uuid> = plan.printers.iter().map(|p| p.id).collect();

    sqlx::query("UPDATE branch_device_slots SET device_id = NULL, receipt_printer_id = NULL WHERE branch_id = $1")
        .bind(branch_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE branch_printers SET host_device_id = NULL WHERE branch_id = $1")
        .bind(branch_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM branch_printers WHERE branch_id = $1 AND NOT (id = ANY($2))")
        .bind(branch_id)
        .bind(&printer_ids)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM branch_device_slots WHERE branch_id = $1 AND NOT (id = ANY($2))")
        .bind(branch_id)
        .bind(&slot_ids)
        .execute(&mut **tx)
        .await?;

    for (i, d) in plan.devices.iter().enumerate() {
        let n = sqlx::query(
            "INSERT INTO branch_device_slots (id, org_id, branch_id, kind, name, device_id, canvas_x, canvas_y, sort_order)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO UPDATE SET kind = EXCLUDED.kind, name = EXCLUDED.name,
                 device_id = EXCLUDED.device_id, sort_order = EXCLUDED.sort_order, canvas_x = EXCLUDED.canvas_x,
                 canvas_y = EXCLUDED.canvas_y, updated_at = now()
             WHERE branch_device_slots.branch_id = EXCLUDED.branch_id",
        )
        .bind(d.id)
        .bind(org_id)
        .bind(branch_id)
        .bind(&d.kind)
        .bind(d.name.trim())
        .bind(d.device_id)
        .bind(d.x)
        .bind(d.y)
        .bind(i as i32)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if n != 1 {
            return Err(AppError::BadRequest(format!(
                "Device {} belongs to another branch",
                d.id
            )));
        }
    }
    for (i, p) in plan.printers.iter().enumerate() {
        let network = p.connection == "network";
        let n = sqlx::query(
            "INSERT INTO branch_printers
                 (id, org_id, branch_id, role, name, connection, brand, ip, port, paper_mm,
                  host_device_id, canvas_x, canvas_y, sort_order)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (id) DO UPDATE SET role = EXCLUDED.role, name = EXCLUDED.name, sort_order = EXCLUDED.sort_order,
                 connection = EXCLUDED.connection, brand = EXCLUDED.brand, ip = EXCLUDED.ip,
                 port = EXCLUDED.port, paper_mm = EXCLUDED.paper_mm,
                 host_device_id = EXCLUDED.host_device_id, canvas_x = EXCLUDED.canvas_x,
                 canvas_y = EXCLUDED.canvas_y, updated_at = now()
             WHERE branch_printers.branch_id = EXCLUDED.branch_id",
        )
        .bind(p.id)
        .bind(org_id)
        .bind(branch_id)
        .bind(&p.role)
        .bind(p.name.trim())
        .bind(&p.connection)
        .bind(&p.brand)
        .bind(if network { p.ip.as_deref().map(str::trim) } else { None })
        .bind(if network { p.port.or(Some(9100)) } else { None })
        .bind(p.paper_mm)
        .bind(if network { None } else { p.host_device_id })
        .bind(p.x)
        .bind(p.y)
        .bind(i as i32)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if n != 1 {
            return Err(AppError::BadRequest(format!(
                "Printer {} belongs to another branch",
                p.id
            )));
        }
    }
    for d in plan
        .devices
        .iter()
        .filter(|d| d.receipt_printer_id.is_some())
    {
        sqlx::query("UPDATE branch_device_slots SET receipt_printer_id = $2 WHERE id = $1")
            .bind(d.id)
            .bind(d.receipt_printer_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Sections are `kitchen_stations`: upsert them, then their categories and
/// outputs. Names are parked on the row id first, so a plan that swaps two
/// names never trips the one-name-per-branch index halfway through.
async fn write_sections(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    branch_id: Uuid,
    plan: &BranchPlan,
) -> Result<(), AppError> {
    let ids: Vec<Uuid> = plan.sections.iter().map(|s| s.id).collect();
    sqlx::query(
        "UPDATE kitchen_stations
            SET is_default = false,
                name = CASE WHEN id = ANY($2) THEN id::text ELSE name END
          WHERE branch_id = $1 AND deleted_at IS NULL",
    )
    .bind(branch_id)
    .bind(&ids)
    .execute(&mut **tx)
    .await?;

    for (i, s) in plan.sections.iter().enumerate() {
        // A section restored from an earlier plan comes back from its soft delete.
        let n = sqlx::query(
            "INSERT INTO kitchen_stations
                 (id, org_id, branch_id, name, sort_order, is_default, is_active, canvas_x, canvas_y)
             VALUES ($1, $2, $3, $4, $5, $6, true, $7, $8)
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, sort_order = EXCLUDED.sort_order,
                 is_default = EXCLUDED.is_default, is_active = true, deleted_at = NULL,
                 canvas_x = EXCLUDED.canvas_x, canvas_y = EXCLUDED.canvas_y, updated_at = now()
             WHERE kitchen_stations.branch_id = EXCLUDED.branch_id",
        )
        .bind(s.id)
        .bind(org_id)
        .bind(branch_id)
        .bind(s.name.trim())
        .bind(i as i32)
        .bind(s.is_default)
        .bind(s.x)
        .bind(s.y)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if n != 1 {
            return Err(AppError::BadRequest(format!(
                "Section {} belongs to another branch",
                s.id
            )));
        }
    }

    sqlx::query(
        "DELETE FROM kitchen_station_screens WHERE station_id IN
            (SELECT id FROM kitchen_stations WHERE branch_id = $1)",
    )
    .bind(branch_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM kitchen_station_printers WHERE station_id IN
            (SELECT id FROM kitchen_stations WHERE branch_id = $1)",
    )
    .bind(branch_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM category_station_routes WHERE branch_id = $1")
        .bind(branch_id)
        .execute(&mut **tx)
        .await?;

    let printers: HashMap<Uuid, &PlanPrinter> = plan.printers.iter().map(|p| (p.id, p)).collect();
    for s in &plan.sections {
        for slot in &s.screen_ids {
            sqlx::query("INSERT INTO kitchen_station_screens (org_id, station_id, slot_id) VALUES ($1, $2, $3)")
                .bind(org_id)
                .bind(s.id)
                .bind(slot)
                .execute(&mut **tx)
                .await?;
        }
        for printer in &s.printer_ids {
            sqlx::query("INSERT INTO kitchen_station_printers (org_id, station_id, printer_id) VALUES ($1, $2, $3)")
                .bind(org_id)
                .bind(s.id)
                .bind(printer)
                .execute(&mut **tx)
                .await?;
        }
        for cat in &s.category_ids {
            sqlx::query(
                "INSERT INTO category_station_routes (branch_id, category_id, station_id) VALUES ($1, $2, $3)",
            )
            .bind(branch_id)
            .bind(cat)
            .bind(s.id)
            .execute(&mut **tx)
            .await?;
        }
        // Compatibility: the POS still prints a section's chits on the
        // station's own printer columns. Keep them on the section's first
        // network kitchen printer until it reads the plan (spec phase 2).
        let first = s
            .printer_ids
            .iter()
            .filter_map(|id| printers.get(id))
            .find(|p| p.connection == "network");
        sqlx::query(
            "UPDATE kitchen_stations SET printer_brand = $2, printer_ip = $3, printer_port = $4 WHERE id = $1",
        )
        .bind(s.id)
        .bind(first.and_then(|p| p.brand.clone()))
        .bind(first.and_then(|p| p.ip.as_deref().map(str::trim)))
        .bind(first.map(|p| p.port.unwrap_or(9100)))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// The branch row: routing mode and the case-1 switch from the plan, and — for
/// the POS that still reads it — the receipt printer of the first POS whose
/// receipts go to a network printer.
async fn write_branch(
    tx: &mut Transaction<'_, Postgres>,
    branch_id: Uuid,
    plan: &BranchPlan,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE branches SET kitchen_routing_mode = $2::kitchen_routing_mode, till_prints_kitchen = $3
          WHERE id = $1",
    )
    .bind(branch_id)
    .bind(routing_mode_for(plan))
    .bind(plan.till_prints_kitchen)
    .execute(&mut **tx)
    .await?;

    let receipt = plan
        .devices
        .iter()
        .filter(|d| d.kind == "pos")
        .filter_map(|d| d.receipt_printer_id)
        .filter_map(|id| plan.printers.iter().find(|p| p.id == id))
        .find(|p| p.connection == "network");
    if let Some(p) = receipt {
        sqlx::query(
            "UPDATE branches SET printer_ip = $2::inet, printer_port = $3, printer_brand = COALESCE($4, printer_brand)
              WHERE id = $1",
        )
        .bind(branch_id)
        .bind(p.ip.as_deref().map(str::trim))
        .bind(p.port.unwrap_or(9100))
        .bind(&p.brand)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}
