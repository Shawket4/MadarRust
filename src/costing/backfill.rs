//! Operator-only backfill: reprice historical order cost snapshots as if
//! each line were ordered TODAY — current recipes at current ingredient
//! costs.
//!
//! Order cost snapshots (`order_items.unit_cost`/`line_cost`, addon /
//! optional / bundle-component costs) are immutable by design — they record
//! what things cost when the order was placed. This module exists for the
//! deliberate exception: after correcting recipes and catalog costs, an
//! operator can rewrite history so COGS reports and the Menu Advisor
//! reflect the fixed numbers. It is wired to the `backfill-cost-snapshots`
//! binary only — never to an HTTP route.
//!
//! Pricing model (mirrors the menu-engineering report's `cost_basis=current`
//! and the live pipeline's cost composition):
//!   - recipe scope: the SKU's CURRENT `menu_item_recipes` rollup at
//!     `COALESCE(open ingredient_cost_history epoch, org_ingredients
//!     .cost_per_unit)` — piastres. `unit_cost` = that rollup; NULL for
//!     bundle lines and whenever the rollup is unresolvable.
//!   - addon rows: the addon item's CURRENT `addon_item_ingredients`
//!     rollup × addon quantity × line quantity.
//!   - optional rows: stored `quantity_deducted` (per parent unit) × the
//!     ingredient's current cost; rows without a linked ingredient keep
//!     their value (genuinely zero marginal cost).
//!   - bundle components: each component item's current recipe rollup ×
//!     component quantity × line quantity.
//!   - `line_cost` = recipe×qty + addons + optionals×qty (bundle lines:
//!     Σ components); NULL — and `cost_missing = true` — when ANY
//!     contributing rollup is unresolvable (never-entered ingredient cost,
//!     unlinked recipe row, recipe/size that no longer exists, addon with
//!     no ingredient links).
//!
//! NOTE: this deliberately re-derives consumption from TODAY's recipes —
//! historical swaps/customizations recorded in `deductions_snapshot` are
//! not consulted (that JSONB stays untouched as the sale-time audit
//! record). "What would this history cost at today's menu" is the question
//! this tool answers.

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

#[derive(Debug, Clone, Copy)]
pub enum BackfillScope {
    Org(Uuid),
    Branch(Uuid),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BackfillSummary {
    pub branches: usize,
    pub order_lines_in_scope: i64,
    pub order_lines_updated: u64,
    pub addon_rows_updated: u64,
    pub optional_rows_updated: u64,
    pub bundle_component_rows_updated: u64,
    /// Σ order_items.line_cost over the scope (piastres).
    pub line_cost_total_before: i64,
    pub line_cost_total_after: i64,
    pub lines_cost_missing_before: i64,
    pub lines_cost_missing_after: i64,
    pub dry_run: bool,
}

/// Current piastre cost per ingredient: open history epoch first, catalog
/// value second — identical to the costing-service resolution. NULL ⟺
/// never entered.
const CURRENT_COSTS_CTE: &str = r#"
    current_costs AS (
        SELECT i.id,
               COALESCE(h.cost_per_unit, i.cost_per_unit) AS cost
        FROM org_ingredients i
        LEFT JOIN LATERAL (
            SELECT cost_per_unit FROM ingredient_cost_history
            WHERE org_ingredient_id = i.id AND effective_until IS NULL
            ORDER BY effective_from DESC LIMIT 1
        ) h ON TRUE
    )
"#;

/// Recipe rollup LATERAL body for one SKU `(item_expr, size_expr)`:
/// NULL when the SKU has no recipe rows or any ingredient is unresolvable.
fn recipe_rollup(item_expr: &str, size_expr: &str) -> String {
    format!(
        r#"
        SELECT CASE
            WHEN COUNT(r.id) = 0 THEN NULL
            WHEN bool_or(r.org_ingredient_id IS NULL OR cc.cost IS NULL) THEN NULL
            ELSE SUM(r.quantity_used * cc.cost)
        END AS rollup
        FROM menu_item_recipes r
        LEFT JOIN current_costs cc ON cc.id = r.org_ingredient_id
        WHERE r.menu_item_id = {item_expr}
          AND COALESCE(r.size_label::text, 'one_size')
              = COALESCE({size_expr}::text, 'one_size')
        "#
    )
}

pub async fn backfill_cost_snapshots(
    pool: &PgPool,
    scope: BackfillScope,
    dry_run: bool,
) -> Result<BackfillSummary, AppError> {
    let branch_ids: Vec<Uuid> = match scope {
        BackfillScope::Org(org_id) => {
            let ids: Vec<Uuid> =
                sqlx::query_scalar("SELECT id FROM branches WHERE org_id = $1")
                    .bind(org_id)
                    .fetch_all(pool)
                    .await?;
            if ids.is_empty() {
                return Err(AppError::NotFound(format!(
                    "No branches found for org {org_id} (does the org exist?)"
                )));
            }
            ids
        }
        BackfillScope::Branch(branch_id) => {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM branches WHERE id = $1)")
                    .bind(branch_id)
                    .fetch_one(pool)
                    .await?;
            if !exists {
                return Err(AppError::NotFound(format!("Branch {branch_id} not found")));
            }
            vec![branch_id]
        }
    };

    let mut tx = pool.begin().await?;

    let (lines_in_scope, total_before, missing_before) =
        scope_stats(&mut tx, &branch_ids).await?;

    // ── 1. Children first: addon / optional / bundle-component rows ─────
    // (their recomputed values also feed the parent line totals via the
    // same rollup expressions, recomputed independently below).

    let addons_sql = format!(
        r#"
        WITH {CURRENT_COSTS_CTE}
        UPDATE order_item_addons a SET line_cost = calc.new_cost
        FROM (
            SELECT a.id,
                   CASE WHEN ar.rollup IS NULL THEN NULL
                        ELSE round(ar.rollup * a.quantity * oi.quantity)::bigint
                   END AS new_cost
            FROM order_item_addons a
            JOIN order_items oi ON oi.id = a.order_item_id
            JOIN orders o ON o.id = oi.order_id
            LEFT JOIN LATERAL (
                SELECT CASE
                    WHEN COUNT(ai.id) = 0 THEN NULL
                    WHEN bool_or(ai.org_ingredient_id IS NULL OR cc.cost IS NULL) THEN NULL
                    ELSE SUM(ai.quantity_used * cc.cost)
                END AS rollup
                FROM addon_item_ingredients ai
                LEFT JOIN current_costs cc ON cc.id = ai.org_ingredient_id
                WHERE ai.addon_item_id = a.addon_item_id
            ) ar ON TRUE
            WHERE o.branch_id = ANY($1)
        ) calc
        WHERE a.id = calc.id
        "#
    );
    let addon_rows_updated = sqlx::query(&addons_sql)
        .bind(&branch_ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    // Optionals: stored quantity_deducted (per parent unit) × current cost.
    // Rows without a linked ingredient keep their value (genuinely free);
    // a linked ingredient that is unresolvable today (NULL cost or deleted)
    // makes the row NULL → the parent line goes cost-missing.
    let optionals_sql = format!(
        r#"
        WITH {CURRENT_COSTS_CTE}
        UPDATE order_item_optionals op
        SET cost = (
            SELECT round(op.quantity_deducted * cc.cost)::bigint
            FROM current_costs cc
            WHERE cc.id = op.org_ingredient_id AND cc.cost IS NOT NULL
        )
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        WHERE oi.id = op.order_item_id
          AND o.branch_id = ANY($1)
          AND op.org_ingredient_id IS NOT NULL
          AND op.quantity_deducted IS NOT NULL
        "#
    );
    let optional_rows_updated = sqlx::query(&optionals_sql)
        .bind(&branch_ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let components_sql = format!(
        r#"
        WITH {CURRENT_COSTS_CTE}
        UPDATE order_line_bundle_components c SET line_cost = calc.new_cost
        FROM (
            SELECT c.order_line_id, c.item_id,
                   CASE WHEN cr.rollup IS NULL THEN NULL
                        ELSE round(cr.rollup * c.quantity * oi.quantity)::bigint
                   END AS new_cost
            FROM order_line_bundle_components c
            JOIN order_items oi ON oi.id = c.order_line_id
            JOIN orders o ON o.id = oi.order_id
            LEFT JOIN LATERAL ({component_rollup}) cr ON TRUE
            WHERE o.branch_id = ANY($1)
        ) calc
        WHERE c.order_line_id = calc.order_line_id AND c.item_id = calc.item_id
        "#,
        component_rollup = recipe_rollup("c.item_id", "c.size_label")
    );
    let bundle_component_rows_updated = sqlx::query(&components_sql)
        .bind(&branch_ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    // ── 2. Parent lines: unit_cost / line_cost / cost_missing ───────────
    //
    // Non-bundle: unit_cost = recipe rollup; line_cost = recipe×qty +
    // Σ addons + Σ optionals×qty. Bundle: unit_cost NULL; line_cost =
    // Σ component costs. Any unresolvable contribution ⟹ cost_missing,
    // line_cost NULL.
    let order_items_sql = format!(
        r#"
        WITH {CURRENT_COSTS_CTE}
        UPDATE order_items oi SET
            cost_missing = calc.cost_missing,
            line_cost    = calc.line_cost,
            unit_cost    = calc.unit_cost
        FROM (
            SELECT
                oi.id,
                CASE WHEN oi.bundle_id IS NOT NULL THEN
                    comp.n = 0 OR comp.missing
                ELSE
                    rr.rollup IS NULL OR ad.missing OR op.missing
                END AS cost_missing,
                CASE
                    WHEN oi.bundle_id IS NOT NULL THEN
                        CASE WHEN comp.n = 0 OR comp.missing THEN NULL
                             ELSE comp.total END
                    WHEN rr.rollup IS NULL OR ad.missing OR op.missing THEN NULL
                    ELSE round(rr.rollup * oi.quantity)::bigint
                         + COALESCE(ad.total, 0)
                         + COALESCE(op.total, 0) * oi.quantity
                END AS line_cost,
                CASE WHEN oi.bundle_id IS NOT NULL THEN NULL
                     ELSE round(rr.rollup)::bigint END AS unit_cost
            FROM order_items oi
            JOIN orders o ON o.id = oi.order_id
            LEFT JOIN LATERAL ({line_recipe_rollup}) rr ON TRUE
            LEFT JOIN LATERAL (
                SELECT COALESCE(bool_or(a.line_cost IS NULL), FALSE) AS missing,
                       SUM(a.line_cost)::bigint AS total
                FROM order_item_addons a WHERE a.order_item_id = oi.id
            ) ad ON TRUE
            LEFT JOIN LATERAL (
                SELECT COALESCE(bool_or(p.cost IS NULL), FALSE) AS missing,
                       SUM(p.cost)::bigint AS total
                FROM order_item_optionals p WHERE p.order_item_id = oi.id
            ) op ON TRUE
            LEFT JOIN LATERAL (
                SELECT COUNT(*) AS n,
                       COALESCE(bool_or(c.line_cost IS NULL), FALSE) AS missing,
                       SUM(c.line_cost)::bigint AS total
                FROM order_line_bundle_components c WHERE c.order_line_id = oi.id
            ) comp ON TRUE
            WHERE o.branch_id = ANY($1)
        ) calc
        WHERE oi.id = calc.id
        "#,
        line_recipe_rollup = recipe_rollup("oi.menu_item_id", "oi.size_label")
    );
    let order_lines_updated = sqlx::query(&order_items_sql)
        .bind(&branch_ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let (_, total_after, missing_after) = scope_stats(&mut tx, &branch_ids).await?;

    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }

    Ok(BackfillSummary {
        branches: branch_ids.len(),
        order_lines_in_scope: lines_in_scope,
        order_lines_updated,
        addon_rows_updated,
        optional_rows_updated,
        bundle_component_rows_updated,
        line_cost_total_before: total_before,
        line_cost_total_after: total_after,
        lines_cost_missing_before: missing_before,
        lines_cost_missing_after: missing_after,
        dry_run,
    })
}

async fn scope_stats(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    branch_ids: &[Uuid],
) -> Result<(i64, i64, i64), AppError> {
    let row: (i64, Option<i64>, i64) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, \
                SUM(oi.line_cost)::bigint, \
                COUNT(*) FILTER (WHERE oi.cost_missing)::bigint \
         FROM order_items oi \
         JOIN orders o ON o.id = oi.order_id \
         WHERE o.branch_id = ANY($1)",
    )
    .bind(branch_ids)
    .fetch_one(&mut **tx)
    .await?;
    Ok((row.0, row.1.unwrap_or(0), row.2))
}
