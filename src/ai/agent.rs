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

use crate::ai::pseudonym::Directory as Pseudonyms;
use crate::analytics::spec::QuerySpec;

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
/// Named in the replay line so the model is told which tool a prior spec goes
/// to rather than left to infer it.
const QUERY_TOOL: &str = tools::QUERY_METRICS;

/// One earlier exchange, as the agent replays it.
///
/// The `spec` is what makes a conversation actually conversational. Replaying
/// only prose means "and last month?" is answered by a model reconstructing its
/// own previous query from its own previous summary — which works for the
/// simple cases and drifts badly for anything carrying filters, a sort
/// direction, or a non-obvious dataset. Replaying the spec makes a follow-up
/// what it should be: copy the last query, change one field.
pub struct PriorTurn {
    pub question: String,
    pub answer: String,
    pub spec: Option<QuerySpec>,
}

impl PriorTurn {
    /// The spec as compact JSON, for the transcript and the cache key. `None`
    /// when the turn ran no query (a clarification, a refusal).
    pub fn spec_digest(&self) -> Option<String> {
        self.spec
            .as_ref()
            .and_then(|s| serde_json::to_string(s).ok())
    }

    /// How the turn is replayed to the model: what was said, plus the exact
    /// call that produced it.
    fn replay(&self) -> String {
        match self.spec_digest() {
            Some(spec) => format!(
                "{}\n[ran {QUERY_TOOL} with {spec} — to follow up, reuse this and change only \
                 what the new question asks for]",
                self.answer
            ),
            None => self.answer.clone(),
        }
    }
}

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

/// What the loop reports as it works, when anyone is listening.
///
/// Kept as a trait object rather than a channel so the non-streaming endpoint
/// pays nothing: it passes `None` and no progress is ever constructed.
pub trait Progress: Send + Sync {
    /// A model call is starting. `step` counts from 1.
    fn thinking(&self, step: usize);
    /// A query is about to run.
    fn querying(&self, title: Option<&str>, dataset: &str);
    /// A query came back.
    fn result(&self, data: &QueryData);
}

/// Run one turn of the conversation.
pub async fn run(
    provider: &dyn LlmProvider,
    ctx: &ToolCtx<'_>,
    grounding: &str,
    history: &[PriorTurn],
    question: &str,
    log: &mut TurnLog,
) -> Result<AgentOutcome, ProviderError> {
    run_with_progress(provider, ctx, grounding, history, question, log, None).await
}

/// [`run`], reporting progress to `progress` as it goes.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_progress(
    provider: &dyn LlmProvider,
    ctx: &ToolCtx<'_>,
    grounding: &str,
    history: &[PriorTurn],
    question: &str,
    log: &mut TurnLog,
    progress: Option<&dyn Progress>,
) -> Result<AgentOutcome, ProviderError> {
    let mut messages: Vec<Message> = Vec::with_capacity(history.len() * 2 + 2 + MAX_STEPS * 2);

    // Grounding first, then a compact replay of earlier turns: the question, the
    // answer, and the SPEC that produced it — never the result tables, which are
    // large and which the model does not need in order to answer the next
    // question. Cost per message therefore stays roughly flat as a conversation
    // grows, while a follow-up still gets the one thing it actually needs.
    messages.push(Message::User(ctx.pseudonyms.redact_text(grounding)));
    for turn in history {
        messages.push(Message::User(ctx.pseudonyms.redact_text(&turn.question)));
        messages.push(Message::Assistant {
            // Replayed answers are stored with REAL names, because that is
            // what the merchant saw. They are re-substituted here so an
            // earlier turn cannot leak what this turn is protecting.
            text: Some(ctx.pseudonyms.redact_text(&turn.replay())),
            calls: Vec::new(),
        });
    }
    // The merchant's own question can name a colleague ("how did Ahmed do?").
    messages.push(Message::User(ctx.pseudonyms.redact_text(question)));

    let mut results: Vec<QueryData> = Vec::new();
    // Kept so a failed run can say what actually went wrong rather than
    // shrugging. See `fallback_text`.
    let mut tool_errors: Vec<String> = Vec::new();

    for step in 0..MAX_STEPS {
        if let Some(p) = progress {
            p.thinking(step + 1);
        }
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
                return Ok(AgentOutcome::Answer {
                    text: ctx.pseudonyms.restore_text(&t),
                    results,
                });
            }
            Turn::Calls(_) => {
                log.finished("empty_call_list");
                return Ok(AgentOutcome::Exhausted {
                    text: fallback_text(&results, &tool_errors),
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

            if let (Some(p), true) = (progress, is_query_tool(&call.name)) {
                p.querying(
                    call.args.get("preset").and_then(Value::as_str),
                    call.args
                        .get("dataset")
                        .and_then(Value::as_str)
                        .unwrap_or("preset"),
                );
            }

            match tools::dispatch(ctx, &call.name, &call.args).await {
                ToolOutcome::Answer(text) => {
                    log.finished("answered");
                    return Ok(AgentOutcome::Answer {
                        // Put the real names back before the merchant sees it.
                        text: ctx.pseudonyms.restore_text(&text),
                        results,
                    });
                }
                ToolOutcome::Clarify(question) => {
                    log.finished("clarified");
                    return Ok(AgentOutcome::Clarify {
                        question: ctx.pseudonyms.restore_text(&question),
                    });
                }
                ToolOutcome::Info(v) => tool_results.push(tool_message(call, v)),
                ToolOutcome::Error(e) => {
                    log.record_tool_error(&e);
                    tool_errors.push(e.clone());
                    // Handed back rather than raised: this is the mechanism that
                    // lets the next step be a correction.
                    tool_results.push(tool_message(call, json!({ "error": e })));
                }
                ToolOutcome::Data(data) => {
                    // Emitted the moment the query returns, so a chart can
                    // render while the model is still writing the sentence
                    // about it — which is the whole reason to stream.
                    if let Some(p) = progress {
                        p.result(&data);
                    }
                    let payload = summarize_for_model(&data, ctx.pseudonyms);
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
        text: fallback_text(&results, &tool_errors),
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
fn summarize_for_model(data: &QueryData, pseudonyms: &Pseudonyms) -> Value {
    let mut v = data.result.to_model_json(tools::MODEL_ROW_SAMPLE);
    // The ONLY place result rows are shown to a model. `data.result.rows` keeps
    // the real names for the client, which never routes through the model.
    if let Some(rows) = v.get_mut("rows").and_then(Value::as_array_mut) {
        for row in rows.iter_mut() {
            if let Some(obj) = row.as_object() {
                *row = Value::Object(pseudonyms.redact_row(obj));
            }
        }
    }
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

/// Said when the loop ends without the model calling `answer`.
///
/// This used to be one sentence — "I couldn't work that one out" — regardless of
/// what happened, which told the merchant nothing and gave them no way to get a
/// different result. The loop always knew more than that: which queries were
/// rejected and why, and whether any data came back at all. It now says so.
///
/// The rule it follows: name what went wrong, then give one concrete thing to
/// try. Never invent a finding.
fn fallback_text(results: &[QueryData], tool_errors: &[String]) -> String {
    if !results.is_empty() {
        // Data exists; only the prose is missing. The table is the answer.
        return "I found the figures for that but couldn't write the summary. \
                The table below has the numbers."
            .to_string();
    }

    // A branch name that matched nothing is the single most actionable failure,
    // and the merchant can fix it themselves by spelling it differently.
    if let Some(name) = tool_errors.iter().find_map(|e| unmatched_branch_in(e)) {
        return format!(
            "I couldn't find a branch called \"{name}\" in the ones you have access to. \
             Check the spelling, or ask without naming a branch to cover all of them."
        );
    }

    match tool_errors.last() {
        Some(err) => format!(
            "I couldn't run that query. {} \
             Try naming the figure and the period explicitly — for example, \
             \"tips by waiter last month\".",
            explain(err)
        ),
        None => "I couldn't work that one out. Try asking about a specific figure and period — \
                 for example, revenue last week, or your top products this month."
            .to_string(),
    }
}

/// Turn a model-facing tool error into a sentence a merchant can act on.
///
/// Tool errors are written for the model and list valid ids so it can correct
/// itself; pasted verbatim they read as gibberish to a person. This keeps the
/// part that says *what* was wrong and drops the machine-readable remainder.
fn explain(err: &str) -> String {
    let lower = err.to_lowercase();
    let reason = if lower.contains("period") {
        "I didn't understand the time range."
    } else if lower.contains("no dimension") || lower.contains("no measure") {
        "That combination of figures isn't available together."
    } else if lower.contains("unknown dataset") {
        "I don't hold data of that kind."
    } else if lower.contains("no filter") || lower.contains("filter") {
        "One of the filters in the question isn't one I can apply."
    } else if lower.contains("timeout") || lower.contains("statement timeout") {
        "The query covered too much data to finish in time — try a shorter period."
    } else {
        // Unclassified: show the first sentence, which is the human-readable
        // part of every tool error, and never the id dump that follows.
        return first_sentence(err);
    };
    reason.to_string()
}

fn first_sentence(err: &str) -> String {
    let s = err
        .split_once(". ")
        .map(|(head, _)| head)
        .unwrap_or(err)
        .trim()
        .trim_end_matches('.');
    if s.is_empty() {
        return "It wasn't a query I could build.".to_string();
    }
    format!("{s}.")
}

/// Pull the branch name out of the executor's unmatched-branch error, if that
/// is what this was.
fn unmatched_branch_in(err: &str) -> Option<String> {
    let lower = err.to_lowercase();
    if !(lower.contains("branch") && (lower.contains("no ") || lower.contains("unmatched"))) {
        return None;
    }
    // The name is quoted in the error.
    let start = err.find('\'')?;
    let rest = &err[start + 1..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
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
        let v = summarize_for_model(&data(0, None), &Pseudonyms::default());
        let note = v["note"].as_str().unwrap();
        assert!(note.contains("do NOT report it as a zero"));
    }

    #[test]
    fn a_non_empty_result_carries_no_empty_note() {
        let v = summarize_for_model(&data(3, None), &Pseudonyms::default());
        assert!(v.get("note").is_none());
        assert_eq!(v["row_count"], 3);
        assert_eq!(v["scope"], "All branches (1)");
    }

    #[test]
    fn an_unmatched_branch_becomes_a_warning_the_model_must_relay() {
        let v = summarize_for_model(&data(2, Some("Alexandria")), &Pseudonyms::default());
        assert!(v["warning"].as_str().unwrap().contains("Alexandria"));
    }

    #[test]
    fn the_model_sees_a_sample_not_the_whole_table() {
        let v = summarize_for_model(&data(500, None), &Pseudonyms::default());
        assert_eq!(v["rows"].as_array().unwrap().len(), tools::MODEL_ROW_SAMPLE);
        // ...but is told the real size, so it never says "40 products".
        assert_eq!(v["row_count"], 500);
        assert_eq!(v["truncated"], true);
    }

    #[test]
    fn the_fallback_never_invents_a_finding() {
        assert!(fallback_text(&[], &[]).contains("couldn't work that one out"));
        let with = fallback_text(std::slice::from_ref(&data(1, None)), &[]);
        assert!(with.contains("table below"));
        // Neither variant states a figure.
        assert!(!with.contains('0'));
    }

    /// The failure message has to say what went wrong. A merchant who is told
    /// only "that didn't work" has no way to ask a better question, which is
    /// what makes a wrong answer and no answer feel equally useless.
    #[test]
    fn a_failure_names_its_cause_and_offers_a_way_forward() {
        let cases = [
            (
                "No branch matched 'Alexandria', valid branches: Maadi, Arkan",
                "Alexandria",
            ),
            ("Unknown period preset 'this_summer'", "time range"),
            (
                "dataset 'orders' has no measure 'profit_margin'",
                "available together",
            ),
            (
                "query failed: canceling statement due to statement timeout",
                "shorter period",
            ),
        ];
        for (err, expected) in cases {
            let text = fallback_text(&[], &[err.to_string()]);
            assert!(
                text.contains(expected),
                "error {err:?} should surface {expected:?}, produced {text:?}"
            );
        }
    }

    #[test]
    fn a_failure_message_never_leaks_the_models_id_dump() {
        // Tool errors list every valid id so the MODEL can self-correct. Pasting
        // that at a person is noise, not help.
        let err = "dataset 'orders' has no dimension 'foo'. Valid dimensions: \
                   branch, waiter, cashier, product, category, hour, day, week";
        let text = fallback_text(&[], &[err.to_string()]);
        assert!(!text.contains("Valid dimensions"), "{text}");
        assert!(!text.contains("cashier"), "{text}");
    }

    #[test]
    fn data_without_prose_points_at_the_table_rather_than_reporting_an_error() {
        // Rows came back; only the summary is missing. That is not a failure to
        // answer, and calling it one would hide a perfectly good table.
        let text = fallback_text(
            std::slice::from_ref(&data(3, None)),
            &["some earlier rejected spec".to_string()],
        );
        assert!(text.contains("table below"), "{text}");
        assert!(!text.contains("couldn't run"), "{text}");
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
