//! A deterministic transport for tests.
//!
//! Two modes, and both matter:
//!
//!   * [`MockProvider::router`] behaves like a competent model — it matches the
//!     question to a metric by keyword, then answers once it has seen a result.
//!     This exercises the whole pipeline (HTTP → agent → tools → compiler →
//!     executor → RLS-scoped query → response) with no network and no API key.
//!   * [`MockProvider::scripted`] replays an exact sequence of turns, which is
//!     how the *interesting* paths get tested: a model that names a measure that
//!     does not exist and has to recover, one that never answers, one that asks
//!     for clarification. Those paths cannot be reached by a keyword router, and
//!     they are precisely the ones a single-shot design could not handle at all.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;

use super::llm::{Completion, LlmProvider, Message, ProviderError, ToolCall, Turn};
use super::tools;

pub struct MockProvider {
    /// Turns to replay in order. Empty = keyword-routing mode.
    script: Mutex<Vec<Turn>>,
    scripted: bool,
}

impl MockProvider {
    /// Keyword routing: pick a preset, then answer.
    pub fn router() -> Self {
        Self {
            script: Mutex::new(Vec::new()),
            scripted: false,
        }
    }

    /// Replay these turns in order. Once exhausted, falls back to answering, so
    /// a script can never hang the loop.
    pub fn scripted(turns: Vec<Turn>) -> Self {
        let mut t = turns;
        t.reverse();
        Self {
            script: Mutex::new(t),
            scripted: true,
        }
    }

    /// A single tool call, for building scripts readably.
    pub fn call(name: &str, args: serde_json::Value) -> Turn {
        Turn::Calls(vec![ToolCall {
            id: format!("{name}-call"),
            name: name.to_string(),
            args,
            // No provider state in a mock.
            signature: None,
        }])
    }

    /// A final answer turn.
    pub fn answer(text: &str) -> Turn {
        Self::call(tools::ANSWER, json!({ "text": text }))
    }
}

/// Map a question to a curated metric, the way a model would route it. Every id
/// here is checked against the registry by a test below, so this cannot rot as
/// presets are renamed.
fn route(question: &str) -> Option<&'static str> {
    let q = question.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| q.contains(n));
    Some(if has(&["waste", "spoil", "هالك"]) {
        "waste_by_ingredient"
    } else if has(&["late", "lateness", "تأخير"]) {
        "lateness_by_employee"
    } else if has(&["overtime"]) {
        "overtime_by_employee"
    } else if has(&["drawer", "variance", "short"]) {
        "drawer_variance"
    } else if has(&["void"]) {
        "voids_by_reason"
    } else if has(&["discount"]) {
        "discount_usage"
    } else if has(&["supplier", "purchas", "spend"]) {
        "spend_by_supplier"
    } else if has(&["no-show", "no show", "reservation", "booking"]) {
        "no_show_rate"
    } else if has(&["shrink", "stocktake"]) {
        "shrinkage_by_ingredient"
    } else if has(&["margin", "profit"]) {
        "product_profit"
    } else if has(&["categor"]) {
        "top_categories"
    } else if has(&["product", "item", "best sell", "منتج"]) {
        "top_products"
    } else if has(&["payment", "cash", "card"]) {
        "payment_mix"
    } else if has(&["hour", "peak", "busy"]) {
        "sales_by_hour"
    } else if has(&["waiter"]) {
        "waiter_performance"
    } else if has(&["branch", "store"]) {
        "sales_by_branch"
    } else if has(&["per day", "daily", "trend"]) {
        "sales_by_day"
    } else if has(&["sale", "revenue", "مبيعات"]) {
        "sales_summary"
    } else {
        return None;
    })
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn complete(&self, req: Completion<'_>) -> Result<Turn, ProviderError> {
        if self.scripted {
            let next = self.script.lock().expect("mock script lock").pop();
            return Ok(next.unwrap_or_else(|| {
                MockProvider::answer("Script exhausted; answering with what I have.")
            }));
        }

        // Routing mode. Once a tool result is in the transcript, answer;
        // otherwise pick a metric for the question.
        let saw_result = req
            .messages
            .iter()
            .any(|m| matches!(m, Message::ToolResult { .. }));
        if saw_result {
            return Ok(MockProvider::answer("Here are the figures you asked for."));
        }

        let question = req
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_default();

        Ok(match route(&question) {
            Some(preset) => MockProvider::call(tools::RUN_PRESET, json!({ "preset": preset })),
            None => MockProvider::call(
                tools::CLARIFY,
                json!({ "question": "What would you like to know about your sales?" }),
            ),
        })
    }

    fn name(&self) -> String {
        "mock".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::presets;

    #[test]
    fn every_routed_preset_id_exists_in_the_registry() {
        // Without this, renaming a preset leaves the mock routing to a metric
        // that no longer exists and the pipeline tests fail somewhere far away.
        for q in [
            "waste",
            "late",
            "overtime",
            "drawer",
            "void",
            "discount",
            "supplier",
            "reservation",
            "shrink",
            "margin",
            "category",
            "product",
            "payment",
            "hour",
            "waiter",
            "branch",
            "daily",
            "revenue",
        ] {
            let id = route(q).unwrap_or_else(|| panic!("'{q}' routed nowhere"));
            assert!(
                presets::preset(id).is_some(),
                "'{q}' routed to unknown preset '{id}'"
            );
        }
    }

    #[test]
    fn an_unroutable_question_asks_for_clarification() {
        assert!(route("what is the weather").is_none());
    }

    #[tokio::test]
    async fn the_router_answers_once_it_has_seen_a_result() {
        let p = MockProvider::router();
        let msgs = vec![
            Message::User("top products".into()),
            Message::ToolResult {
                id: "x".into(),
                name: "run_preset".into(),
                content: json!({ "row_count": 3 }),
            },
        ];
        let turn = p
            .complete(Completion {
                system: "",
                messages: &msgs,
                tools: tools::tool_defs(),
                max_tokens: 100,
                force_tool: true,
            })
            .await
            .unwrap();
        match turn {
            Turn::Calls(c) => assert_eq!(c[0].name, tools::ANSWER),
            other => panic!("expected an answer call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_script_replays_in_order_then_falls_back_to_answering() {
        let p = MockProvider::scripted(vec![
            MockProvider::call(tools::DESCRIBE_DATASET, json!({ "dataset": "orders" })),
            MockProvider::answer("done"),
        ]);
        let req = || Completion {
            system: "",
            messages: &[],
            tools: tools::tool_defs(),
            max_tokens: 100,
            force_tool: true,
        };
        let first = p.complete(req()).await.unwrap();
        assert!(matches!(&first, Turn::Calls(c) if c[0].name == tools::DESCRIBE_DATASET));
        let second = p.complete(req()).await.unwrap();
        assert!(matches!(&second, Turn::Calls(c) if c[0].name == tools::ANSWER));
        // Exhausted scripts answer rather than hanging the loop.
        let third = p.complete(req()).await.unwrap();
        assert!(matches!(&third, Turn::Calls(c) if c[0].name == tools::ANSWER));
    }
}
