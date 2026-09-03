//! `POST /ai/chat` — the merchant-facing analytics chat.
//!
//! The response is a **tagged union with a 200 status**, not an error code. An
//! assistant that cannot answer, or that needs one thing clarified, is having a
//! normal conversation — rendering that as a 400 forces the client to show an
//! error toast where a chat bubble belongs. Only genuine faults (no permission,
//! no configured provider, a broken upstream) are HTTP errors.

use std::time::Instant;

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::ToSchema;

use crate::{
    analytics::{
        compile::CompileCtx,
        scope::{self, ScopeInfo},
        spec::QuerySpec,
        types::{Column, Grain, Viz},
    },
    auth::jwt::Claims,
    db::Db,
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};

use super::{
    AiState,
    agent::{self, AgentOutcome},
    prompt,
    telemetry::TurnLog,
    tools::{QueryData, ToolCtx},
};

/// Longest question accepted — a guard against pathological prompts.
const MAX_QUESTION_LEN: usize = 1000;
/// Prior turns kept. Bounds per-message cost however long a chat runs.
const MAX_HISTORY: usize = 6;

/// One earlier exchange, in compact form.
///
/// Result *tables* are never replayed — they are large, and the model does not
/// need last week's rows to answer this week's question. What it does need is
/// the **query** that answered before, which is why `spec` is here: a follow-up
/// like "and last month?" or "same thing for Marina" is that spec with one
/// field changed. Prose alone forces the model to re-derive the whole query
/// from its own summary, which is exactly where a follow-up silently drifts
/// into answering a different question.
///
/// Clients get the spec back on every result block (`results[].spec`) and
/// should echo it here.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HistoryTurn {
    pub question: String,
    /// What the assistant replied. Optional so a client can send a partial log.
    #[serde(default)]
    pub answer: Option<String>,
    /// The query that produced that answer, from `results[].spec`. Optional so
    /// an older client, or a turn that ran no query, still works.
    #[serde(default)]
    pub spec: Option<QuerySpec>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiChatRequest {
    /// The merchant's plain-language question, e.g. "top 5 products last month"
    /// or "أعلى ٥ منتجات الشهر الماضي".
    pub question: String,
    /// Answer language — "en" or "ar" (default "en"). Drives translated labels
    /// and the reply language.
    #[serde(default)]
    pub locale: Option<String>,
    /// Recent prior turns, oldest first. The server caps the window regardless.
    #[serde(default)]
    pub history: Option<Vec<HistoryTurn>>,
}

/// One dataset the assistant pulled while answering. A turn may carry several —
/// "compare this month to last" is two.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ResultBlock {
    /// Set when the data came from a curated metric.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    /// The exact query that produced this. Sending it back is what makes
    /// "pin this answer to my dashboard" a single client-side action: the spec
    /// is already a valid widget definition.
    pub spec: QuerySpec,
    pub columns: Vec<Column>,
    pub rows: Vec<Map<String, Value>>,
    pub row_count: usize,
    pub truncated: bool,
    pub grain: Grain,
    pub viz: Viz,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facet_by: Option<String>,
    /// Which branches this block covers.
    pub scope: ScopeInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period_to: Option<String>,
}

/// How the turn ended. Every variant is a 200.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AiChatKind {
    /// A finding, with the data behind it.
    Answer {
        text: String,
        results: Vec<ResultBlock>,
    },
    /// One question back before the assistant can proceed.
    Clarify { question: String },
    /// It could not get to an answer. Any data it did gather is still returned.
    Incomplete {
        text: String,
        results: Vec<ResultBlock>,
    },
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AiChatResponse {
    #[serde(flatten)]
    pub outcome: AiChatKind,
    /// Which model answered.
    pub provider: String,
    /// The timezone every date in the answer is expressed in.
    pub timezone: String,
}

fn claims_of(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

fn to_block(d: QueryData) -> ResultBlock {
    ResultBlock {
        title: d.title,
        preset_id: d.preset_id,
        spec: d.spec,
        columns: d.result.columns,
        rows: d.result.rows,
        row_count: d.result.row_count,
        truncated: d.result.truncated,
        grain: d.result.grain,
        viz: d.result.viz,
        facet_by: d.result.facet_by.map(str::to_string),
        period_from: d.result.period.from.map(|t| t.to_rfc3339()),
        period_to: d.result.period.to.map(|t| t.to_rfc3339()),
        scope: d.scope,
    }
}

#[utoipa::path(
    post,
    path = "/ai/chat",
    tag = "ai",
    request_body = AiChatRequest,
    responses(
        (status = 200, description = "An answer with its data, or a clarifying question", body = AiChatResponse),
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
    let started = Instant::now();
    let claims = claims_of(&req)?;
    check_permission(db.get_ref(), &claims, "reports", "read").await?;

    // The chat answers over ONE merchant's data. A tenant token's pool is
    // RLS-scoped to its org; a super-admin token has no single org and would run
    // unscoped, so it is refused — the feature can never aggregate across
    // merchants, whatever is asked.
    if claims.org_id().is_none() {
        return Err(AppError::Forbidden(
            "AI analytics requires an organization-scoped account".into(),
        ));
    }

    let question = body.question.trim();
    if question.is_empty() {
        return Err(AppError::BadRequest("question cannot be empty".into()));
    }
    if question.chars().count() > MAX_QUESTION_LEN {
        return Err(AppError::BadRequest(format!(
            "question is too long (max {MAX_QUESTION_LEN} characters)"
        )));
    }

    let provider = state
        .provider
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("AI analytics is not configured".into()))?;

    let locale = crate::analytics::handlers::normalize_locale(body.locale.as_deref());
    let mut log = TurnLog::new(&provider.name(), &locale, question);

    let mut history: Vec<agent::PriorTurn> = body
        .history
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|t| agent::PriorTurn {
            question: t.question,
            answer: t.answer.unwrap_or_default(),
            spec: t.spec,
        })
        .collect();
    if history.len() > MAX_HISTORY {
        history.drain(0..history.len() - MAX_HISTORY);
    }

    let clock = scope::org_clock(&db).await?;
    let accessible = scope::accessible_branches(&db, &claims).await?;

    // Cache key is scoped by the caller's BRANCH SET rather than their user id:
    // two managers with identical access asking the same question share a hit,
    // where a user-keyed cache would serve neither. Locale and the conversation
    // window are included, so a follow-up is never answered from a different
    // context.
    let cache_key = cache_key(&accessible, &locale, &history, question);
    if let Some(hit) = state.cache.get(&cache_key).await {
        log.served_from_cache();
        log.emit(started.elapsed(), question);
        return Ok(HttpResponse::Ok().json(hit));
    }

    let compile_ctx = CompileCtx {
        tz: clock.tz,
        now: chrono::Utc::now(),
    };
    let today = compile_ctx
        .now
        .with_timezone(&clock.tz)
        .date_naive()
        .to_string();
    let branch_names: Vec<String> = accessible.iter().map(|b| b.name.clone()).collect();
    let grounding = prompt::grounding(&today, &clock.timezone, &locale, &branch_names);

    let tool_ctx = ToolCtx {
        db: &db,
        claims: &claims,
        compile: &compile_ctx,
        accessible: &accessible,
        selected_branch: scope::header_branch_id(&req),
        locale: &locale,
        timezone: &clock.timezone,
    };

    let outcome = agent::run(
        provider.as_ref(),
        &tool_ctx,
        &grounding,
        &history,
        question,
        &mut log,
    )
    .await?;

    let response = AiChatResponse {
        outcome: match outcome {
            AgentOutcome::Answer { text, results } => AiChatKind::Answer {
                text,
                results: results.into_iter().map(to_block).collect(),
            },
            AgentOutcome::Clarify { question } => AiChatKind::Clarify { question },
            AgentOutcome::Exhausted { text, results } => AiChatKind::Incomplete {
                text,
                results: results.into_iter().map(to_block).collect(),
            },
        },
        provider: provider.name(),
        timezone: clock.timezone.clone(),
    };

    // A clarifying question is not cached: it is a prompt for input, and caching
    // it would answer the follow-up with the same question again.
    if !matches!(response.outcome, AiChatKind::Clarify { .. }) {
        state.cache.insert(cache_key, response.clone()).await;
    }
    log.emit(started.elapsed(), question);
    Ok(HttpResponse::Ok().json(response))
}

/// A cache key over the caller's *access*, not their identity.
fn cache_key(
    accessible: &[scope::BranchRef],
    locale: &str,
    history: &[agent::PriorTurn],
    question: &str,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for b in accessible {
        b.id.hash(&mut h);
    }
    for turn in history {
        turn.question.hash(&mut h);
        turn.answer.hash(&mut h);
        // The prior spec changes what a follow-up resolves to, so two
        // conversations that differ only in what was previously run must not
        // share a cached answer.
        turn.spec_digest().hash(&mut h);
    }
    format!("{:x}|{locale}|{question}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::scope::BranchRef;
    use uuid::Uuid;

    fn branch(id: Uuid) -> BranchRef {
        BranchRef {
            id,
            name: "B".into(),
        }
    }

    #[test]
    fn the_cache_key_is_shared_between_users_with_the_same_access() {
        // Two different managers over the same branches ask the same question:
        // one model call should serve both.
        let ids: Vec<Uuid> = (0..2).map(|_| Uuid::new_v4()).collect();
        let a: Vec<BranchRef> = ids.iter().copied().map(branch).collect();
        let b: Vec<BranchRef> = ids.iter().copied().map(branch).collect();
        assert_eq!(
            cache_key(&a, "en", &[], "revenue today"),
            cache_key(&b, "en", &[], "revenue today")
        );
    }

    #[test]
    fn different_branch_access_never_shares_a_cached_answer() {
        // The property that stops one branch's figures reaching another's manager.
        let a = vec![branch(Uuid::new_v4())];
        let b = vec![branch(Uuid::new_v4())];
        assert_ne!(
            cache_key(&a, "en", &[], "revenue today"),
            cache_key(&b, "en", &[], "revenue today")
        );
    }

    #[test]
    fn locale_and_conversation_context_are_part_of_the_key() {
        let a = vec![branch(Uuid::new_v4())];
        assert_ne!(cache_key(&a, "en", &[], "q"), cache_key(&a, "ar", &[], "q"));
        assert_ne!(
            cache_key(&a, "en", &[], "and last month?"),
            cache_key(&a, "en", &[prior_turn(None)], "and last month?")
        );
    }

    fn prior_turn(spec: Option<QuerySpec>) -> agent::PriorTurn {
        agent::PriorTurn {
            question: "revenue".into(),
            answer: "12 EGP".into(),
            spec,
        }
    }

    #[test]
    fn the_previous_query_is_part_of_the_cache_key() {
        // Two conversations that differ only in what was previously RUN resolve
        // a follow-up differently, so they must not share a cached answer.
        let a = vec![branch(Uuid::new_v4())];
        let spec = QuerySpec {
            dataset: "orders".into(),
            ..Default::default()
        };
        assert_ne!(
            cache_key(&a, "en", &[prior_turn(None)], "and last month?"),
            cache_key(&a, "en", &[prior_turn(Some(spec))], "and last month?")
        );
    }

    #[test]
    fn the_response_serializes_as_a_tagged_union() {
        let r = AiChatResponse {
            outcome: AiChatKind::Clarify {
                question: "Which branch?".into(),
            },
            provider: "mock".into(),
            timezone: "Africa/Cairo".into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "clarify");
        assert_eq!(v["question"], "Which branch?");
        // Flattened, so the client reads one object rather than unwrapping.
        assert_eq!(v["provider"], "mock");
    }
}
