//! Wire contract of the Menu Advisor — the single owner of every type that
//! crosses the HTTP boundary.
//!
//! The React dashboard consumes these shapes with a hand-written client
//! (`SufrixDashboard/src/entities/menu-advisor/schemas.ts`), so every serde
//! attribute here is load-bearing: field names, `snake_case` renames, the
//! internally-tagged `Classification` enum, the `#[serde(flatten)]` record
//! wrappers, and `PriceRoundingRule`'s PascalCase variants. Change values,
//! never shapes.
//!
//! Engine, adapter, persistence, and handlers all import from here; nothing
//! here imports from them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

// ═══════════════════════════════════════════════════════════════════
// Item identity
// ═══════════════════════════════════════════════════════════════════

/// One sellable SKU: a (menu_item_id, size_label) pair.
/// `size_label = "one_size"` for items without sizes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
pub struct ItemKey {
    pub menu_item_id: Uuid,
    pub size_label: String,
}

// ═══════════════════════════════════════════════════════════════════
// Analysis configuration (request body field, echoed in PersistedRun)
// ═══════════════════════════════════════════════════════════════════

/// `#[serde(default)]` lets clients send partial configs; missing fields
/// take the values below. Output serialization is unaffected.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct AnalysisConfig {
    pub analysis_window_days: f64,
    pub recency_half_life_days: f64,
    pub target_food_cost_pct: f64,
    pub min_gross_margin_pct: f64,
    pub max_price_change_pct_per_cycle: f64,
    pub min_units_for_classification: f64,
    pub min_cooccurrences_for_bundle: f64,
    pub min_lift_for_bundle: f64,
    pub bundle_discount_pct_range: (f64, f64),
    pub price_rounding_rule: PriceRoundingRule,
    pub bundle_top_k_partners: usize,
    pub bundle_max_size: usize,
    pub bundle_top_n_per_focus: usize,
    pub halo_repeat_rate: f64,
    pub promotion_lift_prior: f64,
    /// Conservative max-raise cap for revenue-only items (no margin floor to
    /// guard against).
    pub revenue_mode_max_raise_pct: f64,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            analysis_window_days:           30.0,
            recency_half_life_days:         14.0,
            target_food_cost_pct:           0.30,
            min_gross_margin_pct:           0.55,
            max_price_change_pct_per_cycle: 0.15,
            min_units_for_classification:   20.0,
            min_cooccurrences_for_bundle:   8.0,
            min_lift_for_bundle:            1.20,
            bundle_discount_pct_range:      (0.10, 0.25),
            price_rounding_rule:            PriceRoundingRule::EgyptianCafe,
            bundle_top_k_partners:          5,
            bundle_max_size:                3,
            bundle_top_n_per_focus:         3,
            halo_repeat_rate:               0.15,
            promotion_lift_prior:           1.25,
            revenue_mode_max_raise_pct:     0.05,
        }
    }
}

/// Serializes as `"EgyptianCafe"` / `"NearestUnit"` — PascalCase on the wire
/// (no `rename_all`); existing clients depend on it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub enum PriceRoundingRule {
    /// Nearest 5 EGP, or 2.5 EGP for items < 25 EGP. No .99 endings.
    EgyptianCafe,
    NearestUnit,
}

// ═══════════════════════════════════════════════════════════════════
// Classification taxonomy
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CmQuadrant {
    Star,
    Plowhorse,
    Puzzle,
    Dog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RevenueClass {
    /// High pop + high price/unit  (analog of Star)
    Hero,
    /// High pop + low price/unit   (analog of Plowhorse)
    Steady,
    /// Low pop + high price/unit   (analog of Puzzle)
    Slow,
    /// Low pop + low price/unit    (analog of Dog)
    Quiet,
}

/// Wire shape: `{"mode":"cm","quadrant":"star"}` / `{"mode":"revenue","class":"hero"}`
/// / `{"mode":"insufficient"}`. By construction `Cm` only ever describes
/// cost-tracked items and `Revenue` only cost-missing ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Classification {
    Cm { quadrant: CmQuadrant },
    Revenue { class: RevenueClass },
    Insufficient,
}

// ═══════════════════════════════════════════════════════════════════
// Common output enums / structs
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Hold,
    RaisePrice,
    LowerPrice,
    Bundle,
    Remove,
    Reformulate,
    Monitor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Ord for Confidence {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let rank = |c: &Confidence| match c {
            Confidence::Low => 0,
            Confidence::Medium => 1,
            Confidence::High => 2,
        };
        rank(self).cmp(&rank(other))
    }
}
impl PartialOrd for Confidence {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GuardClip {
    MarginFloor,
    ChangeCap,
    CulturalRounding,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
pub struct Triplet {
    pub lo: f64,
    pub mid: f64,
    pub hi: f64,
}

// ═══════════════════════════════════════════════════════════════════
// Price suggestion
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PeerComparison {
    pub same_category_count: usize,
    pub median_effective_price_peers: f64,
    /// Only set when this item is CM-tracked AND peers are CM-tracked too.
    pub median_margin_pct_peers: Option<f64>,
    pub median_cm_per_unit_peers: Option<f64>,
    pub your_position: PeerPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeerPosition {
    Above,
    At,
    Below,
}

/// Two anchors are universal; the cost-plus anchor is only meaningful with
/// cost data, so it's optional.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PriceAnchors {
    pub cost_plus: Option<f64>,
    pub peer_median: f64,
    pub status_quo: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PriceSuggestion {
    pub key: ItemKey,
    pub item_name: String,
    pub classification: Classification,
    pub current_price: i64,

    // Volume + revenue (always present)
    pub units_sold_raw: f64,
    pub effective_price: f64,
    pub popularity_share: f64,

    // CM-only fields (None for revenue-only items)
    pub cm_per_unit: Option<f64>,
    pub margin_pct: Option<f64>,
    pub food_cost_pct: Option<f64>,

    pub anchors: PriceAnchors,
    pub suggested_price: Option<i64>,
    pub suggested_delta_abs: Option<i64>,
    pub suggested_delta_pct: Option<f64>,
    pub action: Action,
    pub confidence: Confidence,
    pub explanation: String,
    pub guard_clips: Vec<GuardClip>,
    pub peer_comparison: Option<PeerComparison>,
    pub price_changed_in_window: bool,
    /// Only computed for CM-tracked Plowhorses.
    pub cost_reduction_whatif_margin: Option<f64>,
    /// True when cost data is unavailable for this item. Mirrors
    /// `classification` mode, exposed flat for UI badge rendering.
    pub cost_missing: bool,
}

// ═══════════════════════════════════════════════════════════════════
// Bundle suggestion
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BundleItemPair {
    pub item_a: ItemKey,
    pub item_b: ItemKey,
    pub lift: f64,
    pub support: f64,
    /// Directional: P(item_b in basket | item_a in basket), item_a = focus.
    pub confidence_ab: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BundleAssociation {
    pub pair_lifts: Vec<BundleItemPair>,
    pub composite_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BundleForecast {
    pub expected_velocity: Triplet,
    pub inside_bundle_units_x: f64,
    pub halo_units_x: f64,
    pub total_units_uplift_x: f64,
    /// `None` when any component is cost-missing — CM math is impossible.
    pub incremental_cm: Option<Triplet>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BundleSuggestion {
    pub focus_item: ItemKey,
    pub bundle_items: Vec<ItemKey>,
    pub bundle_list_price: i64,
    pub bundle_suggested_price: i64,
    pub bundle_discount_pct: f64,
    /// All cost-derived fields are `None` when any component lacks cost data.
    pub bundle_cost: Option<i64>,
    pub bundle_cm: Option<i64>,
    pub bundle_margin_pct: Option<f64>,
    pub association: BundleAssociation,
    pub forecast: BundleForecast,
    pub guard_clips: Vec<GuardClip>,
    pub explanation: String,
    /// True ⟺ at least one component is cost-missing.
    pub missing_costs: bool,
}

// ═══════════════════════════════════════════════════════════════════
// Removal scenario
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AbsorbedBy {
    pub key: ItemKey,
    pub absorbed_units: f64,
    pub absorbed_cm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ComplementaryLoss {
    pub key: ItemKey,
    pub lost_units: f64,
    pub lost_cm: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemovalRecommendation {
    Remove,
    KeepAndBundle,
    KeepAndReformulate,
    NoStrongSignal,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RemovalScenario {
    pub key: ItemKey,
    pub item_name: String,
    pub baseline_cm: f64,
    pub absorbed_by: Vec<AbsorbedBy>,
    pub complementary_losses: Vec<ComplementaryLoss>,
    pub net_cm_change: f64,
    pub net_cm_change_lo: f64,
    pub net_cm_change_hi: f64,
    pub recommendation: RemovalRecommendation,
    pub explanation: String,
}

// ═══════════════════════════════════════════════════════════════════
// Report container (engine output, persisted by the shell)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, ToSchema)]
pub struct ModeSummary {
    pub items_total: usize,
    pub items_cm_tracked: usize,
    pub items_revenue_only: usize,
    pub items_insufficient: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AdvisorReport {
    pub generated_at: DateTime<Utc>,
    pub window_days: f64,
    pub mode_summary: ModeSummary,
    pub price_suggestions: Vec<PriceSuggestion>,
    pub bundle_suggestions: Vec<BundleSuggestion>,
    pub removal_scenarios: Vec<RemovalScenario>,
}

// ═══════════════════════════════════════════════════════════════════
// Runs
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    InProgress,
    Completed,
    Failed,
}

impl RunStatus {
    pub fn parse(s: &str) -> Self {
        match s {
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PersistedRun {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub org_id: Uuid,
    pub status: RunStatus,
    pub config: AnalysisConfig,
    pub mode_summary: ModeSummary,
    pub error_message: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub window_days: f64,
}

// ═══════════════════════════════════════════════════════════════════
// Decisions
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Accepted,
    Rejected,
    Ignored,
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Ignored => "ignored",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "accepted" => Some(Self::Accepted),
            "rejected" => Some(Self::Rejected),
            "ignored" => Some(Self::Ignored),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    Price,
    Bundle,
    Removal,
}

impl SuggestionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Price => "price",
            Self::Bundle => "bundle",
            Self::Removal => "removal",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "price" => Some(Self::Price),
            "bundle" => Some(Self::Bundle),
            "removal" => Some(Self::Removal),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DecisionRecord {
    pub id: Uuid,
    pub suggestion_id: Uuid,
    pub suggestion_kind: SuggestionKind,
    pub branch_id: Uuid,
    pub decision: Decision,
    pub notes: Option<String>,
    pub decided_by: Uuid,
    pub decided_at: DateTime<Utc>,
}

// ═══════════════════════════════════════════════════════════════════
// Persisted suggestion records (wrappers flatten the suggestion body)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PriceSuggestionRecord {
    pub id: Uuid,
    pub run_id: Uuid,
    pub branch_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub decision: Option<DecisionRecord>,
    #[serde(flatten)]
    pub suggestion: PriceSuggestion,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BundleSuggestionRecord {
    pub id: Uuid,
    pub run_id: Uuid,
    pub branch_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub decision: Option<DecisionRecord>,
    pub promoted_bundle_id: Option<Uuid>,
    #[serde(flatten)]
    pub suggestion: BundleSuggestion,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RemovalScenarioRecord {
    pub id: Uuid,
    pub run_id: Uuid,
    pub branch_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub decision: Option<DecisionRecord>,
    #[serde(flatten)]
    pub scenario: RemovalScenario,
}

// ═══════════════════════════════════════════════════════════════════
// Calibration
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CalibrationPoint {
    pub suggestion_id: Uuid,
    pub menu_item_id: Uuid,
    pub size_label: String,
    pub item_name: String,
    /// Classification at suggestion time: "cm" or "revenue"
    pub classification_mode: String,
    pub previous_price: i64,
    pub suggested_price: i64,
    pub realized_price: i64,
    pub predicted_delta_pct: f64,
    pub realized_delta_pct: f64,
    pub decided_at: DateTime<Utc>,
    pub realized_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CalibrationSummary {
    pub branch_id: Uuid,
    pub since: Option<DateTime<Utc>>,
    pub points_cm: Vec<CalibrationPoint>,
    pub points_revenue: Vec<CalibrationPoint>,
    /// Fraction of accepted CM suggestions whose realized price landed
    /// within ±2% of the suggested price. `None` below 10 samples.
    pub cm_in_range_pct: Option<f64>,
    pub revenue_in_range_pct: Option<f64>,
}

// ═══════════════════════════════════════════════════════════════════
// Request bodies
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateRunBody {
    pub config: Option<AnalysisConfig>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreateRunResponse {
    pub run_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecordDecisionBody {
    pub suggestion_id: Uuid,
    pub suggestion_kind: SuggestionKind,
    pub branch_id: Uuid,
    /// accepted | rejected | ignored — kept as a string so invalid values
    /// yield a 400 instead of a deserialization error.
    pub decision: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PromoteBundleBody {
    pub bundle_id: Uuid,
}

// ═══════════════════════════════════════════════════════════════════
// Query / filter structs
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Default, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListRunsQuery {
    /// Page size, clamped to [1, 100]. Default 20.
    pub limit: Option<i64>,
    /// Return runs started strictly before this instant (pagination cursor).
    pub before: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LatestRunQuery {
    /// When true, return the latest run regardless of status so the client
    /// can show failed runs (error_message) instead of an empty state.
    #[serde(default)]
    pub any_status: bool,
}

#[derive(Debug, Default, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListDecisionsQuery {
    /// Only decisions made at or after this instant.
    pub since: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PriceSuggestionFilter {
    /// cm | revenue | insufficient
    pub classification_mode: Option<String>,
    /// star | plowhorse | puzzle | dog
    pub cm_quadrant: Option<String>,
    /// hero | steady | slow | quiet
    pub revenue_class: Option<String>,
    /// hold | raise_price | lower_price | bundle | remove | reformulate | monitor
    pub action: Option<String>,
    /// low | medium | high
    pub confidence: Option<String>,
    pub category_id: Option<Uuid>,
    /// accepted | rejected | ignored | pending
    pub decision_status: Option<String>,
    /// Case-insensitive substring match on item name.
    pub search: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BundleSuggestionFilter {
    pub missing_costs: Option<bool>,
    pub focus_menu_item_id: Option<Uuid>,
    /// accepted | rejected | ignored | pending
    pub decision_status: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RemovalScenarioFilter {
    /// remove | keep_and_bundle | keep_and_reformulate | no_strong_signal
    pub recommendation: Option<String>,
    /// accepted | rejected | ignored | pending
    pub decision_status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemKpiPath {
    pub branch_id: Uuid,
    pub menu_item_id: Uuid,
    pub size_label: String,
}
