//! The agent loop.
//!
//! A single model call cannot recover. It picks one query, and if that query was
//! wrong — a bad dataset, a measure that does not exist, a filter that excluded
//! everything — the merchant gets an empty table or an error, and the
//! conversation ends there.
//!
//! This is a bounded loop instead: the model calls a tool, sees the outcome, and
//! decides what to do next. A rejected spec comes back with the valid options
//! and becomes a correction. An empty result becomes an explanation rather than
//! a blank grid. Two questions in one message become two queries.
//!
//! It is bounded in three ways, because an unbounded agent against a paid API is
//! a cost incident waiting to happen: at most [`MAX_STEPS`] model calls, at most
//! [`MAX_QUERIES`] executed queries, and every provider call already carries its
//! own HTTP timeout.

use std::time::Instant;

use serde_json::{Value, json};

use super::{
    llm::{Completion, LlmProvider, Message, ProviderError, ToolCall, Turn},
    prompt,
    telemetry::TurnLog,
    tools::{self, QueryData, ToolCtx, ToolOutcome},
};

/// Maximum model calls in one turn. Four is enough for: query, correct, query,
/// answer — the deepest useful path observed — without letting a confused model
/// spend unbounded tokens.
pub const MAX_STEPS: usize = 4;
/// Maximum queries actually executed in one turn.
pub const MAX_QUERIES: usize = 3;
/// Output cap per model call.
const MAX_TOKENS: u32 = 700;

/// What the turn produced. Every variant is a *successful* HTTP response: a
/// question the assistant cannot answer is a conversational outcome, not a 400.
pub enum AgentOutcome {
    Answer {
        text: String,
        results: Vec<QueryData>,
    },
    /// The model needs one thing from the merchant before it can continue.
    Clarify { question: String },
    /// The loop ran out of steps without reaching an answer.
    Exhausted {
        text: String,
        results: Vec<QueryData>,
    },
}

/// Run one turn of the conversation.
pub async fn run(
    provider: &dyn LlmProvider,
    ctx: &ToolCtx<'_>,
    grounding: &str,
    history: &[(String, String)],
    question: &str,
    log: &mut TurnLog,
) -> Result<AgentOutcome, ProviderError> {
    let mut messages: Vec<Message> = Vec::with_capacity(history.len() * 2 + 2 + MAX_STEPS * 2);

    // Grounding first, then a compact replay of earlier turns — the question and
    // what was answered, never the result tables. Per-message cost then stays
    // flat as a conversation grows.
    messages.push(Message::User(grounding.to_string()));
    for (q, a) in history {
        messages.push(Message::User(q.clone()));
        messages.push(Message::Assistant {
            text: Some(a.clone()),
            calls: Vec::new(),
        });
    }
    messages.push(Message::User(question.to_string()));

    let mut results: Vec<QueryData> = Vec::new();

    for step in 0..MAX_STEPS {
        let started = Instant::now();
        let turn = provider
            .complete(Completion {
                system: prompt::system(),
                messages: &messages,
                tools: tools::tool_defs(),
                max_tokens: MAX_TOKENS,
                force_tool: true,
            })
            .await?;
        log.record_model_call(started.elapsed());

        let calls = match turn {
            Turn::Calls(c) if !c.is_empty() => c,
            // A provider that answered in prose despite `force_tool` is taken at
            // its word rather than being made to retry — the merchant gets the
            // sentence, just without a table.
            Turn::Text(t) => {
                log.finished("text_fallback");
                return Ok(AgentOutcome::Answer { text: t, results });
            }
            Turn::Calls(_) => {
                log.finished("empty_call_list");
                return Ok(AgentOutcome::Exhausted {
                    text: fallback_text(&results),
                    results,
                });
            }
        };

        // Record the assistant turn before dispatching, so the tool results that
        // follow are correctly attached to it in the replayed transcript.
        messages.push(Message::Assistant {
            text: None,
            calls: calls.clone(),
        });

        let mut tool_results: Vec<Message> = Vec::with_capacity(calls.len());
        for call in &calls {
            log.record_tool(&call.name);

            // The query budget is enforced here rather than trusted to the
            // prompt: a model that loops on queries stops costing money.
            if is_query_tool(&call.name) && results.len() >= MAX_QUERIES {
                tool_results.push(tool_message(
                    call,
                    json!({
                        "error": format!(
                            "Query budget reached ({MAX_QUERIES} per message). Answer with \
                             what you already have."
                        )
                    }),
                ));
                continue;
            }

            match tools::dispatch(ctx, &call.name, &call.args).await {
                ToolOutcome::Answer(text) => {
                    log.finished("answered");
                    return Ok(AgentOutcome::Answer { text, results });
                }
                ToolOutcome::Clarify(question) => {
                    log.finished("clarified");
                    return Ok(AgentOutcome::Clarify { question });
                }
                ToolOutcome::Info(v) => tool_results.push(tool_message(call, v)),
                ToolOutcome::Error(e) => {
                    log.record_tool_error(&e);
                    // Handed back rather than raised: this is the mechanism that
                    // lets the next step be a correction.
                    tool_results.push(tool_message(call, json!({ "error": e })));
                }
                ToolOutcome::Data(data) => {
                    let payload = summarize_for_model(&data);
                    log.record_rows(data.result.row_count);
                    results.push(*data);
                    tool_results.push(tool_message(call, payload));
                }
            }
        }
        messages.extend(tool_results);

        // On the last step, tell the model plainly that it must answer now —
        // otherwise it spends the step on another query it will never see.
        if step + 1 == MAX_STEPS - 1 {
            messages.push(Message::User(
                "This is your last step. Call `answer` now with what you have.".into(),
            ));
        }
    }

    log.finished("exhausted");
    Ok(AgentOutcome::Exhausted {
        text: fallback_text(&results),
        results,
    })
}

fn is_query_tool(name: &str) -> bool {
    name == tools::QUERY_METRICS || name == tools::RUN_PRESET
}

fn tool_message(call: &ToolCall, content: Value) -> Message {
    Message::ToolResult {
        id: call.id.clone(),
        name: call.name.clone(),
        content,
    }
}

/// What the model sees of a result: the column metadata, the row count, and a
/// sample. Enough to state a finding and quote figures; not the whole table,
/// which the client already has.
fn summarize_for_model(data: &QueryData) -> Value {
    let mut v = data.result.to_model_json(tools::MODEL_ROW_SAMPLE);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("scope".into(), json!(data.scope.label));
        if let Some(unmatched) = &data.scope.unmatched_branch {
            obj.insert(
                "warning".into(),
                json!(format!(
                    "No branch matched '{unmatched}', so this covers all branches the user \
                     can see. Say so in your answer."
                )),
            );
        }
        if data.result.is_empty() {
            // Stated explicitly: an empty array is otherwise easy for a model to
            // narrate as "zero revenue", which is a different claim.
            obj.insert(
                "note".into(),
                json!(
                    "No rows matched. Tell the merchant there was no activity for this \
                       query — do NOT report it as a zero total."
                ),
            );
        }
        if let (Some(f), Some(t)) = (data.result.period.from, data.result.period.to) {
            obj.insert(
                "period".into(),
                json!({ "from": f.to_rfc3339(), "to": t.to_rfc3339() }),
            );
        }
    }
    v
}

/// Said when the loop ends without the model calling `answer`. It reports the
/// state honestly rather than fabricating a finding.
fn fallback_text(results: &[QueryData]) -> String {
    if results.is_empty() {
        "I couldn't work that one out. Try asking about a specific figure and period — \
         for example, revenue last week, or your top products this month."
            .to_string()
    } else {
        "Here is the data I found for that. I wasn't able to summarize it — the table \
         below has the figures."
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::execute::QueryResult;
    use crate::analytics::scope::ScopeInfo;
    use crate::analytics::spec::{QuerySpec, ResolvedPeriod};
    use crate::analytics::types::{Column, ColumnKind, Grain, Viz};

    fn data(rows: usize, unmatched: Option<&str>) -> QueryData {
        QueryData {
            result: QueryResult {
                columns: vec![Column {
                    key: "revenue",
                    label: "Revenue",
                    kind: ColumnKind::Money,
                }],
                rows: (0..rows)
                    .map(|i| {
                        let mut m = serde_json::Map::new();
                        m.insert("revenue".into(), json!(i * 100));
                        m
                    })
                    .collect(),
                row_count: rows,
                truncated: false,
                grain: Grain::Scalar,
                viz: Viz::Kpi,
                facet_by: None,
                period: ResolvedPeriod {
                    from: None,
                    to: None,
                },
            },
            spec: QuerySpec::default(),
            scope: ScopeInfo {
                all_branches: true,
                branches: vec!["A".into()],
                label: "All branches (1)".into(),
                unmatched_branch: unmatched.map(str::to_string),
            },
            title: None,
            preset_id: None,
        }
    }

    #[test]
    fn an_empty_result_is_labelled_so_it_is_not_narrated_as_zero() {
        let v = summarize_for_model(&data(0, None));
        let note = v["note"].as_str().unwrap();
        assert!(note.contains("do NOT report it as a zero"));
    }

    #[test]
    fn a_non_empty_result_carries_no_empty_note() {
        let v = summarize_for_model(&data(3, None));
        assert!(v.get("note").is_none());
        assert_eq!(v["row_count"], 3);
        assert_eq!(v["scope"], "All branches (1)");
    }

    #[test]
    fn an_unmatched_branch_becomes_a_warning_the_model_must_relay() {
        let v = summarize_for_model(&data(2, Some("Alexandria")));
        assert!(v["warning"].as_str().unwrap().contains("Alexandria"));
    }

    #[test]
    fn the_model_sees_a_sample_not_the_whole_table() {
        let v = summarize_for_model(&data(500, None));
        assert_eq!(v["rows"].as_array().unwrap().len(), tools::MODEL_ROW_SAMPLE);
        // ...but is told the real size, so it never says "40 products".
        assert_eq!(v["row_count"], 500);
        assert_eq!(v["truncated"], true);
    }

    #[test]
    fn the_fallback_never_invents_a_finding() {
        assert!(fallback_text(&[]).contains("couldn't work that one out"));
        let with = fallback_text(std::slice::from_ref(&data(1, None)));
        assert!(with.contains("table below"));
        // Neither variant states a figure.
        assert!(!with.contains('0'));
    }

    #[test]
    fn query_tools_are_the_ones_that_consume_the_budget() {
        assert!(is_query_tool(tools::QUERY_METRICS));
        assert!(is_query_tool(tools::RUN_PRESET));
        // Introspection and answering are free — budgeting them would make the
        // model unable to recover from its own mistakes.
        assert!(!is_query_tool(tools::DESCRIBE_DATASET));
        assert!(!is_query_tool(tools::ANSWER));
        assert!(!is_query_tool(tools::CLARIFY));
    }
}
