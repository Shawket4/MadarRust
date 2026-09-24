//! The CURRENT till's `/sync/replay` envelopes (madar-shared S2).
//!
//! `madar_sync::vectors::REPLAY_CURRENT` is one envelope per op as the
//! current madar-core builds it (`replay_envelope`; regenerated from the POS
//! with `MADAR_WRITE_REPLAY_FIXTURE=1`). Every one must deserialize into THIS
//! server's `ReplayOp` and land on the variant its `op` names — the
//! frozen-release fixtures (`legacy_replay_fixtures_test`) only ever covered
//! old tills. The match below is exhaustive on purpose: a new op here fails to
//! compile until madar-shared's `madar_sync::replay::OPS` and the fixture know
//! it too.

use madar_rust::sync::handlers::ReplayOp;

fn op_of(op: &ReplayOp) -> &'static str {
    match op {
        ReplayOp::OpenTill { .. } => "open_till",
        ReplayOp::CloseTill { .. } => "close_till",
        ReplayOp::CreateOrder { .. } => "create_order",
        ReplayOp::VoidOrder { .. } => "void_order",
        ReplayOp::RefundOrder { .. } => "refund_order",
        ReplayOp::AwardLoyaltyPoints { .. } => "award_loyalty_points",
        ReplayOp::CashMovement { .. } => "cash_movement",
        ReplayOp::SpotReportView { .. } => "spot_report_view",
        ReplayOp::FireOpenTicket { .. } => "fire_open_ticket",
        ReplayOp::AddTicketRound { .. } => "add_ticket_round",
        ReplayOp::SettleOpenTicket { .. } => "settle_open_ticket",
        ReplayOp::VoidOpenTicket { .. } => "void_open_ticket",
        ReplayOp::VoidTicketLine { .. } => "void_ticket_line",
        ReplayOp::BumpKitchenItem { .. } => "bump_kitchen_item",
        ReplayOp::UnbumpKitchenItem { .. } => "unbump_kitchen_item",
        ReplayOp::SwapTables { .. } => "swap_tables",
        ReplayOp::CreateTableTransfer { .. } => "create_table_transfer",
        ReplayOp::CancelTableTransfer { .. } => "cancel_table_transfer",
        ReplayOp::FulfillTableTransfer { .. } => "fulfill_table_transfer",
        ReplayOp::ClearTable { .. } => "clear_table",
        ReplayOp::HoldTable { .. } => "hold_table",
        ReplayOp::ReleaseTable { .. } => "release_table",
        ReplayOp::SeatBooking { .. } => "seat_booking",
        ReplayOp::NoShowBooking { .. } => "no_show_booking",
        ReplayOp::CreateCustomer { .. } => "create_customer",
        ReplayOp::AttachCustomer { .. } => "attach_customer",
        ReplayOp::SetTicketCustomer { .. } => "set_ticket_customer",
        ReplayOp::RecordWaste { .. } => "record_waste",
        ReplayOp::RecordStaffDrink { .. } => "record_staff_drink",
    }
}

#[test]
fn the_current_tills_envelopes_deserialize_into_replay_op() {
    let doc: serde_json::Value =
        serde_json::from_str(madar_sync::vectors::REPLAY_CURRENT).expect("the fixture parses");
    let envelopes = doc["envelopes"].as_array().expect("envelopes");
    let mut failures = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for env in envelopes {
        let op = env["op"].as_str().unwrap_or_default().to_string();
        match serde_json::from_value::<ReplayOp>(env.clone()) {
            Ok(parsed) if op_of(&parsed) == op => {
                seen.insert(op);
            }
            Ok(parsed) => failures.push(format!("{op} parsed as {}", op_of(&parsed))),
            Err(e) => failures.push(format!("{op}: {e}")),
        }
    }
    assert!(
        failures.is_empty(),
        "current envelopes rejected:\n{}",
        failures.join("\n")
    );
    // Every op this server accepts is in madar-shared's list, and the fixture
    // carries every one of them.
    for op in madar_sync::replay::OPS {
        assert!(seen.contains(*op), "the fixture lacks {op}");
    }
    assert_eq!(seen.len(), madar_sync::replay::OPS.len());
}
