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

    // Every recipe line of the org's ingredients. This used to walk the three
    // legacy recipe tables, which are read-only views over recipe_lines on
    // every database since DB Phase 2.4 (it could no longer write them): the
    // one table covers item sizes, add-ons and optional fields alike.
    let rows = sqlx::query!(
        r#"SELECT r.id, r.unit, oi.unit::text AS "base_unit!", r.quantity::float8 AS "qty!",
                  oi.name AS ingredient_name
             FROM recipe_lines r JOIN org_ingredients oi ON oi.id = r.ingredient_id
            WHERE oi.org_id = $1"#,
        org_id
    )
    .fetch_all(&mut *tx)
    .await?;

    for r in rows {
        summary.scanned += 1;
        if r.unit.eq_ignore_ascii_case(&r.base_unit) {
            summary.already_base += 1;
            continue;
        }
        match crate::units::convert(r.qty, &r.unit, &r.base_unit) {
            Ok(nq) => {
                sqlx::query!(
                    "UPDATE recipe_lines SET quantity = ($1::float8)::numeric, unit = $2 WHERE id = $3",
                    nq,
                    &r.base_unit,
                    r.id
                )
                .execute(&mut *tx)
                .await?;
                summary.converted += 1;
            }
            Err(_) => summary.unconvertible.push(Unconvertible {
                table: "recipe_lines".to_string(),
                row_id: r.id,
                ingredient_name: r.ingredient_name,
                recipe_unit: r.unit,
                base_unit: r.base_unit,
            }),
        }
    }

    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(summary)
}
