//! Machine- and model-readable descriptions of the semantic layer.
//!
//! Two audiences, one source:
//!
//!   * **The dashboard** calls `GET /metrics/schema` to build a widget picker —
//!     which datasets exist, what can be grouped and measured, what each figure
//!     actually counts.
//!   * **The AI agent** receives [`schema_digest`], a compact text rendering of
//!     the same registry, as part of its system prompt. Text rather than JSON
//!     Schema because it is a fraction of the tokens for the same information,
//!     and it sits in the cacheable prefix of every request.
//!
//! Because both are generated from [`super::schema`], a metric cannot exist in
//! one and not the other, and neither can drift from what actually executes.

use serde::Serialize;
use utoipa::ToSchema;

use super::presets::{BoardTemplate, DEFAULT_BOARDS, PRESETS, Preset};
use super::schema::{DATASETS, Dataset};
use super::spec::PeriodPreset;
use super::types::{ColumnKind, Grain, Viz};

#[derive(Debug, Serialize, ToSchema)]
pub struct FieldInfo {
    pub id: String,
    pub label: String,
    pub kind: ColumnKind,
    /// One line on exactly what it counts. Absent for dimensions, whose label
    /// is self-explanatory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// True for time axes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub time: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FilterInfo {
    pub id: String,
    pub label: String,
    pub help: String,
    pub values: Vec<String>,
    pub default: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DatasetInfo {
    pub id: String,
    pub title: String,
    /// What one row is, and when to use this dataset instead of another.
    pub help: String,
    pub dimensions: Vec<FieldInfo>,
    pub measures: Vec<FieldInfo>,
    pub filters: Vec<FilterInfo>,
    pub default_measures: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PresetInfo {
    pub id: String,
    pub title: String,
    pub description: String,
    pub category: String,
    pub dataset: String,
    /// Result shape, so a widget picker knows a KPI card from a line chart
    /// before running anything.
    pub grain: Grain,
    pub viz: Viz,
    pub default_period: PeriodPreset,
    /// Permission resource required, with the `read` action.
    pub permission: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BoardInfo {
    pub key: String,
    pub title: String,
    pub description: String,
    pub widgets: Vec<String>,
}

/// The complete registry, as served to a dashboard.
#[derive(Debug, Serialize, ToSchema)]
pub struct RegistryInfo {
    pub datasets: Vec<DatasetInfo>,
    pub presets: Vec<PresetInfo>,
    /// Built-in dashboard layouts a merchant can use or fork.
    pub boards: Vec<BoardInfo>,
    /// Named relative windows accepted in `period.preset`.
    pub period_presets: Vec<String>,
}

fn dataset_info(d: &'static Dataset) -> DatasetInfo {
    DatasetInfo {
        id: d.id.into(),
        title: d.title.into(),
        help: normalize(d.help),
        dimensions: d
            .dims
            .iter()
            .map(|dim| FieldInfo {
                id: dim.id.into(),
                label: dim.label.into(),
                kind: dim.kind,
                help: None,
                time: dim.time,
            })
            .collect(),
        measures: d
            .measures
            .iter()
            .map(|m| FieldInfo {
                id: m.id.into(),
                label: m.label.into(),
                kind: m.kind,
                help: Some(m.help.into()),
                time: false,
            })
            .collect(),
        filters: d
            .filters
            .iter()
            .map(|f| FilterInfo {
                id: f.id.into(),
                label: f.label.into(),
                help: f.help.into(),
                values: f.values().into_iter().map(Into::into).collect(),
                default: f.default.into(),
            })
            .collect(),
        default_measures: d.default_measures.iter().map(|s| (*s).into()).collect(),
    }
}

/// The grain a preset will produce, derived the same way the compiler does it —
/// so the picker's icon always matches what comes back.
fn preset_grain(p: &Preset) -> Grain {
    let time = p
        .dimensions
        .first()
        .and_then(|first| {
            super::schema::dataset(p.dataset).and_then(|ds| ds.dim(first).map(|d| d.time))
        })
        .unwrap_or(false);
    match p.dimensions.len() {
        0 => Grain::Scalar,
        1 if time => Grain::Series,
        1 => Grain::Categorical,
        _ => Grain::Table,
    }
}

fn preset_info(p: &'static Preset) -> PresetInfo {
    PresetInfo {
        id: p.id.into(),
        title: p.title.into(),
        description: p.description.into(),
        category: p.category.into(),
        dataset: p.dataset.into(),
        grain: preset_grain(p),
        viz: p.viz,
        default_period: p.default_period,
        permission: p.permission.into(),
    }
}

fn board_info(b: &'static BoardTemplate) -> BoardInfo {
    BoardInfo {
        key: b.key.into(),
        title: b.title.into(),
        description: b.description.into(),
        widgets: b.widgets.iter().map(|w| (*w).into()).collect(),
    }
}

/// The whole registry. `allowed` filters presets to the permissions the caller
/// actually holds, so a widget picker never offers a metric that would 403.
pub fn registry(allowed: &dyn Fn(&str) -> bool) -> RegistryInfo {
    let presets: Vec<PresetInfo> = PRESETS
        .iter()
        .filter(|p| allowed(p.permission))
        .map(preset_info)
        .collect();
    let visible: Vec<&str> = presets.iter().map(|p| p.id.as_str()).collect();
    RegistryInfo {
        datasets: DATASETS.iter().map(dataset_info).collect(),
        // A board only lists widgets the caller may actually see; a board left
        // with nothing is dropped rather than shown empty.
        boards: DEFAULT_BOARDS
            .iter()
            .map(board_info)
            .map(|mut b| {
                b.widgets.retain(|w| visible.contains(&w.as_str()));
                b
            })
            .filter(|b| !b.widgets.is_empty())
            .collect(),
        presets,
        period_presets: PeriodPreset::ALL.iter().map(|s| (*s).into()).collect(),
    }
}

/// Collapse the multi-line `&'static str` help text (wrapped for source
/// readability with `\` continuations) into a single spaced line.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A compact text rendering of the whole semantic layer, for the model's system
/// prompt. Byte-stable across requests so it sits in the cacheable prefix.
pub fn schema_digest() -> &'static str {
    static DIGEST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DIGEST.get_or_init(|| {
        let mut out = String::with_capacity(8192);
        out.push_str("DATASETS (each fixes the grain — pick the one whose row is the thing being counted):\n");
        for d in DATASETS {
            out.push_str(&format!("\n## {} — {}\n{}\n", d.id, d.title, normalize(d.help)));
            out.push_str("dimensions: ");
            out.push_str(
                &d.dims
                    .iter()
                    .map(|x| x.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            out.push_str("\nmeasures:\n");
            for m in d.measures {
                out.push_str(&format!("  - {}: {}\n", m.id, m.help));
            }
            if !d.filters.is_empty() {
                out.push_str("filters:\n");
                for f in d.filters {
                    out.push_str(&format!(
                        "  - {} = {} (default {}) — {}\n",
                        f.id,
                        f.values().join(" | "),
                        f.default,
                        f.help
                    ));
                }
            }
        }
        out.push_str("\nPRESETS (one-call shortcuts for common questions):\n");
        for p in PRESETS {
            out.push_str(&format!("  - {}: {}\n", p.id, p.description));
        }
        out.push_str("\nPERIOD PRESETS: ");
        out.push_str(&PeriodPreset::ALL.join(", "));
        out.push('\n');
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_covers_every_dataset_measure_and_preset() {
        let d = schema_digest();
        for ds in DATASETS {
            assert!(d.contains(ds.id), "digest omits dataset {}", ds.id);
            for m in ds.measures {
                assert!(d.contains(m.id), "digest omits measure {}", m.id);
            }
        }
        for p in PRESETS {
            assert!(d.contains(p.id), "digest omits preset {}", p.id);
        }
    }

    #[test]
    fn the_digest_is_stable_across_calls() {
        // It sits in the model's cacheable prefix; a byte of drift per request
        // would silently cost a cache miss every time.
        assert_eq!(schema_digest().as_ptr(), schema_digest().as_ptr());
    }

    #[test]
    fn help_text_is_flattened_not_left_with_source_indentation() {
        let info = dataset_info(super::super::schema::dataset("orders").unwrap());
        assert!(!info.help.contains("  "));
        assert!(!info.help.contains('\n'));
    }

    #[test]
    fn permissions_filter_presets_and_prune_boards() {
        // A caller with only `reports` sees no attendance or shift metrics.
        let r = registry(&|p| p == "reports");
        assert!(r.presets.iter().all(|p| p.permission == "reports"));
        assert!(!r.presets.iter().any(|p| p.id == "lateness_by_employee"));
        // The People board is pruned to the one widget this caller may see
        // (waiter performance is order data), rather than rendering rows that
        // would 403.
        let people = r.boards.iter().find(|b| b.key == "people").unwrap();
        assert_eq!(people.widgets, vec!["waiter_performance"]);
        // Sales survives, minus nothing.
        assert!(r.boards.iter().any(|b| b.key == "sales"));
    }

    #[test]
    fn with_every_permission_nothing_is_hidden() {
        let r = registry(&|_| true);
        assert_eq!(r.presets.len(), PRESETS.len());
        assert_eq!(r.boards.len(), DEFAULT_BOARDS.len());
        assert_eq!(r.datasets.len(), DATASETS.len());
    }

    #[test]
    fn preset_grain_matches_what_the_compiler_produces() {
        use crate::analytics::compile::{CompileCtx, compile};
        let ctx = CompileCtx {
            tz: chrono_tz::Africa::Cairo,
            now: crate::analytics::spec::parse_flexible_date("2026-09-02T10:00:00Z").unwrap(),
        };
        for p in PRESETS {
            let compiled = compile(&p.to_spec(None), &ctx).unwrap();
            // `top_per` deliberately reshapes into a faceted table.
            if p.top_per.is_some() {
                continue;
            }
            assert_eq!(
                preset_grain(p),
                compiled.grain,
                "preset {} advertises the wrong grain",
                p.id
            );
        }
    }
}
