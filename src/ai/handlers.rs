//! `POST /ai/chat` — the merchant-facing analytics chat endpoint.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    db::Db,
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    permissions::checker::check_permission,
};

use super::AiState;
use super::catalog::{self, ChartHint, Column};
use super::executor::ExecCtx;
use super::provider::ChatContext;

/// Longest question we accept — a guard against pathological prompts.
const MAX_QUESTION_LEN: usize = 1000;

/// How many result rows we hand the model when asking for a summary. The full
/// result still goes to the client; the summary needs only a representative
/// sample, and a small slice keeps that second call cheap.
const SUMMARY_ROW_SAMPLE: usize = 50;

/// Answer languages we support. Anything else falls back to English. The value
/// is used only as a translation-lookup key (a bound param), so this is about
/// predictability, not safety.
const SUPPORTED_LOCALES: &[&str] = &["en", "ar"];

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiChatRequest {
    /// The merchant's plain-language question, e.g. "top 5 products last month"
    /// or "أعلى ٥ منتجات الشهر الماضي".
    pub question: String,
    /// When true, also return a one-sentence natural-language summary of the
    /// result (a second, small model call, answered in `locale`). Default false.
    #[serde(default)]
    pub include_summary: bool,
    /// Answer language — "en" or "ar" (default "en"). Drives translated labels
    /// and the summary language. Usually the dashboard's active language.
    #[serde(default)]
    pub locale: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AiChatResponse {
    /// The report the assistant chose.
    pub report_id: String,
    pub title: String,
    /// Suggested visualization for the result.
    pub chart: ChartHint,
    /// Column metadata for rendering the table/chart.
    pub columns: Vec<Column>,
    /// Result rows, each an object keyed by column key.
    pub rows: Vec<Map<String, Value>>,
    pub row_count: usize,
    /// True when the result was capped.
    pub truncated: bool,
    /// Optional one-sentence summary (only when `include_summary` was set and
    /// the model produced one), in the requested locale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Which model answered (e.g. "gemini-2.5-flash").
    pub provider: String,
}

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// Normalize the requested locale to a supported one, defaulting to English.
fn normalize_locale(requested: &Option<String>) -> String {
    match requested {
        Some(l) if SUPPORTED_LOCALES.contains(&l.as_str()) => l.clone(),
        _ => "en".to_string(),
    }
}

/// The set of branches this caller may see analytics for — NOT all org branches,
/// NOT a single one, but exactly the caller's access:
///   * org_admin (super_admin is refused earlier) → every branch in the org;
///   * branch_manager / waiter / kitchen → their `user_branch_assignments`;
///   * teller → the branch their token is bound to (falling back to assignments).
///
/// Runs on the RLS-scoped tenant pool, so every query here is already fenced to
/// the caller's org. This set is injected into every report as `:branch_ids` and
/// can never be widened by the model.
async fn accessible_branches(db: &Db, claims: &Claims) -> Result<Vec<Uuid>, AppError> {
    match claims.role {
        UserRole::OrgAdmin | UserRole::SuperAdmin => {
            let ids =
                sqlx::query_scalar::<_, Uuid>("SELECT id FROM branches WHERE deleted_at IS NULL")
                    .fetch_all(db.get_ref())
                    .await?;
            Ok(ids)
        }
        UserRole::Teller => {
            if let Some(b) = claims.branch_id() {
                return Ok(vec![b]);
            }
            assigned_branches(db, claims.user_id()).await
        }
        UserRole::BranchManager | UserRole::Waiter | UserRole::Kitchen => {
            assigned_branches(db, claims.user_id()).await
        }
    }
}

async fn assigned_branches(db: &Db, user_id: Uuid) -> Result<Vec<Uuid>, AppError> {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT uba.branch_id FROM user_branch_assignments uba \
         JOIN branches b ON b.id = uba.branch_id AND b.deleted_at IS NULL \
         WHERE uba.user_id = $1",
    )
    .bind(user_id)
    .fetch_all(db.get_ref())
    .await?;
    Ok(ids)
}

/// The org's timezone name and today's date IN that timezone, so relative dates
/// ("yesterday", "امبارح") resolve correctly and time buckets are local. One
/// round trip on the tenant pool; the org row is visible via RLS.
async fn org_timezone_and_today(db: &Db) -> Result<(String, String), AppError> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT timezone::text, (now() AT TIME ZONE timezone::text)::date::text \
         FROM organizations LIMIT 1",
    )
    .fetch_optional(db.get_ref())
    .await?;
    Ok(row.unwrap_or_else(|| ("Africa/Cairo".to_string(), String::new())))
}

#[utoipa::path(
    post,
    path = "/ai/chat",
    tag = "ai",
    request_body = AiChatRequest,
    responses(
        (status = 200, description = "Answer with a table/chart and optional summary", body = AiChatResponse),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn chat(
    req: HttpRequest,
    db: Db,
    state: web::Data<AiState>,
    body: web::Json<AiChatRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(db.get_ref(), &claims, "reports", "read").await?;

    // The chat answers over ONE merchant's data. A tenant token's `db` is already
    // RLS-scoped to its org; a super-admin token has no single org and would run
    // reports unscoped (cross-tenant) — refuse it so the feature can never
    // aggregate across merchants.
    if claims.org_id().is_none() {
        return Err(AppError::Forbidden(
            "AI analytics requires an organization-scoped account".into(),
        ));
    }

    let question = body.question.trim();
    if question.is_empty() {
        return Err(AppError::BadRequest("question cannot be empty".into()));
    }
    if question.len() > MAX_QUESTION_LEN {
        return Err(AppError::BadRequest(format!(
            "question is too long (max {MAX_QUESTION_LEN} characters)"
        )));
    }

    let provider = state
        .provider
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("AI analytics is not configured".into()))?;

    let locale = normalize_locale(&body.locale);

    // Cache key is scoped by USER (branch access differs per user) + locale +
    // summary flag, so two users in the same org with different branch access can
    // never be served each other's answer.
    let cache_key = format!(
        "{}|{}|{}|{}",
        claims.user_id(),
        locale,
        body.include_summary,
        question
    );
    if let Some(hit) = state.cache.get(&cache_key).await {
        return Ok(HttpResponse::Ok().json(hit));
    }

    // Branch scope (accessible set) + grounding context (timezone/today).
    let branch_ids = accessible_branches(&db, &claims).await?;
    let (timezone, today) = org_timezone_and_today(&db).await?;

    let ctx = ChatContext {
        question: question.to_string(),
        today,
        timezone: timezone.clone(),
        locale: locale.clone(),
    };

    // 1. Model picks a report + fills typed params (never SQL, never branches).
    let choice = provider.choose_report(&ctx).await?;
    let report = catalog::find(&choice.report_id)
        .ok_or_else(|| AppError::BadRequest("The assistant chose an unknown report.".into()))?;

    // 2. Backend runs the pre-written query: read-only, capped, RLS-scoped, and
    //    fenced to the caller's accessible branches, with localized labels.
    let exec_ctx = ExecCtx {
        branch_ids: &branch_ids,
        locale: &locale,
        tz: &timezone,
    };
    let result = super::executor::run(&db, report, &choice.args, &exec_ctx).await?;

    // 3. Optional summary — best-effort in the requested language; never fail the
    //    answer over it.
    let summary = if body.include_summary {
        let sample: Vec<&Map<String, Value>> =
            result.rows.iter().take(SUMMARY_ROW_SAMPLE).collect();
        let data_json = serde_json::to_string(&sample).unwrap_or_else(|_| "[]".into());
        match provider.summarize(&ctx, report.title, &data_json).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("AI summary failed (returning table only): {e}");
                None
            }
        }
    } else {
        None
    };

    let response = AiChatResponse {
        report_id: report.id.to_string(),
        title: report.title.to_string(),
        chart: report.chart,
        columns: result.columns.to_vec(),
        rows: result.rows,
        row_count: result.row_count,
        truncated: result.truncated,
        summary,
        provider: provider.name().to_string(),
    };

    state.cache.insert(cache_key, response.clone()).await;
    Ok(HttpResponse::Ok().json(response))
}
