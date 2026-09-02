//! Presentation-level value types shared by the analytics core, the metrics
//! HTTP API, the dashboard widget layer, and the AI agent.
//!
//! These are deliberately *display* concerns: how a column should be formatted
//! ([`ColumnKind`]), what shape a result has ([`Grain`]), and how it is best
//! drawn ([`Viz`]). The SQL semantics live in [`super::schema`].

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The renderable kind of an output column, so a client can format it and pick
/// a sensible chart without knowing anything about the underlying SQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    /// Integer piastres — render as currency. Money is integer piastres
    /// system-wide (see `CLAUDE.md`); never a float.
    Money,
    /// Integer count.
    Count,
    /// Free text / category label — a natural chart category axis.
    Label,
    /// A calendar date bucket (day / week / month) — a natural time axis.
    Date,
    /// A ratio, percentage, or physical quantity — a decimal.
    Number,
    /// A duration expressed in minutes.
    Minutes,
}

impl ColumnKind {
    /// True when this column is a numeric *value* (a candidate measure axis)
    /// rather than a category/time *key*.
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            ColumnKind::Money | ColumnKind::Count | ColumnKind::Number | ColumnKind::Minutes
        )
    }
}

/// One output column: the SQL alias (also the JSON key on every row) plus how
/// to render it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, ToSchema)]
pub struct Column {
    /// SQL alias / JSON key.
    pub key: &'static str,
    /// Human label for a header or legend.
    pub label: &'static str,
    pub kind: ColumnKind,
}

/// The *shape* of a result set, derived from the dimensions a query groups by.
/// This is what lets a dashboard render any metric with no per-metric code: a
/// scalar becomes a KPI card, a series becomes a line, a breakdown becomes a
/// bar or pie.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Grain {
    /// No dimensions — exactly one row of totals. A KPI card.
    Scalar,
    /// Grouped by a time dimension — an ordered series. A line/area chart.
    Series,
    /// Grouped by one non-time dimension — a ranked breakdown. Bar/pie.
    Categorical,
    /// Two or more dimensions, or a shape with no obvious single axis. A table
    /// (or a grouped/stacked chart, at the client's discretion).
    Table,
}

/// How a result is best visualized. A hint: the client may always override, and
/// [`Viz::Auto`] asks the backend to choose from the [`Grain`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Viz {
    /// Let the backend pick from the result grain.
    Auto,
    /// Single big number.
    Kpi,
    Line,
    Area,
    Bar,
    /// Horizontal bars — better for long category labels.
    Row,
    Pie,
    Donut,
    Table,
    /// Two-dimension intensity grid (e.g. weekday × hour).
    Heatmap,
}

impl Viz {
    /// Resolve [`Viz::Auto`] against the actual result shape. Also downgrades a
    /// visualization that cannot represent the grain (a pie of a 30-day series
    /// is never what the asker meant) rather than emitting something unreadable.
    pub fn resolve(self, grain: Grain, dimension_count: usize) -> Viz {
        let auto = match grain {
            Grain::Scalar => Viz::Kpi,
            Grain::Series => Viz::Line,
            Grain::Categorical => Viz::Bar,
            Grain::Table if dimension_count == 2 => Viz::Heatmap,
            Grain::Table => Viz::Table,
        };
        match (self, grain) {
            (Viz::Auto, _) => auto,
            // A part-to-whole chart of a time series is meaningless.
            (Viz::Pie | Viz::Donut, Grain::Series) => Viz::Line,
            // A KPI needs exactly one row.
            (Viz::Kpi, g) if g != Grain::Scalar => auto,
            // A scalar has no axis to plot.
            (Viz::Line | Viz::Area | Viz::Bar | Viz::Row | Viz::Heatmap, Grain::Scalar) => Viz::Kpi,
            (v, _) => v,
        }
    }
}

/// Sort direction. `Asc` is what unlocks "worst", "slowest", "least" questions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    Asc,
    Desc,
}

impl Dir {
    pub fn sql(self) -> &'static str {
        match self {
            Dir::Asc => "ASC",
            Dir::Desc => "DESC",
        }
    }
}

/// Period-over-period comparison mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Compare {
    #[default]
    None,
    /// The immediately preceding window of equal length.
    PreviousPeriod,
    /// The same window one year earlier.
    PreviousYear,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_viz_follows_the_grain() {
        assert_eq!(Viz::Auto.resolve(Grain::Scalar, 0), Viz::Kpi);
        assert_eq!(Viz::Auto.resolve(Grain::Series, 1), Viz::Line);
        assert_eq!(Viz::Auto.resolve(Grain::Categorical, 1), Viz::Bar);
        assert_eq!(Viz::Auto.resolve(Grain::Table, 2), Viz::Heatmap);
        assert_eq!(Viz::Auto.resolve(Grain::Table, 3), Viz::Table);
    }

    #[test]
    fn nonsensical_viz_is_downgraded_not_emitted() {
        // A pie of a 30-day series is never the intent.
        assert_eq!(Viz::Pie.resolve(Grain::Series, 1), Viz::Line);
        // A KPI card cannot show a breakdown.
        assert_eq!(Viz::Kpi.resolve(Grain::Categorical, 1), Viz::Bar);
        // A line has no axis when there is a single total row.
        assert_eq!(Viz::Line.resolve(Grain::Scalar, 0), Viz::Kpi);
        // A legitimate choice is respected.
        assert_eq!(Viz::Donut.resolve(Grain::Categorical, 1), Viz::Donut);
    }
}
