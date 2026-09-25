//! The combos wire types (COMBOS_CONTRACT.md §2.2, §2.4, §2.5, as adjusted by
//! the owner's answers in §11).
//!
//! §11 changes against the contract's sketch: there are NO per-combo channel
//! toggles (`sell` is gone from `ComboWrite`/`Combo`/`ComboSummary`, and so are
//! the per-combo branch overrides). The channel toggles are org-wide with
//! per-branch overrides ([`ComboSettings`]) and gate every combo and every
//! deal. Windows take optional `valid_from`/`valid_to` dates.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

fn t() -> bool {
    true
}

fn all_days() -> i16 {
    127
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}

fn one() -> i32 {
    1
}

/// Serde default of a line's `line_kind` and an item's `kind`.
pub fn item_kind() -> String {
    "item".into()
}

// ── Channels (§11.1) ────────────────────────────────────────────────────────

/// The four sale channels' toggles, resolved: `pos` (the till), `qr` (the
/// table QR menu), `online` (the storefront, all four delivery sub-channels)
/// and `delivery` (aggregator apps; stored and returned, honoured by the
/// future menu push). Every channel is on until the owner switches it off.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ChannelToggles {
    #[serde(default = "t")]
    pub pos: bool,
    #[serde(default = "t")]
    pub qr: bool,
    #[serde(default = "t")]
    pub online: bool,
    #[serde(default = "t")]
    pub delivery: bool,
}

impl Default for ChannelToggles {
    fn default() -> Self {
        Self {
            pos: true,
            qr: true,
            online: true,
            delivery: true,
        }
    }
}

/// A branch's override of the org's toggles. `null` (or absent) inherits.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ChannelOverride {
    #[serde(default)]
    pub pos: Option<bool>,
    #[serde(default)]
    pub qr: Option<bool>,
    #[serde(default)]
    pub online: Option<bool>,
    #[serde(default)]
    pub delivery: Option<bool>,
}

/// One branch's override, with what it resolves to.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct BranchChannelOverride {
    pub branch_id: Uuid,
    pub sell: ChannelOverride,
    /// The org's toggles with this override applied.
    pub effective: ChannelToggles,
}

/// `GET /settings/combos`: the minimum margin (C11) and the channel toggles
/// (§11.1) that gate every combo and every deal.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ComboSettings {
    /// The owner's minimum margin as a decimal fraction string ("0.5500");
    /// `null` = no margin warning.
    pub min_margin: Option<String>,
    /// The org-wide toggles.
    pub channels: ChannelToggles,
    /// Every branch that overrides at least one toggle.
    pub branch_overrides: Vec<BranchChannelOverride>,
}

/// `PUT /settings/combos`. `min_margin` is replaced (omitted or `null` = no
/// warning); `channels`, when present, replaces the org-wide toggles.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct ComboSettingsWrite {
    /// A fraction between "0" and "1", e.g. "0.55".
    #[serde(default)]
    pub min_margin: Option<String>,
    #[serde(default)]
    pub channels: Option<ChannelToggles>,
}

// ── Windows (C4, §11.3) ─────────────────────────────────────────────────────

/// An availability window. A combo or deal with no window is always
/// available; with windows, it is available while any window that applies to
/// the branch (its own, or an all-branch one) is open.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SaleWindow {
    /// Ignored on write (windows are replaced as a set).
    #[serde(default)]
    pub id: Option<Uuid>,
    /// `null` = every branch.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    /// bit0 = Sunday … bit6 = Saturday; 127 = every day (the default).
    #[serde(default = "all_days")]
    pub weekdays: i16,
    /// "HH:MM"; with `ends_at`, or neither (the whole day). `ends_at` before
    /// `starts_at` crosses midnight: the part after midnight belongs to the day
    /// the window started (its weekday and date range).
    #[serde(default)]
    pub starts_at: Option<String>,
    #[serde(default)]
    pub ends_at: Option<String>,
    /// Optional date range, inclusive, judged on the day the window started.
    #[serde(default)]
    pub valid_from: Option<NaiveDate>,
    #[serde(default)]
    pub valid_to: Option<NaiveDate>,
}

// ── Combo CRUD (§2.2) ───────────────────────────────────────────────────────

/// C9: an owner-set surcharge for picking this size instead of the included
/// one. Without a row, a bigger size costs its usual price difference.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SizeSurcharge {
    pub size_label: String,
    pub surcharge: i32,
}

/// What a slot allows: exactly one of `menu_item_id` (an item) or
/// `category_id` (every kind=item item of that category).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ComboChoiceWrite {
    /// The choice's id, to keep it on an edit; omit for a new choice.
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub menu_item_id: Option<Uuid>,
    #[serde(default)]
    pub category_id: Option<Uuid>,
    /// Per pick unit, piastres (C9 "per-choice surcharge").
    #[serde(default)]
    pub surcharge: i32,
    /// The size the combo price covers; `null` = the item's cheapest active size.
    #[serde(default)]
    pub included_size_label: Option<String>,
    #[serde(default)]
    pub size_surcharges: Vec<SizeSurcharge>,
    #[serde(default)]
    pub sort: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ComboSlotWrite {
    /// The slot's id, to keep it on an edit; omit for a new slot.
    #[serde(default)]
    pub id: Option<Uuid>,
    pub name: String,
    #[serde(default = "empty_object")]
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    #[serde(default)]
    pub sort: i32,
    /// Picks required (0–10). 0 = optional slot.
    pub min: i16,
    /// Picks allowed (1–10, ≥ min).
    pub max: i16,
    /// Pre-selected on the till and used for unpicked replays; must be one of
    /// the slot's choices (by id, or by its category).
    #[serde(default)]
    pub default_item_id: Option<Uuid>,
    #[serde(default)]
    pub default_size_label: Option<String>,
    pub choices: Vec<ComboChoiceWrite>,
}

/// `POST /combos`, `PUT /combos/{id}`, `POST /combos/economics`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ComboWrite {
    pub name: String,
    #[serde(default = "empty_object")]
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    #[serde(default)]
    pub category_id: Option<Uuid>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "empty_object")]
    #[schema(value_type = Object)]
    pub description_translations: serde_json::Value,
    #[serde(default = "t")]
    pub is_active: bool,
    /// P, piastres: the combo's `one_size` price (its `base_price`).
    pub price: i32,
    #[serde(default)]
    pub windows: Vec<SaleWindow>,
    pub slots: Vec<ComboSlotWrite>,
}

/// `POST /combos/economics`: a draft, priced for a branch (or the org).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ComboEconomicsRequest {
    #[serde(flatten)]
    pub combo: ComboWrite,
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

/// A choice as stored, with its target's display name.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ComboChoice {
    pub id: Uuid,
    pub menu_item_id: Option<Uuid>,
    pub category_id: Option<Uuid>,
    /// The item's or the category's name (display only).
    #[serde(default)]
    pub name: String,
    #[serde(default = "empty_object")]
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub surcharge: i32,
    pub included_size_label: Option<String>,
    pub size_surcharges: Vec<SizeSurcharge>,
    pub sort: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ComboSlot {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub sort: i32,
    pub min: i16,
    pub max: i16,
    pub default_item_id: Option<Uuid>,
    pub default_size_label: Option<String>,
    pub choices: Vec<ComboChoice>,
}

/// A margin/saving warning (C11). Never a refusal: a combo saves with any.
/// Codes: `MARGIN_BELOW_MIN {margin, min}`, `NO_SAVING`,
/// `COST_UNKNOWN {menu_item_id}`, `SLOT_EMPTY_NOW {slot_id}`,
/// `CHOICE_INACTIVE {menu_item_id}`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ComboWarning {
    pub code: String,
    #[serde(default = "empty_object")]
    #[schema(value_type = Object)]
    pub vars: serde_json::Value,
}

/// The editor's live panel: list value, cost, margin and saving of a combo at
/// a branch (or the org's catalogue prices when `branch_id` is null).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ComboEconomics {
    pub branch_id: Option<Uuid>,
    /// P at this branch.
    pub price: i64,
    /// À la carte value of the default picks at their included sizes.
    pub list_default: i64,
    /// The cheapest and dearest valid pick sets, at their included sizes.
    pub list_min: i64,
    pub list_max: i64,
    /// `null` when any cost involved is unknown.
    pub cost_default: Option<i64>,
    pub cost_max: Option<i64>,
    /// Fractions as strings ("0.5933"); `null` when the cost is unknown or P is 0.
    pub margin_default: Option<String>,
    pub margin_worst: Option<String>,
    pub min_margin: Option<String>,
    /// `list_default − price`.
    pub saving_default: i64,
    pub warnings: Vec<ComboWarning>,
}

/// `GET /combos/{id}`, `POST /combos`, `PUT /combos/{id}`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Combo {
    pub id: Uuid,
    /// Always `combo`.
    pub kind: String,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub category_id: Option<Uuid>,
    pub description: Option<String>,
    #[schema(value_type = Object)]
    pub description_translations: serde_json::Value,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url: Option<String>,
    pub is_active: bool,
    /// P, piastres (the catalogue price; branch prices live in `/menu/pricing`).
    pub price: i32,
    /// C1's fixed bundle: every slot has exactly one item choice with min == max.
    pub is_fixed: bool,
    pub windows: Vec<SaleWindow>,
    pub slots: Vec<ComboSlot>,
    /// Sellable right now on the till at the requested branch (or anywhere,
    /// org-level): active, the POS channel on, a window open, every required
    /// slot with an available choice.
    pub available_now: bool,
    pub economics: ComboEconomics,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One row of `GET /combos`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ComboSummary {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url: Option<String>,
    pub category_id: Option<Uuid>,
    pub price: i32,
    pub is_active: bool,
    pub is_fixed: bool,
    pub slot_count: i64,
    pub window_count: i64,
    /// Org-level: active, a window open now (org time zone), POS channel on.
    pub available_now: bool,
    pub margin_default: Option<String>,
    pub warning_count: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct PaginatedCombos {
    pub data: Vec<ComboSummary>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
    pub total_pages: i64,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ComboListQuery {
    /// Name search (EN or AR).
    pub q: Option<String>,
    pub category_id: Option<Uuid>,
    pub is_active: Option<bool>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ComboGetQuery {
    /// Price the economics for this branch; omitted = the org's catalogue.
    pub branch_id: Option<Uuid>,
}

// ── Make it a meal (C14) ────────────────────────────────────────────────────

/// An item's "make it a meal" upsell: the combo, and the slot this item fills.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct MealLink {
    pub combo_id: Uuid,
    pub slot_id: Uuid,
}

/// `PUT /menu-items/{id}/meal`. Both set = link; both `null` (or a JSON
/// `null` body) = unlink.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct MealLinkWrite {
    #[serde(default)]
    pub combo_id: Option<Uuid>,
    #[serde(default)]
    pub slot_id: Option<Uuid>,
}

// ── The POS catalogue feed (§2.4) ───────────────────────────────────────────

/// The `combo` object on a `kind=combo` row of `GET /menu-items?full=true`,
/// `/catalog/sync` and the `/sync/pull` `menu_item` row. The price is in the
/// row's usual price fields. Choices keep `category_id`: the till expands a
/// category from its own menu.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ComboFeed {
    pub is_fixed: bool,
    /// The org's channel toggles, resolved for the requested branch.
    pub sell: ChannelToggles,
    /// Only the windows for this branch or for every branch.
    pub windows: Vec<SaleWindow>,
    pub slots: Vec<ComboSlot>,
}

// ── Public menus (§2.5) ─────────────────────────────────────────────────────

/// A size of a public combo choice.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct PublicComboSize {
    pub label: String,
    /// Its normal channel price.
    pub price: i32,
    /// What picking it adds inside the combo (the owner's surcharge, else the
    /// difference over the included size, floored at 0; 0 for the included size).
    pub extra: i32,
}

/// One concrete item a public combo slot offers (categories expanded to the
/// items available on this channel and branch).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct PublicComboChoice {
    pub menu_item_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url: Option<String>,
    /// The included size's channel price (what the split weighs it by).
    pub base_price: i32,
    pub included_size_label: String,
    pub sizes: Vec<PublicComboSize>,
    /// The choice's own surcharge, per pick unit.
    pub surcharge: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct PublicComboSlot {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub sort: i32,
    pub min: i16,
    pub max: i16,
    pub default_item_id: Option<Uuid>,
    pub default_size_label: Option<String>,
    pub choices: Vec<PublicComboChoice>,
}

/// The `combo` object of a public menu item.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct PublicCombo {
    pub is_fixed: bool,
    pub slots: Vec<PublicComboSlot>,
}

// ── Order lines (§3.1) ──────────────────────────────────────────────────────

/// One pick of a combo line. On replay the till's `share` and `surcharge`
/// (per combo unit) and add-on prices are stored as charged; live they are
/// ignored and the server prices every part.
#[derive(Clone, Serialize, Deserialize, ToSchema)]
pub struct ComboPickInput {
    pub slot_id: Uuid,
    pub menu_item_id: Uuid,
    #[serde(default)]
    pub size_label: Option<String>,
    /// Units per combo unit; the part line's quantity is this × the line's.
    #[serde(default = "one")]
    pub quantity: i32,
    #[serde(default)]
    pub addons: Vec<crate::orders::component_resolve::AddonInput>,
    #[serde(default)]
    pub optional_field_ids: Vec<Uuid>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Replay only: this pick's share of P, per combo unit.
    #[serde(default)]
    pub share: Option<i32>,
    /// Replay only: this pick's surcharge (choice + size), per combo unit.
    #[serde(default)]
    pub surcharge: Option<i32>,
}

/// The `combo` field of an order line naming a combo item.
#[derive(Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ComboInput {
    pub picks: Vec<ComboPickInput>,
}

/// The combo a kitchen line belongs to (C12): the KDS groups and tags by it.
/// Old KDS builds ignore it.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct KitchenComboTag {
    /// The combo's header line (`order_items.id`, or the ticket line's id).
    pub line_id: Uuid,
    pub name: String,
    #[serde(default = "empty_object")]
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
}
