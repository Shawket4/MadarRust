//! Public demo playground — throwaway, per-visitor orgs.
//!
//! `POST /demo/session` mints a fresh, isolated org (flagged `is_demo` with a
//! `demo_expires_at` TTL), an org_admin user, a scoped JWT, and — for the
//! `full` variant — a seeded café so the dashboard is alive on arrival. The
//! `empty` variant lands the visitor in the real first-run onboarding wizard.
//! A background [`sweeper`] garbage-collects expired demo orgs (and all their
//! child rows, since org FKs don't cascade).
//!
//! Everything here is gated on `DEMO_MODE=1` and is meant to run on a
//! SEPARATE backend + database from production.

pub mod config;
pub mod handlers;
pub mod routes;
pub mod seed;
pub mod sweeper;
