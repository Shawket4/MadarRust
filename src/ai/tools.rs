//! The agent's tool surface.
//!
//! Everything the model can *do* is here, and every one of them bottoms out in
//! the analytics core — the same [`crate::analytics::compile`] and
//! [`crate::analytics::execute`] path a dashboard widget uses. The model never
//! reaches the database by any other route, never writes SQL, and never chooses
//! which branches it may see.
//!
//! Note what a tool *error* is for. A rejected spec is not a failure of the
//! request: it is returned to the model as a tool result carrying the reason and
//! the valid alternatives, so the next step of the loop is a correction. That is
//! the whole reason the agent can answer questions a single-shot router could
//! only refuse.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    analytics::{
        compile::{CompileCtx, compile},
        execute::{self, ExecCtx, QueryResult},
        presets,
        schema::{self, Dataset},
        scope::{self, BranchRef, ScopeInfo},
        spec::{Period, PeriodPreset, QuerySpec},
    },
    auth::jwt::Claims,
    db::Db,
    permissions::checker::check_permission,
};

use super::llm::ToolDef;

pub const QUERY_METRICS: &str = "query_metrics";
pub const RUN_PRESET: &str = "run_preset";
pub const DESCRIBE_DATASET: &str = "describe_dataset";
pub const ANSWER: &str = "answer";
pub const CLARIFY: &str = "clarify";

/// How many rows of a result are shown to the model. It needs enough to state a
/// takeaway and quote a few figures, not the whole table — the full result goes
/// to the client regardless.
pub const MODEL_ROW_SAMPLE: usize = 40;

/// Everything a tool needs that does not come from the model.
pub struct ToolCtx<'a> {
    pub db: &'a Db,
    pub claims: &'a Claims,
    pub compile: &'a CompileCtx,
    /// Branches this caller may see. The ceiling on every query.
    pub accessible: &'a [BranchRef],
    /// The dashboard's currently selected branch, used as a default only.
    pub selected_branch: Option<Uuid>,
    pub locale: &'a str,
    pub timezone: &'a str,
    /// Staff-name map for this organization. Result rows and every piece of
    /// text that reaches the model pass through it; see `ai::pseudonym`.
    pub pseudonyms: &'a super::pseudonym::Directory,
}

/// A tool ran. `Error` is a normal, recoverable outcome.
pub enum ToolOutcome {
    /// A query succeeded. Carries everything the client needs to render it and
    /// everything the model needs to talk about it.
    Data(Box<QueryData>),
    /// Schema introspection.
    Info(Value),
    /// The model finished and produced prose.
    Answer(String),
    /// The model needs something from the user before it can continue.
    Clarify(String),
    /// Recoverable: handed back to the model so it can try again.
    Error(String),
}

pub struct QueryData {
    pub result: QueryResult,
    pub spec: QuerySpec,
    pub scope: ScopeInfo,
    /// Set when the query came from a curated preset.
    pub title: Option<String>,
    pub preset_id: Option<String>,
}

/// The tool declarations sent to the model.
///
/// Built once and cached: the declarations are a large, invariant prefix, and
/// keeping them byte-stable is what lets an upstream implicit cache hit on every
/// request. Only the trailing conversation varies.
pub fn tool_defs() -> &'static [ToolDef] {
    static DEFS: std::sync::OnceLock<Vec<ToolDef>> = std::sync::OnceLock::new();
    DEFS.get_or_init(build_tool_defs)
}

fn build_tool_defs() -> Vec<ToolDef> {
    let dataset_ids: Vec<&str> = schema::DATASETS.iter().map(|d| d.id).collect();
    let preset_ids: Vec<&str> = presets::PRESETS.iter().map(|p| p.id).collect();

    vec![
        ToolDef {
            name: QUERY_METRICS,
            description: "Run an analytics query over the merchant's own data. This is the \
                main tool: it can express nearly any question by combining a dataset with \
                dimensions to group by, measures to compute, and filters. Consult the \
                schema in the system prompt for what each dataset supports. If a query is \
                rejected, read the error — it lists the valid options — and try again."
                .into(),
            parameters: query_spec_schema(&dataset_ids),
        },
        ToolDef {
            name: RUN_PRESET,
            description: format!(
                "Run a curated, pre-built metric by id. Prefer this over {QUERY_METRICS} \
                 when one clearly matches the question — the definitions are known-good. \
                 The available ids and what each measures are listed in the system prompt."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "preset": { "type": "string", "enum": preset_ids,
                        "description": "The curated metric id." },
                    "period": period_schema(),
                    "branch": branch_schema(),
                },
                "required": ["preset"]
            }),
        },
        ToolDef {
            name: DESCRIBE_DATASET,
            description: "Look up exactly which dimensions, measures and filters a dataset \
                supports, with an explanation of what each measure counts. Use this when \
                you are unsure whether a field exists rather than guessing."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "dataset": { "type": "string", "enum": dataset_ids,
                        "description": "The dataset to describe." }
                },
                "required": ["dataset"]
            }),
        },
        ToolDef {
            name: ANSWER,
            description: "Give the merchant your final answer. Call this once you have the \
                data you need. State the key figures plainly in the same language as the \
                question. If a query came back empty, say so and say what that means — \
                never present an empty result as if it were a zero."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string",
                        "description": "One to three sentences. No markdown, no preamble." }
                },
                "required": ["text"]
            }),
        },
        ToolDef {
            name: CLARIFY,
            description: "Ask the merchant one short question when the request is genuinely \
                ambiguous and a wrong guess would be misleading. Do not use this for \
                anything you can reasonably assume — prefer answering with a stated \
                assumption."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "One short question." }
                },
                "required": ["question"]
            }),
        },
    ]
}

fn period_schema() -> Value {
    json!({
        "type": "object",
        "description": "The reporting window. Strongly prefer 'preset' — it is resolved in \
            the merchant's own timezone, so you never need to do calendar arithmetic. Use \
            from/to only for a window no preset covers.",
        "properties": {
            "preset": { "type": "string", "enum": PeriodPreset::ALL,
                "description": "A named relative window." },
            "from": { "type": "string", "description": "ISO-8601 start, inclusive." },
            "to": { "type": "string", "description": "ISO-8601 end, inclusive." }
        }
    })
}

fn branch_schema() -> Value {
    json!({
        "type": "string",
        "description": "Optional: restrict to ONE branch by the name the merchant used. \
            Omit to cover every branch they can see. The backend matches this within their \
            accessible branches and can only narrow, never widen."
    })
}

/// JSON Schema for [`QuerySpec`]. Mirrors the Rust type; the deserializer is the
/// authority, and this is what tells the model how to satisfy it.
fn query_spec_schema(dataset_ids: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "dataset": { "type": "string", "enum": dataset_ids,
                "description": "Which dataset. This fixes the grain — pick the one whose \
                    single row is the thing being counted." },
            "dimensions": {
                "type": "array", "maxItems": 2, "items": { "type": "string" },
                "description": "Group by these, outermost first. At most 2. Leave empty for \
                    a single total. A time dimension (day/week/month) makes a trend."
            },
            "measures": {
                "type": "array", "maxItems": 8, "items": { "type": "string" },
                "description": "What to compute. Leave empty for the dataset's headline \
                    measures. The first one drives sorting."
            },
            "filters": {
                "type": "object", "additionalProperties": { "type": "string" },
                "description": "Filter id to value, e.g. {\"status\": \"voided\"}. Each \
                    dataset's filters and their allowed values are in the system prompt. \
                    Defaults apply when omitted — notably, sales already exclude voided \
                    and refunded orders."
            },
            "period": period_schema(),
            "sort": {
                "type": "object",
                "properties": {
                    "measure": { "type": "string", "description": "Must be one of your chosen measures." },
                    "dir": { "type": "string", "enum": ["asc", "desc"] }
                },
                "required": ["measure"],
                "description": "Use dir 'asc' for worst/slowest/least questions."
            },
            "limit": { "type": "integer", "description": "Max rows (default 100, max 1000)." },
            "compare": { "type": "string", "enum": ["none", "previous_period", "previous_year"],
                "description": "Contrast the period with the one before it, adding previous \
                    and change_pct columns. Needs a bounded period and no time dimension." },
            "transform": {
                "type": "object",
                "properties": {
                    "share": { "type": "boolean", "description": "Add each row's % of the total." },
                    "cumulative": { "type": "boolean", "description": "Add a running total. Needs a time dimension." },
                    "top_per": {
                        "type": "object",
                        "properties": {
                            "dimension": { "type": "string" },
                            "n": { "type": "integer" }
                        },
                        "required": ["dimension"],
                        "description": "Keep the top N within each value of this dimension — \
                            'the best seller in every branch'. Needs 2 dimensions."
                    }
                }
            },
            "having_min": { "type": "integer", "description": "Drop groups whose sort measure is below this." },
            "viz": { "type": "string",
                "enum": ["auto", "kpi", "line", "area", "bar", "row", "pie", "donut", "table", "heatmap"],
                "description": "Leave unset or 'auto' unless the merchant asked for a specific chart." },
            "branch": branch_schema()
        },
        "required": ["dataset"]
    })
}

/// Run one tool call.
pub async fn dispatch(ctx: &ToolCtx<'_>, name: &str, args: &Value) -> ToolOutcome {
    match name {
        ANSWER => match args.get("text").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => ToolOutcome::Answer(t.trim().to_string()),
            _ => ToolOutcome::Error("'text' is required and cannot be empty".into()),
        },
        CLARIFY => match args.get("question").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => ToolOutcome::Clarify(q.trim().to_string()),
            _ => ToolOutcome::Error("'question' is required and cannot be empty".into()),
        },
        DESCRIBE_DATASET => describe(args),
        RUN_PRESET => run_preset(ctx, args).await,
        QUERY_METRICS => run_spec_value(ctx, args, None, None).await,
        other => ToolOutcome::Error(format!(
            "Unknown tool '{other}'. Available: {QUERY_METRICS}, {RUN_PRESET}, \
             {DESCRIBE_DATASET}, {ANSWER}, {CLARIFY}"
        )),
    }
}

fn describe(args: &Value) -> ToolOutcome {
    let id = args.get("dataset").and_then(Value::as_str).unwrap_or("");
    match schema::dataset(id) {
        Some(d) => ToolOutcome::Info(describe_dataset(d)),
        None => ToolOutcome::Error(format!(
            "Unknown dataset '{id}'. Valid options: {}",
            schema::DATASETS
                .iter()
                .map(|d| d.id)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn describe_dataset(d: &'static Dataset) -> Value {
    json!({
        "dataset": d.id,
        "describes": d.help.split_whitespace().collect::<Vec<_>>().join(" "),
        "dimensions": d.dims.iter().map(|x| json!({
            "id": x.id, "label": x.label, "is_time_axis": x.time
        })).collect::<Vec<_>>(),
        "measures": d.measures.iter().map(|m| json!({
            "id": m.id, "label": m.label, "counts": m.help
        })).collect::<Vec<_>>(),
        "filters": d.filters.iter().map(|f| json!({
            "id": f.id, "values": f.values(), "default": f.default, "meaning": f.help
        })).collect::<Vec<_>>(),
    })
}

async fn run_preset(ctx: &ToolCtx<'_>, args: &Value) -> ToolOutcome {
    let id = args.get("preset").and_then(Value::as_str).unwrap_or("");
    let Some(p) = presets::preset(id) else {
        return ToolOutcome::Error(format!(
            "Unknown metric '{id}'. Valid ids: {}",
            presets::PRESETS
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    // A preset carries its own permission. Refusing here — rather than silently
    // omitting the metric from the prompt — means the model can tell the
    // merchant why, instead of inventing an answer.
    if check_permission(ctx.db.get_ref(), ctx.claims, p.permission, "read")
        .await
        .is_err()
    {
        return ToolOutcome::Error(format!(
            "This user does not have permission to view '{}'. Tell them so; do not \
             try another metric that covers the same data.",
            p.title
        ));
    }

    let period: Option<Period> = match args.get("period") {
        Some(v) if !v.is_null() => match serde_json::from_value::<Period>(v.clone()) {
            Ok(p) => Some(p),
            Err(e) => return ToolOutcome::Error(format!("Invalid 'period': {e}")),
        },
        _ => None,
    };
    let mut spec = p.to_spec(period);
    spec.branch = args
        .get("branch")
        .and_then(Value::as_str)
        .map(str::to_string);

    execute_spec(ctx, spec, Some(p.title.to_string()), Some(p.id.to_string())).await
}

async fn run_spec_value(
    ctx: &ToolCtx<'_>,
    args: &Value,
    title: Option<String>,
    preset_id: Option<String>,
) -> ToolOutcome {
    let spec: QuerySpec = match serde_json::from_value(args.clone()) {
        Ok(s) => s,
        Err(e) => {
            // Deserialization errors are precise ("unknown field `dimension`,
            // expected `dimensions`") and are exactly what the model needs.
            return ToolOutcome::Error(format!(
                "That query is not valid: {e}. Check the field names and try again."
            ));
        }
    };
    execute_spec(ctx, spec, title, preset_id).await
}

async fn execute_spec(
    ctx: &ToolCtx<'_>,
    spec: QuerySpec,
    title: Option<String>,
    preset_id: Option<String>,
) -> ToolOutcome {
    let compiled = match compile(&spec, ctx.compile) {
        Ok(c) => c,
        Err(e) => return ToolOutcome::Error(e.detail()),
    };

    // Scope is resolved here, from the caller's access — the model's `branch`
    // is only a name to match within it.
    let (branch_ids, scope_info) =
        scope::resolve(ctx.accessible, spec.branch.as_deref(), ctx.selected_branch);

    let exec_ctx = ExecCtx {
        branch_ids: &branch_ids,
        locale: ctx.locale,
        tz: ctx.timezone,
    };
    match execute::run(ctx.db, &compiled, &exec_ctx).await {
        Ok(result) => ToolOutcome::Data(Box::new(QueryData {
            result,
            spec,
            scope: scope_info,
            title,
            preset_id,
        })),
        Err(execute::ExecError::Db(sqlx::Error::Database(ref d)))
            if d.code().as_deref() == Some("57014") =>
        {
            ToolOutcome::Error(
                "That query took too long. Narrow the period, reduce the breakdown, \
                 or add a filter."
                    .into(),
            )
        }
        Err(e) => {
            // The model gets a generic message it can act on; the operator gets
            // an issue. The turn still returns 200, so without this the failure
            // would be invisible to everything downstream.
            crate::observability::report::report(
                crate::observability::report::Failure::new("ai", "tool_query")
                    .with("dataset", Value::from(spec.dataset.clone())),
                &e,
            );
            ToolOutcome::Error("That query could not be run. Try a simpler breakdown.".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_declares_a_valid_object_schema() {
        for t in tool_defs() {
            assert_eq!(
                t.parameters["type"], "object",
                "{}: not an object schema",
                t.name
            );
            assert!(
                t.parameters.get("properties").is_some(),
                "{}: no properties",
                t.name
            );
            assert!(t.description.len() > 40, "{}: description too thin", t.name);
        }
    }

    #[test]
    fn the_tool_set_is_exactly_what_dispatch_handles() {
        let names: Vec<&str> = tool_defs().iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![QUERY_METRICS, RUN_PRESET, DESCRIBE_DATASET, ANSWER, CLARIFY]
        );
    }

    #[test]
    fn the_definitions_are_built_once_and_stay_byte_stable() {
        // They are the cacheable prefix of every upstream request.
        assert_eq!(tool_defs().as_ptr(), tool_defs().as_ptr());
    }

    #[test]
    fn enumerated_ids_come_from_the_registry_not_a_hand_written_list() {
        let defs = tool_defs();
        let preset_enum = defs[1].parameters["properties"]["preset"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(preset_enum.len(), presets::PRESETS.len());
        let dataset_enum = defs[0].parameters["properties"]["dataset"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(dataset_enum.len(), schema::DATASETS.len());
    }

    #[test]
    fn describe_dataset_reports_the_real_registry() {
        let out = describe(&json!({ "dataset": "orders" }));
        let ToolOutcome::Info(v) = out else {
            panic!("expected info")
        };
        let measures = v["measures"].as_array().unwrap();
        assert!(measures.iter().any(|m| m["id"] == "revenue"));
        // Every measure explains itself, so the model is never guessing.
        assert!(
            measures
                .iter()
                .all(|m| m["counts"].as_str().unwrap().len() > 5)
        );
    }

    #[test]
    fn describing_an_unknown_dataset_lists_the_real_ones() {
        let ToolOutcome::Error(e) = describe(&json!({ "dataset": "sales" })) else {
            panic!("expected a recoverable error")
        };
        assert!(e.contains("orders"), "the error must teach the model: {e}");
    }
}
