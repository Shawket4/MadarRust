//! `POST /sync/replay/batch` (sync protocol v2, DB audit Phase 6): flush many
//! queued ops in one round trip.
//!
//! Each op is replayed through EXACTLY the single-op path (`handlers::replay`,
//! unchanged): the same attribution, permission, accept-and-flag, idempotency
//! and `X-Madar-Sync-Seq` stamping, in the same order the till queued them. A
//! batch is a transport change, not a semantic one, so:
//!
//! - ops run one after another, in order, each in its own transaction(s); a
//!   batch is NOT atomic, exactly like N single calls;
//! - each op's result is what its single call would have answered (status,
//!   JSON body, sync seq);
//! - by default the batch stops at the first op that did not succeed and
//!   reports the rest as `skipped`, which keeps the till's FIFO / dependency
//!   gating intact (a sale must not land before the till it rings on); a till
//!   that knows its ops are independent may pass `stop_on_error: false`;
//! - a till that retries a whole batch after a lost response is deduplicated
//!   per op by the idempotency keys inside the ops, as a single retry is.
//!
//! Old tills keep calling `POST /sync/replay` one op at a time; nothing about
//! that route changed.

use actix_web::{HttpRequest, HttpResponse, ResponseError, web};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::errors::{AppError, AppErrorResponse};
use crate::realtime::hub::BranchEventHub;

/// Most ops one batch may carry.
pub const MAX_OPS: usize = 100;
/// Largest batch body (a whole sale is a few KB; 100 of them fit).
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReplayBatchRequest {
    /// The sync protocol the till speaks; 2 when absent (this route is v2).
    #[serde(default)]
    pub protocol: Option<u16>,
    /// Queued ops, oldest first: each exactly the body `POST /sync/replay`
    /// takes (`op`, the actor, `request`, optional `approval`, ...).
    #[schema(value_type = Vec<Object>)]
    pub ops: Vec<Value>,
    /// Stop at the first op that does not succeed (default true).
    #[serde(default)]
    pub stop_on_error: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReplayOutcome {
    /// The op was replayed (2xx).
    Applied,
    /// The op was refused or failed; `body` is its error.
    Failed,
    /// Not attempted: an earlier op failed and `stop_on_error` was on.
    Skipped,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ReplayBatchResult {
    /// The op's position in `ops`.
    pub index: usize,
    pub outcome: ReplayOutcome,
    /// The HTTP status the single-op call would have answered (absent when skipped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The single-op call's JSON body (its ack, or its error body).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub body: Option<Value>,
    /// The single-op call's `X-Madar-Sync-Seq`, when it carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_seq: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ReplayBatchResponse {
    /// The negotiated sync protocol.
    pub protocol: u16,
    /// One per op, in order.
    pub results: Vec<ReplayBatchResult>,
    /// How many ops were applied.
    pub applied: usize,
    /// The index the batch stopped at, when it stopped early.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<usize>,
    /// The highest `sync_seq` of the batch: a pull from here sees every op.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_seq: Option<i64>,
}

async fn to_result(index: usize, resp: HttpResponse) -> ReplayBatchResult {
    let status = resp.status();
    let sync_seq = resp
        .headers()
        .get(super::handlers::SYNC_SEQ_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let bytes = actix_web::body::to_bytes(resp.into_body())
        .await
        .unwrap_or_default();
    let body = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            Value::String(String::from_utf8_lossy(&bytes).into_owned())
        }))
    };
    ReplayBatchResult {
        index,
        outcome: if status.is_success() {
            ReplayOutcome::Applied
        } else {
            ReplayOutcome::Failed
        },
        status: Some(status.as_u16()),
        body,
        sync_seq,
    }
}

/// POST /sync/replay/batch
#[utoipa::path(post, path = "/sync/replay/batch", tag = "sync",
    request_body = ReplayBatchRequest,
    responses(
        (status = 200, description = "Each op's result, in order (the batch itself succeeded even when an op did not)", body = ReplayBatchResponse),
        AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn replay_batch(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    body: web::Json<ReplayBatchRequest>,
) -> Result<HttpResponse, AppError> {
    let body = body.into_inner();
    if body.ops.is_empty() {
        return Err(AppError::Coded {
            status: 400,
            code: "EMPTY_BATCH",
            reason: "A batch carries at least one op".into(),
        });
    }
    if body.ops.len() > MAX_OPS {
        return Err(AppError::Coded {
            status: 400,
            code: "BATCH_TOO_LARGE",
            reason: format!("A batch carries at most {MAX_OPS} ops"),
        });
    }
    let protocol = super::protocol::negotiate(Some(body.protocol.unwrap_or(super::protocol::V2)));
    let stop_on_error = body.stop_on_error.unwrap_or(true);
    let mut results = Vec::with_capacity(body.ops.len());
    let mut stopped_at = None;
    for (index, op) in body.ops.into_iter().enumerate() {
        if stopped_at.is_some() {
            results.push(ReplayBatchResult {
                index,
                outcome: ReplayOutcome::Skipped,
                status: None,
                body: None,
                sync_seq: None,
            });
            continue;
        }
        let resp = match super::handlers::replay(req.clone(), pool.clone(), hub.clone(), web::Json(op)).await {
            Ok(r) => r,
            Err(e) => e.error_response(),
        };
        let r = to_result(index, resp).await;
        if r.outcome == ReplayOutcome::Failed && stop_on_error {
            stopped_at = Some(index);
        }
        results.push(r);
    }
    let applied = results
        .iter()
        .filter(|r| r.outcome == ReplayOutcome::Applied)
        .count();
    let sync_seq = results.iter().filter_map(|r| r.sync_seq).max();
    Ok(HttpResponse::Ok().json(ReplayBatchResponse {
        protocol,
        results,
        applied,
        stopped_at,
        sync_seq,
    }))
}
