//! Prompts + tool-parameter schemas shared by every LLM provider.
//!
//! The report picker must behave IDENTICALLY no matter which model runs it, so
//! the system instruction, the trailing user turn (question + grounding), the
//! summary prompt, and the per-report JSON-Schema live here — not duplicated in
//! each provider. Providers differ only in how they WRAP this (Gemini
//! `functionDeclarations` vs OpenAI-style `tools`) and in their HTTP.

use serde_json::{Map, Value, json};

use super::catalog::{ParamKind, Report};
use super::provider::ChatContext;

/// System instruction for the report-picking call.
pub const SYSTEM_PROMPT: &str = "You are the analytics assistant for a restaurant \
point-of-sale system. The merchant asks about THEIR OWN business data in plain \
language, in English or Arabic (including Egyptian dialect). Choose exactly one \
of the provided report functions and fill in its parameters from the question. \
The user's message states today's date and timezone — resolve relative dates \
(\"last week\", \"this month\", \"yesterday\", \"الأسبوع الماضي\", \"امبارح\", \
\"الشهر ده\") to concrete ISO-8601 dates relative to that. You do NOT choose \
which branches to include — branch access is enforced by the backend. Prefer a \
specific report when one clearly fits. Otherwise — when the question needs a \
custom breakdown (by day/branch/waiter/product/…), a metric or grouping the \
fixed reports lack, a per-group ranking (e.g. the top item in EACH branch), or \
a particular table-vs-chart output — use the flexible `analytics_query` function \
and compose it from its dataset/dimensions/measures/filters/output parameters. \
Only fall back to the closest fixed report if `analytics_query` cannot express \
the request. You never write SQL and never invent data.";

/// System instruction for the one-line summary call.
pub const SUMMARY_SYSTEM_PROMPT: &str = "You summarize restaurant analytics \
results. Given the user's question and the resulting data as JSON, reply with \
ONE short, factual sentence stating the key takeaway. Reply in the SAME language \
as the question. No preamble, no markdown, no lists.";

/// The trailing user turn for the report-picking call: the question plus the
/// per-request grounding (today's date, timezone, answer language, and the
/// branches the caller may narrow to).
pub fn user_text(ctx: &ChatContext) -> String {
    let branches = if ctx.branch_names.is_empty() {
        "none".to_string()
    } else {
        ctx.branch_names.join(", ")
    };
    format!(
        "Today is {} in timezone {}. Answer language: {}.\n\
         Branches available: {}. If the question names a branch, pass its name as \
         the `branch` argument; otherwise omit it.\n\n\
         Question: {}",
        ctx.today, ctx.timezone, ctx.locale, branches, ctx.question
    )
}

/// The user turn for the summary call.
pub fn summary_user_text(ctx: &ChatContext, report_title: &str, data_json: &str) -> String {
    format!(
        "Language: {}\nQuestion: {}\nReport: {report_title}\nData: {data_json}",
        ctx.locale, ctx.question
    )
}

/// JSON-Schema `parameters` object for a report's function/tool declaration: its
/// typed params plus the universal optional `branch` narrowing. Standard JSON
/// Schema, so both Gemini and OpenAI-style tool APIs accept it verbatim.
pub fn report_parameters_schema(report: &Report) -> Value {
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for p in report.params {
        let schema = match p.kind {
            ParamKind::Date => json!({
                "type": "string",
                "format": "date-time",
                "description": p.description
            }),
            ParamKind::Int { .. } => json!({
                "type": "integer",
                "description": p.description
            }),
            ParamKind::Enum { variants, .. } => json!({
                "type": "string",
                "enum": variants,
                "description": p.description
            }),
            ParamKind::StrList { variants } => json!({
                "type": "array",
                "items": { "type": "string", "enum": variants },
                "description": p.description
            }),
            ParamKind::Bool { .. } => json!({
                "type": "boolean",
                "description": p.description
            }),
        };
        properties.insert(p.name.to_string(), schema);
        if p.required {
            required.push(Value::from(p.name));
        }
    }
    // Every report is branch-scoped, so all accept an optional branch narrowing.
    // The backend fuzzy-matches this within the caller's accessible branches and
    // can only narrow, never widen.
    properties.insert(
        "branch".to_string(),
        json!({
            "type": "string",
            "description": "Optional: restrict to ONE branch, by the name the user used \
                (e.g. 'Sidi Henish'). Omit to cover every branch the user can access."
        }),
    );
    json!({ "type": "object", "properties": properties, "required": required })
}
