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
use uuid::Uuid;

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
    observability::report::{self, Failure},
    permissions::checker::check_permission,
};

use super::{
    AiState,
    agent::{self, AgentOutcome},
    compaction, prompt,
    store::{self, ConversationDetail, ConversationSummary},
    telemetry::TurnLog,
    tools::{QueryData, ToolCtx},
};

/// Longest question accepted — a guard against pathological prompts.
const MAX_QUESTION_LEN: usize = 1000;
/// Absolute ceiling on replayed turns, whichever path supplied them.
///
/// For a stored conversation the real bound is
/// [`compaction::MAX_VERBATIM_TURNS`] — everything older has been folded into a
/// summary. This is the backstop that also covers a client sending an
/// arbitrarily long `history` array.
const MAX_HISTORY_CEILING: usize = compaction::MAX_VERBATIM_TURNS as usize;

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
    /// Continue a stored conversation. When set, history is loaded from the
    /// server and `history` below is ignored — this is the path that gives
    /// resumable chats and unlimited, compacted context.
    ///
    /// Omit it to start a new conversation; the response says which one was
    /// created.
    #[serde(default)]
    pub conversation_id: Option<Uuid>,
    /// Recent prior turns, oldest first. The stateless fallback, kept for
    /// clients that manage their own window and for one-off questions. Ignored
    /// when `conversation_id` is set. The server caps it regardless.
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
    /// The conversation this turn belongs to. Present whenever the turn was
    /// stored — send it back on the next message to continue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<Uuid>,
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

/// Borrowing variant, for emitting a block before the turn has finished.
pub fn to_block_ref(d: &QueryData) -> ResultBlock {
    ResultBlock {
        title: d.title.clone(),
        preset_id: d.preset_id.clone(),
        spec: d.spec.clone(),
        columns: d.result.columns.clone(),
        rows: d.result.rows.clone(),
        row_count: d.result.row_count,
        truncated: d.result.truncated,
        grain: d.result.grain,
        viz: d.result.viz,
        facet_by: d.result.facet_by.map(str::to_string),
        period_from: d.result.period.from.map(|t| t.to_rfc3339()),
        period_to: d.result.period.to.map(|t| t.to_rfc3339()),
        scope: d.scope.clone(),
    }
}

/// How many rows of a block are persisted with the conversation.
///
/// A live block may carry up to the executor's 1000-row cap. Persisting that
/// for every turn of every conversation is a lot of JSONB for something a
/// person scrolls past, and the value of a stored chart is in seeing its shape
/// again, not in re-reading row 700. Anything longer keeps its head and says so.
const MAX_STORED_ROWS: usize = 200;

/// The renderable part of a block, trimmed for storage.
///
/// Deliberately the same field names the live response uses, so the client
/// renders a replayed block with the same component and no second code path —
/// two renderers for the same data is how a stored chart quietly starts
/// disagreeing with a live one.
fn snapshot_of(b: &ResultBlock) -> Value {
    let stored: Vec<&Map<String, Value>> = b.rows.iter().take(MAX_STORED_ROWS).collect();
    serde_json::json!({
        "columns": b.columns,
        "rows": stored,
        "row_count": b.row_count,
        // True if the ORIGINAL query was capped, or if storage trimmed it here.
        "truncated": b.truncated || b.rows.len() > MAX_STORED_ROWS,
        "grain": b.grain,
        "viz": b.viz,
        "facet_by": b.facet_by,
        "scope": b.scope,
        "period_from": b.period_from,
        "period_to": b.period_to,
    })
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

/// Everything a turn needs, resolved before any model call.
///
/// Extracted so `/ai/chat` and `/ai/chat/stream` run the *same* preparation.
/// Two endpoints that each assembled their own context would drift, and the
/// thing that drifts first is always a scoping or redaction step.
pub struct PreparedTurn {
    pub provider: std::sync::Arc<dyn super::llm::LlmProvider>,
    pub question: String,
    pub locale: String,
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub conversation_id: Option<Uuid>,
    pub history: Vec<agent::PriorTurn>,
    pub condensed: Option<String>,
    pub accessible: Vec<scope::BranchRef>,
    pub timezone: String,
    pub tz: chrono_tz::Tz,
    pub grounding: String,
    pub pseudonyms: super::pseudonym::Directory,
    pub cache_key: String,
    /// A cached response for exactly this turn, when one exists.
    pub cached: Option<AiChatResponse>,
}

/// Validate, authorize and assemble a turn. Everything that can fail cheaply
/// fails here, before a stream opens or a model is called.
pub async fn prepare_turn(
    db: &Db,
    claims: &Claims,
    req: &HttpRequest,
    body: &AiChatRequest,
    state: &AiState,
) -> Result<PreparedTurn, AppError> {
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
        .clone()
        .ok_or_else(|| AppError::ServiceUnavailable("AI analytics is not configured".into()))?;

    let locale = crate::analytics::handlers::normalize_locale(body.locale.as_deref());
    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("AI analytics requires an organization".into()))?;
    let user_id = claims.user_id();

    // A named conversation is loaded from the server; otherwise fall back to
    // whatever window the client supplied. The server path is what makes a chat
    // resumable and its context unlimited-but-compacted — a client-supplied
    // window can be neither, because it can only ever hold what the client
    // chose to re-upload.
    let (conversation_id, condensed, mut history) = match body.conversation_id {
        Some(id) => {
            store::ensure_owned(db, id, user_id).await?;
            let ctx =
                store::replay_context(db, id, user_id, compaction::MAX_VERBATIM_TURNS).await?;
            let turns: Vec<agent::PriorTurn> = ctx
                .turns
                .iter()
                .map(|t| agent::PriorTurn {
                    question: t.question.clone(),
                    answer: t.answer.clone().unwrap_or_default(),
                    spec: store::primary_spec(t),
                })
                .collect();
            (Some(id), ctx.summary, turns)
        }
        None => {
            let turns: Vec<agent::PriorTurn> = body
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
            (None, None, turns)
        }
    };
    if history.len() > MAX_HISTORY_CEILING {
        history.drain(0..history.len() - MAX_HISTORY_CEILING);
    }

    let clock = scope::org_clock(db).await?;
    let accessible = scope::accessible_branches(db, claims).await?;

    // Cache key is scoped by the caller's BRANCH SET rather than their user id:
    // two managers with identical access asking the same question share a hit,
    // where a user-keyed cache would serve neither. Locale and the conversation
    // window are included, so a follow-up is never answered from a different
    // context.
    let key = cache_key(
        &accessible,
        &locale,
        condensed.as_deref(),
        &history,
        question,
    );
    let cached = state.cache.get(&key).await;

    let now = chrono::Utc::now();
    let today = now.with_timezone(&clock.tz).date_naive().to_string();
    let branch_names: Vec<String> = accessible.iter().map(|b| b.name.clone()).collect();
    let grounding = prompt::grounding(
        &today,
        &clock.timezone,
        &locale,
        &branch_names,
        condensed.as_deref(),
    );

    // Built once per turn from the org's users, so a person's code is stable
    // across every query in this turn AND across turns — a follow-up referring
    // to "the second one" means the same person it meant before.
    let pseudonyms = match super::pseudonym::Directory::load(db).await {
        Ok(d) => d,
        Err(e) => {
            // Failing OPEN here would send real staff names to the model, so it
            // fails closed instead: an empty directory redacts every personal
            // cell to an opaque marker rather than passing names through.
            report::report(Failure::new("ai", "load_pseudonyms"), &e);
            super::pseudonym::Directory::default()
        }
    };

    let _ = req;
    Ok(PreparedTurn {
        provider,
        question: question.to_string(),
        locale,
        org_id,
        user_id,
        conversation_id,
        history,
        condensed,
        accessible,
        timezone: clock.timezone,
        tz: clock.tz,
        grounding,
        pseudonyms,
        cache_key: key,
        cached,
    })
}

/// Persist the turn, trigger compaction, cache, and build the response body.
///
/// Shared by both endpoints for the same reason `prepare_turn` is: the storage
/// and caching rules are part of the contract, not part of the transport.
pub async fn finish_turn(
    db: &Db,
    state: &AiState,
    _claims: &Claims,
    prepared: &PreparedTurn,
    outcome: AgentOutcome,
    provider: &dyn super::llm::LlmProvider,
) -> AiChatResponse {
    let kind = match outcome {
        AgentOutcome::Answer { text, results } => AiChatKind::Answer {
            text,
            results: results.into_iter().map(to_block).collect(),
        },
        AgentOutcome::Clarify { question } => AiChatKind::Clarify { question },
        AgentOutcome::Exhausted { text, results } => AiChatKind::Incomplete {
            text,
            results: results.into_iter().map(to_block).collect(),
        },
    };

    // A conversation is created lazily on the first message that produced
    // something, so a rejected or empty question does not litter the list.
    //
    // Persistence failures are reported but do NOT fail the turn: the merchant
    // has their answer, and losing it from the history is a far smaller harm
    // than replacing a correct answer with a 500.
    let conversation_id = match prepared.conversation_id {
        Some(id) => Some(id),
        None => match store::create(
            db,
            prepared.org_id,
            prepared.user_id,
            &prepared.locale,
            &prepared.question,
        )
        .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                report::report(Failure::new("ai", "create_conversation"), &e);
                None
            }
        },
    };
    if let Some(id) = conversation_id {
        let record = store::TurnRecord {
            question: prepared.question.clone(),
            answer: Some(kind.reply_text()),
            kind: kind.name().to_string(),
            specs: kind.specs_json(),
            provider: Some(provider.name()),
        };
        match store::append_turn(db, id, prepared.org_id, prepared.user_id, &record).await {
            Ok(seq) => {
                // Fold older turns into the running summary once the window has
                // slid past them. Spawned, so this message does not pay for it.
                if seq > compaction::VERBATIM_TURNS {
                    if let Some(p) = state.provider.clone() {
                        compaction::spawn(db.clone(), p, id);
                    }
                }
            }
            Err(e) => report::report(Failure::new("ai", "append_turn"), &e),
        }
    }

    let response = AiChatResponse {
        outcome: kind,
        conversation_id,
        provider: provider.name(),
        timezone: prepared.timezone.clone(),
    };

    // A clarifying question is not cached: it is a prompt for input, and caching
    // it would answer the follow-up with the same question again.
    if !matches!(response.outcome, AiChatKind::Clarify { .. }) {
        state
            .cache
            .insert(prepared.cache_key.clone(), response.clone())
            .await;
    }
    response
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

    let prepared = prepare_turn(&db, &claims, &req, &body, &state).await?;
    let mut log = TurnLog::new(
        &prepared.provider.name(),
        &prepared.locale,
        &prepared.question,
    );

    if let Some(hit) = prepared.cached.clone() {
        log.served_from_cache();
        log.emit(started.elapsed(), &prepared.question);
        return Ok(HttpResponse::Ok().json(hit));
    }

    let compile_ctx = CompileCtx {
        tz: prepared.tz,
        now: chrono::Utc::now(),
    };
    let tool_ctx = ToolCtx {
        db: &db,
        claims: &claims,
        compile: &compile_ctx,
        accessible: &prepared.accessible,
        selected_branch: scope::header_branch_id(&req),
        locale: &prepared.locale,
        timezone: &prepared.timezone,
        pseudonyms: &prepared.pseudonyms,
    };

    let outcome = agent::run(
        prepared.provider.as_ref(),
        &tool_ctx,
        &prepared.grounding,
        &prepared.history,
        &prepared.question,
        &mut log,
    )
    .await?;

    let response = finish_turn(
        &db,
        &state,
        &claims,
        &prepared,
        outcome,
        prepared.provider.as_ref(),
    )
    .await;

    log.emit(started.elapsed(), &prepared.question);
    Ok(HttpResponse::Ok().json(response))
}

impl AiChatKind {
    /// The stored `kind` discriminator.
    fn name(&self) -> &'static str {
        match self {
            AiChatKind::Answer { .. } => "answer",
            AiChatKind::Clarify { .. } => "clarify",
            AiChatKind::Incomplete { .. } => "incomplete",
        }
    }

    /// What the assistant said, whichever shape it said it in. A clarification
    /// is stored as the reply too — it is what the merchant saw, and a stored
    /// turn with a blank answer would read to the summarizer as a failure.
    fn reply_text(&self) -> String {
        match self {
            AiChatKind::Answer { text, .. } => text.clone(),
            AiChatKind::Clarify { question } => question.clone(),
            AiChatKind::Incomplete { text, .. } => text.clone(),
        }
    }

    /// The queries this turn ran AND what they returned, in the shape
    /// `ai_messages.specs` stores:
    /// `[{title, preset_id, spec, captured_at, snapshot}]`.
    ///
    /// The snapshot is why reopening a conversation shows its charts again
    /// instead of an empty shell. The original design stored only the spec and
    /// re-ran it, on the reasoning that stored rows go stale — which is true,
    /// but it made scrolling back through your own conversation a page of
    /// prose referring to tables that were not there.
    ///
    /// Both concerns are satisfied by keeping the spec *and* stamping the
    /// snapshot with `captured_at`. History renders instantly from the
    /// snapshot, labelled with when it was taken; the spec is still there to
    /// re-run for current figures, and to pin the answer to a dashboard. What
    /// is never done is presenting a stored number as if it were current.
    fn specs_json(&self) -> Value {
        let blocks = match self {
            AiChatKind::Answer { results, .. } | AiChatKind::Incomplete { results, .. } => {
                results.as_slice()
            }
            AiChatKind::Clarify { .. } => &[],
        };
        let captured_at = chrono::Utc::now().to_rfc3339();
        Value::Array(
            blocks
                .iter()
                .map(|b| {
                    serde_json::json!({
                        "title": b.title,
                        "preset_id": b.preset_id,
                        "spec": b.spec,
                        "captured_at": captured_at,
                        "snapshot": snapshot_of(b),
                    })
                })
                .collect(),
        )
    }
}

/// A cache key over the caller's *access*, not their identity.
fn cache_key(
    accessible: &[scope::BranchRef],
    locale: &str,
    condensed: Option<&str>,
    history: &[agent::PriorTurn],
    question: &str,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for b in accessible {
        b.id.hash(&mut h);
    }
    // The condensed head is part of the context the model sees, so a compaction
    // that rewrites it must invalidate the cached answer with it.
    condensed.hash(&mut h);
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

    fn block_with(rows: usize) -> ResultBlock {
        use crate::analytics::types::ColumnKind;
        ResultBlock {
            title: Some("Tips by waiter".into()),
            preset_id: Some("tips_by_waiter".into()),
            spec: serde_json::from_value(serde_json::json!({
                "dataset": "orders",
                "dimensions": ["waiter"],
                "measures": ["tip_total"],
                "period": { "preset": "last_month" }
            }))
            .expect("a valid spec"),
            columns: vec![
                Column {
                    key: "waiter",
                    label: "Waiter",
                    kind: ColumnKind::Label,
                },
                Column {
                    key: "tip_total",
                    label: "Tips",
                    kind: ColumnKind::Money,
                },
            ],
            rows: (0..rows)
                .map(|i| {
                    let mut m = Map::new();
                    m.insert("waiter".into(), serde_json::json!(format!("W{i}")));
                    m.insert("tip_total".into(), serde_json::json!(i as i64 * 100));
                    m
                })
                .collect(),
            row_count: rows,
            truncated: false,
            grain: Grain::Categorical,
            viz: Viz::Bar,
            facet_by: None,
            scope: ScopeInfo {
                all_branches: true,
                branches: vec!["Maadi".into()],
                label: "All branches (1)".into(),
                unmatched_branch: None,
            },
            period_from: Some("2026-08-01T00:00:00Z".into()),
            period_to: Some("2026-09-01T00:00:00Z".into()),
        }
    }

    /// A reopened conversation must show its charts, not prose referring to
    /// tables that are no longer there. The snapshot is what makes that work.
    #[test]
    fn a_stored_turn_carries_the_chart_that_was_shown() {
        let kind = AiChatKind::Answer {
            text: "Forkesha led on tips.".into(),
            results: vec![block_with(3)],
        };
        let stored = kind.specs_json();
        let entry = &stored[0];

        // The spec survives: re-running for current figures, and pinning the
        // answer to a dashboard, both still work.
        assert_eq!(entry["spec"]["dataset"], "orders");
        assert_eq!(entry["preset_id"], "tips_by_waiter");

        // ...and so does everything needed to draw it again.
        let snap = &entry["snapshot"];
        assert_eq!(snap["rows"].as_array().unwrap().len(), 3);
        assert_eq!(snap["columns"].as_array().unwrap().len(), 2);
        assert_eq!(snap["viz"], "bar");
        assert_eq!(snap["scope"]["label"], "All branches (1)");
        assert_eq!(snap["period_from"], "2026-08-01T00:00:00Z");

        // A stored figure is never presented as a current one, so it has to
        // carry the moment it was taken.
        assert!(
            entry["captured_at"]
                .as_str()
                .is_some_and(|t| t.contains('T')),
            "a snapshot without a timestamp cannot be labelled as historical"
        );
    }

    #[test]
    fn a_large_result_is_trimmed_for_storage_and_says_so() {
        // Persisting the executor's full 1000-row cap for every turn of every
        // conversation is a lot of JSONB for rows nobody scrolls to.
        let kind = AiChatKind::Answer {
            text: "Long table.".into(),
            results: vec![block_with(MAX_STORED_ROWS + 250)],
        };
        let snap = &kind.specs_json()[0]["snapshot"];
        assert_eq!(snap["rows"].as_array().unwrap().len(), MAX_STORED_ROWS);
        // The trim must be visible, or the chart silently misrepresents itself.
        assert_eq!(snap["truncated"], true);
        // The true size is still reported.
        assert_eq!(snap["row_count"], MAX_STORED_ROWS + 250);
    }

    #[test]
    fn a_clarification_stores_no_snapshot_because_it_ran_nothing() {
        let kind = AiChatKind::Clarify {
            question: "Which branch?".into(),
        };
        assert_eq!(kind.specs_json(), serde_json::json!([]));
    }

    /// `store::primary_spec` reads `specs[0].spec`. Adding the snapshot beside
    /// it must not disturb that, or follow-up questions lose their context.
    #[test]
    fn adding_the_snapshot_did_not_move_the_spec() {
        let kind = AiChatKind::Answer {
            text: "x".into(),
            results: vec![block_with(1)],
        };
        let turn = crate::ai::store::StoredTurn {
            id: Uuid::new_v4(),
            seq: 1,
            question: "q".into(),
            answer: Some("a".into()),
            kind: "answer".into(),
            specs: kind.specs_json(),
            provider: None,
            created_at: chrono::Utc::now(),
        };
        let spec = crate::ai::store::primary_spec(&turn).expect("the spec is still readable");
        assert_eq!(spec.dataset, "orders");
    }

    #[test]
    fn the_cache_key_is_shared_between_users_with_the_same_access() {
        // Two different managers over the same branches ask the same question:
        // one model call should serve both.
        let ids: Vec<Uuid> = (0..2).map(|_| Uuid::new_v4()).collect();
        let a: Vec<BranchRef> = ids.iter().copied().map(branch).collect();
        let b: Vec<BranchRef> = ids.iter().copied().map(branch).collect();
        assert_eq!(
            cache_key(&a, "en", None, &[], "revenue today"),
            cache_key(&b, "en", None, &[], "revenue today")
        );
    }

    #[test]
    fn different_branch_access_never_shares_a_cached_answer() {
        // The property that stops one branch's figures reaching another's manager.
        let a = vec![branch(Uuid::new_v4())];
        let b = vec![branch(Uuid::new_v4())];
        assert_ne!(
            cache_key(&a, "en", None, &[], "revenue today"),
            cache_key(&b, "en", None, &[], "revenue today")
        );
    }

    #[test]
    fn locale_and_conversation_context_are_part_of_the_key() {
        let a = vec![branch(Uuid::new_v4())];
        assert_ne!(
            cache_key(&a, "en", None, &[], "q"),
            cache_key(&a, "ar", None, &[], "q")
        );
        assert_ne!(
            cache_key(&a, "en", None, &[], "and last month?"),
            cache_key(&a, "en", None, &[prior_turn(None)], "and last month?")
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
            cache_key(&a, "en", None, &[prior_turn(None)], "and last month?"),
            cache_key(&a, "en", None, &[prior_turn(Some(spec))], "and last month?")
        );
    }

    #[test]
    fn the_response_serializes_as_a_tagged_union() {
        let r = AiChatResponse {
            outcome: AiChatKind::Clarify {
                question: "Which branch?".into(),
            },
            conversation_id: None,
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

// ─────────────────────────────────────────────────────────────────────────────
// Conversation management
//
// A stored conversation is private to the user who created it. RLS fences the
// organization; the user fence is in `ai::store`, and every handler below goes
// through it rather than filtering here — one place to be right.
// ─────────────────────────────────────────────────────────────────────────────

/// Most conversations returned in one page.
const MAX_CONVERSATIONS_PAGE: i64 = 100;
/// Most turns returned when opening a conversation. The newest ones — a chat UI
/// scrolls up, and a client wanting more can page.
const MAX_TURNS_PAGE: i64 = 200;

#[derive(Debug, Deserialize, ToSchema, utoipa::IntoParams)]
pub struct ListConversationsQuery {
    /// Page size (default 30, max 100).
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema, utoipa::IntoParams)]
pub struct ConversationQuery {
    /// How many of the most recent turns to return (default 50, max 200).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RenameConversationRequest {
    pub title: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationList {
    pub conversations: Vec<ConversationSummary>,
}

/// Shared preamble: authenticated, permitted, and org-scoped.
async fn conversation_caller(req: &HttpRequest, db: &Db) -> Result<Claims, AppError> {
    let claims = claims_of(req)?;
    check_permission(db.get_ref(), &claims, "reports", "read").await?;
    if claims.org_id().is_none() {
        return Err(AppError::Forbidden(
            "AI analytics requires an organization-scoped account".into(),
        ));
    }
    Ok(claims)
}

#[utoipa::path(
    get,
    path = "/ai/conversations",
    tag = "ai",
    params(ListConversationsQuery),
    responses(
        (status = 200, description = "The caller's conversations, most recently used first", body = ConversationList),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_conversations(
    req: HttpRequest,
    db: Db,
    query: web::Query<ListConversationsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = conversation_caller(&req, &db).await?;
    let limit = query.limit.unwrap_or(30).clamp(1, MAX_CONVERSATIONS_PAGE);
    let offset = query.offset.unwrap_or(0).max(0);
    let conversations = store::list(&db, claims.user_id(), limit, offset).await?;
    Ok(HttpResponse::Ok().json(ConversationList { conversations }))
}

#[utoipa::path(
    get,
    path = "/ai/conversations/{id}",
    tag = "ai",
    params(("id" = Uuid, Path, description = "Conversation id"), ConversationQuery),
    responses(
        (status = 200, description = "The conversation with its most recent turns", body = ConversationDetail),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_conversation(
    req: HttpRequest,
    db: Db,
    path: web::Path<Uuid>,
    query: web::Query<ConversationQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = conversation_caller(&req, &db).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_TURNS_PAGE);
    let detail = store::detail(&db, path.into_inner(), claims.user_id(), limit).await?;
    Ok(HttpResponse::Ok().json(detail))
}

#[utoipa::path(
    patch,
    path = "/ai/conversations/{id}",
    tag = "ai",
    params(("id" = Uuid, Path, description = "Conversation id")),
    request_body = RenameConversationRequest,
    responses(
        (status = 204, description = "Renamed"),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn rename_conversation(
    req: HttpRequest,
    db: Db,
    path: web::Path<Uuid>,
    body: web::Json<RenameConversationRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = conversation_caller(&req, &db).await?;
    store::rename(&db, path.into_inner(), claims.user_id(), &body.title).await?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    delete,
    path = "/ai/conversations/{id}",
    tag = "ai",
    params(("id" = Uuid, Path, description = "Conversation id")),
    responses(
        (status = 204, description = "Deleted"),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_conversation(
    req: HttpRequest,
    db: Db,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = conversation_caller(&req, &db).await?;
    store::delete(&db, path.into_inner(), claims.user_id()).await?;
    Ok(HttpResponse::NoContent().finish())
}
