//! Menu Advisor: read-only pricing, bundle, and removal suggestions.
//!
//! Module map:
//!   - `dto`         — the wire contract (single owner of every HTTP type)
//!   - `engine/`     — pure analytics (no I/O, deterministic, panic-free)
//!   - `adapter`     — SQL → engine inputs (all money in piastres)
//!   - `persistence` — payload-JSONB storage of runs/suggestions/decisions
//!   - `handlers`    — HTTP layer (permissions + branch ownership on every route)
//!   - `routes`      — the route table (contract; paths never change)

pub mod adapter;
pub mod dto;
pub mod engine;
pub mod handlers;
pub mod persistence;
pub mod routes;

#[cfg(test)]
mod tests;
