//! The branch plan: what hardware a branch has and how it talks.
//!
//! The dashboard's branch builder reads the whole plan (`GET /branch-plan`) and
//! saves the whole plan (`PUT /branch-plan`) in one transaction; see the kitchen
//! target spec (`madar/docs/specs/kitchen-target-spec.md`, BB-* and CH-*) and
//! the migration `20261003000000_branch_hardware_plan.sql`.
//!
//! A plan has three kinds of piece:
//!   * device slots (`branch_device_slots`): a POS, a waiter tablet or a
//!     kitchen screen, filled by a real install once a slot code is used;
//!   * printers (`branch_printers`): receipt or kitchen, on the network or
//!     plugged into one device;
//!   * sections: the existing `kitchen_stations`, with their categories
//!     (`category_station_routes`) and outputs (`kitchen_station_screens`,
//!     `kitchen_station_printers`).
//!
//! The blocking checks live here as a pure function (`check_plan`) so the save
//! handler and the tests share one rule; the dashboard mirrors them to show
//! problems while drawing (`features/branch-builder/plan.ts`).

pub mod handlers;
pub mod routes;

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::branches::handlers::PrinterBrand;

pub const DEVICE_KINDS: [&str; 3] = ["pos", "waiter", "kitchen"];
pub const PRINTER_ROLES: [&str; 2] = ["receipt", "kitchen"];
pub const CONNECTIONS: [&str; 3] = ["network", "usb", "bluetooth"];

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PlanDevice {
    pub id: Uuid,
    /// `pos` | `waiter` | `kitchen`
    pub kind: String,
    pub name: String,
    /// For a POS or waiter device: the receipt printer its receipts go to.
    #[serde(default)]
    pub receipt_printer_id: Option<Uuid>,
    /// The registered install filling this slot, if any.
    #[serde(default)]
    pub device_id: Option<Uuid>,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PlanPrinter {
    pub id: Uuid,
    /// `receipt` | `kitchen`
    pub role: String,
    pub name: String,
    /// `network` | `usb` | `bluetooth`
    pub connection: String,
    #[serde(default)]
    pub brand: Option<PrinterBrand>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub port: Option<i32>,
    pub paper_mm: i32,
    /// For a USB or Bluetooth printer: the device slot it is plugged into.
    #[serde(default)]
    pub host_device_id: Option<Uuid>,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PlanSection {
    pub id: Uuid,
    pub name: String,
    pub is_default: bool,
    #[serde(default)]
    pub category_ids: Vec<Uuid>,
    /// Kitchen screens (device slots of kind `kitchen`) this section shows on.
    #[serde(default)]
    pub screen_ids: Vec<Uuid>,
    /// Kitchen printers this section prints on.
    #[serde(default)]
    pub printer_ids: Vec<Uuid>,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct BranchPlan {
    #[serde(default)]
    pub devices: Vec<PlanDevice>,
    #[serde(default)]
    pub printers: Vec<PlanPrinter>,
    #[serde(default)]
    pub sections: Vec<PlanSection>,
    /// Case 1 (till only): the till's receipt printer also prints kitchen chits.
    #[serde(default)]
    pub till_prints_kitchen: bool,
}

/// The routing mode a plan implies (spec KS-8), in `kitchen_routing_mode` terms:
/// no sections → the till shows the kitchen (`till`) or nothing does (`off`);
/// sections with a screen → `kds` (or `both` when the till also prints); only
/// kitchen printers → `till`, which is where chits are printed from today.
pub fn routing_mode_for(plan: &BranchPlan) -> &'static str {
    let has_screens = plan.sections.iter().any(|s| !s.screen_ids.is_empty());
    match (
        plan.sections.is_empty(),
        has_screens,
        plan.till_prints_kitchen,
    ) {
        (true, _, true) => "till",
        (true, _, false) => "off",
        (false, true, true) => "both",
        (false, true, false) => "kds",
        (false, false, _) => "till",
    }
}

/// Every blocking problem with a plan, as plain sentences for a 400. A plan
/// that passes is one the tables can hold and that loses no order: every
/// section has somewhere to send its items, every printer can be reached, and
/// every link points at a piece of the right kind in the same plan.
pub fn check_plan(plan: &BranchPlan) -> Vec<String> {
    let mut problems = Vec::new();

    let mut ids = HashSet::new();
    for id in plan
        .devices
        .iter()
        .map(|d| d.id)
        .chain(plan.printers.iter().map(|p| p.id))
        .chain(plan.sections.iter().map(|s| s.id))
    {
        if !ids.insert(id) {
            problems.push(format!("Piece {id} appears twice in the plan"));
        }
    }

    let devices: HashMap<Uuid, &PlanDevice> = plan.devices.iter().map(|d| (d.id, d)).collect();
    let printers: HashMap<Uuid, &PlanPrinter> = plan.printers.iter().map(|p| (p.id, p)).collect();

    let mut claimed = HashSet::new();
    for d in &plan.devices {
        if !DEVICE_KINDS.contains(&d.kind.as_str()) {
            problems.push(format!("Device {} has an unknown kind '{}'", d.id, d.kind));
        }
        if d.name.trim().is_empty() {
            problems.push("Every device needs a name".into());
        }
        if let Some(pid) = d.receipt_printer_id {
            let ok = matches!(d.kind.as_str(), "pos" | "waiter")
                && printers.get(&pid).is_some_and(|p| p.role == "receipt");
            if !ok {
                problems.push(format!(
                    "'{}' prints receipts on a piece that is not a receipt printer",
                    d.name.trim()
                ));
            }
        }
        if let Some(dev) = d.device_id
            && !claimed.insert(dev)
        {
            problems.push("One device can fill only one slot".into());
        }
    }

    for p in &plan.printers {
        let name = p.name.trim();
        if !PRINTER_ROLES.contains(&p.role.as_str()) {
            problems.push(format!("Printer {} has an unknown role '{}'", p.id, p.role));
        }
        if !CONNECTIONS.contains(&p.connection.as_str()) {
            problems.push(format!(
                "Printer {} has an unknown connection '{}'",
                p.id, p.connection
            ));
        }
        if name.is_empty() {
            problems.push("Every printer needs a name".into());
        }
        if !matches!(p.paper_mm, 58 | 80) {
            problems.push(format!("'{name}' must use 58 or 80 mm paper"));
        }
        if let Some(port) = p.port
            && !(1..=65535).contains(&port)
        {
            problems.push(format!("'{name}' has a port outside 1-65535"));
        }
        if p.connection == "network" {
            let valid =
                p.ip.as_deref()
                    .is_some_and(|ip| ip.trim().parse::<Ipv4Addr>().is_ok());
            if !valid {
                problems.push(format!(
                    "'{name}' is on the network but has no valid IP address"
                ));
            }
        } else {
            match p.host_device_id.and_then(|h| devices.get(&h)) {
                Some(_) => {}
                None => problems.push(format!(
                    "'{name}' is plugged in but not into a device in this plan"
                )),
            }
        }
    }

    let mut names = HashSet::new();
    let mut routed = HashSet::new();
    for s in &plan.sections {
        let name = s.name.trim();
        if name.is_empty() {
            problems.push("Every section needs a name".into());
        } else if !names.insert(name.to_lowercase()) {
            problems.push(format!("Two sections are called '{name}'"));
        }
        if s.screen_ids.is_empty() && s.printer_ids.is_empty() {
            problems.push(format!(
                "'{name}' has no screen or printer, so its orders would go nowhere"
            ));
        }
        for id in &s.screen_ids {
            if devices.get(id).is_none_or(|d| d.kind != "kitchen") {
                problems.push(format!(
                    "'{name}' shows on a piece that is not a kitchen screen"
                ));
            }
        }
        for id in &s.printer_ids {
            if printers.get(id).is_none_or(|p| p.role != "kitchen") {
                problems.push(format!(
                    "'{name}' prints on a piece that is not a kitchen printer"
                ));
            }
        }
        for c in &s.category_ids {
            if !routed.insert(*c) {
                problems.push(format!("A category is in two sections (again in '{name}')"));
            }
        }
    }
    if !plan.sections.is_empty() && plan.sections.iter().filter(|s| s.is_default).count() != 1 {
        problems.push("Exactly one section must take the items no other section claims".into());
    }

    problems.dedup();
    problems
}
