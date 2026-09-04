//! Staff names never reach the language model.
//!
//! # The problem
//!
//! To answer "who sold the most last week" the model has to see the result
//! rows, and on the people dimensions ([`schema::PERSONAL_DIMENSIONS`]) those
//! rows are your employees' real names. Sending them to Gemini or Groq is a
//! disclosure of personal data to a third party that nobody decided to make.
//!
//! Withholding the rows is not an answer: the model would have nothing to state
//! and every reply would degrade to "the table below has your figures", which
//! is the whole feature gone.
//!
//! # What this does instead
//!
//! Substitute on the way out, restore on the way in.
//!
//! ```text
//!   to the model:   {"waiter": "E-3", "revenue": 128400}
//!   model replies:  "E-3 led on revenue at 1,284 EGP."
//!   to the merchant:"Ahmed Hassan led on revenue at 1,284 EGP."
//! ```
//!
//! The model never sees a name; the merchant sees the right answer. The result
//! blocks the client renders are untouched — they never pass through the model
//! at all, so a chart or a table always shows real names regardless.
//!
//! # Why the directory is built up front
//!
//! Codes are assigned from the org's user list, ordered by id, rather than from
//! whatever rows a particular query happened to return. That makes a person's
//! code **stable across queries and across turns**, which matters twice: a
//! follow-up ("what about the second one?") refers to the same code it saw
//! before, and a prior turn's answer text — which is stored with real names for
//! display — can be re-substituted on replay using the same map. A per-query map
//! would give the same person different codes in consecutive messages, which
//! reads to a model as different people.
//!
//! # Failure mode
//!
//! Restoration is a whole-word replacement of `E-<n>`. If it misses, the
//! merchant sees `E-3` — visibly wrong, not silently wrong. That is the right
//! way round: a leaked name cannot be un-leaked, whereas a stray code is
//! obvious and harmless.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::{analytics::entities as schema_entities, analytics::schema, db::Db, errors::AppError};

/// Prefix for a staff placeholder. Short, and shaped like an identifier so a
/// model copies it verbatim instead of rewording it the way it would reword
/// "Employee 3".
const STAFF_PREFIX: &str = "E-";

/// Names shorter than this are not substituted in free text.
///
/// A two-letter name would match inside ordinary words and mangle the replayed
/// history. Row values are still substituted exactly regardless of length —
/// that path compares whole cells, not text.
const MIN_FREE_TEXT_NAME: usize = 3;

/// A per-organization map between staff names and their placeholders.
#[derive(Debug, Clone, Default)]
pub struct Directory {
    /// Real name → placeholder.
    to_code: HashMap<String, String>,
    /// Placeholder → real name.
    to_name: HashMap<String, String>,
    /// Names ordered longest-first, so "Ahmed Hassan" is matched before
    /// "Ahmed" and a full name is never left half-substituted.
    ordered_names: Vec<String>,
}

impl Directory {
    /// Build from the organization's users.
    ///
    /// Runs on the RLS-scoped tenant pool, so it sees exactly one merchant's
    /// staff. Ordering by id — not by name — is what makes a code stable when
    /// somebody is renamed or a new hire lands alphabetically in the middle.
    pub async fn load(db: &Db) -> Result<Self, AppError> {
        // Sourced from the entity registry rather than a query written here, so
        // adding a personal kind automatically extends what gets pseudonymised.
        // `list_people` deduplicates by user id across kinds: one human who is
        // both a waiter and an attendance record must get ONE code, or an answer
        // mentioning them twice reads as two different people.
        let people = schema_entities::list_people(db).await?;
        Ok(Self::from_names(people.into_iter().map(|e| e.name)))
    }

    /// Build from an explicit list, for tests and for callers that already hold
    /// the names.
    pub fn from_names<I: IntoIterator<Item = String>>(names: I) -> Self {
        let mut dir = Directory::default();
        for name in names {
            let name = name.trim().to_string();
            if name.is_empty() || dir.to_code.contains_key(&name) {
                // Two people with the same name are indistinguishable in a
                // result — the SQL groups by name — so they share one code.
                continue;
            }
            let code = format!("{STAFF_PREFIX}{}", dir.to_code.len() + 1);
            dir.to_name.insert(code.clone(), name.clone());
            dir.to_code.insert(name.clone(), code);
            dir.ordered_names.push(name);
        }
        // Longest first: "Ahmed Hassan" must win over "Ahmed".
        dir.ordered_names
            .sort_by(|a, b| b.chars().count().cmp(&a.chars().count()).then(a.cmp(b)));
        dir
    }

    pub fn is_empty(&self) -> bool {
        self.to_code.is_empty()
    }

    /// The placeholder for a name, if it is known staff.
    fn code_for(&self, name: &str) -> Option<&str> {
        self.to_code.get(name.trim()).map(String::as_str)
    }

    /// Replace personal cells in a result row with their placeholders.
    ///
    /// Only columns the registry marks personal are touched, and only by exact
    /// cell match. A product called "Ahmed's Special" is untouched, because
    /// this never scans text — it compares whole values in known columns.
    ///
    /// A name with no code (a deleted user still referenced by an old order)
    /// is replaced with a generic marker rather than passed through: an unknown
    /// person is still a person.
    pub fn redact_row(&self, row: &Map<String, Value>) -> Map<String, Value> {
        let mut out = Map::with_capacity(row.len());
        for (key, value) in row {
            if !schema::is_personal_dimension(key) {
                out.insert(key.clone(), value.clone());
                continue;
            }
            let redacted = match value.as_str() {
                Some(name) => match self.code_for(name) {
                    Some(code) => Value::String(code.to_string()),
                    // Placeholders that carry no identity: "Unassigned",
                    // "Unknown" and the like are labels the SQL produced, not
                    // people, and the model needs them to say "unassigned
                    // orders". Anything else unknown becomes an opaque marker.
                    None if is_structural_label(name) => value.clone(),
                    None => Value::String(format!("{STAFF_PREFIX}?")),
                },
                None => value.clone(),
            };
            out.insert(key.clone(), redacted);
        }
        out
    }

    /// Replace staff names appearing in free text with their placeholders.
    ///
    /// Used on replayed answers from earlier turns, which are stored with real
    /// names because that is what the merchant saw. Whole-word matching only,
    /// longest name first.
    pub fn redact_text(&self, text: &str) -> String {
        if self.is_empty() || text.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for name in &self.ordered_names {
            if name.chars().count() < MIN_FREE_TEXT_NAME {
                continue;
            }
            let Some(code) = self.to_code.get(name) else {
                continue;
            };
            out = replace_whole_word(&out, name, code);
        }
        out
    }

    /// Put the real names back into text the model produced.
    ///
    /// An unknown code is left as-is: visible and obviously wrong beats
    /// silently substituting the wrong person.
    pub fn restore_text(&self, text: &str) -> String {
        if self.to_name.is_empty() || text.is_empty() {
            return text.to_string();
        }
        code_pattern()
            .replace_all(text, |caps: &regex::Captures| {
                let code = &caps[0];
                self.to_name
                    .get(code)
                    .cloned()
                    .unwrap_or_else(|| code.to_string())
            })
            .into_owned()
    }
}

/// `E-12` as a standalone token.
fn code_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bE-\d+\b").expect("code_pattern is a valid regex"))
}

/// Labels the SQL produced for a missing relation, rather than a person's name.
/// They carry no identity and the model needs them to talk about unassigned
/// work, so they pass through.
fn is_structural_label(value: &str) -> bool {
    matches!(
        value,
        "Unassigned" | "Unknown" | "No department" | "Unspecified"
    )
}

/// Case-sensitive whole-word replacement.
///
/// Written by hand rather than with a regex because a name is arbitrary text —
/// escaping it into a pattern for every name on every turn is both slower and a
/// quiet injection risk if the escaping is ever wrong. "Word" here means the
/// match is not flanked by an alphanumeric character, which is the right rule
/// for Arabic and Latin script alike.
fn replace_whole_word(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let mut out = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(pos) = rest.find(needle) {
        let before_ok = rest[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after = &rest[pos + needle.len()..];
        let after_ok = after.chars().next().is_none_or(|c| !c.is_alphanumeric());

        out.push_str(&rest[..pos]);
        if before_ok && after_ok {
            out.push_str(replacement);
        } else {
            out.push_str(needle);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> Directory {
        Directory::from_names(
            ["Ahmed Hassan", "Ahmed", "Mona Farouk", "Li"]
                .into_iter()
                .map(String::from),
        )
    }

    #[test]
    fn codes_are_assigned_in_order_and_are_reversible() {
        let d = directory();
        assert_eq!(d.code_for("Ahmed Hassan"), Some("E-1"));
        assert_eq!(d.code_for("Ahmed"), Some("E-2"));
        assert_eq!(
            d.restore_text("E-1 and E-3"),
            "Ahmed Hassan and Mona Farouk"
        );
    }

    #[test]
    fn a_repeated_name_shares_one_code() {
        // Two people with the same name are indistinguishable in a grouped
        // result, so giving them separate codes would be a lie.
        let d = Directory::from_names(["Ali", "Ali"].into_iter().map(String::from));
        assert_eq!(d.to_name.len(), 1);
    }

    #[test]
    fn only_personal_columns_are_redacted() {
        let d = directory();
        let row: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "waiter": "Ahmed Hassan",
            "branch": "Marina",
            "product": "Latte",
            "revenue": 128400
        }))
        .unwrap();
        let out = d.redact_row(&row);
        assert_eq!(out["waiter"], "E-1");
        // The model needs these to reason at all.
        assert_eq!(out["branch"], "Marina");
        assert_eq!(out["product"], "Latte");
        assert_eq!(out["revenue"], 128400);
    }

    #[test]
    fn a_product_that_contains_a_staff_name_is_untouched() {
        // Row redaction compares whole cells in known columns; it never scans
        // text, so this cannot happen — pinned because a "smarter" scanning
        // implementation would break it.
        let d = directory();
        let row: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "product": "Ahmed's Special", "units_sold": 3
        }))
        .unwrap();
        assert_eq!(d.redact_row(&row)["product"], "Ahmed's Special");
    }

    #[test]
    fn an_unknown_person_becomes_an_opaque_marker_not_a_passthrough() {
        // A deleted user still referenced by an old order is still a person.
        let d = directory();
        let row: Map<String, Value> =
            serde_json::from_value(serde_json::json!({ "waiter": "Someone Else" })).unwrap();
        assert_eq!(d.redact_row(&row)["waiter"], "E-?");
    }

    #[test]
    fn structural_labels_pass_through_so_the_model_can_talk_about_them() {
        let d = directory();
        for label in ["Unassigned", "Unknown"] {
            let row: Map<String, Value> =
                serde_json::from_value(serde_json::json!({ "waiter": label })).unwrap();
            assert_eq!(d.redact_row(&row)["waiter"], label);
        }
    }

    #[test]
    fn free_text_substitution_prefers_the_longest_name() {
        // "Ahmed Hassan" must not be left as "E-2 Hassan".
        let d = directory();
        assert_eq!(d.redact_text("Ahmed Hassan led"), "E-1 led");
        assert_eq!(d.redact_text("Ahmed led"), "E-2 led");
    }

    #[test]
    fn free_text_substitution_is_whole_word_only() {
        let d = Directory::from_names(["Sam".to_string()]);
        // A name inside another word is not a mention of that person.
        assert_eq!(d.redact_text("Samosa sold 12"), "Samosa sold 12");
        assert_eq!(d.redact_text("Sam sold 12"), "E-1 sold 12");
        assert_eq!(d.redact_text("(Sam)"), "(E-1)");
    }

    #[test]
    fn very_short_names_are_left_alone_in_free_text() {
        // "Li" would match inside "Line", "Limit", "Delivery".
        let d = directory();
        assert_eq!(d.redact_text("Li sold Line items"), "Li sold Line items");
        // ...but the row path still redacts it exactly.
        let row: Map<String, Value> =
            serde_json::from_value(serde_json::json!({ "waiter": "Li" })).unwrap();
        assert_eq!(d.redact_row(&row)["waiter"], "E-4");
    }

    #[test]
    fn arabic_names_round_trip() {
        let d = Directory::from_names(["أحمد حسن".to_string(), "منى فاروق".to_string()]);
        let redacted = d.redact_text("أحمد حسن باع الأكثر");
        assert!(!redacted.contains("أحمد"), "{redacted}");
        assert_eq!(d.restore_text(&redacted), "أحمد حسن باع الأكثر");
    }

    #[test]
    fn an_unknown_code_is_left_visible_rather_than_guessed() {
        // Fails obviously, not silently as the wrong person.
        let d = directory();
        assert_eq!(d.restore_text("E-99 led"), "E-99 led");
    }

    #[test]
    fn restoring_does_not_touch_ordinary_text() {
        let d = directory();
        for text in [
            "Revenue was 1,284 EGP across 3 branches.",
            "No sales in that period.",
            "Order MDR-260831-0042 was voided.",
        ] {
            assert_eq!(d.restore_text(text), text);
        }
    }

    #[test]
    fn an_empty_directory_is_a_no_op() {
        // A merchant with no staff rows must not have their text mangled.
        let d = Directory::default();
        assert_eq!(d.redact_text("Latte led"), "Latte led");
        assert_eq!(d.restore_text("E-1 led"), "E-1 led");
    }

    #[test]
    fn a_full_round_trip_survives_the_model() {
        let d = directory();
        let row: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "waiter": "Mona Farouk", "revenue": 90000
        }))
        .unwrap();
        let seen = d.redact_row(&row);
        // What the model is given carries no name anywhere.
        let wire = serde_json::to_string(&seen).unwrap();
        assert!(!wire.contains("Mona"), "{wire}");
        // What it says comes back with the name restored.
        let reply = format!(
            "{} led on revenue at 900 EGP.",
            seen["waiter"].as_str().unwrap()
        );
        assert_eq!(
            d.restore_text(&reply),
            "Mona Farouk led on revenue at 900 EGP."
        );
    }
}
