//! The sync protocol version (DB audit Phase 6, §7.2 "Envelope").
//!
//! A till that sends no `protocol` speaks [`V1`], the protocol every shipped
//! build speaks, and is answered byte for byte as before: no new field, no
//! new behaviour. A till that sends `protocol` gets the negotiated version
//! echoed on the response and the additions of that version:
//!
//! - **v2**: `POST /sync/replay/batch` (many queued ops per round trip), and
//!   `ledger_retention_hours` on `/sync/pull` responses (how long a ledger row
//!   stays on the incremental feed before a cursor that old must resync).
//!
//! An additive field never needs a bump; a removed or renamed one does, and
//! the old version's projection then lives in the compat layer keyed by it.
//! The constants belong in madar-shared's `madar_sync` beside `ALL_TYPES` once
//! the POS core speaks v2 (one copy for both sides); they are here until that
//! tag.

/// What every shipped till speaks (no `protocol` on the wire).
pub const V1: u16 = 1;
/// Batched replay + the ledger retention hint.
pub const V2: u16 = 2;
/// The newest version this server speaks.
pub const CURRENT: u16 = V2;

/// The version both sides speak: the client's, capped at ours; v1 when the
/// client names none (or something below 1).
pub fn negotiate(client: Option<u16>) -> u16 {
    client.unwrap_or(V1).clamp(V1, CURRENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_old_version_is_v1_and_a_newer_one_is_capped() {
        assert_eq!(negotiate(None), V1);
        assert_eq!(negotiate(Some(0)), V1);
        assert_eq!(negotiate(Some(2)), V2);
        assert_eq!(negotiate(Some(9)), CURRENT);
    }
}
