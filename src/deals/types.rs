//! The deals wire types (COMBOS_CONTRACT.md §2.3, §3.1, §3.2, §5, as adjusted
//! by the owner's answers in §11: deals also apply automatically on QR and
//! online checkout, and follow the org/branch channel toggles).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::combos::types::{ChannelToggles, SaleWindow};

fn t() -> bool {
    true
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}

/// An item or a category (every kind=item item of it) a deal counts, at one
/// size or any (`size_label` null).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct DealPoolEntry {
    #[serde(default)]
    pub menu_item_id: Option<Uuid>,
    #[serde(default)]
    pub category_id: Option<Uuid>,
    #[serde(default)]
    pub size_label: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct DealBranchOverride {
    pub branch_id: Uuid,
    pub is_active: bool,
}

/// A deal rule. `n_for_price`: any `qty` units of the pool for `price`.
/// `buy_get`: buy `qty`, get `get_qty` at `get_percent`% off (100 = free),
/// the rewarded units drawn from `reward_pool` (or the pool when empty).
/// A deal covers the item's size price only; add-ons always pay.
///
/// Also the `deal_rule` feed row of `/sync/pull`, where `is_active` is
/// resolved for the device's branch and `sell` carries the branch's channel
/// toggles.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct DealRule {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// `n_for_price` | `buy_get`.
    pub kind: String,
    pub qty: i16,
    /// n_for_price: the price of `qty` units, piastres.
    pub price: Option<i32>,
    pub get_qty: Option<i16>,
    pub get_percent: Option<i16>,
    pub max_per_order: Option<i16>,
    pub sort: i32,
    pub is_active: bool,
    pub pool: Vec<DealPoolEntry>,
    /// buy_get only; `[]` = the rewarded units come from `pool`.
    pub reward_pool: Vec<DealPoolEntry>,
    pub windows: Vec<SaleWindow>,
    pub branch_overrides: Vec<DealBranchOverride>,
    /// Feed rows only: the branch's channel toggles (§11.1). Absent elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sell: Option<ChannelToggles>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `POST /deals`, `PUT /deals/{id}`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct DealWrite {
    pub name: String,
    #[serde(default = "empty_object")]
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// `n_for_price` | `buy_get`.
    pub kind: String,
    /// N (n_for_price, 2–20) or the "buy" count (buy_get, 1–20).
    pub qty: i16,
    /// n_for_price only.
    #[serde(default)]
    pub price: Option<i32>,
    /// buy_get only (1–20).
    #[serde(default)]
    pub get_qty: Option<i16>,
    /// buy_get only (1–100; 100 = free).
    #[serde(default)]
    pub get_percent: Option<i16>,
    #[serde(default)]
    pub max_per_order: Option<i16>,
    #[serde(default)]
    pub sort: i32,
    #[serde(default = "t")]
    pub is_active: bool,
    pub pool: Vec<DealPoolEntry>,
    #[serde(default)]
    pub reward_pool: Vec<DealPoolEntry>,
    #[serde(default)]
    pub windows: Vec<SaleWindow>,
}

/// `PUT /deals/{id}/branches/{branch_id}`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema)]
pub struct DealBranchWrite {
    pub is_active: bool,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DealListQuery {
    pub is_active: Option<bool>,
}

// ── Orders (§3.1, §3.2) ─────────────────────────────────────────────────────

/// Units of one order line a deal takes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct DealLineInput {
    /// Index into the order's `items[]`.
    pub line_index: i32,
    pub units: i32,
}

/// An applied deal on an order (the till's teller applied it). Live, the
/// server re-prices it exactly over these units (`409 DEAL_NOT_ELIGIBLE` when
/// they don't satisfy the rule; `orders.deals.apply` required). On replay the
/// till's `discount` is kept and the server's verdict stored beside it.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct DealApplicationInput {
    pub deal_rule_id: Uuid,
    pub times: i32,
    pub lines: Vec<DealLineInput>,
    /// Replay only: what the till took off, piastres.
    #[serde(default)]
    pub discount: Option<i32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct OrderDealLine {
    pub order_item_id: Uuid,
    pub units: i32,
    pub discount: i32,
}

/// An applied deal as stored: `OrderFull.deals[]`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct OrderDeal {
    pub id: Uuid,
    pub deal_rule_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub times: i32,
    /// What came off the lines (the till's figure on a replay).
    pub discount: i32,
    /// The server's verdict; equals `discount` live; `null` when not computable.
    pub discount_server: Option<i32>,
    pub lines: Vec<OrderDealLine>,
}

// ── Public cart quote (§11.2) ───────────────────────────────────────────────

/// `POST /public/branches/{id}/cart-quote` (online) and
/// `POST /public/tables/{id}/cart-quote` (QR): the cart priced by the server
/// exactly as the order will be, with the best deals applied automatically.
#[derive(Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicCartQuoteRequest {
    pub items: Vec<crate::orders::handlers::OrderItemInput>,
    /// Online only: the delivery sub-channel whose prices apply
    /// (`in_mall` | `outside` | `umbrella` | `pickup`); default `pickup`.
    #[serde(default)]
    pub channel: Option<String>,
}

/// One part of a quoted combo line.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct QuotedComboPart {
    pub slot_id: Uuid,
    pub menu_item_id: Uuid,
    pub size_label: String,
    pub quantity: i32,
    /// The item's normal price at this size.
    pub unit_price: i32,
    pub combo_share: i32,
    pub combo_surcharge: i32,
    /// `combo_share + combo_surcharge`.
    pub line_total: i32,
    /// Its add-ons and optional fields, whole line.
    pub addons_total: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct QuotedCombo {
    /// P per combo unit.
    pub price: i32,
    /// One combo with its surcharges and add-ons.
    pub unit_total: i32,
    /// À la carte value of one combo minus `unit_total` (may be ≤ 0).
    pub saving_unit: i32,
    pub parts: Vec<QuotedComboPart>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct QuotedLine {
    /// Index into the request's `items[]`.
    pub index: i32,
    pub quantity: i32,
    /// The size price per unit (a combo: 0; see `combo`).
    pub unit_price: i32,
    /// The whole line with its add-ons, before deals.
    pub line_total: i32,
    /// What the applied deals took off this line.
    pub deal_minor: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combo: Option<QuotedCombo>,
}

/// A deal the server applied to the cart.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct QuotedDeal {
    pub deal_rule_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub times: i32,
    pub discount: i32,
    pub lines: Vec<DealLineInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct CartQuote {
    pub lines: Vec<QuotedLine>,
    /// Σ line_total, before deals.
    pub items_total: i32,
    pub deals: Vec<QuotedDeal>,
    /// Σ deals' discount.
    pub deal_discount: i32,
    /// `items_total − deal_discount` (before the channel discount, tax and fees).
    pub total_after_deals: i32,
}
