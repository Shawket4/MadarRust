//! The analytics core: one description of everything this system can measure,
//! one way to turn a question into SQL, and one place that runs it.
//!
//! ```text
//!   QuerySpec ──compile──> CompiledQuery ──execute──> QueryResult
//!       ▲                                                  │
//!       ├── presets      (curated metrics, widget catalog)  ├─> /metrics/query
//!       ├── widgets      (saved dashboards)                 └─> AI agent
//!       └── ai::agent    (a question, parsed)
//! ```
//!
//! Everything that wants a number goes through [`spec::QuerySpec`]. There is no
//! second path, no hand-written report SQL living beside it, and no way for a
//! caller — merchant or model — to reach the database except through
//! [`execute::run`], which holds the RLS scoping, branch fence, read-only
//! transaction, statement timeout and row cap for all of them.
//!
//! Adding a metric is one entry in [`presets::PRESETS`]; adding a new *kind* of
//! metric is one entry in [`schema::DATASETS`]. Nothing else changes.

pub mod compile;
pub mod entities;
pub mod execute;
pub mod handlers;
pub mod presets;
pub mod registry;
pub mod routes;
pub mod schema;
pub mod scope;
pub mod spec;
pub mod types;

#[cfg(test)]
pub(crate) mod tests;
