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
use super::provider::{ChatContext, HistoryTurn};

/// Longest question we accept — a guard against pathological prompts.
const MAX_QUESTION_LEN: usize = 1000;

/// Most prior turns we keep in the conversation window. Bounds per-message cost
/// regardless of how long the chat runs.
const MAX_HISTORY: usize = 8;

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
    /// Recent prior turns in this conversation (oldest → newest), so follow-ups
    /// like "and last month?" resolve. Send only the last few; the server caps
    /// the window regardless.
    #[serde(default)]
    pub history: Option<Vec<HistoryTurn>>,
}

/// Which branches an answer actually covers — surfaced on every response so the
/// scope is never ambiguous ("all branches" vs a specific one).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ScopeInfo {
    /// True when the answer spans EVERY branch the caller can access.
    pub all_branches: bool,
    /// The branch names the answer covers.
    pub branches: Vec<String>,
    /// Human-readable label, e.g. "All branches (3)" or "Sidi Henish".
    pub label: String,
    /// Set when the user named a branch that couldn't be matched; the answer
    /// then falls back to all accessible branches and this flags the mismatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmatched_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AiChatResponse {
    /// The report the assistant chose.
    pub report_id: String,
    pub title: String,
    /// Which branches this answer covers.
    pub scope: ScopeInfo,
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
async fn accessible_branches(db: &Db, claims: &Claims) -> Result<Vec<(Uuid, String)>, AppError> {
    match claims.role {
        UserRole::OrgAdmin | UserRole::SuperAdmin => {
            let rows = sqlx::query_as::<_, (Uuid, String)>(
                "SELECT id, name FROM branches WHERE deleted_at IS NULL ORDER BY name",
            )
            .fetch_all(db.get_ref())
            .await?;
            Ok(rows)
        }
        UserRole::Teller => {
            if let Some(b) = claims.branch_id() {
                let row = sqlx::query_as::<_, (Uuid, String)>(
                    "SELECT id, name FROM branches WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(b)
                .fetch_optional(db.get_ref())
                .await?;
                return Ok(row.into_iter().collect());
            }
            assigned_branches(db, claims.user_id()).await
        }
        UserRole::BranchManager | UserRole::Waiter | UserRole::Kitchen => {
            assigned_branches(db, claims.user_id()).await
        }
    }
}

async fn assigned_branches(db: &Db, user_id: Uuid) -> Result<Vec<(Uuid, String)>, AppError> {
    let rows = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT b.id, b.name FROM user_branch_assignments uba \
         JOIN branches b ON b.id = uba.branch_id AND b.deleted_at IS NULL \
         WHERE uba.user_id = $1 ORDER BY b.name",
    )
    .bind(user_id)
    .fetch_all(db.get_ref())
    .await?;
    Ok(rows)
}

/// Resolve the branch set to query + a human-readable scope, from the caller's
/// accessible branches and the optional branch name the model lifted from the
/// question. The named branch is fuzzy-matched WITHIN the accessible set, so it
/// can only ever NARROW the scope, never widen it. An unmatched name falls back
/// to all accessible branches and is flagged so the answer states what it covered.
fn resolve_scope(accessible: &[(Uuid, String)], requested: Option<&str>) -> (Vec<Uuid>, ScopeInfo) {
    let all_ids: Vec<Uuid> = accessible.iter().map(|(id, _)| *id).collect();
    let all_names: Vec<String> = accessible.iter().map(|(_, n)| n.clone()).collect();
    let all_label = match all_names.len() {
        0 => "No branches".to_string(),
        1 => all_names[0].clone(),
        n => format!("All branches ({n})"),
    };
    let all_scope = |unmatched: Option<String>| ScopeInfo {
        all_branches: true,
        branches: all_names.clone(),
        label: all_label.clone(),
        unmatched_branch: unmatched,
    };

    let requested = requested.map(str::trim).filter(|s| !s.is_empty());
    let Some(req) = requested else {
        return (all_ids, all_scope(None));
    };

    let matches = fuzzy_match_branches(accessible, req);
    if matches.is_empty() {
        return (all_ids, all_scope(Some(req.to_string())));
    }
    let ids = matches.iter().map(|(id, _)| *id).collect();
    let names: Vec<String> = matches.iter().map(|(_, n)| n.clone()).collect();
    let label = names.join(", ");
    (
        ids,
        ScopeInfo {
            all_branches: false,
            branches: names,
            label,
            unmatched_branch: None,
        },
    )
}

/// Case/whitespace-insensitive branch-name match within the accessible set.
/// Prefers an exact match; otherwise a substring either way — handling
/// "sidi henish" vs "Sidi Henish", partials, and Arabic names.
fn fuzzy_match_branches(accessible: &[(Uuid, String)], query: &str) -> Vec<(Uuid, String)> {
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let q = norm(query);
    let exact: Vec<(Uuid, String)> = accessible
        .iter()
        .filter(|(_, n)| norm(n) == q)
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    accessible
        .iter()
        .filter(|(_, n)| {
            let nn = norm(n);
            nn.contains(&q) || q.contains(&nn)
        })
        .cloned()
        .collect()
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

    // Recent conversation window (sliding, server-capped) for follow-up
    // continuity. Compact by construction (question + report id), so cost stays
    // constant per message.
    let mut history = body.history.clone().unwrap_or_default();
    if history.len() > MAX_HISTORY {
        history.drain(0..history.len() - MAX_HISTORY);
    }

    // Cache key is scoped by USER (branch access differs per user) + locale +
    // summary flag + a signature of the conversation window, so two users in the
    // same org — or the same user with different prior context — are never served
    // each other's (or a stale) answer.
    let hist_sig = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for turn in &history {
            turn.question.hash(&mut h);
            turn.report_id.hash(&mut h);
        }
        h.finish()
    };
    let cache_key = format!(
        "{}|{}|{}|{}|{}",
        claims.user_id(),
        locale,
        body.include_summary,
        hist_sig,
        question
    );
    if let Some(hit) = state.cache.get(&cache_key).await {
        return Ok(HttpResponse::Ok().json(hit));
    }

    // Accessible branches (id + name) + grounding context (timezone/today).
    let accessible = accessible_branches(&db, &claims).await?;
    let (timezone, today) = org_timezone_and_today(&db).await?;

    let ctx = ChatContext {
        question: question.to_string(),
        today,
        timezone: timezone.clone(),
        locale: locale.clone(),
        branch_names: accessible.iter().map(|(_, n)| n.clone()).collect(),
        history,
    };

    // 1. Model picks a report + fills typed params, and MAY name a branch (never
    //    SQL). The branch name can only narrow within the accessible set below.
    let choice = provider.choose_report(&ctx).await?;
    let report = catalog::find(&choice.report_id)
        .ok_or_else(|| AppError::BadRequest("The assistant chose an unknown report.".into()))?;

    let requested_branch = choice.args.get("branch").and_then(Value::as_str);
    let (branch_ids, scope) = resolve_scope(&accessible, requested_branch);

    // 2. Backend runs the pre-written query: read-only, capped, RLS-scoped, and
    //    fenced to the resolved branch set, with localized labels.
    let exec_ctx = ExecCtx {
        branch_ids: &branch_ids,
        locale: &locale,
        tz: &timezone,
    };
    let result = super::executor::run(&db, report, &choice.args, &exec_ctx).await?;

    // 3. Optional summary — best-effort in the requested language; the scope is
    //    included so the sentence states which branch(es) it covers. Never fail
    //    the answer over the summary.
    let summary = if body.include_summary {
        let sample: Vec<&Map<String, Value>> =
            result.rows.iter().take(SUMMARY_ROW_SAMPLE).collect();
        let data_json = serde_json::json!({ "scope": scope.label, "rows": sample }).to_string();
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
        scope,
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
