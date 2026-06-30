//! One-time normalization of existing recipe quantities to each linked
//! ingredient's base stock unit.
//!
//! Historically recipe `ingredient_unit` was free text and could differ from
//! the ingredient's `inventory_unit` (e.g. a recipe in "g" for an ingredient
//! stocked in "kg"), which made the sale deduction and cost rollups wrong.
//! Going forward, recipe-save handlers normalize on write; this backfill fixes
//! pre-existing rows: convertible mismatches are rewritten (quantity ×factor,
//! unit set to base); cross-family / unknown-unit mismatches are left untouched
//! and reported for manual correction.
//!
//! Historical order snapshots are deliberately NOT touched here — run the
//! existing `backfill-cost-snapshots` afterwards to reprice history if desired.

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

#[derive(Clone, Copy)]
pub enum BackfillScope {
    Org(Uuid),
    Branch(Uuid),
}

pub struct Unconvertible {
    pub table: String,
    pub row_id: Uuid,
    pub ingredient_name: String,
    pub recipe_unit: String,
    pub base_unit: String,
}

#[derive(Default)]
pub struct RecipeUnitSummary {
    pub scanned: i64,
    pub already_base: i64,
    pub converted: i64,
    pub unconvertible: Vec<Unconvertible>,
}

async fn resolve_org(pool: &PgPool, scope: BackfillScope) -> Result<Uuid, AppError> {
    match scope {
        BackfillScope::Org(o) => Ok(o),
        BackfillScope::Branch(b) => {
            sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
                .bind(b)
                .fetch_optional(pool)
                .await?
                .flatten()
                .ok_or_else(|| AppError::NotFound("Branch not found".into()))
        }
    }
}

pub async fn backfill_recipe_units(
    pool: &PgPool,
    scope: BackfillScope,
    dry_run: bool,
) -> Result<RecipeUnitSummary, AppError> {
    let org_id = resolve_org(pool, scope).await?;
    let mut tx = pool.begin().await?;
    let mut summary = RecipeUnitSummary::default();

    // (display name, candidate SELECT, per-row UPDATE). Each SELECT returns
    // (id, recipe_unit, base_unit, quantity_used::float8, ingredient_name).
    let tables: [(&str, &str, &str); 3] = [
        (
            "menu_item_recipes",
            "SELECT r.id, r.ingredient_unit, oi.unit::text, r.quantity_used::float8, r.ingredient_name \
             FROM menu_item_recipes r JOIN org_ingredients oi ON oi.id = r.org_ingredient_id \
             WHERE oi.org_id = $1",
            "UPDATE menu_item_recipes SET quantity_used = $1, ingredient_unit = $2 WHERE id = $3",
        ),
        (
            "addon_item_ingredients",
            "SELECT r.id, r.ingredient_unit, oi.unit::text, r.quantity_used::float8, r.ingredient_name \
             FROM addon_item_ingredients r JOIN org_ingredients oi ON oi.id = r.org_ingredient_id \
             WHERE oi.org_id = $1",
            "UPDATE addon_item_ingredients SET quantity_used = $1, ingredient_unit = $2 WHERE id = $3",
        ),
        (
            "menu_item_optional_fields",
            "SELECT r.id, r.ingredient_unit, oi.unit::text, r.quantity_used::float8, r.ingredient_name \
             FROM menu_item_optional_fields r JOIN org_ingredients oi ON oi.id = r.org_ingredient_id \
             WHERE oi.org_id = $1 AND r.quantity_used IS NOT NULL AND r.ingredient_unit IS NOT NULL",
            "UPDATE menu_item_optional_fields SET quantity_used = $1, ingredient_unit = $2 WHERE id = $3",
        ),
    ];

    for (name, select_sql, update_sql) in tables {
        let rows: Vec<(Uuid, String, String, f64, String)> = sqlx::query_as(select_sql)
            .bind(org_id)
            .fetch_all(&mut *tx)
            .await?;

        for (id, recipe_unit, base_unit, qty, ing_name) in rows {
            summary.scanned += 1;
            if recipe_unit.eq_ignore_ascii_case(&base_unit) {
                summary.already_base += 1;
                continue;
            }
            match crate::units::convert(qty, &recipe_unit, &base_unit) {
                Ok(nq) => {
                    sqlx::query(update_sql)
                        .bind(nq)
                        .bind(&base_unit)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    summary.converted += 1;
                }
                Err(_) => summary.unconvertible.push(Unconvertible {
                    table: name.to_string(),
                    row_id: id,
                    ingredient_name: ing_name,
                    recipe_unit,
                    base_unit,
                }),
            }
        }
    }

    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(summary)
}
