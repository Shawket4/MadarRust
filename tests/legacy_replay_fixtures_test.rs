//! Old-client wire compatibility (tills rework guard, decision 12).
//!
//! POS v0.5.1, v0.6.0 and v0.6.1 are in the field with queued outbox rows. Every
//! envelope under tests/fixtures/legacy_replay/<release>/ is what that release's
//! core POSTs to /sync/replay (regenerate with madar's
//! `tool/old_client_api_check.sh --regen-envelopes`). The replay aliases are
//! accepted PERMANENTLY, so each must keep deserializing into the CURRENT
//! `ReplayOp` — and the live delivery-finalize body into `FinalizeInput`.

use std::path::Path;

use madar_rust::delivery::staff::FinalizeInput;
use madar_rust::sync::handlers::ReplayOp;

const RELEASES: &[&str] = &["v0.5.1", "v0.6.0", "v0.6.1"];

#[test]
fn legacy_replay_envelopes_deserialize_into_current_replay_op() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy_replay");
    let mut failures = Vec::new();
    for release in RELEASES {
        let dir = root.join(release);
        let mut envelopes = 0;
        let mut finalize = 0;
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let text = std::fs::read_to_string(&path).unwrap();
            if name.ends_with(".body.json") {
                // delivery_finalize.body.json: a live POST body, not an envelope.
                if let Err(e) = serde_json::from_str::<FinalizeInput>(&text) {
                    failures.push(format!("{release}/{name} as FinalizeInput: {e}"));
                }
                finalize += 1;
                continue;
            }
            let raw: serde_json::Value = serde_json::from_str(&text).unwrap();
            let op = raw["op"].as_str().unwrap_or_default().to_string();
            match serde_json::from_str::<ReplayOp>(&text) {
                Ok(parsed) => {
                    // The tag must land on the variant the old client meant.
                    let variant = match parsed {
                        ReplayOp::OpenTill { .. } => "open_shift",
                        ReplayOp::CloseTill { .. } => "close_shift",
                        ReplayOp::CashMovement { .. } => "cash_movement",
                        ReplayOp::CreateOrder { .. } => "create_order",
                        ReplayOp::SettleOpenTicket { .. } => "settle_open_ticket",
                        ReplayOp::RefundOrder { .. } => "refund_order",
                        ReplayOp::VoidOrder { .. } => "void_order",
                        ReplayOp::FireOpenTicket { .. } => "fire_open_ticket",
                        ReplayOp::AddTicketRound { .. } => "add_ticket_round",
                        _ => "other",
                    };
                    if variant != op {
                        failures.push(format!("{release}/{name}: op {op} parsed as {variant}"));
                    }
                }
                Err(e) => failures.push(format!("{release}/{name}: {e}")),
            }
            envelopes += 1;
        }
        assert!(
            envelopes >= 14,
            "{release}: only {envelopes} envelopes found"
        );
        assert_eq!(
            finalize, 1,
            "{release}: delivery_finalize.body.json missing"
        );
    }
    assert!(
        failures.is_empty(),
        "legacy envelopes rejected:\n{}",
        failures.join("\n")
    );
}
