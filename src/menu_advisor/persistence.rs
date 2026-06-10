//! Persistence layer for the Menu Advisor.
//!
//! Engine outputs ↔ Postgres rows. The engine is pure; this module is the
//! one place that knows about run rows, suggestion IDs, decision joins, and
//! JSONB payloads for the nested forecast / association / anchors structs.
//!
//! Table contract (the migration creates these — see persistence_schema.sql
//! in the migration phase for the exact DDL):
//!   - menu_advisor_runs
//!   - menu_advisor_price_suggestions
//!   - menu_advisor_bundle_suggestions
//!   - menu_advisor_removal_scenarios
//!   - menu_advisor_decisions

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::errors::AppError;
use super::engine::{
    AdvisorReport, AnalysisConfig, BundleSuggestion, Classification, CmQuadrant, Confidence,
    GuardClip, ItemKey, ModeSummary, PriceSuggestion, RemovalRecommendation, RemovalScenario,
    RevenueClass,
};

// ═══════════════════════════════════════════════════════════════════
// Public types — what the HTTP layer returns
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    InProgress,
    Completed,
    Failed,
}

impl RunStatus {
    fn parse(s: &str) -> Self {
        match s {
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Accepted,
    Rejected,
    Ignored,
}

impl Decision {
    fn as_str(&self) -> &'static str {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    Price,
    Bundle,
    Removal,
}

impl SuggestionKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Price => "price",
            Self::Bundle => "bundle",
            Self::Removal => "removal",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PriceSuggestionRecord {
    pub id: Uuid,
    pub run_id: Uuid,
    pub branch_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub decision: Option<DecisionRecord>,
    #[serde(flatten)]
    pub suggestion: PriceSuggestion,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
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
// Filters
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Default, Clone, Deserialize)]
pub struct PriceSuggestionFilter {
    pub classification_mode: Option<String>, // cm | revenue | insufficient
    pub cm_quadrant: Option<String>,         // star | plowhorse | puzzle | dog
    pub revenue_class: Option<String>,       // hero | steady | slow | quiet
    pub action: Option<String>,
    pub confidence: Option<String>,
    pub category_id: Option<Uuid>,
    pub decision_status: Option<String>, // accepted | rejected | ignored | pending
    pub search: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct BundleSuggestionFilter {
    pub missing_costs: Option<bool>,
    pub focus_menu_item_id: Option<Uuid>,
    pub decision_status: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct RemovalScenarioFilter {
    pub recommendation: Option<String>,
    pub decision_status: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════
// Run lifecycle
// ═══════════════════════════════════════════════════════════════════

pub async fn create_run(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    config: &AnalysisConfig,
) -> Result<Uuid, AppError> {
    let id = Uuid::new_v4();
    let cfg_json = serde_json::to_value(config)
        .map_err(|e| {
            tracing::error!("serialize config: {e}");
            AppError::Internal
        })?;
    sqlx::query(
        r#"
        INSERT INTO menu_advisor_runs (
            id, branch_id, org_id, status, config_json, started_at
        ) VALUES ($1, $2, $3, 'in_progress', $4, NOW())
        "#,
    )
    .bind(id)
    .bind(branch_id)
    .bind(org_id)
    .bind(cfg_json)
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn mark_run_failed(
    pool: &PgPool,
    run_id: Uuid,
    error_message: &str,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        UPDATE menu_advisor_runs
        SET status        = 'failed',
            error_message = $2,
            completed_at  = NOW()
        WHERE id = $1
        "#,
    )
    .bind(run_id)
    .bind(error_message)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist the engine report and mark the run complete.
/// One transaction: all-or-nothing.
pub async fn save_completed_report(
    pool: &PgPool,
    run_id: Uuid,
    branch_id: Uuid,
    snaps_by_key: &HashMap<ItemKey, (Option<Uuid>, String)>, // key -> (category_id, name)
    report: &AdvisorReport,
) -> Result<(), AppError> {
    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;

    for s in &report.price_suggestions {
        insert_price_suggestion(&mut tx, run_id, branch_id, snaps_by_key, s).await?;
    }
    for b in &report.bundle_suggestions {
        insert_bundle_suggestion(&mut tx, run_id, branch_id, b).await?;
    }
    for r in &report.removal_scenarios {
        insert_removal_scenario(&mut tx, run_id, branch_id, r).await?;
    }

    sqlx::query(
        r#"
        UPDATE menu_advisor_runs
        SET status               = 'completed',
            completed_at         = NOW(),
            window_days          = $2,
            items_total          = $3,
            items_cm_tracked     = $4,
            items_revenue_only   = $5,
            items_insufficient   = $6
        WHERE id = $1
        "#,
    )
    .bind(run_id)
    .bind(report.window_days)
    .bind(report.mode_summary.items_total as i32)
    .bind(report.mode_summary.items_cm_tracked as i32)
    .bind(report.mode_summary.items_revenue_only as i32)
    .bind(report.mode_summary.items_insufficient as i32)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
// Insert helpers
// ═══════════════════════════════════════════════════════════════════

fn split_classification(c: Classification) -> (&'static str, Option<&'static str>, Option<&'static str>) {
    match c {
        Classification::Cm { quadrant } => ("cm", Some(cm_quadrant_str(quadrant)), None),
        Classification::Revenue { class } => ("revenue", None, Some(revenue_class_str(class))),
        Classification::Insufficient => ("insufficient", None, None),
    }
}

fn cm_quadrant_str(q: CmQuadrant) -> &'static str {
    match q {
        CmQuadrant::Star => "star",
        CmQuadrant::Plowhorse => "plowhorse",
        CmQuadrant::Puzzle => "puzzle",
        CmQuadrant::Dog => "dog",
    }
}

fn revenue_class_str(c: RevenueClass) -> &'static str {
    match c {
        RevenueClass::Hero => "hero",
        RevenueClass::Steady => "steady",
        RevenueClass::Slow => "slow",
        RevenueClass::Quiet => "quiet",
    }
}

fn parse_classification(
    mode: &str,
    cm_q: Option<&str>,
    rev_c: Option<&str>,
) -> Classification {
    match mode {
        "cm" => Classification::Cm {
            quadrant: match cm_q.unwrap_or("") {
                "star" => CmQuadrant::Star,
                "plowhorse" => CmQuadrant::Plowhorse,
                "puzzle" => CmQuadrant::Puzzle,
                _ => CmQuadrant::Dog,
            },
        },
        "revenue" => Classification::Revenue {
            class: match rev_c.unwrap_or("") {
                "hero" => RevenueClass::Hero,
                "steady" => RevenueClass::Steady,
                "slow" => RevenueClass::Slow,
                _ => RevenueClass::Quiet,
            },
        },
        _ => Classification::Insufficient,
    }
}

fn action_str(a: super::engine::Action) -> &'static str {
    use super::engine::Action;
    match a {
        Action::Hold => "hold",
        Action::RaisePrice => "raise_price",
        Action::LowerPrice => "lower_price",
        Action::Bundle => "bundle",
        Action::Remove => "remove",
        Action::Reformulate => "reformulate",
        Action::Monitor => "monitor",
    }
}
fn parse_action(s: &str) -> super::engine::Action {
    use super::engine::Action;
    match s {
        "raise_price" => Action::RaisePrice,
        "lower_price" => Action::LowerPrice,
        "bundle" => Action::Bundle,
        "remove" => Action::Remove,
        "reformulate" => Action::Reformulate,
        "monitor" => Action::Monitor,
        _ => Action::Hold,
    }
}

fn confidence_str(c: Confidence) -> &'static str {
    match c {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        Confidence::High => "high",
    }
}
fn parse_confidence(s: &str) -> Confidence {
    match s {
        "high" => Confidence::High,
        "medium" => Confidence::Medium,
        _ => Confidence::Low,
    }
}

fn removal_rec_str(r: RemovalRecommendation) -> &'static str {
    match r {
        RemovalRecommendation::Remove => "remove",
        RemovalRecommendation::KeepAndBundle => "keep_and_bundle",
        RemovalRecommendation::KeepAndReformulate => "keep_and_reformulate",
        RemovalRecommendation::NoStrongSignal => "no_strong_signal",
    }
}
fn parse_removal_rec(s: &str) -> RemovalRecommendation {
    match s {
        "remove" => RemovalRecommendation::Remove,
        "keep_and_bundle" => RemovalRecommendation::KeepAndBundle,
        "keep_and_reformulate" => RemovalRecommendation::KeepAndReformulate,
        _ => RemovalRecommendation::NoStrongSignal,
    }
}

async fn insert_price_suggestion(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
    branch_id: Uuid,
    snaps_by_key: &HashMap<ItemKey, (Option<Uuid>, String)>,
    s: &PriceSuggestion,
) -> Result<(), AppError> {
    let (mode, cm_q, rev_c) = split_classification(s.classification);
    let category_id = snaps_by_key.get(&s.key).and_then(|(c, _)| *c);
    let anchors_json = serde_json::to_value(&s.anchors).unwrap_or(JsonValue::Null);
    let peer_json = match &s.peer_comparison {
        Some(p) => serde_json::to_value(p).unwrap_or(JsonValue::Null),
        None => JsonValue::Null,
    };
    let guard_clips_json = serde_json::to_value(&s.guard_clips).unwrap_or(JsonValue::Array(vec![]));

    sqlx::query(
        r#"
        INSERT INTO menu_advisor_price_suggestions (
            id, run_id, branch_id,
            menu_item_id, size_label, item_name, category_id,
            classification_mode, cm_quadrant, revenue_class,
            current_price, units_sold_raw, effective_price, popularity_share,
            cm_per_unit, margin_pct, food_cost_pct,
            anchors_json, peer_comparison_json,
            suggested_price, suggested_delta_abs, suggested_delta_pct,
            action, confidence, explanation,
            guard_clips_json, price_changed_in_window,
            cost_reduction_whatif_margin, cost_missing, created_at
        ) VALUES (
            $1, $2, $3,
            $4, $5, $6, $7,
            $8, $9, $10,
            $11, $12, $13, $14,
            $15, $16, $17,
            $18, $19,
            $20, $21, $22,
            $23, $24, $25,
            $26, $27,
            $28, $29, NOW()
        )
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(run_id)
    .bind(branch_id)
    .bind(s.key.menu_item_id)
    .bind(&s.key.size_label)
    .bind(&s.item_name)
    .bind(category_id)
    .bind(mode)
    .bind(cm_q)
    .bind(rev_c)
    .bind(s.current_price)
    .bind(s.units_sold_raw)
    .bind(s.effective_price)
    .bind(s.popularity_share)
    .bind(s.cm_per_unit)
    .bind(s.margin_pct)
    .bind(s.food_cost_pct)
    .bind(anchors_json)
    .bind(peer_json)
    .bind(s.suggested_price)
    .bind(s.suggested_delta_abs)
    .bind(s.suggested_delta_pct)
    .bind(action_str(s.action))
    .bind(confidence_str(s.confidence))
    .bind(&s.explanation)
    .bind(guard_clips_json)
    .bind(s.price_changed_in_window)
    .bind(s.cost_reduction_whatif_margin)
    .bind(s.cost_missing)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_bundle_suggestion(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
    branch_id: Uuid,
    b: &BundleSuggestion,
) -> Result<(), AppError> {
    let components_json = serde_json::to_value(&b.bundle_items).unwrap_or(JsonValue::Array(vec![]));
    let association_json = serde_json::to_value(&b.association).unwrap_or(JsonValue::Null);
    let forecast_json = serde_json::to_value(&b.forecast).unwrap_or(JsonValue::Null);
    let guard_clips_json = serde_json::to_value(&b.guard_clips).unwrap_or(JsonValue::Array(vec![]));

    sqlx::query(
        r#"
        INSERT INTO menu_advisor_bundle_suggestions (
            id, run_id, branch_id,
            focus_menu_item_id, focus_size_label,
            components_json,
            bundle_list_price, bundle_suggested_price, bundle_discount_pct,
            bundle_cost, bundle_cm, bundle_margin_pct,
            association_json, forecast_json,
            guard_clips_json, explanation, missing_costs, created_at
        ) VALUES (
            $1, $2, $3,
            $4, $5,
            $6,
            $7, $8, $9,
            $10, $11, $12,
            $13, $14,
            $15, $16, $17, NOW()
        )
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(run_id)
    .bind(branch_id)
    .bind(b.focus_item.menu_item_id)
    .bind(&b.focus_item.size_label)
    .bind(components_json)
    .bind(b.bundle_list_price)
    .bind(b.bundle_suggested_price)
    .bind(b.bundle_discount_pct)
    .bind(b.bundle_cost)
    .bind(b.bundle_cm)
    .bind(b.bundle_margin_pct)
    .bind(association_json)
    .bind(forecast_json)
    .bind(guard_clips_json)
    .bind(&b.explanation)
    .bind(b.missing_costs)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_removal_scenario(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
    branch_id: Uuid,
    r: &RemovalScenario,
) -> Result<(), AppError> {
    let absorbed_json = serde_json::to_value(&r.absorbed_by).unwrap_or(JsonValue::Array(vec![]));
    let losses_json = serde_json::to_value(&r.complementary_losses).unwrap_or(JsonValue::Array(vec![]));

    sqlx::query(
        r#"
        INSERT INTO menu_advisor_removal_scenarios (
            id, run_id, branch_id,
            menu_item_id, size_label, item_name,
            baseline_cm, absorbed_by_json, complementary_losses_json,
            net_cm_change, net_cm_change_lo, net_cm_change_hi,
            recommendation, explanation, created_at
        ) VALUES (
            $1, $2, $3,
            $4, $5, $6,
            $7, $8, $9,
            $10, $11, $12,
            $13, $14, NOW()
        )
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(run_id)
    .bind(branch_id)
    .bind(r.key.menu_item_id)
    .bind(&r.key.size_label)
    .bind(&r.item_name)
    .bind(r.baseline_cm)
    .bind(absorbed_json)
    .bind(losses_json)
    .bind(r.net_cm_change)
    .bind(r.net_cm_change_lo)
    .bind(r.net_cm_change_hi)
    .bind(removal_rec_str(r.recommendation))
    .bind(&r.explanation)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
// Run queries
// ═══════════════════════════════════════════════════════════════════

#[derive(sqlx::FromRow)]
struct RunRow {
    id: Uuid,
    branch_id: Uuid,
    org_id: Uuid,
    status: String,
    config_json: JsonValue,
    error_message: Option<String>,
    items_total: Option<i32>,
    items_cm_tracked: Option<i32>,
    items_revenue_only: Option<i32>,
    items_insufficient: Option<i32>,
    window_days: Option<f64>,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

fn row_to_run(r: RunRow) -> PersistedRun {
    let cfg: AnalysisConfig = serde_json::from_value(r.config_json).unwrap_or_default();
    PersistedRun {
        id: r.id,
        branch_id: r.branch_id,
        org_id: r.org_id,
        status: RunStatus::parse(&r.status),
        config: cfg,
        mode_summary: ModeSummary {
            items_total: r.items_total.unwrap_or(0).max(0) as usize,
            items_cm_tracked: r.items_cm_tracked.unwrap_or(0).max(0) as usize,
            items_revenue_only: r.items_revenue_only.unwrap_or(0).max(0) as usize,
            items_insufficient: r.items_insufficient.unwrap_or(0).max(0) as usize,
        },
        error_message: r.error_message,
        started_at: r.started_at,
        completed_at: r.completed_at,
        window_days: r.window_days.unwrap_or(30.0),
    }
}

pub async fn get_run(pool: &PgPool, run_id: Uuid) -> Result<PersistedRun, AppError> {
    let row: RunRow = sqlx::query_as::<_, RunRow>(
        r#"
        SELECT id, branch_id, org_id, status, config_json, error_message,
               items_total, items_cm_tracked, items_revenue_only, items_insufficient,
               window_days, started_at, completed_at
        FROM   menu_advisor_runs
        WHERE  id = $1
        "#,
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("run not found".into()))?;
    Ok(row_to_run(row))
}

pub async fn list_runs(
    pool: &PgPool,
    branch_id: Uuid,
    limit: i64,
    before: Option<DateTime<Utc>>,
) -> Result<Vec<PersistedRun>, AppError> {
    let rows: Vec<RunRow> = match before {
        Some(b) => sqlx::query_as::<_, RunRow>(
            r#"
            SELECT id, branch_id, org_id, status, config_json, error_message,
                   items_total, items_cm_tracked, items_revenue_only, items_insufficient,
                   window_days, started_at, completed_at
            FROM   menu_advisor_runs
            WHERE  branch_id = $1 AND started_at < $2
            ORDER BY started_at DESC
            LIMIT  $3
            "#,
        )
        .bind(branch_id)
        .bind(b)
        .bind(limit)
        .fetch_all(pool)
        .await?,
        None => sqlx::query_as::<_, RunRow>(
            r#"
            SELECT id, branch_id, org_id, status, config_json, error_message,
                   items_total, items_cm_tracked, items_revenue_only, items_insufficient,
                   window_days, started_at, completed_at
            FROM   menu_advisor_runs
            WHERE  branch_id = $1
            ORDER BY started_at DESC
            LIMIT  $2
            "#,
        )
        .bind(branch_id)
        .bind(limit)
        .fetch_all(pool)
        .await?,
    };
    Ok(rows.into_iter().map(row_to_run).collect())
}

pub async fn get_latest_completed_run(
    pool: &PgPool,
    branch_id: Uuid,
) -> Result<Option<PersistedRun>, AppError> {
    let row: Option<RunRow> = sqlx::query_as::<_, RunRow>(
        r#"
        SELECT id, branch_id, org_id, status, config_json, error_message,
               items_total, items_cm_tracked, items_revenue_only, items_insufficient,
               window_days, started_at, completed_at
        FROM   menu_advisor_runs
        WHERE  branch_id = $1 AND status = 'completed'
        ORDER BY completed_at DESC NULLS LAST
        LIMIT 1
        "#,
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_run))
}

/// Latest run regardless of status — lets the dashboard surface failed runs
/// (`error_message`) instead of rendering an unexplained empty state.
pub async fn get_latest_run_any(
    pool: &PgPool,
    branch_id: Uuid,
) -> Result<Option<PersistedRun>, AppError> {
    let row: Option<RunRow> = sqlx::query_as::<_, RunRow>(
        r#"
        SELECT id, branch_id, org_id, status, config_json, error_message,
               items_total, items_cm_tracked, items_revenue_only, items_insufficient,
               window_days, started_at, completed_at
        FROM   menu_advisor_runs
        WHERE  branch_id = $1
        ORDER BY started_at DESC
        LIMIT 1
        "#,
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_run))
}

pub async fn get_in_progress_run(
    pool: &PgPool,
    branch_id: Uuid,
) -> Result<Option<PersistedRun>, AppError> {
    let row: Option<RunRow> = sqlx::query_as::<_, RunRow>(
        r#"
        SELECT id, branch_id, org_id, status, config_json, error_message,
               items_total, items_cm_tracked, items_revenue_only, items_insufficient,
               window_days, started_at, completed_at
        FROM   menu_advisor_runs
        WHERE  branch_id = $1 AND status = 'in_progress'
        ORDER BY started_at DESC
        LIMIT 1
        "#,
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_run))
}

// ═══════════════════════════════════════════════════════════════════
// Price suggestion queries
// ═══════════════════════════════════════════════════════════════════

#[derive(sqlx::FromRow)]
struct PriceSuggestionRow {
    id: Uuid,
    run_id: Uuid,
    branch_id: Uuid,
    menu_item_id: Uuid,
    size_label: String,
    item_name: String,
    category_id: Option<Uuid>,
    classification_mode: String,
    cm_quadrant: Option<String>,
    revenue_class: Option<String>,
    current_price: i64,
    units_sold_raw: f64,
    effective_price: f64,
    popularity_share: f64,
    cm_per_unit: Option<f64>,
    margin_pct: Option<f64>,
    food_cost_pct: Option<f64>,
    anchors_json: JsonValue,
    peer_comparison_json: Option<JsonValue>,
    suggested_price: Option<i64>,
    suggested_delta_abs: Option<i64>,
    suggested_delta_pct: Option<f64>,
    action: String,
    confidence: String,
    explanation: String,
    guard_clips_json: JsonValue,
    price_changed_in_window: bool,
    cost_reduction_whatif_margin: Option<f64>,
    cost_missing: bool,
    created_at: DateTime<Utc>,
    // Joined from decisions
    decision_id: Option<Uuid>,
    decision: Option<String>,
    decision_notes: Option<String>,
    decision_decided_by: Option<Uuid>,
    decision_decided_at: Option<DateTime<Utc>>,
}

fn row_to_price_suggestion(r: PriceSuggestionRow) -> PriceSuggestionRecord {
    let classification = parse_classification(
        &r.classification_mode,
        r.cm_quadrant.as_deref(),
        r.revenue_class.as_deref(),
    );
    let anchors = serde_json::from_value(r.anchors_json).unwrap_or({
        super::engine::PriceAnchors {
            cost_plus: None,
            peer_median: 0.0,
            status_quo: r.current_price as f64,
        }
    });
    let peer_comparison = r
        .peer_comparison_json
        .and_then(|v| serde_json::from_value(v).ok());
    let guard_clips: Vec<GuardClip> = serde_json::from_value(r.guard_clips_json).unwrap_or_default();

    let suggestion = PriceSuggestion {
        key: ItemKey {
            menu_item_id: r.menu_item_id,
            size_label: r.size_label,
        },
        item_name: r.item_name,
        classification,
        current_price: r.current_price,
        units_sold_raw: r.units_sold_raw,
        effective_price: r.effective_price,
        popularity_share: r.popularity_share,
        cm_per_unit: r.cm_per_unit,
        margin_pct: r.margin_pct,
        food_cost_pct: r.food_cost_pct,
        anchors,
        suggested_price: r.suggested_price,
        suggested_delta_abs: r.suggested_delta_abs,
        suggested_delta_pct: r.suggested_delta_pct,
        action: parse_action(&r.action),
        confidence: parse_confidence(&r.confidence),
        explanation: r.explanation,
        guard_clips,
        peer_comparison,
        price_changed_in_window: r.price_changed_in_window,
        cost_reduction_whatif_margin: r.cost_reduction_whatif_margin,
        cost_missing: r.cost_missing,
    };

    let decision = r.decision_id.map(|did| DecisionRecord {
        id: did,
        suggestion_id: r.id,
        suggestion_kind: SuggestionKind::Price,
        branch_id: r.branch_id,
        decision: r.decision.as_deref().and_then(Decision::parse).unwrap_or(Decision::Ignored),
        notes: r.decision_notes,
        decided_by: r.decision_decided_by.unwrap_or_default(),
        decided_at: r.decision_decided_at.unwrap_or(r.created_at),
    });

    let _ = r.category_id; // already captured implicitly by query filter

    PriceSuggestionRecord {
        id: r.id,
        run_id: r.run_id,
        branch_id: r.branch_id,
        created_at: r.created_at,
        decision,
        suggestion,
    }
}

pub async fn list_price_suggestions(
    pool: &PgPool,
    run_id: Uuid,
    filter: &PriceSuggestionFilter,
) -> Result<Vec<PriceSuggestionRecord>, AppError> {
    let rows: Vec<PriceSuggestionRow> = sqlx::query_as::<_, PriceSuggestionRow>(
        r#"
        SELECT
            ps.id, ps.run_id, ps.branch_id,
            ps.menu_item_id, ps.size_label, ps.item_name, ps.category_id,
            ps.classification_mode, ps.cm_quadrant, ps.revenue_class,
            ps.current_price, ps.units_sold_raw, ps.effective_price, ps.popularity_share,
            ps.cm_per_unit, ps.margin_pct, ps.food_cost_pct,
            ps.anchors_json, ps.peer_comparison_json,
            ps.suggested_price, ps.suggested_delta_abs, ps.suggested_delta_pct,
            ps.action, ps.confidence, ps.explanation,
            ps.guard_clips_json, ps.price_changed_in_window,
            ps.cost_reduction_whatif_margin, ps.cost_missing,
            ps.created_at,
            d.id           AS decision_id,
            d.decision     AS decision,
            d.notes        AS decision_notes,
            d.decided_by   AS decision_decided_by,
            d.decided_at   AS decision_decided_at
        FROM menu_advisor_price_suggestions ps
        LEFT JOIN LATERAL (
            SELECT id, decision, notes, decided_by, decided_at
            FROM menu_advisor_decisions
            WHERE suggestion_id = ps.id AND suggestion_kind = 'price'
            ORDER BY decided_at DESC
            LIMIT 1
        ) d ON TRUE
        WHERE ps.run_id = $1
          AND ($2::text IS NULL OR ps.classification_mode = $2)
          AND ($3::text IS NULL OR ps.cm_quadrant   = $3)
          AND ($4::text IS NULL OR ps.revenue_class = $4)
          AND ($5::text IS NULL OR ps.action        = $5)
          AND ($6::text IS NULL OR ps.confidence    = $6)
          AND ($7::uuid IS NULL OR ps.category_id   = $7)
          AND (
                $8::text IS NULL
             OR ($8 = 'pending'  AND d.id IS NULL)
             OR ($8 IN ('accepted','rejected','ignored') AND d.decision = $8)
          )
          AND ($9::text IS NULL OR ps.item_name ILIKE '%' || $9 || '%')
        ORDER BY ps.popularity_share DESC, ps.item_name
        "#,
    )
    .bind(run_id)
    .bind(filter.classification_mode.as_ref())
    .bind(filter.cm_quadrant.as_ref())
    .bind(filter.revenue_class.as_ref())
    .bind(filter.action.as_ref())
    .bind(filter.confidence.as_ref())
    .bind(filter.category_id)
    .bind(filter.decision_status.as_ref())
    .bind(filter.search.as_ref())
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(row_to_price_suggestion).collect())
}

pub async fn get_price_suggestion(
    pool: &PgPool,
    id: Uuid,
) -> Result<PriceSuggestionRecord, AppError> {
    let row: PriceSuggestionRow = sqlx::query_as::<_, PriceSuggestionRow>(
        r#"
        SELECT
            ps.id, ps.run_id, ps.branch_id,
            ps.menu_item_id, ps.size_label, ps.item_name, ps.category_id,
            ps.classification_mode, ps.cm_quadrant, ps.revenue_class,
            ps.current_price, ps.units_sold_raw, ps.effective_price, ps.popularity_share,
            ps.cm_per_unit, ps.margin_pct, ps.food_cost_pct,
            ps.anchors_json, ps.peer_comparison_json,
            ps.suggested_price, ps.suggested_delta_abs, ps.suggested_delta_pct,
            ps.action, ps.confidence, ps.explanation,
            ps.guard_clips_json, ps.price_changed_in_window,
            ps.cost_reduction_whatif_margin, ps.cost_missing,
            ps.created_at,
            d.id AS decision_id, d.decision AS decision, d.notes AS decision_notes, d.decided_by AS decision_decided_by, d.decided_at AS decision_decided_at
        FROM menu_advisor_price_suggestions ps
        LEFT JOIN LATERAL (
            SELECT id, decision, notes, decided_by, decided_at
            FROM menu_advisor_decisions
            WHERE suggestion_id = ps.id AND suggestion_kind = 'price'
            ORDER BY decided_at DESC LIMIT 1
        ) d ON TRUE
        WHERE ps.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("price suggestion not found".into()))?;
    Ok(row_to_price_suggestion(row))
}

/// Fetch the most recent CM-tracked or revenue-only suggestion for an item
/// from the latest completed run for the branch. Used by the per-item
/// integration on menu item pages.
pub async fn get_latest_item_kpi(
    pool: &PgPool,
    branch_id: Uuid,
    menu_item_id: Uuid,
    size_label: &str,
) -> Result<Option<PriceSuggestionRecord>, AppError> {
    let row: Option<PriceSuggestionRow> = sqlx::query_as::<_, PriceSuggestionRow>(
        r#"
        SELECT
            ps.id, ps.run_id, ps.branch_id,
            ps.menu_item_id, ps.size_label, ps.item_name, ps.category_id,
            ps.classification_mode, ps.cm_quadrant, ps.revenue_class,
            ps.current_price, ps.units_sold_raw, ps.effective_price, ps.popularity_share,
            ps.cm_per_unit, ps.margin_pct, ps.food_cost_pct,
            ps.anchors_json, ps.peer_comparison_json,
            ps.suggested_price, ps.suggested_delta_abs, ps.suggested_delta_pct,
            ps.action, ps.confidence, ps.explanation,
            ps.guard_clips_json, ps.price_changed_in_window,
            ps.cost_reduction_whatif_margin, ps.cost_missing,
            ps.created_at,
            d.id AS decision_id, d.decision AS decision, d.notes AS decision_notes, d.decided_by AS decision_decided_by, d.decided_at AS decision_decided_at
        FROM menu_advisor_price_suggestions ps
        JOIN menu_advisor_runs r ON r.id = ps.run_id
        LEFT JOIN LATERAL (
            SELECT id, decision, notes, decided_by, decided_at
            FROM menu_advisor_decisions
            WHERE suggestion_id = ps.id AND suggestion_kind = 'price'
            ORDER BY decided_at DESC LIMIT 1
        ) d ON TRUE
        WHERE ps.branch_id = $1
          AND ps.menu_item_id = $2
          AND ps.size_label = $3
          AND r.status = 'completed'
        ORDER BY r.completed_at DESC NULLS LAST
        LIMIT 1
        "#,
    )
    .bind(branch_id)
    .bind(menu_item_id)
    .bind(size_label)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_price_suggestion))
}

// ═══════════════════════════════════════════════════════════════════
// Bundle suggestion queries
// ═══════════════════════════════════════════════════════════════════

#[derive(sqlx::FromRow)]
struct BundleSuggestionRow {
    id: Uuid,
    run_id: Uuid,
    branch_id: Uuid,
    focus_menu_item_id: Uuid,
    focus_size_label: String,
    components_json: JsonValue,
    bundle_list_price: i64,
    bundle_suggested_price: i64,
    bundle_discount_pct: f64,
    bundle_cost: Option<i64>,
    bundle_cm: Option<i64>,
    bundle_margin_pct: Option<f64>,
    association_json: JsonValue,
    forecast_json: JsonValue,
    guard_clips_json: JsonValue,
    explanation: String,
    missing_costs: bool,
    promoted_bundle_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    decision_id: Option<Uuid>,
    decision: Option<String>,
    decision_notes: Option<String>,
    decision_decided_by: Option<Uuid>,
    decision_decided_at: Option<DateTime<Utc>>,
}

fn row_to_bundle(r: BundleSuggestionRow) -> BundleSuggestionRecord {
    let components: Vec<ItemKey> =
        serde_json::from_value(r.components_json).unwrap_or_default();
    let association = serde_json::from_value(r.association_json).unwrap_or_else(|_| {
        super::engine::BundleAssociation { pair_lifts: vec![], composite_score: 0.0 }
    });
    let forecast = serde_json::from_value(r.forecast_json).unwrap_or({
        super::engine::BundleForecast {
            expected_velocity: super::engine::Triplet { lo: 0.0, mid: 0.0, hi: 0.0 },
            inside_bundle_units_x: 0.0,
            halo_units_x: 0.0,
            total_units_uplift_x: 0.0,
            incremental_cm: None,
        }
    });
    let guard_clips: Vec<GuardClip> = serde_json::from_value(r.guard_clips_json).unwrap_or_default();

    let focus_item = ItemKey {
        menu_item_id: r.focus_menu_item_id,
        size_label: r.focus_size_label,
    };

    let suggestion = BundleSuggestion {
        focus_item,
        bundle_items: components,
        bundle_list_price: r.bundle_list_price,
        bundle_suggested_price: r.bundle_suggested_price,
        bundle_discount_pct: r.bundle_discount_pct,
        bundle_cost: r.bundle_cost,
        bundle_cm: r.bundle_cm,
        bundle_margin_pct: r.bundle_margin_pct,
        association,
        forecast,
        guard_clips,
        explanation: r.explanation,
        missing_costs: r.missing_costs,
    };

    let decision = r.decision_id.map(|did| DecisionRecord {
        id: did,
        suggestion_id: r.id,
        suggestion_kind: SuggestionKind::Bundle,
        branch_id: r.branch_id,
        decision: r.decision.as_deref().and_then(Decision::parse).unwrap_or(Decision::Ignored),
        notes: r.decision_notes,
        decided_by: r.decision_decided_by.unwrap_or_default(),
        decided_at: r.decision_decided_at.unwrap_or(r.created_at),
    });

    BundleSuggestionRecord {
        id: r.id,
        run_id: r.run_id,
        branch_id: r.branch_id,
        created_at: r.created_at,
        decision,
        promoted_bundle_id: r.promoted_bundle_id,
        suggestion,
    }
}

pub async fn list_bundle_suggestions(
    pool: &PgPool,
    run_id: Uuid,
    filter: &BundleSuggestionFilter,
) -> Result<Vec<BundleSuggestionRecord>, AppError> {
    let rows: Vec<BundleSuggestionRow> = sqlx::query_as::<_, BundleSuggestionRow>(
        r#"
        SELECT
            bs.id, bs.run_id, bs.branch_id,
            bs.focus_menu_item_id, bs.focus_size_label,
            bs.components_json,
            bs.bundle_list_price, bs.bundle_suggested_price, bs.bundle_discount_pct,
            bs.bundle_cost, bs.bundle_cm, bs.bundle_margin_pct,
            bs.association_json, bs.forecast_json,
            bs.guard_clips_json, bs.explanation, bs.missing_costs,
            bs.promoted_bundle_id, bs.created_at,
            d.id AS decision_id, d.decision AS decision, d.notes AS decision_notes, d.decided_by AS decision_decided_by, d.decided_at AS decision_decided_at
        FROM menu_advisor_bundle_suggestions bs
        LEFT JOIN LATERAL (
            SELECT id, decision, notes, decided_by, decided_at
            FROM menu_advisor_decisions
            WHERE suggestion_id = bs.id AND suggestion_kind = 'bundle'
            ORDER BY decided_at DESC LIMIT 1
        ) d ON TRUE
        WHERE bs.run_id = $1
          AND ($2::bool IS NULL OR bs.missing_costs = $2)
          AND ($3::uuid IS NULL OR bs.focus_menu_item_id = $3)
          AND (
                $4::text IS NULL
             OR ($4 = 'pending' AND d.id IS NULL)
             OR ($4 IN ('accepted','rejected','ignored') AND d.decision = $4)
          )
        ORDER BY bs.bundle_cm DESC NULLS LAST, bs.bundle_discount_pct
        "#,
    )
    .bind(run_id)
    .bind(filter.missing_costs)
    .bind(filter.focus_menu_item_id)
    .bind(filter.decision_status.as_ref())
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_bundle).collect())
}

pub async fn get_bundle_suggestion(
    pool: &PgPool,
    id: Uuid,
) -> Result<BundleSuggestionRecord, AppError> {
    let row: BundleSuggestionRow = sqlx::query_as::<_, BundleSuggestionRow>(
        r#"
        SELECT
            bs.id, bs.run_id, bs.branch_id,
            bs.focus_menu_item_id, bs.focus_size_label,
            bs.components_json,
            bs.bundle_list_price, bs.bundle_suggested_price, bs.bundle_discount_pct,
            bs.bundle_cost, bs.bundle_cm, bs.bundle_margin_pct,
            bs.association_json, bs.forecast_json,
            bs.guard_clips_json, bs.explanation, bs.missing_costs,
            bs.promoted_bundle_id, bs.created_at,
            d.id AS decision_id, d.decision AS decision, d.notes AS decision_notes, d.decided_by AS decision_decided_by, d.decided_at AS decision_decided_at
        FROM menu_advisor_bundle_suggestions bs
        LEFT JOIN LATERAL (
            SELECT id, decision, notes, decided_by, decided_at
            FROM menu_advisor_decisions
            WHERE suggestion_id = bs.id AND suggestion_kind = 'bundle'
            ORDER BY decided_at DESC LIMIT 1
        ) d ON TRUE
        WHERE bs.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("bundle suggestion not found".into()))?;
    Ok(row_to_bundle(row))
}

pub async fn set_bundle_promoted(
    pool: &PgPool,
    suggestion_id: Uuid,
    bundle_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        UPDATE menu_advisor_bundle_suggestions
        SET promoted_bundle_id = $2
        WHERE id = $1
        "#,
    )
    .bind(suggestion_id)
    .bind(bundle_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
// Removal scenario queries
// ═══════════════════════════════════════════════════════════════════

#[derive(sqlx::FromRow)]
struct RemovalScenarioRow {
    id: Uuid,
    run_id: Uuid,
    branch_id: Uuid,
    menu_item_id: Uuid,
    size_label: String,
    item_name: String,
    baseline_cm: f64,
    absorbed_by_json: JsonValue,
    complementary_losses_json: JsonValue,
    net_cm_change: f64,
    net_cm_change_lo: f64,
    net_cm_change_hi: f64,
    recommendation: String,
    explanation: String,
    created_at: DateTime<Utc>,
    decision_id: Option<Uuid>,
    decision: Option<String>,
    decision_notes: Option<String>,
    decision_decided_by: Option<Uuid>,
    decision_decided_at: Option<DateTime<Utc>>,
}

fn row_to_removal(r: RemovalScenarioRow) -> RemovalScenarioRecord {
    let absorbed_by = serde_json::from_value(r.absorbed_by_json).unwrap_or_default();
    let complementary_losses = serde_json::from_value(r.complementary_losses_json).unwrap_or_default();

    let scenario = RemovalScenario {
        key: ItemKey {
            menu_item_id: r.menu_item_id,
            size_label: r.size_label,
        },
        item_name: r.item_name,
        baseline_cm: r.baseline_cm,
        absorbed_by,
        complementary_losses,
        net_cm_change: r.net_cm_change,
        net_cm_change_lo: r.net_cm_change_lo,
        net_cm_change_hi: r.net_cm_change_hi,
        recommendation: parse_removal_rec(&r.recommendation),
        explanation: r.explanation,
    };

    let decision = r.decision_id.map(|did| DecisionRecord {
        id: did,
        suggestion_id: r.id,
        suggestion_kind: SuggestionKind::Removal,
        branch_id: r.branch_id,
        decision: r.decision.as_deref().and_then(Decision::parse).unwrap_or(Decision::Ignored),
        notes: r.decision_notes,
        decided_by: r.decision_decided_by.unwrap_or_default(),
        decided_at: r.decision_decided_at.unwrap_or(r.created_at),
    });

    RemovalScenarioRecord {
        id: r.id,
        run_id: r.run_id,
        branch_id: r.branch_id,
        created_at: r.created_at,
        decision,
        scenario,
    }
}

pub async fn list_removal_scenarios(
    pool: &PgPool,
    run_id: Uuid,
    filter: &RemovalScenarioFilter,
) -> Result<Vec<RemovalScenarioRecord>, AppError> {
    let rows: Vec<RemovalScenarioRow> = sqlx::query_as::<_, RemovalScenarioRow>(
        r#"
        SELECT
            rs.id, rs.run_id, rs.branch_id,
            rs.menu_item_id, rs.size_label, rs.item_name,
            rs.baseline_cm, rs.absorbed_by_json, rs.complementary_losses_json,
            rs.net_cm_change, rs.net_cm_change_lo, rs.net_cm_change_hi,
            rs.recommendation, rs.explanation, rs.created_at,
            d.id AS decision_id, d.decision AS decision, d.notes AS decision_notes, d.decided_by AS decision_decided_by, d.decided_at AS decision_decided_at
        FROM menu_advisor_removal_scenarios rs
        LEFT JOIN LATERAL (
            SELECT id, decision, notes, decided_by, decided_at
            FROM menu_advisor_decisions
            WHERE suggestion_id = rs.id AND suggestion_kind = 'removal'
            ORDER BY decided_at DESC LIMIT 1
        ) d ON TRUE
        WHERE rs.run_id = $1
          AND ($2::text IS NULL OR rs.recommendation = $2)
          AND (
                $3::text IS NULL
             OR ($3 = 'pending' AND d.id IS NULL)
             OR ($3 IN ('accepted','rejected','ignored') AND d.decision = $3)
          )
        ORDER BY rs.net_cm_change DESC
        "#,
    )
    .bind(run_id)
    .bind(filter.recommendation.as_ref())
    .bind(filter.decision_status.as_ref())
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_removal).collect())
}

pub async fn get_removal_scenario(
    pool: &PgPool,
    id: Uuid,
) -> Result<RemovalScenarioRecord, AppError> {
    let row: RemovalScenarioRow = sqlx::query_as::<_, RemovalScenarioRow>(
        r#"
        SELECT
            rs.id, rs.run_id, rs.branch_id,
            rs.menu_item_id, rs.size_label, rs.item_name,
            rs.baseline_cm, rs.absorbed_by_json, rs.complementary_losses_json,
            rs.net_cm_change, rs.net_cm_change_lo, rs.net_cm_change_hi,
            rs.recommendation, rs.explanation, rs.created_at,
            d.id AS decision_id, d.decision AS decision, d.notes AS decision_notes, d.decided_by AS decision_decided_by, d.decided_at AS decision_decided_at
        FROM menu_advisor_removal_scenarios rs
        LEFT JOIN LATERAL (
            SELECT id, decision, notes, decided_by, decided_at
            FROM menu_advisor_decisions
            WHERE suggestion_id = rs.id AND suggestion_kind = 'removal'
            ORDER BY decided_at DESC LIMIT 1
        ) d ON TRUE
        WHERE rs.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("removal scenario not found".into()))?;
    Ok(row_to_removal(row))
}

// ═══════════════════════════════════════════════════════════════════
// Decisions
// ═══════════════════════════════════════════════════════════════════

pub async fn record_decision(
    pool: &PgPool,
    suggestion_id: Uuid,
    suggestion_kind: SuggestionKind,
    branch_id: Uuid,
    decision: Decision,
    notes: Option<String>,
    decided_by: Uuid,
) -> Result<DecisionRecord, AppError> {
    let id = Uuid::new_v4();
    let decided_at: DateTime<Utc> = Utc::now();
    sqlx::query(
        r#"
        INSERT INTO menu_advisor_decisions (
            id, suggestion_id, suggestion_kind, branch_id, decision, notes,
            decided_by, decided_at
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(id)
    .bind(suggestion_id)
    .bind(suggestion_kind.as_str())
    .bind(branch_id)
    .bind(decision.as_str())
    .bind(&notes)
    .bind(decided_by)
    .bind(decided_at)
    .execute(pool)
    .await?;
    Ok(DecisionRecord {
        id,
        suggestion_id,
        suggestion_kind,
        branch_id,
        decision,
        notes,
        decided_by,
        decided_at,
    })
}

#[derive(sqlx::FromRow)]
struct DecisionRow {
    id: Uuid,
    suggestion_id: Uuid,
    suggestion_kind: String,
    branch_id: Uuid,
    decision: String,
    notes: Option<String>,
    decided_by: Uuid,
    decided_at: DateTime<Utc>,
}

pub async fn list_decisions(
    pool: &PgPool,
    branch_id: Uuid,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<DecisionRecord>, AppError> {
    let rows: Vec<DecisionRow> = match since {
        Some(t) => sqlx::query_as::<_, DecisionRow>(
            r#"
            SELECT id, suggestion_id, suggestion_kind, branch_id, decision,
                   notes, decided_by, decided_at
            FROM   menu_advisor_decisions
            WHERE  branch_id = $1 AND decided_at >= $2
            ORDER BY decided_at DESC
            "#,
        )
        .bind(branch_id)
        .bind(t)
        .fetch_all(pool)
        .await?,
        None => sqlx::query_as::<_, DecisionRow>(
            r#"
            SELECT id, suggestion_id, suggestion_kind, branch_id, decision,
                   notes, decided_by, decided_at
            FROM   menu_advisor_decisions
            WHERE  branch_id = $1
            ORDER BY decided_at DESC
            "#,
        )
        .bind(branch_id)
        .fetch_all(pool)
        .await?,
    };
    Ok(rows
        .into_iter()
        .map(|r| DecisionRecord {
            id: r.id,
            suggestion_id: r.suggestion_id,
            suggestion_kind: match r.suggestion_kind.as_str() {
                "bundle" => SuggestionKind::Bundle,
                "removal" => SuggestionKind::Removal,
                _ => SuggestionKind::Price,
            },
            branch_id: r.branch_id,
            decision: Decision::parse(&r.decision).unwrap_or(Decision::Ignored),
            notes: r.notes,
            decided_by: r.decided_by,
            decided_at: r.decided_at,
        })
        .collect())
}

// ═══════════════════════════════════════════════════════════════════
// Calibration (realized vs predicted for accepted price suggestions)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationSummary {
    pub branch_id: Uuid,
    pub since: Option<DateTime<Utc>>,
    pub points_cm: Vec<CalibrationPoint>,
    pub points_revenue: Vec<CalibrationPoint>,
    /// Fraction of accepted CM suggestions whose realized price landed
    /// within ±2% of the suggested price.
    pub cm_in_range_pct: Option<f64>,
    pub revenue_in_range_pct: Option<f64>,
}

#[derive(sqlx::FromRow)]
struct CalibRow {
    suggestion_id: Uuid,
    menu_item_id: Uuid,
    size_label: String,
    item_name: String,
    classification_mode: String,
    current_price: i64,
    suggested_price: Option<i64>,
    suggested_delta_pct: Option<f64>,
    decided_at: DateTime<Utc>,
    realized_price: Option<i64>,
    realized_at: Option<DateTime<Utc>>,
}

pub async fn get_calibration(
    pool: &PgPool,
    branch_id: Uuid,
    since: Option<DateTime<Utc>>,
) -> Result<CalibrationSummary, AppError> {
    // For each accepted price suggestion: find the next price epoch that
    // started AFTER decided_at and BEFORE now, that affects this item.
    let rows: Vec<CalibRow> = sqlx::query_as::<_, CalibRow>(
        r#"
        SELECT
            ps.id              AS suggestion_id,
            ps.menu_item_id,
            ps.size_label,
            ps.item_name,
            ps.classification_mode,
            ps.current_price,
            ps.suggested_price,
            ps.suggested_delta_pct,
            d.decided_at,
            (
                SELECT e.price::bigint
                FROM   menu_item_price_epochs e
                WHERE  e.menu_item_id = ps.menu_item_id
                  AND  (e.size_label IS NULL OR e.size_label = ps.size_label)
                  AND  e.effective_from > d.decided_at
                ORDER BY e.effective_from ASC
                LIMIT 1
            ) AS realized_price,
            (
                SELECT e.effective_from
                FROM   menu_item_price_epochs e
                WHERE  e.menu_item_id = ps.menu_item_id
                  AND  (e.size_label IS NULL OR e.size_label = ps.size_label)
                  AND  e.effective_from > d.decided_at
                ORDER BY e.effective_from ASC
                LIMIT 1
            ) AS realized_at
        FROM menu_advisor_price_suggestions ps
        JOIN menu_advisor_decisions d
          ON d.suggestion_id = ps.id
         AND d.suggestion_kind = 'price'
         AND d.decision = 'accepted'
        WHERE ps.branch_id = $1
          AND ($2::timestamptz IS NULL OR d.decided_at >= $2)
        "#,
    )
    .bind(branch_id)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let mut points_cm = Vec::new();
    let mut points_revenue = Vec::new();
    let mut cm_in_range = (0u32, 0u32);
    let mut rev_in_range = (0u32, 0u32);

    for r in rows {
        let (Some(suggested_price), Some(realized_price), Some(realized_at), Some(predicted_dp)) =
            (r.suggested_price, r.realized_price, r.realized_at, r.suggested_delta_pct)
        else {
            continue;
        };
        let realized_dp = (realized_price - r.current_price) as f64
            / (r.current_price as f64).max(1.0);
        let in_range = (realized_price as f64 - suggested_price as f64).abs()
            / (suggested_price as f64).max(1.0)
            <= 0.02;
        let point = CalibrationPoint {
            suggestion_id: r.suggestion_id,
            menu_item_id: r.menu_item_id,
            size_label: r.size_label,
            item_name: r.item_name,
            classification_mode: r.classification_mode.clone(),
            previous_price: r.current_price,
            suggested_price,
            realized_price,
            predicted_delta_pct: predicted_dp,
            realized_delta_pct: realized_dp,
            decided_at: r.decided_at,
            realized_at,
        };
        match r.classification_mode.as_str() {
            "cm" => {
                cm_in_range.1 += 1;
                if in_range { cm_in_range.0 += 1; }
                points_cm.push(point);
            }
            "revenue" => {
                rev_in_range.1 += 1;
                if in_range { rev_in_range.0 += 1; }
                points_revenue.push(point);
            }
            _ => {}
        }
    }

    let cm_in_range_pct = if cm_in_range.1 >= 10 {
        Some(cm_in_range.0 as f64 / cm_in_range.1 as f64)
    } else {
        None
    };
    let revenue_in_range_pct = if rev_in_range.1 >= 10 {
        Some(rev_in_range.0 as f64 / rev_in_range.1 as f64)
    } else {
        None
    };

    Ok(CalibrationSummary {
        branch_id,
        since,
        points_cm,
        points_revenue,
        cm_in_range_pct,
        revenue_in_range_pct,
    })
}

// ═══════════════════════════════════════════════════════════════════
// AppError helper
// ═══════════════════════════════════════════════════════════════════

// The existing AppError variants are matched optimistically:
//   - AppError::Internal_msg(String) for generic internal errors
//   - AppError::NotFound(String) for missing rows
// If your project's variants differ, adjust the names above.
//
// (See errors.rs in the existing codebase.)

#[allow(dead_code)]
trait _ErrorShim {}