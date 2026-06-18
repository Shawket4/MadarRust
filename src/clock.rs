//! Shared guard for client-supplied timestamps.
//!
//! The POS submits `created_at` / `opened_at` / `closed_at` / `voided_at` so
//! that orders and shifts created OFFLINE land on the real business day they
//! happened: the POS stamps a local time, then re-bases it to a fresh server
//! offset when it syncs (sync requires connectivity, so the offset is always
//! trustworthy at send time). We therefore TRUST a past client instant, but must
//! REJECT a future one beyond a small clock-skew tolerance — a future timestamp
//! would mint an order_ref / bucket a report in a *future* business day and is a
//! sign of a misconfigured device clock the correction couldn't fix.
//!
//! The business day itself is always derived server-side from the (corrected)
//! instant `AT TIME ZONE` the branch's timezone — never the device's zone.

use chrono::{DateTime, Duration, Utc};

use crate::errors::AppError;

/// Forward clock skew tolerated between a client device and the server. A device
/// that is online keeps its offset fresh to within seconds; this leaves slack
/// for in-flight latency without admitting genuinely future-dated writes.
pub fn clock_skew_tolerance() -> Duration {
    Duration::minutes(5)
}

/// Reject a client-supplied instant that is too far in the future. Past instants
/// are allowed (legitimate offline backdating).
pub fn reject_if_future(ts: DateTime<Utc>, label: &str) -> Result<(), AppError> {
    if ts > Utc::now() + clock_skew_tolerance() {
        return Err(AppError::BadRequest(format!(
            "{label} is too far in the future — check the device clock."
        )));
    }
    Ok(())
}
