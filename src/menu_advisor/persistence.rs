//! Persistence layer for the Menu Advisor.
//!
//! Suggestion storage is payload-first: each row's `payload` JSONB column IS
//! the serialized wire body (`dto::PriceSuggestion` / `BundleSuggestion` /
//! `RemovalScenario`), and the database mirrors every filterable field as a
//! STORED generated column — so what was inserted is byte-identical to what
//! the API returns, and the scalars can never drift from the payload.
//!
//! Tables (see migrations/20260612100000_menu_advisor_rebuild.sql):
//!   menu_advisor_runs, menu_advisor_price_suggestions,
//!   menu_advisor_bundle_suggestions, menu_advisor_removal_scenarios,
//!   menu_advisor_decisions

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::menu_advisor::dto::{
    AdvisorReport, AnalysisConfig, BundleSuggestion, BundleSuggestionFilter,
    BundleSuggestionRecord, CalibrationPoint, CalibrationSummary, Classification, CmQuadrant,
    Decision, DecisionRecord, ItemKey, ModeSummary, PersistedRun, PriceSuggestion,
    PriceSuggestionFilter, PriceSuggestionRecord, RemovalScenario, RemovalScenarioFilter,
    RemovalScenarioRecord, RevenueClass, RunStatus, SuggestionKind,
};

/// Unique-index name the create-run conflict mapping matches on.
const ONE_ACTIVE_RUN_CONSTRAINT: &str = "menu_advisor_runs_one_active_per_branch";

// ═══════════════════════════════════════════════════════════════════
// Runs
// ═══════════════════════════════════════════════════════════════════

#[derive(sqlx::FromRow)]
struct RunRow {
    id: Uuid,
    branch_id: Uuid,
    org_id: Uuid,
    status: String,
    config: JsonValue,
    error_message: Option<String>,
    items_total: i32,
    items_cm_tracked: i32,
    items_revenue_only: i32,
    items_insufficient: i32,
    window_days: f64,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

fn row_to_run(r: RunRow) -> PersistedRun {
    PersistedRun {
        id: r.id,
        branch_id: r.branch_id,
        org_id: r.org_id,
        status: RunStatus::parse(&r.status),
        // Configs evolve across engine versions; defaulting a stale shape is
        // the right behavior for a display-only echo.
        config: serde_json::from_value::<AnalysisConfig>(r.config).unwrap_or_default(),
        mode_summary: ModeSummary {
            items_total: r.items_total.max(0) as usize,
            items_cm_tracked: r.items_cm_tracked.max(0) as usize,
            items_revenue_only: r.items_revenue_only.max(0) as usize,
            items_insufficient: r.items_insufficient.max(0) as usize,
        },
        error_message: r.error_message,
        started_at: r.started_at,
        completed_at: r.completed_at,
        window_days: r.window_days,
    }
}

const RUN_COLUMNS: &str = "id, branch_id, org_id, status, config, error_message, \
     items_total, items_cm_tracked, items_revenue_only, items_insufficient, \
     window_days, started_at, completed_at";

pub async fn create_run(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    config: &AnalysisConfig,
) -> Result<Uuid, AppError> {
    let config_json = serde_json::to_value(config).map_err(|_| AppError::Internal)?;
    sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO menu_advisor_runs (branch_id, org_id, status, config, window_days)
        VALUES ($1, $2, 'in_progress', $3, $4)
        RETURNING id
        "#,
    )
    .bind(branch_id)
    .bind(org_id)
    .bind(config_json)
    .bind(config.analysis_window_days)
    .fetch_one(pool)
    .await
    .map_err(|e| {
        // The partial unique index closes the TOCTOU race between two
        // concurrent POSTs; map it to the same 409 the pre-check produces.
        if let sqlx::Error::Database(db) = &e
            && db.constraint() == Some(ONE_ACTIVE_RUN_CONSTRAINT)
        {
            return AppError::Conflict("A run is already in progress for this branch".into());
        }
        AppError::from(e)
    })
}

pub async fn mark_run_failed(
    pool: &PgPool,
    run_id: Uuid,
    error_message: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE menu_advisor_runs \
         SET status = 'failed', error_message = $2, completed_at = now() \
         WHERE id = $1",
    )
    .bind(run_id)
    .bind(error_message)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert all suggestions and flip the run to completed — atomically.
pub async fn save_completed_report(
    pool: &PgPool,
    run_id: Uuid,
    branch_id: Uuid,
    category_by_key: &HashMap<ItemKey, Option<Uuid>>,
    report: &AdvisorReport,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await?;

    for s in &report.price_suggestions {
        let payload = serde_json::to_value(s).map_err(|_| AppError::Internal)?;
        let category_id = category_by_key.get(&s.key).copied().flatten();
        sqlx::query(
            "INSERT INTO menu_advisor_price_suggestions \
                 (run_id, branch_id, category_id, payload) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(run_id)
        .bind(branch_id)
        .bind(category_id)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
    }

    for s in &report.bundle_suggestions {
        let payload = serde_json::to_value(s).map_err(|_| AppError::Internal)?;
        sqlx::query(
            "INSERT INTO menu_advisor_bundle_suggestions (run_id, branch_id, payload) \
             VALUES ($1, $2, $3)",
        )
        .bind(run_id)
        .bind(branch_id)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
    }

    for s in &report.removal_scenarios {
        let payload = serde_json::to_value(s).map_err(|_| AppError::Internal)?;
        sqlx::query(
            "INSERT INTO menu_advisor_removal_scenarios (run_id, branch_id, payload) \
             VALUES ($1, $2, $3)",
        )
        .bind(run_id)
        .bind(branch_id)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        "UPDATE menu_advisor_runs \
         SET status = 'completed', completed_at = now(), window_days = $2, \
             items_total = $3, items_cm_tracked = $4, \
             items_revenue_only = $5, items_insufficient = $6 \
         WHERE id = $1",
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

pub async fn get_run(pool: &PgPool, run_id: Uuid) -> Result<Option<PersistedRun>, AppError> {
    let row: Option<RunRow> = sqlx::query_as(&format!(
        "SELECT {RUN_COLUMNS} FROM menu_advisor_runs WHERE id = $1"
    ))
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_run))
}

pub async fn list_runs(
    pool: &PgPool,
    branch_id: Uuid,
    limit: i64,
    before: Option<DateTime<Utc>>,
) -> Result<Vec<PersistedRun>, AppError> {
    let rows: Vec<RunRow> = sqlx::query_as(&format!(
        "SELECT {RUN_COLUMNS} FROM menu_advisor_runs \
         WHERE branch_id = $1 \
           AND ($2::timestamptz IS NULL OR started_at < $2) \
         ORDER BY started_at DESC \
         LIMIT $3"
    ))
    .bind(branch_id)
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_run).collect())
}

pub async fn get_latest_run(
    pool: &PgPool,
    branch_id: Uuid,
    any_status: bool,
) -> Result<Option<PersistedRun>, AppError> {
    let sql = if any_status {
        format!(
            "SELECT {RUN_COLUMNS} FROM menu_advisor_runs \
             WHERE branch_id = $1 ORDER BY started_at DESC LIMIT 1"
        )
    } else {
        format!(
            "SELECT {RUN_COLUMNS} FROM menu_advisor_runs \
             WHERE branch_id = $1 AND status = 'completed' \
             ORDER BY completed_at DESC NULLS LAST LIMIT 1"
        )
    };
    let row: Option<RunRow> = sqlx::query_as(&sql)
        .bind(branch_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_run))
}

pub async fn get_in_progress_run(
    pool: &PgPool,
    branch_id: Uuid,
) -> Result<Option<PersistedRun>, AppError> {
    let row: Option<RunRow> = sqlx::query_as(&format!(
        "SELECT {RUN_COLUMNS} FROM menu_advisor_runs \
         WHERE branch_id = $1 AND status = 'in_progress' \
         ORDER BY started_at DESC LIMIT 1"
    ))
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_run))
}

// ═══════════════════════════════════════════════════════════════════
// Suggestion rows (shared shape: payload + latest decision via LATERAL)
// ═══════════════════════════════════════════════════════════════════

#[derive(sqlx::FromRow)]
struct SuggestionRow {
    id: Uuid,
    run_id: Uuid,
    branch_id: Uuid,
    created_at: DateTime<Utc>,
    payload: JsonValue,
    promoted_bundle_id: Option<Uuid>, // always NULL for non-bundle tables
    decision_id: Option<Uuid>,
    decision: Option<String>,
    decision_notes: Option<String>,
    decision_decided_by: Option<Uuid>,
    decision_decided_at: Option<DateTime<Utc>>,
}

impl SuggestionRow {
    fn decision_record(&self, kind: SuggestionKind) -> Result<Option<DecisionRecord>, AppError> {
        match (
            self.decision_id,
            &self.decision,
            self.decision_decided_by,
            self.decision_decided_at,
        ) {
            (Some(id), Some(decision), Some(decided_by), Some(decided_at)) => {
                let decision = Decision::parse(decision).ok_or_else(|| {
                    tracing::error!(decision_id = %id, "Invalid decision value in DB");
                    AppError::Internal
                })?;
                Ok(Some(DecisionRecord {
                    id,
                    suggestion_id: self.id,
                    suggestion_kind: kind,
                    branch_id: self.branch_id,
                    decision,
                    notes: self.decision_notes.clone(),
                    decided_by,
                    decided_at,
                }))
            }
            _ => Ok(None),
        }
    }

    fn payload_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, AppError> {
        serde_json::from_value(self.payload.clone()).map_err(|e| {
            tracing::error!(suggestion_id = %self.id, error = %e, "Corrupted advisor payload");
            AppError::Internal
        })
    }

    fn into_price_record(self) -> Result<PriceSuggestionRecord, AppError> {
        Ok(PriceSuggestionRecord {
            id: self.id,
            run_id: self.run_id,
            branch_id: self.branch_id,
            created_at: self.created_at,
            decision: self.decision_record(SuggestionKind::Price)?,
            suggestion: self.payload_as::<PriceSuggestion>()?,
        })
    }

    fn into_bundle_record(self) -> Result<BundleSuggestionRecord, AppError> {
        Ok(BundleSuggestionRecord {
            id: self.id,
            run_id: self.run_id,
            branch_id: self.branch_id,
            created_at: self.created_at,
            decision: self.decision_record(SuggestionKind::Bundle)?,
            promoted_bundle_id: self.promoted_bundle_id,
            suggestion: self.payload_as::<BundleSuggestion>()?,
        })
    }

    fn into_removal_record(self) -> Result<RemovalScenarioRecord, AppError> {
        Ok(RemovalScenarioRecord {
            id: self.id,
            run_id: self.run_id,
            branch_id: self.branch_id,
            created_at: self.created_at,
            decision: self.decision_record(SuggestionKind::Removal)?,
            scenario: self.payload_as::<RemovalScenario>()?,
        })
    }
}

/// SELECT prefix joining the latest decision; `{extra}` is the
/// promoted_bundle_id slot (real column for bundles, NULL elsewhere).
fn suggestion_select(table: &str, kind: &str, promoted_col: &str) -> String {
    format!(
        "SELECT s.id, s.run_id, s.branch_id, s.created_at, s.payload, \
                {promoted_col} AS promoted_bundle_id, \
                d.id AS decision_id, d.decision, d.notes AS decision_notes, \
                d.decided_by AS decision_decided_by, d.decided_at AS decision_decided_at \
         FROM {table} s \
         LEFT JOIN LATERAL ( \
             SELECT id, decision, notes, decided_by, decided_at \
             FROM menu_advisor_decisions \
             WHERE suggestion_id = s.id AND suggestion_kind = '{kind}' \
             ORDER BY decided_at DESC LIMIT 1 \
         ) d ON TRUE"
    )
}

/// `accepted | rejected | ignored` match the latest decision; `pending`
/// matches no-decision rows; any other value matches nothing.
const DECISION_STATUS_PREDICATE: &str = "($DS::text IS NULL \
     OR ($DS = 'pending' AND d.id IS NULL) \
     OR d.decision = $DS)";

fn decision_status_clause(param: usize) -> String {
    DECISION_STATUS_PREDICATE.replace("$DS", &format!("${param}"))
}

// ── Price suggestions ────────────────────────────────────────────────

pub async fn list_price_suggestions(
    pool: &PgPool,
    run_id: Uuid,
    filter: &PriceSuggestionFilter,
) -> Result<Vec<PriceSuggestionRecord>, AppError> {
    let sql = format!(
        "{} WHERE s.run_id = $1 \
           AND ($2::text IS NULL OR s.classification_mode = $2) \
           AND ($3::text IS NULL OR s.cm_quadrant = $3) \
           AND ($4::text IS NULL OR s.revenue_class = $4) \
           AND ($5::text IS NULL OR s.action = $5) \
           AND ($6::text IS NULL OR s.confidence = $6) \
           AND ($7::uuid IS NULL OR s.category_id = $7) \
           AND {} \
           AND ($9::text IS NULL OR s.item_name ILIKE '%' || $9 || '%') \
         ORDER BY s.popularity_share DESC, s.item_name",
        suggestion_select("menu_advisor_price_suggestions", "price", "NULL::uuid"),
        decision_status_clause(8),
    );
    let rows: Vec<SuggestionRow> = sqlx::query_as(&sql)
        .bind(run_id)
        .bind(&filter.classification_mode)
        .bind(&filter.cm_quadrant)
        .bind(&filter.revenue_class)
        .bind(&filter.action)
        .bind(&filter.confidence)
        .bind(filter.category_id)
        .bind(&filter.decision_status)
        .bind(&filter.search)
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(SuggestionRow::into_price_record)
        .collect()
}

pub async fn get_price_suggestion(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<PriceSuggestionRecord>, AppError> {
    let sql = format!(
        "{} WHERE s.id = $1",
        suggestion_select("menu_advisor_price_suggestions", "price", "NULL::uuid")
    );
    let row: Option<SuggestionRow> = sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    row.map(SuggestionRow::into_price_record).transpose()
}

/// Latest completed-run price suggestion for one SKU.
pub async fn get_latest_item_kpi(
    pool: &PgPool,
    branch_id: Uuid,
    menu_item_id: Uuid,
    size_label: &str,
) -> Result<Option<PriceSuggestionRecord>, AppError> {
    let sql = format!(
        "{}, menu_advisor_runs r \
         WHERE r.id = s.run_id AND r.status = 'completed' \
           AND s.branch_id = $1 AND s.menu_item_id = $2 AND s.size_label = $3 \
         ORDER BY r.completed_at DESC NULLS LAST LIMIT 1",
        suggestion_select("menu_advisor_price_suggestions", "price", "NULL::uuid")
    );
    let row: Option<SuggestionRow> = sqlx::query_as(&sql)
        .bind(branch_id)
        .bind(menu_item_id)
        .bind(size_label)
        .fetch_optional(pool)
        .await?;
    row.map(SuggestionRow::into_price_record).transpose()
}

// ── Bundle suggestions ───────────────────────────────────────────────

pub async fn list_bundle_suggestions(
    pool: &PgPool,
    run_id: Uuid,
    filter: &BundleSuggestionFilter,
) -> Result<Vec<BundleSuggestionRecord>, AppError> {
    let sql = format!(
        "{} WHERE s.run_id = $1 \
           AND ($2::boolean IS NULL OR s.missing_costs = $2) \
           AND ($3::uuid IS NULL OR s.focus_menu_item_id = $3) \
           AND {} \
         ORDER BY s.bundle_cm DESC NULLS LAST, s.bundle_discount_pct",
        suggestion_select(
            "menu_advisor_bundle_suggestions",
            "bundle",
            "s.promoted_bundle_id"
        ),
        decision_status_clause(4),
    );
    let rows: Vec<SuggestionRow> = sqlx::query_as(&sql)
        .bind(run_id)
        .bind(filter.missing_costs)
        .bind(filter.focus_menu_item_id)
        .bind(&filter.decision_status)
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(SuggestionRow::into_bundle_record)
        .collect()
}

pub async fn get_bundle_suggestion(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<BundleSuggestionRecord>, AppError> {
    let sql = format!(
        "{} WHERE s.id = $1",
        suggestion_select(
            "menu_advisor_bundle_suggestions",
            "bundle",
            "s.promoted_bundle_id"
        )
    );
    let row: Option<SuggestionRow> = sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    row.map(SuggestionRow::into_bundle_record).transpose()
}

pub async fn set_bundle_promoted(
    pool: &PgPool,
    suggestion_id: Uuid,
    bundle_id: Uuid,
) -> Result<(), AppError> {
    let result = sqlx::query(
        "UPDATE menu_advisor_bundle_suggestions SET promoted_bundle_id = $2 WHERE id = $1",
    )
    .bind(suggestion_id)
    .bind(bundle_id)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Bundle suggestion not found".into()));
    }
    Ok(())
}

// ── Removal scenarios ────────────────────────────────────────────────

pub async fn list_removal_scenarios(
    pool: &PgPool,
    run_id: Uuid,
    filter: &RemovalScenarioFilter,
) -> Result<Vec<RemovalScenarioRecord>, AppError> {
    let sql = format!(
        "{} WHERE s.run_id = $1 \
           AND ($2::text IS NULL OR s.recommendation = $2) \
           AND {} \
         ORDER BY s.net_cm_change DESC",
        suggestion_select("menu_advisor_removal_scenarios", "removal", "NULL::uuid"),
        decision_status_clause(3),
    );
    let rows: Vec<SuggestionRow> = sqlx::query_as(&sql)
        .bind(run_id)
        .bind(&filter.recommendation)
        .bind(&filter.decision_status)
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(SuggestionRow::into_removal_record)
        .collect()
}

pub async fn get_removal_scenario(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<RemovalScenarioRecord>, AppError> {
    let sql = format!(
        "{} WHERE s.id = $1",
        suggestion_select("menu_advisor_removal_scenarios", "removal", "NULL::uuid")
    );
    let row: Option<SuggestionRow> = sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    row.map(SuggestionRow::into_removal_record).transpose()
}

// ═══════════════════════════════════════════════════════════════════
// Decisions
// ═══════════════════════════════════════════════════════════════════

/// Branch a suggestion belongs to, looked up in the table its kind names.
/// `None` ⟺ no such suggestion.
pub async fn get_suggestion_branch(
    pool: &PgPool,
    kind: SuggestionKind,
    suggestion_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    let table = match kind {
        SuggestionKind::Price => "menu_advisor_price_suggestions",
        SuggestionKind::Bundle => "menu_advisor_bundle_suggestions",
        SuggestionKind::Removal => "menu_advisor_removal_scenarios",
    };
    let branch: Option<Uuid> =
        sqlx::query_scalar(&format!("SELECT branch_id FROM {table} WHERE id = $1"))
            .bind(suggestion_id)
            .fetch_optional(pool)
            .await?;
    Ok(branch)
}

pub async fn record_decision(
    pool: &PgPool,
    suggestion_id: Uuid,
    suggestion_kind: SuggestionKind,
    branch_id: Uuid,
    decision: Decision,
    notes: Option<String>,
    decided_by: Uuid,
) -> Result<DecisionRecord, AppError> {
    let (id, decided_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO menu_advisor_decisions \
             (suggestion_id, suggestion_kind, branch_id, decision, notes, decided_by) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING id, decided_at",
    )
    .bind(suggestion_id)
    .bind(suggestion_kind.as_str())
    .bind(branch_id)
    .bind(decision.as_str())
    .bind(&notes)
    .bind(decided_by)
    .fetch_one(pool)
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
    let rows: Vec<DecisionRow> = sqlx::query_as(
        "SELECT id, suggestion_id, suggestion_kind, branch_id, decision, notes, \
                decided_by, decided_at \
         FROM menu_advisor_decisions \
         WHERE branch_id = $1 \
           AND ($2::timestamptz IS NULL OR decided_at >= $2) \
         ORDER BY decided_at DESC",
    )
    .bind(branch_id)
    .bind(since)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            // CHECK constraints make invalid values impossible; fail loudly
            // rather than guessing if one ever appears.
            let suggestion_kind =
                SuggestionKind::parse(&r.suggestion_kind).ok_or(AppError::Internal)?;
            let decision = Decision::parse(&r.decision).ok_or(AppError::Internal)?;
            Ok(DecisionRecord {
                id: r.id,
                suggestion_id: r.suggestion_id,
                suggestion_kind,
                branch_id: r.branch_id,
                decision,
                notes: r.notes,
                decided_by: r.decided_by,
                decided_at: r.decided_at,
            })
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════════════
// Hysteresis input: previous run's classifications
// ═══════════════════════════════════════════════════════════════════

#[derive(sqlx::FromRow)]
struct ClassificationRow {
    menu_item_id: Uuid,
    size_label: String,
    classification_mode: String,
    cm_quadrant: Option<String>,
    revenue_class: Option<String>,
}

/// Classifications from the latest COMPLETED run, for hysteresis. `None`
/// when the branch has no completed run yet.
pub async fn load_latest_classifications(
    pool: &PgPool,
    branch_id: Uuid,
) -> Result<Option<HashMap<ItemKey, Classification>>, AppError> {
    let rows: Vec<ClassificationRow> = sqlx::query_as(
        "SELECT ps.menu_item_id, ps.size_label, ps.classification_mode, \
                ps.cm_quadrant, ps.revenue_class \
         FROM menu_advisor_price_suggestions ps \
         WHERE ps.run_id = ( \
             SELECT id FROM menu_advisor_runs \
             WHERE branch_id = $1 AND status = 'completed' \
             ORDER BY completed_at DESC NULLS LAST LIMIT 1 \
         )",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut map = HashMap::new();
    for r in rows {
        let classification = match r.classification_mode.as_str() {
            "cm" => match r.cm_quadrant.as_deref() {
                Some("star") => Classification::Cm {
                    quadrant: CmQuadrant::Star,
                },
                Some("plowhorse") => Classification::Cm {
                    quadrant: CmQuadrant::Plowhorse,
                },
                Some("puzzle") => Classification::Cm {
                    quadrant: CmQuadrant::Puzzle,
                },
                Some("dog") => Classification::Cm {
                    quadrant: CmQuadrant::Dog,
                },
                _ => continue,
            },
            "revenue" => match r.revenue_class.as_deref() {
                Some("hero") => Classification::Revenue {
                    class: RevenueClass::Hero,
                },
                Some("steady") => Classification::Revenue {
                    class: RevenueClass::Steady,
                },
                Some("slow") => Classification::Revenue {
                    class: RevenueClass::Slow,
                },
                Some("quiet") => Classification::Revenue {
                    class: RevenueClass::Quiet,
                },
                _ => continue,
            },
            _ => Classification::Insufficient,
        };
        map.insert(
            ItemKey {
                menu_item_id: r.menu_item_id,
                size_label: r.size_label,
            },
            classification,
        );
    }
    Ok(Some(map))
}

// ═══════════════════════════════════════════════════════════════════
// Calibration (realized vs predicted for accepted price suggestions)
// ═══════════════════════════════════════════════════════════════════

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
    // For each accepted price suggestion: the first price epoch that started
    // AFTER decided_at and affects this SKU is the realized price.
    let rows: Vec<CalibRow> = sqlx::query_as(
        r#"
        SELECT
            ps.id AS suggestion_id,
            ps.menu_item_id,
            ps.size_label,
            ps.item_name,
            ps.classification_mode,
            ps.current_price,
            ps.suggested_price,
            ps.suggested_delta_pct,
            d.decided_at,
            e.price AS realized_price,
            e.effective_from AS realized_at
        FROM menu_advisor_price_suggestions ps
        JOIN menu_advisor_decisions d
          ON d.suggestion_id = ps.id
         AND d.suggestion_kind = 'price'
         AND d.decision = 'accepted'
        LEFT JOIN LATERAL (
            SELECT e.price::bigint AS price, e.effective_from
            FROM menu_item_price_epochs e
            WHERE e.menu_item_id = ps.menu_item_id
              AND COALESCE(e.size_label::text, 'one_size') = ps.size_label
              AND e.effective_from > d.decided_at
            ORDER BY e.effective_from ASC
            LIMIT 1
        ) e ON TRUE
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
        let (Some(suggested_price), Some(realized_price), Some(realized_at), Some(predicted_dp)) = (
            r.suggested_price,
            r.realized_price,
            r.realized_at,
            r.suggested_delta_pct,
        ) else {
            continue;
        };
        let realized_dp =
            (realized_price - r.current_price) as f64 / (r.current_price as f64).max(1.0);
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
                if in_range {
                    cm_in_range.0 += 1;
                }
                points_cm.push(point);
            }
            "revenue" => {
                rev_in_range.1 += 1;
                if in_range {
                    rev_in_range.0 += 1;
                }
                points_revenue.push(point);
            }
            _ => {}
        }
    }

    // Percentages only once the sample is meaningful.
    let pct = |hits: u32, n: u32| (n >= 10).then(|| hits as f64 / n as f64);

    Ok(CalibrationSummary {
        branch_id,
        since,
        points_cm,
        points_revenue,
        cm_in_range_pct: pct(cm_in_range.0, cm_in_range.1),
        revenue_in_range_pct: pct(rev_in_range.0, rev_in_range.1),
    })
}
