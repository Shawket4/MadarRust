//! `POST /ai/chat/stream` — the same turn as `/ai/chat`, delivered as it happens.
//!
//! # Why this exists
//!
//! A turn is two to four model calls plus the queries between them. Delivered as
//! one JSON response that is several seconds of nothing, which reads as a hang —
//! the merchant cannot tell a slow answer from a broken one, and the honest
//! signal that work is happening is exactly what a chat UI needs.
//!
//! # Shape
//!
//! Server-sent events, one `event:` line and one `data:` JSON line per frame,
//! terminating in `done`. The final frame carries the **same payload the
//! non-streaming endpoint returns**, so a client can ignore every progress event
//! and still be correct — progress is decoration, `answer` is the contract.
//!
//! `POST` rather than `GET` because the body carries the question and the
//! conversation, and because `EventSource` cannot send an `Authorization`
//! header anyway. Clients consume it with `fetch` + a `ReadableStream` reader.
//!
//! # Why the work is spawned
//!
//! The agent loop borrows a request-scoped context. Streaming it means the loop
//! and the response body have to make progress together, so the loop runs in its
//! own task over OWNED data and reports through a channel. That also means a
//! disconnected client cannot wedge a half-finished turn: the send fails, the
//! loop notices, and the task ends.

use std::sync::Arc;

use actix_web::web::Bytes;
use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::ToSchema;

use crate::{
    analytics::compile::CompileCtx,
    auth::jwt::Claims,
    db::Db,
    errors::{AppError, AppErrorResponse},
    observability::report::{self, Failure},
    permissions::checker::check_permission,
};

use super::{
    AiState, agent, compaction, prompt,
    store::{self},
    telemetry::TurnLog,
    tools::ToolCtx,
};

/// How many frames may queue before the producer waits. Small on purpose: a
/// client that has stopped reading should slow the loop down rather than let
/// frames pile up in memory.
const CHANNEL_DEPTH: usize = 16;

/// One frame of a streamed turn.
///
/// Every variant except `Answer`/`Clarify`/`Incomplete` is progress and may be
/// ignored. Deliberately coarse: emitting the model's partial tokens would mean
/// streaming text that has not been through pseudonym restoration yet, and a
/// half-restored name is exactly what must never render.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ChatFrame {
    /// Sent first. Carries the conversation id when RESUMING one, so a client
    /// can bind the stream immediately; `null` when starting a new conversation,
    /// because it is created lazily on the first turn that produces something —
    /// the id then arrives on the `answer` frame.
    Started { conversation_id: Option<uuid::Uuid> },
    /// A model call is in flight. `step` counts from 1.
    Thinking { step: usize },
    /// A query is about to run. Carries what it is, not its results.
    Querying {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        dataset: String,
    },
    /// A query came back. The block is identical to one in the final payload.
    Result {
        block: Box<super::handlers::ResultBlock>,
    },
    /// The finished turn — the same body `/ai/chat` returns.
    Answer {
        response: Box<super::handlers::AiChatResponse>,
    },
    /// The turn failed. A client should render this, not retry blindly.
    Error { message: String },
}

impl ChatFrame {
    /// Render as an SSE frame. A serialization failure becomes an `error` frame
    /// rather than a dropped one: silence is the worst outcome for a client
    /// that is waiting.
    fn to_sse(&self) -> Bytes {
        let name = match self {
            ChatFrame::Started { .. } => "started",
            ChatFrame::Thinking { .. } => "thinking",
            ChatFrame::Querying { .. } => "querying",
            ChatFrame::Result { .. } => "result",
            ChatFrame::Answer { .. } => "answer",
            ChatFrame::Error { .. } => "error",
        };
        let data = serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"event":"error","message":"could not encode this frame"}"#.to_string()
        });
        Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
    }
}

/// Forwards the agent's progress into the SSE channel.
///
/// `blocking_send` is wrong inside async, and the loop is not async at its
/// callback points, so frames are offered with `try_send`: if the client has
/// stopped reading and the buffer is full, PROGRESS is dropped rather than the
/// loop being stalled. The terminal `answer` frame is sent with `send().await`
/// and so is never dropped — progress is decoration, the answer is the contract.
struct SseProgress {
    tx: mpsc::Sender<ChatFrame>,
}

impl agent::Progress for SseProgress {
    fn thinking(&self, step: usize) {
        let _ = self.tx.try_send(ChatFrame::Thinking { step });
    }

    fn querying(&self, title: Option<&str>, dataset: &str) {
        let _ = self.tx.try_send(ChatFrame::Querying {
            title: title.map(str::to_string),
            dataset: dataset.to_string(),
        });
    }

    fn result(&self, data: &super::tools::QueryData) {
        let _ = self.tx.try_send(ChatFrame::Result {
            block: Box::new(super::handlers::to_block_ref(data)),
        });
    }
}

fn claims_of(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

#[utoipa::path(
    post,
    path = "/ai/chat/stream",
    tag = "ai",
    request_body = super::handlers::AiChatRequest,
    responses(
        (status = 200, content_type = "text/event-stream",
         description = "Progress frames (`started`, `thinking`, `querying`, `result`) \
            followed by exactly one terminal frame (`answer` or `error`). The `answer` \
            frame's `response` is byte-identical to what POST /ai/chat returns, so a \
            client may ignore every progress frame and still be correct."),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn chat_stream(
    req: HttpRequest,
    db: Db,
    state: web::Data<AiState>,
    body: web::Json<super::handlers::AiChatRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    check_permission(db.get_ref(), &claims, "reports", "read").await?;
    if claims.org_id().is_none() {
        return Err(AppError::Forbidden(
            "AI analytics requires an organization-scoped account".into(),
        ));
    }

    // Everything that can fail cheaply fails BEFORE the stream opens, so a bad
    // request is an HTTP error the client can handle rather than an `error`
    // frame inside a 200 it has to parse.
    let prepared = super::handlers::prepare_turn(&db, &claims, &req, &body, &state).await?;

    let (tx, rx) = mpsc::channel::<ChatFrame>(CHANNEL_DEPTH);
    let selected_branch = crate::analytics::scope::header_branch_id(&req);
    let provider = prepared.provider.clone();
    let db_owned = db.clone();
    let claims_owned = claims.clone();
    let question = prepared.question.clone();
    let state_owned = state.clone();

    // The turn runs in its own task over owned data. It gets its own hub with a
    // cleared scope for the same reason every other spawned task does: it
    // outlives the request and would otherwise report against whatever context
    // is bound to the worker thread it lands on.
    tokio::spawn(async move {
        let hub = Arc::new(sentry::Hub::new_from_top(sentry::Hub::current()));
        hub.configure_scope(|scope| {
            scope.clear();
            scope.set_tag("job", "ai_chat_stream");
        });

        let started = std::time::Instant::now();
        let mut log = TurnLog::new(&provider.name(), &prepared.locale, &question);

        let _ = tx
            .send(ChatFrame::Started {
                conversation_id: prepared.conversation_id,
            })
            .await;

        let compile_ctx = CompileCtx {
            tz: prepared.tz,
            now: chrono::Utc::now(),
        };
        let tool_ctx = ToolCtx {
            db: &db_owned,
            claims: &claims_owned,
            compile: &compile_ctx,
            accessible: &prepared.accessible,
            selected_branch,
            locale: &prepared.locale,
            timezone: &prepared.timezone,
            pseudonyms: &prepared.pseudonyms,
        };

        let progress = SseProgress { tx: tx.clone() };
        let outcome = agent::run_with_progress(
            provider.as_ref(),
            &tool_ctx,
            &prepared.grounding,
            &prepared.history,
            &question,
            &mut log,
            Some(&progress),
        )
        .await;

        match outcome {
            Ok(outcome) => {
                // Result frames were already emitted by the progress sink as
                // each query returned; nothing to replay here.
                let response = super::handlers::finish_turn(
                    &db_owned,
                    &state_owned,
                    &claims_owned,
                    &prepared,
                    outcome,
                    provider.as_ref(),
                )
                .await;

                let _ = tx
                    .send(ChatFrame::Answer {
                        response: Box::new(response),
                    })
                    .await;
            }
            Err(e) => {
                sentry::Hub::run(hub, || {
                    report::report(Failure::new("ai", "chat_stream"), &e);
                });
                let _ = tx
                    .send(ChatFrame::Error {
                        message: AppError::from(e).to_string(),
                    })
                    .await;
            }
        }

        log.emit(started.elapsed(), &question);
    });

    let frames = ReceiverStream::new(rx).map(|frame| Ok::<Bytes, actix_web::Error>(frame.to_sse()));

    Ok(HttpResponse::Ok()
        .content_type("text/event-stream")
        // Proxies that buffer would defeat the entire point.
        .insert_header(("Cache-Control", "no-cache, no-transform"))
        .insert_header(("X-Accel-Buffering", "no"))
        .streaming(frames))
}

/// Marker so `compaction` is not flagged unused when the module is compiled
/// without the paths that reference it.
#[allow(unused_imports)]
use compaction as _compaction;
#[allow(unused_imports)]
use prompt as _prompt;
#[allow(unused_imports)]
use store as _store;

use futures::StreamExt as _;
