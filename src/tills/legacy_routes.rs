//! Legacy `/shifts/*` and `/tills` entity routes (TILLS_CONTRACT §2.6) as thin
//! adapters over the tills core. Mounted until cutover.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use super::handlers::{
    self as h, CashMovementRequest, CloseTillRequest, ForceCloseRequest, ListTillsQuery,
};
use super::legacy::{
    CloseShiftResponse, LegacyJoins, LegacyTill, OpenShiftRequest, PaginatedShifts, Shift,
    ShiftPreFill, ShiftReportResponse, legacy_shift, synthesized_till_id,
};
use crate::{
    auth::middleware::JwtMiddleware,
    devices::ClientHeader,
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
    realtime::hub::BranchEventHub,
    sync::ActingContext,
};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/shifts")
            .wrap(JwtMiddleware)
            .route(
                "/branches/{branch_id}/current",
                web::get().to(get_current_shift),
            )
            .route("/branches/{branch_id}/open", web::post().to(open_shift))
            .route("/branches/{branch_id}", web::get().to(list_shifts))
            .route("/{shift_id}/report", web::get().to(get_shift_report))
            .route(
                "/{shift_id}/cash-movements",
                web::post().to(add_cash_movement),
            )
            .route(
                "/{shift_id}/cash-movements",
                web::get().to(h::list_cash_movements),
            )
            .route("/{shift_id}/close", web::post().to(close_shift))
            .route("/{shift_id}/force-close", web::post().to(force_close_shift))
            .route("/{shift_id}", web::get().to(get_shift))
            .route("/{shift_id}", web::delete().to(h::delete_till)),
    );
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CurrentShiftQuery {
    /// Ignored (the drawer entity is gone).
    #[serde(default)]
    pub till_id: Option<Uuid>,
}

#[utoipa::path(get, path = "/shifts/branches/{branch_id}/current", tag = "shifts",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), CurrentShiftQuery),
    responses((status = 200, description = "DEPRECATED — use /tills/branches/{branch_id}/current", body = ShiftPreFill), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_current_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    _q: web::Query<CurrentShiftQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = h::extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "read").await?;
    h::require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
    let pre = h::current_till(pool.get_ref(), *branch_id, claims.user_id(), None).await?;
    // Whoever may see the branch's tills sees its open one when they hold none
    // themselves. Before architecture E this was "anyone but a teller", which
    // was the same set by accident, and hid it from an owner on a tablet.
    let sees_branch_tills =
        crate::authz::require::effective_for_claims(pool.get_ref(), &claims, Some(*branch_id))
            .await?
            .can(crate::authz::Cap::TillReadBranch);
    let open = match pre.open_till {
        Some(t) => Some(t),
        None if sees_branch_tills => sqlx::query_as::<_, h::Till>(&format!(
            "SELECT {} {} WHERE s.branch_id = $1 AND s.status = 'open' ORDER BY s.opened_at DESC LIMIT 1",
            h::TILL_COLUMNS,
            h::TILL_FROM
        ))
        .bind(*branch_id)
        .fetch_optional(pool.get_ref())
        .await?,
        None => None,
    };
    Ok(HttpResponse::Ok().json(ShiftPreFill {
        has_open_shift: open.is_some(),
        suggested_opening_cash: if open.is_some() {
            0
        } else {
            pre.suggested_opening_cash
        },
        open_shift: match open {
            Some(t) => Some(legacy_shift(pool.get_ref(), t, LegacyJoins::Till).await?),
            None => None,
        },
    }))
}

#[utoipa::path(post, path = "/shifts/branches/{branch_id}/open", tag = "shifts",
    params(("branch_id" = Uuid, Path, description = "Branch ID")), request_body = OpenShiftRequest,
    responses((status = 201, description = "DEPRECATED — use /tills/branches/{branch_id}/open", body = Shift), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn open_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    branch_id: web::Path<Uuid>,
    body: web::Json<OpenShiftRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = h::extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "create").await?;
    h::require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
    let res = h::open_till_inner(
        pool.get_ref(),
        hub.as_ref().map(|h| h.get_ref()),
        *branch_id,
        body.into_inner().into(),
        ActingContext::live(&claims)?.scoped(pool.get_ref()).await?,
        h::OpenMeta::default(),
    )
    .await;
    match res {
        Ok((till, true)) => Ok(HttpResponse::Created()
            .json(legacy_shift(pool.get_ref(), till, LegacyJoins::Till).await?)),
        Ok((till, false)) => {
            Ok(HttpResponse::Ok()
                .json(legacy_shift(pool.get_ref(), till, LegacyJoins::Till).await?))
        }
        Err(AppError::RefusedWith { code, .. }) => {
            Err(AppError::Conflict(if code == "TILL_OPEN_AT_OTHER_BRANCH" {
                crate::client_seen::legacy_hit_at(
                    crate::client_seen::KIND_ERROR_WORDING,
                    "open_shift_other_branch",
                );
                "You already have an open shift at another branch. Close it before opening a new one.".into()
            } else {
                crate::client_seen::legacy_hit_at(
                    crate::client_seen::KIND_ERROR_WORDING,
                    "open_shift_this_branch",
                );
                "You already have an open shift at this branch.".into()
            }))
        }
        Err(e) => Err(e),
    }
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListShiftsQuery {
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

#[utoipa::path(get, path = "/shifts/branches/{branch_id}", tag = "shifts",
    params(("branch_id" = Uuid, Path, description = "Branch ID (nil UUID = all branches)"), ListShiftsQuery),
    responses((status = 200, description = "DEPRECATED — use /tills/branches/{branch_id}", body = PaginatedShifts), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_shifts(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    q: web::Query<ListShiftsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = h::extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "read").await?;
    let query = ListTillsQuery {
        page: q.page,
        per_page: q.per_page,
        ..Default::default()
    };
    let p = h::list_tills_core(&req, pool.get_ref(), &claims, *branch_id, &query).await?;
    let mut data = Vec::with_capacity(p.data.len());
    for t in p.data {
        data.push(legacy_shift(pool.get_ref(), t, LegacyJoins::TillAndBranch).await?);
    }
    Ok(HttpResponse::Ok().json(PaginatedShifts {
        data,
        total: p.total,
        page: p.page,
        per_page: p.per_page,
        total_pages: p.total_pages,
    }))
}

#[utoipa::path(post, path = "/shifts/{shift_id}/force-close", tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Till ID")), request_body = ForceCloseRequest,
    responses((status = 200, description = "DEPRECATED — use /tills/{till_id}/force-close", body = Shift), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn force_close_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    id: web::Path<Uuid>,
    body: web::Json<ForceCloseRequest>,
    device: crate::devices::DeviceHeader,
) -> Result<HttpResponse, AppError> {
    let till_id = *id;
    let resp = h::force_close_till(req, pool.clone(), hub, id, body, device).await?;
    if !resp.status().is_success() {
        return Ok(resp);
    }
    let till = h::fetch_till_or_404(pool.get_ref(), till_id).await?;
    Ok(HttpResponse::Ok().json(legacy_shift(pool.get_ref(), till, LegacyJoins::None).await?))
}

#[utoipa::path(get, path = "/shifts/{shift_id}", tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Till ID")),
    responses((status = 200, description = "DEPRECATED — use /tills/{till_id}", body = Shift), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = h::extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "read").await?;
    let till = h::fetch_till_or_404(pool.get_ref(), *id).await?;
    h::require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    Ok(HttpResponse::Ok().json(legacy_shift(pool.get_ref(), till, LegacyJoins::Till).await?))
}

#[utoipa::path(get, path = "/shifts/{shift_id}/report", tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Till ID")),
    responses((status = 200, description = "DEPRECATED — use /tills/{till_id}/report", body = ShiftReportResponse), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_shift_report(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = h::extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "read").await?;
    let till = h::fetch_till_or_404(pool.get_ref(), *id).await?;
    h::require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    let figures = h::report_figures(pool.get_ref(), &till).await?;
    Ok(HttpResponse::Ok().json(ShiftReportResponse {
        shift: legacy_shift(pool.get_ref(), till, LegacyJoins::Till).await?,
        figures,
    }))
}

#[utoipa::path(post, path = "/shifts/{shift_id}/cash-movements", tag = "shifts", operation_id = "legacy_add_shift_cash_movement",
    params(("shift_id" = Uuid, Path, description = "Till ID")), request_body = CashMovementRequest,
    responses((status = 201, description = "DEPRECATED — use /tills/{till_id}/cash-movements", body = h::CashMovement), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn add_cash_movement(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    id: web::Path<Uuid>,
    body: web::Json<CashMovementRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = h::extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "update").await?;
    let till = h::fetch_till_or_404(pool.get_ref(), *id).await?;
    h::require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    h::add_cash_movement_inner(
        pool.get_ref(),
        hub.as_ref().map(|h| h.get_ref()),
        *id,
        body.into_inner(),
        ActingContext::live(&claims)?.scoped(pool.get_ref()).await?,
    )
    .await
    .map_err(legacy_error)
}

/// Old clients read prose, not codes: keep 400 bodies they used to get.
pub(crate) fn legacy_error(e: AppError) -> AppError {
    match e {
        AppError::Coded {
            status: 400,
            reason,
            ..
        } => {
            crate::client_seen::legacy_hit_at(
                crate::client_seen::KIND_ERROR_WORDING,
                "legacy_error_till_to_shift",
            );
            AppError::BadRequest(reason.replace("till", "shift"))
        }
        other => other,
    }
}

#[utoipa::path(post, path = "/shifts/{shift_id}/close", tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Till ID")), request_body = CloseTillRequest,
    responses((status = 200, description = "DEPRECATED — use /tills/{till_id}/close", body = CloseShiftResponse), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn close_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    id: web::Path<Uuid>,
    body: web::Json<CloseTillRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = h::extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "tills", "update").await?;
    let till = h::fetch_till_or_404(pool.get_ref(), *id).await?;
    h::require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    let already_closed = till.status != "open";
    let mut body = body.into_inner();
    body.reconciliation = None;
    let out = h::close_till_inner(
        pool.get_ref(),
        hub.as_ref().map(|h| h.get_ref()),
        *id,
        body,
        ActingContext::live(&claims)?.scoped(pool.get_ref()).await?,
    )
    .await?;
    Ok(HttpResponse::Ok().json(CloseShiftResponse {
        shift: legacy_shift(
            pool.get_ref(),
            out.till,
            if already_closed {
                LegacyJoins::Till
            } else {
                LegacyJoins::None
            },
        )
        .await?,
    }))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LegacyTillsQuery {
    pub branch_id: Option<Uuid>,
}

/// `GET /tills` — the removed entity list, synthesized (one "Till 1" per branch).
#[utoipa::path(get, path = "/tills", tag = "tills", params(LegacyTillsQuery),
    responses((status = 200, description = "DEPRECATED (POS < 0.7 only): one synthesized drawer per branch", body = Vec<LegacyTill>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn legacy_list_till_entities(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<LegacyTillsQuery>,
    client: ClientHeader,
) -> Result<HttpResponse, AppError> {
    if !client.is_legacy_pos() {
        return Err(AppError::NotFound("Not found".into()));
    }
    crate::client_seen::legacy_hit(crate::client_seen::KIND_TILLS_ENTITY);
    let claims = h::extract_claims(&req)?;
    let branch_id = match q.branch_id {
        Some(b) => b,
        None => claims
            .branch_id()
            .ok_or_else(|| AppError::BadRequest("branch_id is required".into()))?,
    };
    h::require_branch_access(pool.get_ref(), &claims, branch_id).await?;
    let row: Option<LegacyTill> = sqlx::query_as(
        "SELECT COALESCE((SELECT e.id FROM archive.till_entities e WHERE e.branch_id = b.id AND e.is_default \
                           AND e.deleted_at IS NULL ORDER BY e.created_at LIMIT 1), $2) AS id, \
                b.org_id, b.id AS branch_id, 'Till 1' AS name, true AS is_default, true AS is_active, \
                b.created_at, b.updated_at, b.standard_float \
         FROM branches b WHERE b.id = $1",
    )
    .bind(branch_id)
    .bind(synthesized_till_id(branch_id))
    .fetch_optional(pool.get_ref())
    .await
    .or_else(|_| Ok::<_, AppError>(None))?;
    let row = match row {
        Some(r) => Some(r),
        // `archive` not readable by the tenant role → synthesized id only.
        None => sqlx::query_as(
            "SELECT $2::uuid AS id, b.org_id, b.id AS branch_id, 'Till 1' AS name, true AS is_default, \
                    true AS is_active, b.created_at, b.updated_at, b.standard_float FROM branches b WHERE b.id = $1",
        )
        .bind(branch_id)
        .bind(synthesized_till_id(branch_id))
        .fetch_optional(pool.get_ref())
        .await?,
    };
    Ok(HttpResponse::Ok().json(row.into_iter().collect::<Vec<_>>()))
}

pub async fn till_entity_gone() -> Result<HttpResponse, AppError> {
    crate::client_seen::legacy_hit(crate::client_seen::KIND_TILLS_ENTITY_GONE);
    Err(AppError::Coded {
        status: 410,
        code: "TILL_ENTITY_REMOVED",
        reason: "Tills as drawers were removed; a till is now a person's sales session".into(),
    })
}

/// `DELETE /tills/{id}`: a sales session (T13) when the id is one, else the gone entity.
pub async fn delete_till_or_entity(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tills WHERE id = $1)")
        .bind(*id)
        .fetch_one(pool.get_ref())
        .await?;
    if exists {
        h::delete_till(req, pool, id).await
    } else {
        till_entity_gone().await
    }
}
