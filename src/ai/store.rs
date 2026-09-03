//! Persistence for conversations.
//!
//! Every query here is fenced twice: RLS puts the connection in one
//! organization, and every statement additionally filters on `user_id`. Both
//! are needed — the tenant pool is per-org, not per-user, so RLS alone would
//! let one manager read another's chats inside the same merchant.
//!
//! # Sequence allocation
//!
//! `seq` is allocated by bumping `turn_count` under a row lock
//! (`SELECT … FOR UPDATE`) in the same transaction that inserts the turn. Two
//! messages sent into one conversation at the same moment — a double-tap, or a
//! phone and a laptop — therefore get 5 and 6 rather than both getting 5 and one
//! losing to the unique constraint.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{analytics::spec::QuerySpec, db::Db, errors::AppError};

/// Longest stored title. The first question, truncated — a model call purely to
/// name a chat is not worth the latency or the cost, and a merchant can rename.
const MAX_TITLE_LEN: usize = 80;

/// One stored exchange.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StoredTurn {
    pub id: Uuid,
    pub seq: i32,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// `answer` | `clarify` | `incomplete`.
    pub kind: String,
    /// The queries that produced the answer — `[{title, preset_id, spec}]`.
    /// Re-running these is how a reopened conversation shows CURRENT figures
    /// rather than the numbers that were true when it was first asked.
    pub specs: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A conversation without its turns, for the list view.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConversationSummary {
    pub id: Uuid,
    pub title: String,
    pub turn_count: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// True once older turns have been folded into a summary — surfaced so a
    /// client can say "earlier messages condensed" rather than appearing to
    /// have lost them.
    pub compacted: bool,
}

/// A conversation with its turns.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConversationDetail {
    #[serde(flatten)]
    pub summary: ConversationSummary,
    /// The running summary of everything before the verbatim window. Returned
    /// so the UI can show what was condensed instead of a silent gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condensed: Option<String>,
    pub turns: Vec<StoredTurn>,
}

/// The replay context for one turn of the agent: a condensed head plus the
/// verbatim tail.
#[derive(Debug, Clone, Default)]
pub struct ReplayContext {
    pub summary: Option<String>,
    pub turns: Vec<StoredTurn>,
}

/// What a completed turn contributes back to the store.
#[derive(Debug, Clone, Deserialize)]
pub struct TurnRecord {
    pub question: String,
    pub answer: Option<String>,
    pub kind: String,
    pub specs: Value,
    pub provider: Option<String>,
}

fn title_from(question: &str) -> String {
    let trimmed = question.trim();
    if trimmed.chars().count() <= MAX_TITLE_LEN {
        return trimmed.to_string();
    }
    // Truncate on a character boundary, then back off to the last word so a
    // title never ends mid-word.
    let cut: String = trimmed.chars().take(MAX_TITLE_LEN).collect();
    match cut.rsplit_once(' ') {
        Some((head, _)) if head.chars().count() > MAX_TITLE_LEN / 2 => format!("{head}…"),
        _ => format!("{cut}…"),
    }
}

/// Create a conversation, titled from its first question.
pub async fn create(
    db: &Db,
    org_id: Uuid,
    user_id: Uuid,
    locale: &str,
    first_question: &str,
) -> Result<Uuid, AppError> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO ai_conversations (org_id, user_id, title, locale) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(title_from(first_question))
    .bind(locale)
    .fetch_one(db.get_ref())
    .await?;
    Ok(id)
}

/// Confirm a conversation exists, belongs to this user, and is not deleted.
///
/// Returns `NotFound` rather than `Forbidden` for another user's conversation:
/// the id is opaque, and distinguishing "not yours" from "does not exist" would
/// confirm the existence of chats the caller may not read.
pub async fn ensure_owned(db: &Db, id: Uuid, user_id: Uuid) -> Result<(), AppError> {
    let exists: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM ai_conversations \
         WHERE id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(db.get_ref())
    .await?;
    exists
        .map(|_| ())
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))
}

/// The context to replay for the next turn: the running summary, plus every
/// turn it does not yet cover.
///
/// `max_verbatim` bounds the tail. It is a *ceiling*, not the compaction
/// threshold: turns are normally summarized well before this many accumulate,
/// and the extra headroom is what keeps a lagging or failed summarization from
/// silently dropping context. If it is ever hit, the oldest uncondensed turns
/// are the ones left out — the summary will cover them on the next successful
/// compaction.
pub async fn replay_context(
    db: &Db,
    conversation_id: Uuid,
    user_id: Uuid,
    max_verbatim: i64,
) -> Result<ReplayContext, AppError> {
    let head: Option<(Option<String>, i32)> = sqlx::query_as(
        "SELECT summary, summarized_through_seq FROM ai_conversations \
         WHERE id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_optional(db.get_ref())
    .await?;
    let Some((summary, through)) = head else {
        return Ok(ReplayContext::default());
    };

    // Newest-first with a limit, then reversed: the tail is what matters, and
    // ordering ascending with an offset would need the count first.
    let mut turns = fetch_turns(
        db,
        conversation_id,
        "AND m.seq > $2 ORDER BY m.seq DESC LIMIT $3",
        Some(through),
        Some(max_verbatim),
    )
    .await?;
    turns.reverse();

    Ok(ReplayContext { summary, turns })
}

async fn fetch_turns(
    db: &Db,
    conversation_id: Uuid,
    tail: &str,
    after_seq: Option<i32>,
    limit: Option<i64>,
) -> Result<Vec<StoredTurn>, AppError> {
    let sql = format!(
        "SELECT m.id, m.seq, m.question, m.answer, m.kind, m.specs, m.provider, m.created_at \
         FROM ai_messages m WHERE m.conversation_id = $1 {tail}"
    );
    let mut q = sqlx::query(&sql).bind(conversation_id);
    if let Some(seq) = after_seq {
        q = q.bind(seq);
    }
    if let Some(limit) = limit {
        q = q.bind(limit);
    }
    let rows = q.fetch_all(db.get_ref()).await?;
    Ok(rows
        .into_iter()
        .map(|r| StoredTurn {
            id: r.get("id"),
            seq: r.get("seq"),
            question: r.get("question"),
            answer: r.get("answer"),
            kind: r.get("kind"),
            specs: r.get("specs"),
            provider: r.get("provider"),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// Append a completed turn, allocating its sequence number under a row lock.
///
/// Returns the new sequence number and the conversation's turn count, which is
/// what the compaction check keys off.
pub async fn append_turn(
    db: &Db,
    conversation_id: Uuid,
    org_id: Uuid,
    user_id: Uuid,
    record: &TurnRecord,
) -> Result<i32, AppError> {
    let mut tx = db.begin().await?;

    // The lock is what makes concurrent sends in one conversation safe. It is
    // held for one INSERT, so contention is a non-issue.
    let seq: Option<i32> = sqlx::query_scalar(
        "SELECT turn_count + 1 FROM ai_conversations \
         WHERE id = $1 AND user_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(seq) = seq else {
        return Err(AppError::NotFound("Conversation not found".into()));
    };

    sqlx::query(
        "INSERT INTO ai_messages \
             (conversation_id, org_id, seq, question, answer, kind, specs, provider) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(conversation_id)
    .bind(org_id)
    .bind(seq)
    .bind(&record.question)
    .bind(&record.answer)
    .bind(&record.kind)
    .bind(&record.specs)
    .bind(&record.provider)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE ai_conversations \
         SET turn_count = $2, last_turn_at = now(), updated_at = now() \
         WHERE id = $1",
    )
    .bind(conversation_id)
    .bind(seq)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(seq)
}

/// The turns a compaction pass should fold in, and the sequence it would then
/// have covered. `None` when there is nothing to do.
pub async fn pending_compaction(
    db: &Db,
    conversation_id: Uuid,
    keep_verbatim: i32,
) -> Result<Option<(String, Vec<StoredTurn>, i32)>, AppError> {
    let head: Option<(Option<String>, i32, i32)> = sqlx::query_as(
        "SELECT summary, summarized_through_seq, turn_count FROM ai_conversations \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(conversation_id)
    .fetch_optional(db.get_ref())
    .await?;
    let Some((summary, through, turn_count)) = head else {
        return Ok(None);
    };

    // Everything except the most recent `keep_verbatim` turns is eligible.
    let target = turn_count - keep_verbatim;
    if target <= through {
        return Ok(None);
    }

    // Written out rather than routed through `fetch_turns`, whose parameter
    // slots are positional: this needs two sequence bounds and no limit.
    let rows = sqlx::query(
        "SELECT m.id, m.seq, m.question, m.answer, m.kind, m.specs, m.provider, m.created_at \
         FROM ai_messages m \
         WHERE m.conversation_id = $1 AND m.seq > $2 AND m.seq <= $3 \
         ORDER BY m.seq ASC",
    )
    .bind(conversation_id)
    .bind(through)
    .bind(target)
    .fetch_all(db.get_ref())
    .await?;
    let turns: Vec<StoredTurn> = rows
        .into_iter()
        .map(|r| StoredTurn {
            id: r.get("id"),
            seq: r.get("seq"),
            question: r.get("question"),
            answer: r.get("answer"),
            kind: r.get("kind"),
            specs: r.get("specs"),
            provider: r.get("provider"),
            created_at: r.get("created_at"),
        })
        .collect();
    if turns.is_empty() {
        return Ok(None);
    }
    Ok(Some((summary.unwrap_or_default(), turns, target)))
}

/// Store a new summary, but only if nothing has advanced past it meanwhile.
///
/// The conditional `WHERE summarized_through_seq = $3` makes compaction
/// idempotent and safe to run concurrently: a second pass that lost the race
/// updates nothing and returns `false` rather than rewinding the summary and
/// dropping the turns the winner already folded in.
pub async fn commit_summary(
    db: &Db,
    conversation_id: Uuid,
    summary: &str,
    through_seq: i32,
    expected_through: i32,
) -> Result<bool, AppError> {
    let affected = sqlx::query(
        "UPDATE ai_conversations SET summary = $2, summarized_through_seq = $3, \
             updated_at = now() \
         WHERE id = $1 AND summarized_through_seq = $4",
    )
    .bind(conversation_id)
    .bind(summary)
    .bind(through_seq)
    .bind(expected_through)
    .execute(db.get_ref())
    .await?
    .rows_affected();
    Ok(affected == 1)
}

/// The caller's conversations, most recently used first.
pub async fn list(
    db: &Db,
    user_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<ConversationSummary>, AppError> {
    let rows = sqlx::query(
        "SELECT id, title, turn_count, last_turn_at, created_at, \
                (summarized_through_seq > 0) AS compacted \
         FROM ai_conversations \
         WHERE user_id = $1 AND deleted_at IS NULL \
         ORDER BY COALESCE(last_turn_at, created_at) DESC \
         LIMIT $2 OFFSET $3",
    )
    .bind(user_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(db.get_ref())
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ConversationSummary {
            id: r.get("id"),
            title: r.get("title"),
            turn_count: r.get("turn_count"),
            last_turn_at: r.get("last_turn_at"),
            created_at: r.get("created_at"),
            compacted: r.get("compacted"),
        })
        .collect())
}

/// One conversation with its turns, newest `limit` turns.
pub async fn detail(
    db: &Db,
    conversation_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<ConversationDetail, AppError> {
    let row = sqlx::query(
        "SELECT id, title, turn_count, last_turn_at, created_at, summary, \
                (summarized_through_seq > 0) AS compacted \
         FROM ai_conversations \
         WHERE id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_optional(db.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let mut turns = fetch_turns(
        db,
        conversation_id,
        "ORDER BY m.seq DESC LIMIT $2",
        None,
        Some(limit),
    )
    .await?;
    turns.reverse();

    Ok(ConversationDetail {
        summary: ConversationSummary {
            id: row.get("id"),
            title: row.get("title"),
            turn_count: row.get("turn_count"),
            last_turn_at: row.get("last_turn_at"),
            created_at: row.get("created_at"),
            compacted: row.get("compacted"),
        },
        condensed: row.get("summary"),
        turns,
    })
}

/// Rename a conversation.
pub async fn rename(
    db: &Db,
    conversation_id: Uuid,
    user_id: Uuid,
    title: &str,
) -> Result<(), AppError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AppError::BadRequest("title cannot be empty".into()));
    }
    let affected = sqlx::query(
        "UPDATE ai_conversations SET title = $3, updated_at = now() \
         WHERE id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(conversation_id)
    .bind(user_id)
    .bind(title_from(title))
    .execute(db.get_ref())
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(AppError::NotFound("Conversation not found".into()));
    }
    Ok(())
}

/// Soft-delete a conversation.
pub async fn delete(db: &Db, conversation_id: Uuid, user_id: Uuid) -> Result<(), AppError> {
    let affected = sqlx::query(
        "UPDATE ai_conversations SET deleted_at = now(), updated_at = now() \
         WHERE id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(conversation_id)
    .bind(user_id)
    .execute(db.get_ref())
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(AppError::NotFound("Conversation not found".into()));
    }
    Ok(())
}

/// The spec a stored turn ran, for replay. The first one only: a turn that ran
/// several queries is rare, and the first is the one a follow-up almost always
/// means.
pub fn primary_spec(turn: &StoredTurn) -> Option<QuerySpec> {
    turn.specs
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.get("spec"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_the_question_trimmed_to_a_word_boundary() {
        assert_eq!(title_from("  top products  "), "top products");
        let long = "what were my ten best selling products across every branch during the whole of last month please";
        let title = title_from(long);
        assert!(title.chars().count() <= MAX_TITLE_LEN + 1);
        assert!(title.ends_with('…'));
        // Never cut mid-word.
        assert!(!title.trim_end_matches('…').ends_with(' '));
        assert!(long.starts_with(title.trim_end_matches('…')));
    }

    #[test]
    fn a_title_with_no_spaces_still_truncates() {
        // A pasted identifier, or any language that does not space its words.
        let title = title_from(&"x".repeat(200));
        assert!(title.chars().count() <= MAX_TITLE_LEN + 1);
    }

    #[test]
    fn arabic_titles_are_cut_by_character_not_byte() {
        // A byte cut here would both truncate at the wrong place and risk
        // splitting a codepoint.
        let arabic = "أعلى المنتجات مبيعا ".repeat(20);
        let title = title_from(&arabic);
        assert!(title.chars().count() <= MAX_TITLE_LEN + 1);
    }

    #[test]
    fn the_primary_spec_is_read_back_out_of_a_stored_turn() {
        let turn = StoredTurn {
            id: Uuid::new_v4(),
            seq: 1,
            question: "top products".into(),
            answer: Some("Latte".into()),
            kind: "answer".into(),
            specs: serde_json::json!([{
                "title": "Top products",
                "preset_id": "top_products",
                "spec": { "dataset": "order_items", "dimensions": ["product"] }
            }]),
            provider: Some("mock".into()),
            created_at: Utc::now(),
        };
        let spec = primary_spec(&turn).expect("the stored spec must round-trip");
        assert_eq!(spec.dataset, "order_items");
        assert_eq!(spec.dimensions, vec!["product".to_string()]);
    }

    #[test]
    fn a_turn_that_ran_no_query_has_no_spec() {
        let turn = StoredTurn {
            id: Uuid::new_v4(),
            seq: 1,
            question: "how is it going".into(),
            answer: Some("Which figure?".into()),
            kind: "clarify".into(),
            specs: serde_json::json!([]),
            provider: None,
            created_at: Utc::now(),
        };
        assert!(primary_spec(&turn).is_none());
    }
}
