//! The single SSE endpoint every client connects to: `GET /realtime/stream`.
//! Multiplexes all topics for one branch over one connection, filtered to the
//! topics the caller asked for AND is permitted to read.

use std::time::Duration;

use actix_web::web::Bytes;
use actix_web::{HttpRequest, HttpResponse, web};
use futures::stream::StreamExt;
use serde::Deserialize;
use sqlx::PgPool;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use utoipa::IntoParams;
use uuid::Uuid;

use super::event::Topic;
use super::hub::BranchEventHub;
use crate::delivery::require_branch_access;
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::permissions::checker::check_permission;

#[derive(Deserialize, IntoParams)]
pub struct StreamQuery {
    pub branch_id: Uuid,
    /// Comma-separated topics: `delivery,tickets,kitchen,orders`. Omit to receive
    /// every topic the caller is permitted to read.
    #[serde(default)]
    pub topics: Option<String>,
}

/// Resolve the topics this caller will actually receive: the requested set (or
/// all topics if unspecified), intersected with the topics they hold `:read` on.
/// Fails closed — a permission-check error drops the topic rather than leaking it.
pub(crate) async fn permitted_topics(
    pool: &PgPool,
    claims: &crate::auth::jwt::Claims,
    requested: Option<&str>,
) -> Vec<Topic> {
    let candidates: Vec<Topic> = match requested {
        Some(s) => s.split(',').filter_map(Topic::parse).collect(),
        None => Topic::ALL.to_vec(),
    };
    let mut out = Vec::new();
    for t in candidates {
        let (res, act) = t.permission();
        if check_permission(pool, claims, res, act).await.is_ok() {
            out.push(t);
        }
    }
    out
}

/// SSE stream of all realtime events for a branch, filtered by topic + permission.
/// **Updates-only**: the client seeds current state from the per-feature list
/// endpoints (or `/realtime/snapshot`) first, then connects. On any error/close it
/// re-seeds and reconnects.
#[utoipa::path(
    get, path = "/realtime/stream", tag = "realtime", params(StreamQuery),
    responses(
        (status = 200, content_type = "text/event-stream",
         description = "SSE stream. Each event is `event: <type>` (e.g. delivery.updated, \
            ticket.fired, kitchen.item_bumped) followed by a `data:` JSON line. `: ping` \
            keep-alive comments arrive ~every 20s. On ANY error/close, re-seed and reconnect."),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn stream(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    query: web::Query<StreamQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let topics = permitted_topics(pool.get_ref(), &claims, query.topics.as_deref()).await;
    if topics.is_empty() {
        return Err(AppError::Forbidden(
            "No realtime topics you're permitted to read".into(),
        ));
    }

    // Resume support: a reconnecting client sends `Last-Event-ID` (the last id it
    // saw). Subscribe FIRST so no live event is missed while we read the buffer, then
    // replay what it missed. Events in the overlap (published between subscribe and
    // the buffer read) appear in BOTH the replay and the live stream — the live
    // filter drops id <= max_replayed so each is delivered exactly once.
    let last_event_id: Option<u64> = req
        .headers()
        .get("Last-Event-ID")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok());

    let rx = hub.subscribe(query.branch_id);

    let replayed = match last_event_id {
        Some(id) => hub.replay_since(query.branch_id, id),
        None => Vec::new(), // a fresh connect seeds from the snapshot endpoint, not replay
    };
    let max_replayed = replayed
        .iter()
        .map(|e| e.id)
        .max()
        .unwrap_or_else(|| last_event_id.unwrap_or(0));
    let replay_frames: Vec<Result<Bytes, actix_web::Error>> = replayed
        .into_iter()
        .filter(|e| topics.contains(&e.topic))
        .map(|e| {
            Ok(Bytes::from(format!(
                "id: {}\nevent: {}\ndata: {}\n\n",
                e.id, e.event_type, e.data
            )))
        })
        .collect();

    // Broadcast events → SSE frames, dropping topics the caller isn't permitted for
    // and any id already delivered via replay. A lagged/closed receiver yields `Err`,
    // surfaced as a body error so actix drops the connection; the client reconnects.
    let events = BroadcastStream::new(rx).filter_map(move |res| {
        let out: Option<Result<Bytes, actix_web::Error>> = match res {
            Ok(ev) if ev.id > max_replayed && topics.contains(&ev.topic) => {
                Some(Ok(Bytes::from(format!(
                    "id: {}\nevent: {}\ndata: {}\n\n",
                    ev.id, ev.event_type, ev.data
                ))))
            }
            Ok(_) => None,
            Err(_) => Some(Err(actix_web::error::ErrorInternalServerError(
                "realtime stream lagged",
            ))),
        };
        futures::future::ready(out)
    });

    let keepalive = IntervalStream::new(tokio::time::interval(Duration::from_secs(20)))
        .map(|_| Ok::<Bytes, actix_web::Error>(Bytes::from_static(b": ping\n\n")));

    // Replay buffered events first, then the live stream; keepalive interleaves.
    let body = futures::stream::select(
        futures::stream::iter(replay_frames).chain(events),
        keepalive,
    );

    Ok(HttpResponse::Ok()
        .content_type("text/event-stream")
        .insert_header(("Cache-Control", "no-cache"))
        .insert_header((actix_web::http::header::CONTENT_ENCODING, "identity"))
        .insert_header(("X-Accel-Buffering", "no"))
        .streaming(body))
}
