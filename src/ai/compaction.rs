//! Rolling compaction — unlimited context at a bounded cost.
//!
//! A conversation window has to be bounded or every message gets more expensive
//! than the last and eventually stops fitting. The usual answer is to drop the
//! oldest turns, which is cheap and quietly wrong: the merchant asked about a
//! branch twenty messages ago, the assistant has forgotten, and nothing says so.
//!
//! So instead of dropping, fold. The most recent [`VERBATIM_TURNS`] replay in
//! full — with their specs, so a follow-up can still say "and last month?" —
//! and everything older is condensed into one running summary that is rewritten
//! each time the window slides. A conversation can then run indefinitely while
//! the replayed context stays roughly constant in size.
//!
//! # Why it runs in the background
//!
//! Compaction is a model call. Doing it inline would add its latency to the
//! message that happened to cross the threshold — one message in seven would be
//! mysteriously slow. It is spawned instead, so the merchant never waits, and
//! the next turn simply replays a couple more verbatim turns if it has not
//! finished yet ([`store::replay_context`] takes everything the summary does not
//! cover, up to [`MAX_VERBATIM_TURNS`]).
//!
//! # Why it is safe to fail
//!
//! Nothing depends on compaction succeeding. If the model call fails, the
//! summary and `summarized_through_seq` are left exactly as they were, so the
//! next turn replays more verbatim history and tries again. The failure is
//! reported, and the conversation keeps working. That is deliberate: an
//! assistant that stops answering because a *summary* could not be written
//! would be a much worse outcome than one carrying slightly more context than
//! intended.

use std::sync::Arc;

use uuid::Uuid;

use crate::{
    db::Db,
    observability::report::{self, Failure},
};

use super::{
    llm::{Completion, LlmProvider, Message},
    store,
};

/// Turns kept verbatim at the tail of the window. Six is enough to carry a
/// normal back-and-forth ("top products" → "and last month?" → "just Marina?")
/// without the replayed prefix growing without bound.
pub const VERBATIM_TURNS: i32 = 6;

/// Hard ceiling on verbatim turns replayed when compaction has not caught up.
/// The headroom above [`VERBATIM_TURNS`] is what stops a lagging or failed
/// summarization from silently dropping context.
pub const MAX_VERBATIM_TURNS: i64 = 14;

/// Longest stored summary. A summary that grows every round is just the
/// unbounded window again wearing a hat, so the prompt is told to rewrite
/// within a budget and the result is truncated if it ignores that.
pub const MAX_SUMMARY_CHARS: usize = 1_400;

/// Output cap for the summarizing call.
const MAX_TOKENS: u32 = 400;

const SYSTEM: &str = "\
You maintain a running summary of a conversation between a restaurant merchant \
and an analytics assistant. You will be given the summary so far (possibly \
empty) and the next few exchanges. Rewrite the summary so it covers everything, \
old and new.

What matters, in order:
- Standing context the merchant established and has not retracted: which branch, \
which period, which products or staff they are focused on, what they are trying \
to find out.
- Findings already reported, with their figures, so the assistant does not \
contradict itself or re-run work.
- Questions asked and answered, in one clause each.

Rules:
- Write plain prose, at most 6 sentences. No markdown, no bullet points.
- Keep concrete figures and names; drop pleasantries and phrasing.
- Never invent anything that is not in the material you were given.
- Write in the same language the merchant is using.
- If the summary so far and the new exchanges conflict, the NEWER one wins.";

/// Build the user turn for the summarizing call.
fn user_text(existing: &str, turns: &[store::StoredTurn]) -> String {
    let mut out = String::with_capacity(1024);
    if existing.trim().is_empty() {
        out.push_str("Summary so far: (none — this is the start of the conversation)\n\n");
    } else {
        out.push_str("Summary so far:\n");
        out.push_str(existing.trim());
        out.push_str("\n\n");
    }
    out.push_str("New exchanges to fold in:\n");
    for turn in turns {
        out.push_str(&format!("\n[{}] Merchant: {}\n", turn.seq, turn.question));
        match turn
            .answer
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            Some(answer) => out.push_str(&format!("    Assistant: {answer}\n")),
            None => out.push_str("    Assistant: (no answer produced)\n"),
        }
    }
    out.push_str(&format!(
        "\nRewrite the summary now, covering everything above in at most \
         {MAX_SUMMARY_CHARS} characters."
    ));
    out
}

/// Trim a model-written summary to the budget, on a word boundary.
fn bound(summary: &str) -> String {
    let s = summary.trim();
    if s.chars().count() <= MAX_SUMMARY_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX_SUMMARY_CHARS).collect();
    match cut.rsplit_once(' ') {
        Some((head, _)) if head.chars().count() > MAX_SUMMARY_CHARS / 2 => format!("{head}…"),
        _ => format!("{cut}…"),
    }
}

/// Fold this conversation's uncondensed history into its summary, if it has
/// grown past the verbatim window.
///
/// Returns `Ok(true)` when a summary was written. Safe to call after every
/// turn: it is a cheap `SELECT` when there is nothing to do.
pub async fn compact(
    db: &Db,
    provider: &dyn LlmProvider,
    conversation_id: Uuid,
) -> Result<bool, crate::errors::AppError> {
    let Some((existing, turns, through)) =
        store::pending_compaction(db, conversation_id, VERBATIM_TURNS).await?
    else {
        return Ok(false);
    };

    // The sequence the summary covered when this pass started. `commit_summary`
    // requires it to be unchanged, so a pass that lost a race writes nothing
    // rather than rewinding the winner's work.
    let expected_through = through - turns.len() as i32;

    let messages = [Message::User(user_text(&existing, &turns))];
    let turn = provider
        .complete(Completion {
            system: SYSTEM,
            messages: &messages,
            tools: &[],
            max_tokens: MAX_TOKENS,
            // Prose, not a tool call — this is the one place in the module that
            // wants the model to just write.
            force_tool: false,
        })
        .await
        .map_err(crate::errors::AppError::from)?;

    let summary = match turn {
        super::llm::Turn::Text(text) if !text.trim().is_empty() => bound(&text),
        // A model that answered with a tool call, or with nothing, has not
        // produced a summary. Leaving the old one in place is correct: the next
        // turn replays more verbatim history and tries again.
        _ => {
            return Err(crate::errors::AppError::ServiceUnavailable(
                "the model did not return a summary".into(),
            ));
        }
    };

    store::commit_summary(db, conversation_id, &summary, through, expected_through).await
}

/// Run [`compact`] in the background.
///
/// Fire and forget by design: the merchant's message must never wait on a
/// summary being rewritten. Failures are reported and the conversation carries
/// on with a longer verbatim window until the next attempt succeeds.
pub fn spawn(db: Db, provider: Arc<dyn LlmProvider>, conversation_id: Uuid) {
    tokio::spawn(async move {
        // Its own hub with a cleared scope: this outlives the request that
        // started it and would otherwise land on whichever worker thread picks
        // it up, arriving attributed to an unrelated merchant's request.
        let hub = Arc::new(sentry::Hub::new_from_top(sentry::Hub::current()));
        hub.configure_scope(|scope| {
            scope.clear();
            scope.set_tag("job", "ai_compaction");
        });

        let outcome = compact(&db, provider.as_ref(), conversation_id).await;

        sentry::Hub::run(hub, || match outcome {
            Ok(_) => {}
            Err(e) => report::report(
                Failure::new("ai", "compact_conversation")
                    .with("conversation_id", conversation_id.to_string()),
                &e,
            ),
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn turn(seq: i32, q: &str, a: Option<&str>) -> store::StoredTurn {
        store::StoredTurn {
            id: Uuid::new_v4(),
            seq,
            question: q.into(),
            answer: a.map(str::to_string),
            kind: "answer".into(),
            specs: serde_json::json!([]),
            provider: Some("mock".into()),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn the_prompt_carries_the_existing_summary_and_the_new_exchanges() {
        let text = user_text(
            "The merchant is looking at the Marina branch.",
            &[
                turn(3, "top products", Some("Latte led on revenue.")),
                turn(4, "and last month?", Some("Mocha led last month.")),
            ],
        );
        assert!(text.contains("Marina branch"));
        assert!(text.contains("[3] Merchant: top products"));
        assert!(text.contains("Assistant: Mocha led last month."));
        assert!(text.contains(&MAX_SUMMARY_CHARS.to_string()));
    }

    #[test]
    fn an_empty_starting_summary_says_so_rather_than_leaving_a_blank() {
        // A blank line where context should be reads to a model as "there was
        // context and it is missing".
        let text = user_text("   ", &[turn(1, "revenue", Some("163 EGP."))]);
        assert!(text.contains("(none — this is the start of the conversation)"));
    }

    #[test]
    fn a_turn_with_no_answer_is_labelled_not_silently_skipped() {
        let text = user_text("", &[turn(1, "revenue", None)]);
        assert!(text.contains("(no answer produced)"));
    }

    #[test]
    fn a_summary_is_bounded_on_a_word_boundary() {
        // An unbounded summary is the unbounded window again by another name.
        let long = "revenue was strong ".repeat(400);
        let out = bound(&long);
        assert!(out.chars().count() <= MAX_SUMMARY_CHARS + 1);
        assert!(out.ends_with('…'));
        assert!(!out.trim_end_matches('…').ends_with(' '));
    }

    #[test]
    fn a_summary_within_budget_is_untouched() {
        let s = "The merchant is reviewing the Marina branch for last month.";
        assert_eq!(bound(s), s);
        assert_eq!(bound(&format!("  {s}  ")), s);
    }

    #[test]
    fn the_verbatim_window_is_smaller_than_the_ceiling() {
        // The gap is the headroom that keeps a lagging compaction from
        // dropping context. Equal values would mean a failed summary loses
        // turns silently, which is the exact failure this design avoids.
        assert!(MAX_VERBATIM_TURNS > VERBATIM_TURNS as i64);
    }
}
