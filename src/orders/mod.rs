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

/// The DRAWER's scope — the one deliberate second predicate, for
/// `shifts::handlers::compute_system_cash` and nothing else. Every sale whose
/// tender physically entered the till: that INCLUDES a sale later refunded in
/// full, because the refund is its own row (`order_refunds`, subtracted by the
/// shift it was issued in), and the notes moved twice. Only a void says the
/// money never arrived. Scoping the drawer on [`SOLD`] would make a cash sale
/// refunded in cash net to −total instead of zero. Not a revenue scope: a
/// report counting sales uses `SOLD`, and the lint above holds it to that.
pub const TENDERED: &str = "status::text <> 'voided'";

/// Why a sale or a bill was torn up — the `void_reason` enum, shared by
/// `orders` and `open_tickets` so a void-rate report reads counter and dine-in
/// alike without a translation layer. Bound as text and cast in SQL
/// (`$n::void_reason`), the way the order void has always done it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum VoidReason {
    CustomerRequest,
    WrongOrder,
    QualityIssue,
    /// The catch-all. A handler that accepts it requires a note, because
    /// "other" on its own tells the report nothing.
    Other,
}

impl VoidReason {
    pub fn as_str(self) -> &'static str {
        match self {
            VoidReason::CustomerRequest => "customer_request",
            VoidReason::WrongOrder => "wrong_order",
            VoidReason::QualityIssue => "quality_issue",
            VoidReason::Other => "other",
        }
    }

    /// The wire spelling (`snake_case`), as a client sends it.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "customer_request" => Some(VoidReason::CustomerRequest),
            "wrong_order" => Some(VoidReason::WrongOrder),
            "quality_issue" => Some(VoidReason::QualityIssue),
            "other" => Some(VoidReason::Other),
            _ => None,
        }
    }

    /// Read a void reason off the wire, in whatever vocabulary the client
    /// speaks.
    ///
    /// The enum's own four values first; then the LEGACY picker's labels,
    /// which older builds send either alone or as `<label> — <note>`; and
    /// failing both, `other` carrying the whole string as the note, so nothing
    /// a person typed is thrown away.
    ///
    /// Shared, because both void paths meet the same field from the same field
    /// devices. The ticket's wire type has read it leniently since the enum
    /// landed; the ORDER's validated strictly, so an old till voiding a
    /// counter sale got a 400 for the reason it had always sent — the same
    /// shape of break as the discount convention, on the next field along.
    pub fn read(raw: &str) -> (Self, Option<String>) {
        let raw = raw.trim();
        if let Some(reason) = Self::parse(raw) {
            return (reason, None);
        }
        let (label, tail) = match raw.split_once(" — ") {
            Some((l, n)) => (l, Some(n.trim().to_string()).filter(|s| !s.is_empty())),
            None => (raw, None),
        };
        match Self::from_legacy_label(label) {
            Some(reason) => (reason, tail),
            None => (Self::Other, Some(raw.to_string()).filter(|s| !s.is_empty())),
        }
    }

    /// The labels the ticket-void picker used before the reason was typed,
    /// mapped exactly as migration `20260912020000` mapped the stored rows, so
    /// a void queued offline by an older till lands as the same reason the
    /// migration gave its contemporaries. Case-insensitive, whitespace-trimmed.
    pub fn from_legacy_label(label: &str) -> Option<Self> {
        match label.trim().to_lowercase().as_str() {
            "order mistake" | "wrong order" => Some(VoidReason::WrongOrder),
            "customer request" => Some(VoidReason::CustomerRequest),
            "quality issue" => Some(VoidReason::QualityIssue),
            "other" => Some(VoidReason::Other),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
