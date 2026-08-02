pub mod component_resolve;
pub mod cost_math;
pub mod handlers;
pub mod routes;

/// **The** definition of "this order counts as a sale", as a SQL predicate
/// fragment to be prefixed with a table alias (`o.{SOLD}`).
///
/// Every money aggregate in the codebase — the orders KPI strip, the branch
/// sales report, the shift report, insights, bundle sales — must scope on this
/// and nothing else. They historically each picked their own (`= 'completed'`,
/// `!= 'voided'`, `NOT IN ('voided','refunded')`), so the same day's revenue
/// read differently on three screens depending on whether any ticket was still
/// open on the KDS. `src/reports/tests.rs::status_predicates_are_unified` scans
/// the source to keep a new query from inventing a fourth variant.
///
/// Included: `pending`, `preparing`, `ready`, `completed` — an order is a sale
/// the moment it is rung, which is also when its `order_payments` row is written.
/// Excluded: `voided`, `refunded`.
pub const SOLD: &str = "status::text NOT IN ('voided', 'refunded')";

#[cfg(test)]
mod tests;
