//! `/metrics/*` — the metrics API behind dashboards and custom widgets.
//!
//! Two endpoints, deliberately:
//!
//!   * `GET  /metrics/schema` — the whole semantic layer, filtered to what the
//!     caller may see. This is what a widget picker is built from.
//!   * `POST /metrics/query` — run up to [`MAX_WIDGETS`] metrics in one round
//!     trip and get a per-widget result or a per-widget error.
//!
//! Batching is the reason this is not one endpoint per metric: a dashboard
//! renders a dozen widgets at once, and a dozen HTTP requests against a
//! one-vCPU box with a co-resident Postgres is the difference between a snappy
//! board and a stalled one. Per-widget errors matter for the same reason — one
//! bad widget must not blank the whole dashboard.

use std::collections::BTreeMap;

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    auth::jwt::Claims,
    db::Db,
    errors::{AppError, AppErrorResponse},
    observability::report,
    permissions::checker::check_permission,
};

use super::{
    compile::{CompileCtx, compile},
    execute::{self, ExecCtx},
    presets,
    registry::{self, RegistryInfo},
    scope,
    spec::{Period, QuerySpec},
    types::{Column, Grain, Viz},
};

/// Most widgets one request may ask for. A dashboard beyond this is a design
/// problem, not a batching problem.
pub const MAX_WIDGETS: usize = 24;
/// How many widget queries run at once. Tuned for the production box (1 vCPU
/// with Postgres co-resident): more concurrency here just queues in the
/// database while holding tenant-pool connections.
pub const QUERY_CONCURRENCY: usize = 4;

// ── Request/response shapes ──────────────────────────────────────────────────

/// One widget in a batch. Exactly one of `preset` or `spec`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct WidgetRequest {
    /// Caller-chosen key, echoed back so results can be matched to widgets.
    pub key: String,
    /// A curated metric id from `GET /metrics/schema`.
    #[serde(default)]
    pub preset: Option<String>,
    /// A fully custom query. Same IR the AI agent produces.
    #[serde(default)]
    pub spec: Option<QuerySpec>,
    /// Overrides the batch-level period for this widget alone.
    #[serde(default)]
    pub period: Option<Period>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct MetricsQueryRequest {
    /// Default window for every widget that does not set its own.
    #[serde(default)]
    pub period: Option<Period>,
    /// Answer language for translated labels ("en" or "ar").
    #[serde(default)]
    pub locale: Option<String>,
    pub widgets: Vec<WidgetRequest>,
}

/// The resolved window, echoed so a client can label an answer without
/// re-deriving "last month" itself.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PeriodInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MetricResult {
    pub columns: Vec<Column>,
    pub rows: Vec<serde_json::Map<String, serde_json::Value>>,
    pub row_count: usize,
    pub truncated: bool,
    pub grain: Grain,
    pub viz: Viz,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facet_by: Option<String>,
    pub period: PeriodInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// A widget either produced a result or an explanation. Never both, never
/// neither — and a failure here is a 200 with an `error`, not a failed batch.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WidgetOutcome {
    Ok(Box<MetricResult>),
    Error { error: String },
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MetricsQueryResponse {
    /// Which branches every result covers.
    pub scope: scope::ScopeInfo,
    /// The timezone all time buckets were computed in.
    pub timezone: String,
    pub results: BTreeMap<String, WidgetOutcome>,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

fn claims_of(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// Normalize a requested locale to one we hold translations for.
pub fn normalize_locale(requested: Option<&str>) -> String {
    match requested.map(str::trim) {
        Some("ar") => "ar".to_string(),
        _ => "en".to_string(),
    }
}

/// Every permission resource any preset requires, so the registry endpoint can
/// resolve them all in one pass instead of per preset.
fn required_resources() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = presets::PRESETS.iter().map(|p| p.permission).collect();
    v.sort_unstable();
    v.dedup();
    v
}

#[utoipa::path(
    get,
    path = "/metrics/schema",
    tag = "metrics",
    responses(
        (status = 200, description = "Datasets, measures, curated metrics and default boards", body = RegistryInfo),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn schema(req: HttpRequest, db: Db) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    check_permission(db.get_ref(), &claims, "reports", "read").await?;

    // Resolve every distinct permission once, then hand the registry a cheap
    // lookup. A metric the caller cannot read is never offered in the picker,
    // so the UI cannot construct a request that would 403.
    let mut granted: Vec<&'static str> = Vec::new();
    for resource in required_resources() {
        if check_permission(db.get_ref(), &claims, resource, "read")
            .await
            .is_ok()
        {
            granted.push(resource);
        }
    }
    Ok(HttpResponse::Ok().json(registry::registry(&|p| granted.contains(&p))))
}

#[utoipa::path(
    post,
    path = "/metrics/query",
    // Named explicitly, because utoipa defaults the operation id to the handler
    // name and the client generator turns that into a hook name. `query` here
    // becomes `useQuery`, which collides with React Query's own `useQuery` in
    // the generated module and fails the dashboard build outright.
    operation_id = "run_metrics_query",
    tag = "metrics",
    request_body = MetricsQueryRequest,
    responses(
        (status = 200, description = "One result (or one error) per requested widget", body = MetricsQueryResponse),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn query(
    req: HttpRequest,
    db: Db,
    body: web::Json<MetricsQueryRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    check_permission(db.get_ref(), &claims, "reports", "read").await?;

    if body.widgets.is_empty() {
        return Err(AppError::BadRequest("no widgets requested".into()));
    }
    if body.widgets.len() > MAX_WIDGETS {
        return Err(AppError::BadRequest(format!(
            "too many widgets in one request (max {MAX_WIDGETS})"
        )));
    }

    let locale = normalize_locale(body.locale.as_deref());
    let clock = scope::org_clock(&db).await?;
    let accessible = scope::accessible_branches(&db, &claims).await?;
    let (branch_ids, scope_info) = scope::resolve(&accessible, None, scope::header_branch_id(&req));

    let ctx = CompileCtx {
        tz: clock.tz,
        now: chrono::Utc::now(),
    };

    // Run the batch with bounded concurrency. Each widget is independent, so a
    // failure is recorded against that key and the rest still render.
    let results: Vec<(String, WidgetOutcome)> = stream::iter(body.widgets.iter())
        .map(|w| {
            let db = &db;
            let claims = &claims;
            let ctx = &ctx;
            let locale = locale.as_str();
            let tz = clock.timezone.as_str();
            let branch_ids = branch_ids.as_slice();
            let batch_period = body.period.clone();
            async move {
                let outcome =
                    run_widget(db, claims, w, batch_period, ctx, branch_ids, locale, tz).await;
                (
                    w.key.clone(),
                    match outcome {
                        Ok(r) => WidgetOutcome::Ok(Box::new(r)),
                        Err(e) => WidgetOutcome::Error { error: e },
                    },
                )
            }
        })
        .buffer_unordered(QUERY_CONCURRENCY)
        .collect()
        .await;

    // One event for a round where EVERYTHING failed — the shape of "the
    // database is down", which forty per-widget issues would describe no
    // better. Partial failure is normal and is already visible per widget.
    let failed = results
        .iter()
        .filter(|(_, o)| matches!(o, WidgetOutcome::Error { .. }))
        .count();
    report::report_round("metrics", "query_batch", results.len(), failed);

    Ok(HttpResponse::Ok().json(MetricsQueryResponse {
        scope: scope_info,
        timezone: clock.timezone.clone(),
        results: results.into_iter().collect(),
    }))
}

/// Resolve one widget to a spec, compile it, and run it. The error type is a
/// plain string because it is per-widget UI text, not an HTTP status.
#[allow(clippy::too_many_arguments)]
async fn run_widget(
    db: &Db,
    claims: &Claims,
    w: &WidgetRequest,
    batch_period: Option<Period>,
    ctx: &CompileCtx,
    branch_ids: &[uuid::Uuid],
    locale: &str,
    tz: &str,
) -> Result<MetricResult, String> {
    let period = w.period.clone().or(batch_period);

    let (spec, title) = match (&w.preset, &w.spec) {
        (Some(id), None) => {
            let p = presets::preset(id).ok_or_else(|| format!("Unknown metric '{id}'"))?;
            // A preset carries its own permission: a dashboard someone shared
            // renders per viewer, and a metric the viewer may not read is
            // reported as such rather than silently omitted.
            check_permission(db.get_ref(), claims, p.permission, "read")
                .await
                .map_err(|_| format!("You do not have permission to view '{}'", p.title))?;
            (p.to_spec(period), Some(p.title.to_string()))
        }
        (None, Some(spec)) => {
            let mut spec = spec.clone();
            if let Some(p) = period {
                spec.period = p;
            }
            (spec, None)
        }
        (Some(_), Some(_)) => {
            return Err("Specify either 'preset' or 'spec', not both".into());
        }
        (None, None) => return Err("Specify either 'preset' or 'spec'".into()),
    };

    let compiled = compile(&spec, ctx).map_err(|e| e.detail())?;
    let exec_ctx = ExecCtx {
        branch_ids,
        locale,
        tz,
    };
    let result = execute::run(db, &compiled, &exec_ctx)
        .await
        .map_err(|e| match e {
            // A statement timeout is the expected failure for an over-broad
            // widget, and the caller can act on it — say so rather than "error".
            execute::ExecError::Db(sqlx::Error::Database(ref d))
                if d.code().as_deref() == Some("57014") =>
            {
                "This metric took too long. Narrow the period or the breakdown.".to_string()
            }
            other => {
                // This endpoint answers 200 with a per-widget error, so nothing
                // downstream — not the middleware, not a status-keyed client —
                // would ever see this. It is reported explicitly for that
                // reason. A rejected SPEC is not reported: that is the caller
                // asking for something that does not exist, not a fault.
                report::report(
                    report::Failure::new("metrics", "widget_query")
                        .with("dataset", serde_json::Value::from(spec.dataset.clone())),
                    &other,
                );
                "This metric could not be computed.".to_string()
            }
        })?;

    Ok(MetricResult {
        columns: result.columns,
        rows: result.rows,
        row_count: result.row_count,
        truncated: result.truncated,
        grain: result.grain,
        viz: result.viz,
        facet_by: result.facet_by.map(str::to_string),
        period: PeriodInfo {
            from: result.period.from.map(|t| t.to_rfc3339()),
            to: result.period.to.map(|t| t.to_rfc3339()),
        },
        title,
    })
}
