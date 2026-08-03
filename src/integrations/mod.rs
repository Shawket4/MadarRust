//! Outbound partner integrations.
//!
//! Today this is one thing: a read-only order-analytics pull that a third
//! party (an aggregator, a group's own BI stack) authenticates to with HTTP
//! Basic and scopes to a single branch. See [`auth`] for the credential model
//! and [`handlers`] for the endpoint plus the org-admin issuing surface.

pub mod auth;
pub mod handlers;
pub mod routes;

#[cfg(test)]
mod tests;
