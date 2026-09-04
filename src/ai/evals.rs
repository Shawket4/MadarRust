//! The golden evaluation set.
//!
//! `evals/cases.json` holds 200 questions with the tool call each should
//! produce. Two things use it, and they are deliberately separate:
//!
//!   * **This module** validates the cases against the live registry, in the
//!     ordinary test suite. Every preset, dataset, dimension, measure, period
//!     and filter value a case expects must exist. Renaming a preset therefore
//!     fails the build rather than quietly making an eval case unsatisfiable —
//!     which is the failure mode that turns an eval suite into decoration.
//!   * **`src/bin/eval.rs`** actually runs them against a live model. That needs
//!     an API key and costs money per run, so it is a binary you invoke, not a
//!     test that runs on every commit.
//!
//! The split matters: the half of each case that is *schema-derived* is
//! verifiable for free and checked constantly; the half that is *intent* — what
//! a given Arabic phrase should resolve to — can only be judged by running it,
//! and is what the runner is for.
//!
//! # Confidence
//!
//! Every case is tagged. `high` means the expectation follows mechanically from
//! the schema and a literal reading of the question. `review` means a judgement
//! was made about intent — "the weekend", "best", "the new branch" — and the
//! right answer is a business decision, not a code fact. Accuracy is reported
//! separately for the two, because averaging them hides which is which.

use serde::Deserialize;

/// The whole case file.
#[derive(Debug, Clone, Deserialize)]
pub struct CaseFile {
    /// Frozen "now", so period expectations are checkable.
    pub now: String,
    pub cases: Vec<Case>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub id: String,
    /// `en`, `ar`, or `mixed`.
    pub lang: String,
    pub question: String,
    pub now: String,
    pub category: String,
    /// `high` | `review`.
    pub confidence: String,
    pub expect: Expect,
    #[serde(default)]
    pub note: Option<String>,
}

/// What the model should produce. Fields are optional because a case asserts
/// only what it is about — a period case does not pin the measures.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub dataset: Option<String>,
    #[serde(default)]
    pub dimensions: Option<Vec<String>>,
    #[serde(default)]
    pub measures: Option<Vec<String>>,
    #[serde(default)]
    pub period: Option<String>,
    #[serde(default)]
    pub filters: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub compare: Option<String>,
    #[serde(default)]
    pub share: Option<bool>,
    #[serde(default)]
    pub cumulative: Option<bool>,
    #[serde(default)]
    pub top_per: Option<String>,
    /// For a case whose right answer is to resolve a name to a specific KIND of
    /// entity rather than to run a particular query.
    #[serde(default)]
    pub entity_kind: Option<String>,
    /// For negative cases: `unknown_entity`, `no_tool`, `clarify`, `refused`,
    /// `invalid_period`, `review`.
    #[serde(default)]
    pub outcome: Option<String>,
    /// Equally-correct alternatives.
    ///
    /// Several questions have more than one right answer: "what hours are
    /// busiest" is served by `sales_by_hour` OR `peak_hours_heatmap`, and a
    /// custom `query_metrics` that computes the same thing as a preset is not
    /// wrong either. Scoring those as misses measures the eval's opinion rather
    /// than the model's accuracy.
    #[serde(default)]
    pub accept_presets: Vec<String>,
    /// True when composing the query by hand instead of using the preset is
    /// also correct.
    #[serde(default)]
    pub accept_custom_query: bool,
}

/// The case file, embedded at build time so the binary and the tests read the
/// same bytes and neither depends on the working directory.
pub const CASES_JSON: &str = include_str!("../../evals/cases.json");

pub fn load() -> CaseFile {
    serde_json::from_str(CASES_JSON).expect("evals/cases.json is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::{entities, presets, schema, spec::PeriodPreset};
    use std::collections::HashSet;

    #[test]
    fn the_case_file_parses_and_ids_are_unique() {
        let f = load();
        assert!(
            f.cases.len() >= 200,
            "the set is sized at 200+ cases, found {}",
            f.cases.len()
        );
        let mut seen = HashSet::new();
        for c in &f.cases {
            assert!(seen.insert(c.id.clone()), "duplicate case id {}", c.id);
            assert!(!c.question.trim().is_empty(), "{}: empty question", c.id);
            assert!(
                matches!(c.confidence.as_str(), "high" | "review"),
                "{}: confidence must be high|review",
                c.id
            );
        }
    }

    /// The guard that keeps this an eval rather than decoration: a case may only
    /// expect things that actually exist. Renaming a preset breaks the build
    /// here instead of silently making a case unsatisfiable.
    #[test]
    fn every_expectation_names_something_real() {
        for c in load().cases {
            let e = &c.expect;
            if let Some(p) = &e.preset {
                assert!(
                    presets::preset(p).is_some(),
                    "{}: unknown preset '{p}'",
                    c.id
                );
            }
            // Alternatives are expectations too. An `accept_presets` entry that
            // names nothing real quietly collapses the case back to a single
            // right answer — the same decoration failure this file guards.
            for alt in &e.accept_presets {
                assert!(
                    presets::preset(alt).is_some(),
                    "{}: unknown accept_preset '{alt}'",
                    c.id
                );
            }
            if let Some(d) = &e.dataset {
                let ds =
                    schema::dataset(d).unwrap_or_else(|| panic!("{}: unknown dataset '{d}'", c.id));
                for dim in e.dimensions.iter().flatten() {
                    assert!(
                        ds.dim(dim).is_some(),
                        "{}: '{d}' has no dimension '{dim}'",
                        c.id
                    );
                }
                for m in e.measures.iter().flatten() {
                    assert!(
                        ds.measure(m).is_some(),
                        "{}: '{d}' has no measure '{m}'",
                        c.id
                    );
                }
                for (k, v) in e.filters.iter().flatten() {
                    let f = ds
                        .filter(k)
                        .unwrap_or_else(|| panic!("{}: '{d}' has no filter '{k}'", c.id));
                    assert!(
                        f.option(v).is_some(),
                        "{}: filter '{k}' has no value '{v}'",
                        c.id
                    );
                }
                if let Some(tp) = &e.top_per {
                    assert!(
                        ds.dim(tp).is_some(),
                        "{}: top_per '{tp}' is not a dimension",
                        c.id
                    );
                }
            }
            if let Some(p) = &e.period {
                assert!(
                    PeriodPreset::ALL.contains(&p.as_str()),
                    "{}: unknown period preset '{p}'",
                    c.id
                );
            }
            if let Some(k) = &e.entity_kind {
                assert!(
                    entities::kind(k).is_some(),
                    "{}: unknown entity kind '{k}'",
                    c.id
                );
            }
            if let Some(t) = &e.tool {
                assert!(
                    crate::ai::tools::tool_defs().iter().any(|d| d.name == t),
                    "{}: unknown tool '{t}'",
                    c.id
                );
            }
        }
    }

    /// Coverage is the point of sizing the set, not the count. Report what is
    /// missing rather than asserting a number.
    #[test]
    fn the_set_covers_the_surface_it_is_meant_to() {
        let cases = load().cases;

        // Every period preset appears somewhere.
        let periods: HashSet<&str> = cases
            .iter()
            .filter_map(|c| c.expect.period.as_deref())
            .collect();
        for p in PeriodPreset::ALL {
            assert!(periods.contains(p), "no case exercises period '{p}'");
        }

        // Every dataset appears.
        let datasets: HashSet<&str> = cases
            .iter()
            .filter_map(|c| c.expect.dataset.as_deref())
            .collect();
        for ds in schema::DATASETS {
            assert!(
                datasets.contains(ds.id),
                "no case exercises dataset '{}'",
                ds.id
            );
        }

        // Every entity kind that can be named in a question appears.
        let kinds: HashSet<&str> = cases
            .iter()
            .filter_map(|c| c.expect.entity_kind.as_deref())
            .collect();
        for k in [
            "waiter",
            "cashier",
            "employee",
            "branch",
            "product",
            "ingredient",
        ] {
            assert!(kinds.contains(k), "no case distinguishes entity kind '{k}'");
        }

        // Both transforms and negatives are represented.
        let categories: HashSet<&str> = cases.iter().map(|c| c.category.as_str()).collect();
        for cat in [
            "tool_selection",
            "period_resolution",
            "custom_query",
            "entity_resolution",
            "transform",
            "filters",
            "negative",
            "adversarial",
        ] {
            assert!(categories.contains(cat), "no cases in category '{cat}'");
        }
    }

    #[test]
    fn roughly_half_the_set_is_arabic() {
        // A bilingual product whose eval is all English measures the wrong
        // thing: Arabic routing is where the failures actually are.
        let cases = load().cases;
        let arabic = cases.iter().filter(|c| c.lang == "ar").count();
        let ratio = arabic as f64 / cases.len() as f64;
        assert!(
            (0.4..=0.6).contains(&ratio),
            "Arabic share is {ratio:.2}, expected roughly half"
        );
    }

    #[test]
    fn judgement_calls_are_tagged_and_explained() {
        // A `review` case without a stated reason cannot be confirmed or
        // corrected by anyone but its author.
        for c in load().cases.iter().filter(|c| c.confidence == "review") {
            assert!(
                c.note.as_deref().is_some_and(|n| n.len() > 20),
                "{}: a review case must explain the judgement made",
                c.id
            );
        }
    }

    /// Every regression case must trace to a real failure. A regression suite
    /// whose cases have no story attached decays into ordinary cases nobody
    /// dares delete.
    #[test]
    fn regression_cases_record_the_failure_they_pin() {
        let cases = load().cases;
        let regressions: Vec<_> = cases
            .iter()
            .filter(|c| c.category == "regression")
            .collect();
        assert!(
            !regressions.is_empty(),
            "the regression category exists but is empty"
        );
        for c in regressions {
            assert!(
                c.note.as_deref().is_some_and(|n| n.len() > 40),
                "{}: a regression case must say what actually broke",
                c.id
            );
        }
    }

    #[test]
    fn negative_cases_expect_an_outcome_not_a_query() {
        // A negative case that accidentally pins a tool would be testing the
        // opposite of what it is for.
        for c in load()
            .cases
            .iter()
            .filter(|c| c.category == "negative" || c.category == "adversarial")
        {
            assert!(c.expect.outcome.is_some(), "{}: needs an outcome", c.id);
            assert!(
                c.expect.preset.is_none(),
                "{}: negative case pins a preset",
                c.id
            );
        }
    }
}
