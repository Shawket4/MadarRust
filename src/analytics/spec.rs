//! [`QuerySpec`] — the single intermediate representation every analytics
//! request is expressed in.
//!
//! A dashboard widget, a curated preset, and a question the AI agent just
//! parsed all produce the *same* value here, and it is the only thing
//! [`super::compile`] accepts. That is what stops the two-engines problem: there
//! is exactly one path from "what was asked" to SQL, so there is exactly one
//! place where correctness and security have to hold.
//!
//! The spec is `Deserialize`, and a language model's tool-call arguments are
//! deserialized straight into it. Anything malformed is a typed rejection with
//! a message good enough to hand *back* to the model so it can retry — see
//! [`super::compile::compile`].

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

use super::types::{Compare, Dir, Viz};

/// Hard ceiling on the rows a single query may return.
pub const MAX_LIMIT: u32 = 1000;
/// Applied when a caller names no limit.
pub const DEFAULT_LIMIT: u32 = 100;

/// A fully-specified analytics question.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuerySpec {
    /// Dataset id — fixes the grain. See `GET /metrics/schema`.
    pub dataset: String,
    /// GROUP BY axes, outermost first. Empty = a single total row.
    #[serde(default)]
    pub dimensions: Vec<String>,
    /// Aggregates to compute. Empty = the dataset's headline measures.
    #[serde(default)]
    pub measures: Vec<String>,
    /// Filter id → chosen value. Each value selects a pre-written predicate.
    #[serde(default)]
    pub filters: BTreeMap<String, String>,
    #[serde(default)]
    pub period: Period,
    /// Which measure orders the result, and in which direction.
    #[serde(default)]
    pub sort: Option<Sort>,
    /// Row cap, clamped to [`MAX_LIMIT`].
    #[serde(default)]
    pub limit: Option<u32>,
    /// Period-over-period comparison.
    #[serde(default)]
    pub compare: Compare,
    #[serde(default)]
    pub transform: Transform,
    /// Only keep groups whose sort measure reaches this value.
    #[serde(default)]
    pub having_min: Option<i64>,
    /// Preferred visualization. Omitted or [`Viz::Auto`] lets the backend pick
    /// from the result shape.
    #[serde(default)]
    pub viz: Option<Viz>,
    /// Narrow to ONE branch by name. Fuzzy-matched *within* the caller's
    /// accessible branches, so it can only ever narrow, never widen. Dashboards
    /// use the request-level scope instead and leave this unset.
    #[serde(default)]
    pub branch: Option<String>,
}

/// Post-aggregation shaping.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Transform {
    /// Add each row's percentage of the grand total.
    #[serde(default)]
    pub share: bool,
    /// Add a running total in time order. Needs a time dimension.
    #[serde(default)]
    pub cumulative: bool,
    /// Keep only the top N rows *within* each value of a dimension — "the best
    /// seller in every branch".
    #[serde(default)]
    pub top_per: Option<TopPer>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TopPer {
    /// Which of the chosen dimensions to rank within.
    pub dimension: String,
    /// How many rows to keep per group.
    #[serde(default = "one")]
    pub n: u32,
}

fn one() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Sort {
    /// A measure id from `measures`.
    pub measure: String,
    #[serde(default = "desc")]
    pub dir: Dir,
}

fn desc() -> Dir {
    Dir::Desc
}

/// The reporting window.
///
/// Prefer a [`PeriodPreset`]: it is resolved server-side against the merchant's
/// timezone at query time, which means a dashboard widget saying "last 30 days"
/// stays correct forever, and a language model never has to do calendar
/// arithmetic — historically the single largest source of wrong answers.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Period {
    /// A named relative window. Takes precedence over `from`/`to`.
    #[serde(default)]
    pub preset: Option<PeriodPreset>,
    /// Explicit inclusive lower bound.
    #[serde(default, deserialize_with = "de_opt_flexible_date")]
    #[schema(value_type = Option<String>)]
    pub from: Option<DateTime<Utc>>,
    /// Explicit inclusive upper bound.
    #[serde(default, deserialize_with = "de_opt_flexible_date")]
    #[schema(value_type = Option<String>)]
    pub to: Option<DateTime<Utc>>,
}

/// Named relative windows, resolved in the merchant's timezone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeriodPreset {
    Today,
    Yesterday,
    ThisWeek,
    LastWeek,
    ThisMonth,
    LastMonth,
    ThisYear,
    LastYear,
    // EXPLICIT renames. serde's `rename_all = "snake_case"` does not insert an
    // underscore before a digit group, so `Last30Days` derives as `last30_days`
    // — while `ALL` below (which builds the tool schema and the OpenAPI enum)
    // advertises `last_30_days`. The two disagreed silently: every client that
    // sent the DOCUMENTED value was rejected as an unknown variant, so four of
    // the thirteen windows could not be used at all.
    //
    // `ALL` and these names are pinned together by
    // `every_advertised_period_round_trips`.
    //
    // The `alias` on each is for DATA ALREADY WRITTEN. Stored conversation
    // specs (`ai_messages.specs`) and any saved widget were serialized with the
    // old derived spelling, so dropping it would make every existing follow-up
    // silently lose its query. The alias is read-only compatibility: new writes
    // use the canonical name.
    #[serde(rename = "last_7_days", alias = "last7_days")]
    Last7Days,
    #[serde(rename = "last_30_days", alias = "last30_days")]
    Last30Days,
    #[serde(rename = "last_90_days", alias = "last90_days")]
    Last90Days,
    #[serde(rename = "last_12_months", alias = "last12_months")]
    Last12Months,
    AllTime,
}

impl PeriodPreset {
    pub const ALL: &'static [&'static str] = &[
        "today",
        "yesterday",
        "this_week",
        "last_week",
        "this_month",
        "last_month",
        "this_year",
        "last_year",
        "last_7_days",
        "last_30_days",
        "last_90_days",
        "last_12_months",
        "all_time",
    ];

    /// Inclusive local date bounds for this preset, given the merchant's
    /// *current local date*. Returns `None` for [`PeriodPreset::AllTime`].
    ///
    /// Every window **includes today**, rolling ones included. A merchant
    /// checking "last 7 days" at eight in the evening means to see the day they
    /// are standing in; excluding it to keep a rolling average tidy would be
    /// technically defensible and practically wrong. `last_7_days` is therefore
    /// today and the six days before it — seven days, ending now.
    fn local_bounds(self, today: NaiveDate) -> Option<(NaiveDate, NaiveDate)> {
        use PeriodPreset::*;
        let yesterday = today - Duration::days(1);
        // Monday-based week start.
        let week_start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
        let month_start = today.with_day(1).expect("day 1 is always valid");
        Some(match self {
            Today => (today, today),
            Yesterday => (yesterday, yesterday),
            ThisWeek => (week_start, today),
            LastWeek => (
                week_start - Duration::days(7),
                week_start - Duration::days(1),
            ),
            ThisMonth => (month_start, today),
            LastMonth => {
                let prev_end = month_start - Duration::days(1);
                (
                    prev_end.with_day(1).expect("day 1 is always valid"),
                    prev_end,
                )
            }
            ThisYear => (NaiveDate::from_ymd_opt(today.year(), 1, 1)?, today),
            LastYear => (
                NaiveDate::from_ymd_opt(today.year() - 1, 1, 1)?,
                NaiveDate::from_ymd_opt(today.year() - 1, 12, 31)?,
            ),
            Last7Days => (today - Duration::days(6), today),
            Last30Days => (today - Duration::days(29), today),
            Last90Days => (today - Duration::days(89), today),
            Last12Months => (today - Duration::days(364), today),
            AllTime => return None,
        })
    }
}

/// A period resolved to absolute UTC instants, ready to bind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedPeriod {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

impl ResolvedPeriod {
    /// Both bounds present — required by period-over-period comparison, which
    /// has to know the window length to shift it.
    pub fn is_bounded(&self) -> bool {
        self.from.is_some() && self.to.is_some()
    }
}

impl Period {
    /// A preset window, the common case.
    pub fn preset(p: PeriodPreset) -> Self {
        Self {
            preset: Some(p),
            from: None,
            to: None,
        }
    }

    /// Resolve to absolute UTC bounds in the merchant's timezone.
    ///
    /// A preset wins over explicit bounds. Local day bounds are widened to the
    /// full day — `from` at 00:00:00 local, `to` at 23:59:59.999999 local — so
    /// "yesterday" covers the whole of yesterday rather than a single instant,
    /// and a DST-ambiguous local midnight resolves to the earliest valid instant
    /// rather than failing.
    pub fn resolve(&self, tz: Tz, now: DateTime<Utc>) -> ResolvedPeriod {
        if let Some(preset) = self.preset {
            let today = now.with_timezone(&tz).date_naive();
            return match preset.local_bounds(today) {
                Some((start, end)) => ResolvedPeriod {
                    from: local_start_of_day(tz, start),
                    to: local_end_of_day(tz, end),
                },
                None => ResolvedPeriod {
                    from: None,
                    to: None,
                },
            };
        }
        ResolvedPeriod {
            from: self.from,
            to: self.to,
        }
    }
}

fn local_start_of_day(tz: Tz, d: NaiveDate) -> Option<DateTime<Utc>> {
    let naive = d.and_hms_opt(0, 0, 0)?;
    // `.earliest()` handles a DST spring-forward where local midnight does not
    // exist; falling back to noon guarantees a usable instant on that one day.
    tz.from_local_datetime(&naive)
        .earliest()
        .or_else(|| tz.from_local_datetime(&d.and_hms_opt(12, 0, 0)?).earliest())
        .map(|dt| dt.with_timezone(&Utc))
}

fn local_end_of_day(tz: Tz, d: NaiveDate) -> Option<DateTime<Utc>> {
    let naive = d.and_hms_micro_opt(23, 59, 59, 999_999)?;
    tz.from_local_datetime(&naive)
        .latest()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Parse a date liberally, because a language model emits several shapes for
/// what it means as one instant. In order: RFC-3339 with an offset; a naive
/// date-time with no offset (taken as UTC); a bare `YYYY-MM-DD`.
///
/// Being liberal here is what stopped single-day questions ("yesterday",
/// "امبارح") from failing: the model commonly emits `2026-07-07T23:59:59` with
/// no offset, which is neither valid RFC-3339 nor a bare date.
pub fn parse_flexible_date(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    const NAIVE_FORMS: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ];
    for fmt in NAIVE_FORMS {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

fn de_opt_flexible_date<'de, D>(d: D) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    let raw: Option<String> = Option::deserialize(d)?;
    match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => parse_flexible_date(s)
            .map(Some)
            .ok_or_else(|| D::Error::custom(format!("'{s}' is not a valid ISO-8601 date"))),
    }
}

/// Parse an IANA timezone name, falling back to Cairo — the deployment's home
/// timezone and the DB column default.
pub fn parse_tz(name: &str) -> Tz {
    name.parse().unwrap_or(chrono_tz::Africa::Cairo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cairo() -> Tz {
        chrono_tz::Africa::Cairo
    }

    fn at(s: &str) -> DateTime<Utc> {
        parse_flexible_date(s).unwrap()
    }

    #[test]
    fn flexible_dates_accept_what_models_actually_emit() {
        assert!(parse_flexible_date("2026-07-07").is_some());
        assert!(parse_flexible_date("2026-07-07T23:59:59Z").is_some());
        assert!(parse_flexible_date("2026-07-07T00:00:00+02:00").is_some());
        // The offset-less forms that used to be rejected outright.
        assert!(parse_flexible_date("2026-07-07T23:59:59").is_some());
        assert!(parse_flexible_date("2026-07-07 00:00:00").is_some());
        assert!(parse_flexible_date("  2026-07-07T12:30  ").is_some());
        // Garbage and injection attempts are never coerced into a date.
        assert!(parse_flexible_date("not a date").is_none());
        assert!(parse_flexible_date("2026-13-40").is_none());
        assert!(parse_flexible_date("'; DROP TABLE orders; --").is_none());
    }

    #[test]
    fn yesterday_covers_a_whole_local_day() {
        // 2026-09-02 10:00 UTC = 13:00 in Cairo, which observes DST (UTC+3 in
        // September). Hard-coding +2 here is exactly the bug the tz database
        // exists to prevent, so the expectation is the real offset.
        let now = at("2026-09-02T10:00:00Z");
        let p = Period::preset(PeriodPreset::Yesterday).resolve(cairo(), now);
        // Cairo yesterday = 2026-09-01 00:00 local = 2026-08-31 21:00 UTC.
        assert_eq!(p.from.unwrap(), at("2026-08-31T21:00:00Z"));
        assert!(p.to.unwrap() > at("2026-09-01T20:59:00Z"));
        assert!(p.to.unwrap() < at("2026-09-01T21:00:01Z"));
    }

    #[test]
    fn every_window_includes_today_and_spans_the_stated_length() {
        let now = at("2026-09-02T10:00:00Z"); // Wednesday, Cairo
        let last7 = Period::preset(PeriodPreset::Last7Days).resolve(cairo(), now);
        // Ends at the end of TODAY: a merchant checking at 8pm means today too.
        assert!(last7.to.unwrap() > at("2026-09-02T20:00:00Z"));
        // ...and covers seven days in total, not eight.
        assert_eq!(last7.from.unwrap(), at("2026-08-26T21:00:00Z"));

        let this_month = Period::preset(PeriodPreset::ThisMonth).resolve(cairo(), now);
        // Starts 2026-09-01 local, ends at the end of today.
        assert_eq!(this_month.from.unwrap(), at("2026-08-31T21:00:00Z"));
        assert!(this_month.to.unwrap() > at("2026-09-02T20:00:00Z"));
    }

    #[test]
    fn last_month_is_the_previous_calendar_month() {
        let now = at("2026-09-02T10:00:00Z");
        let p = Period::preset(PeriodPreset::LastMonth).resolve(cairo(), now);
        // 2026-08-01 00:00 Cairo .. 2026-08-31 23:59:59 Cairo (UTC+3 in summer).
        assert_eq!(p.from.unwrap(), at("2026-07-31T21:00:00Z"));
        assert!(p.to.unwrap() > at("2026-08-31T20:59:00Z"));
        assert!(p.to.unwrap() < at("2026-08-31T21:00:01Z"));
    }

    #[test]
    fn month_rollover_does_not_panic_on_day_31() {
        // `with_day(1)` on the 31st and stepping back a month is where naive
        // date arithmetic usually falls over.
        let now = at("2026-03-31T10:00:00Z");
        let p = Period::preset(PeriodPreset::LastMonth).resolve(cairo(), now);
        assert!(p.from.is_some() && p.to.is_some());
    }

    /// The guard that was missing.
    ///
    /// `ALL` is what the model's tool schema and the OpenAPI enum advertise;
    /// the serde names are what deserialization accepts. Nothing forced them to
    /// agree, and they did not: `last_30_days` was documented and rejected.
    #[test]
    fn every_advertised_period_round_trips() {
        for advertised in PeriodPreset::ALL {
            let parsed: PeriodPreset =
                serde_json::from_value(serde_json::Value::String((*advertised).to_string()))
                    .unwrap_or_else(|e| {
                        panic!("'{advertised}' is advertised but cannot be parsed: {e}")
                    });
            let back = serde_json::to_value(parsed).unwrap();
            assert_eq!(
                back.as_str(),
                Some(*advertised),
                "'{advertised}' does not serialize back to itself"
            );
        }
    }

    #[test]
    fn the_legacy_spelling_still_parses() {
        // Conversations and widgets stored before the rename hold the old
        // derived spelling. Rejecting it would quietly drop the query from
        // every existing follow-up.
        for (legacy, canonical) in [
            ("last7_days", "last_7_days"),
            ("last30_days", "last_30_days"),
            ("last90_days", "last_90_days"),
            ("last12_months", "last_12_months"),
        ] {
            let parsed: PeriodPreset =
                serde_json::from_value(serde_json::Value::String(legacy.into()))
                    .unwrap_or_else(|e| panic!("legacy '{legacy}' must still parse: {e}"));
            // ...but it is written back in the canonical form.
            assert_eq!(
                serde_json::to_value(parsed).unwrap().as_str(),
                Some(canonical)
            );
        }
    }

    #[test]
    fn all_lists_every_variant() {
        // A variant missing from ALL is one the model is never told about.
        use PeriodPreset::*;
        let every = [
            Today,
            Yesterday,
            ThisWeek,
            LastWeek,
            ThisMonth,
            LastMonth,
            ThisYear,
            LastYear,
            Last7Days,
            Last30Days,
            Last90Days,
            Last12Months,
            AllTime,
        ];
        assert_eq!(every.len(), PeriodPreset::ALL.len());
        for v in every {
            let name = serde_json::to_value(v).unwrap();
            assert!(
                PeriodPreset::ALL.contains(&name.as_str().unwrap()),
                "{name} is a real variant but is not advertised in ALL"
            );
        }
    }

    #[test]
    fn all_time_is_unbounded() {
        let p = Period::preset(PeriodPreset::AllTime).resolve(cairo(), Utc::now());
        assert!(p.from.is_none() && p.to.is_none() && !p.is_bounded());
    }

    #[test]
    fn timezone_actually_changes_the_bounds() {
        let now = at("2026-09-02T10:00:00Z");
        let cairo_today = Period::preset(PeriodPreset::Today).resolve(cairo(), now);
        let utc_today = Period::preset(PeriodPreset::Today).resolve(chrono_tz::UTC, now);
        assert_ne!(cairo_today.from, utc_today.from);
    }

    #[test]
    fn unknown_timezone_falls_back_to_cairo_not_a_panic() {
        assert_eq!(parse_tz("Not/AZone"), chrono_tz::Africa::Cairo);
        assert_eq!(parse_tz("Europe/Paris"), chrono_tz::Europe::Paris);
    }

    #[test]
    fn spec_deserializes_from_a_models_tool_arguments() {
        let spec: QuerySpec = serde_json::from_value(serde_json::json!({
            "dataset": "order_items",
            "dimensions": ["product"],
            "measures": ["units_sold", "item_revenue"],
            "filters": { "status": "sold" },
            "period": { "preset": "last_month" },
            "sort": { "measure": "item_revenue", "dir": "desc" },
            "limit": 5
        }))
        .unwrap();
        assert_eq!(spec.dataset, "order_items");
        assert_eq!(spec.period.preset, Some(PeriodPreset::LastMonth));
        assert_eq!(spec.limit, Some(5));
    }

    #[test]
    fn an_unparseable_date_is_a_deserialize_error_not_a_silent_null() {
        let err = serde_json::from_value::<QuerySpec>(serde_json::json!({
            "dataset": "orders",
            "period": { "from": "last tuesday" }
        }))
        .unwrap_err();
        assert!(err.to_string().contains("not a valid ISO-8601 date"));
    }

    #[test]
    fn unknown_fields_are_rejected_so_a_typo_is_never_silently_ignored() {
        // A model writing `dimension` instead of `dimensions` must fail loudly
        // and get a chance to retry, not silently return ungrouped totals.
        assert!(
            serde_json::from_value::<QuerySpec>(serde_json::json!({
                "dataset": "orders",
                "dimension": ["branch"]
            }))
            .is_err()
        );
    }
}
