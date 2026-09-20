pub mod figures_guard;
pub mod handlers;
pub mod legacy;
pub mod legacy_routes;
pub mod reconcile;
pub mod routes;
pub mod spot_views;

#[cfg(test)]
mod figures_guard_tests;
#[cfg(test)]
mod followup_tests;
#[cfg(test)]
mod legacy_tests;
#[cfg(test)]
mod reconcile_tests;
#[cfg(test)]
mod report_vectors_tests;
#[cfg(test)]
mod spot_view_tests;
