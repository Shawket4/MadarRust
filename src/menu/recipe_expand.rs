//! Write-time expansion of recipe BASES, PACKAGING RULES and LINKED COPIES into
//! ordinary `recipe_lines` rows (menu modeling B7/B8/B10).
//!
//! Nothing downstream knows these concepts exist: the resolver, the legacy shim
//! views, the POS changefeed and every till in the field read plain
//! `recipe_lines (owner_type='item_size')`. `recipe_lines.source` only records which
//! rows an expansion owns, so the next expansion can replace exactly those.
//!
//! ## What a size's recipe is
//!
//! For an item that is NOT a linked copy, per size:
//!
//! 1. **own** lines (`source` NULL or `'own'`) — typed by the owner; never touched
//!    here.
//! 2. **base** lines (`'base'`) — from `menu_item_sizes.base_id` when that base is
//!    active and not deleted. A base line whose `size_label` equals the size's label
//!    wins over a NULL-label line for the same ingredient; lines labelled for another
//!    size are ignored. An ingredient the size already has as an own line is skipped
//!    (own overrides base).
//! 3. **rule** lines (`'rule'`) — from the single most specific active
//!    `packaging_rules` match (see [`best_rule_for`]). Ingredients already present as
//!    own or base lines are skipped.
//!
//! For a **linked copy** (`menu_items.recipe_source_item_id` set) every line of each
//! size is a copy (`'linked'`) of the source's size with the same label, whatever the
//! source line's own origin. A copy size whose label the source lacks gets no lines.
//!
//! Expansion diffs against what is stored (update in place, insert, delete) so an
//! unchanged recipe causes no writes and no changefeed noise, and line ids stay stable.
//! [`rebuild_item`] always re-copies onto the item's linked copies afterwards.

use rust_decimal::Decimal;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::errors::AppError;

/// One desired line for a size.
#[derive(Debug, Clone, PartialEq)]
struct Want {
    ingredient_id: Uuid,
    quantity: Decimal,
    unit: String,
    source: &'static str,
}

/// Outcome of an expansion.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExpandStats {
    /// Sizes whose stored lines were changed (the item's own + its linked copies).
    pub sizes_changed: usize,
    /// Sizes examined.
    pub sizes_seen: usize,
}

impl std::ops::AddAssign for ExpandStats {
    fn add_assign(&mut self, o: Self) {
        self.sizes_changed += o.sizes_changed;
        self.sizes_seen += o.sizes_seen;
    }
}

/// Re-expand every size of `item_id`, then re-copy onto its linked copies.
pub async fn rebuild_item(conn: &mut PgConnection, item_id: Uuid) -> Result<ExpandStats, AppError> {
    let mut stats = rebuild_one(conn, item_id).await?;
    let copies: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM menu_items WHERE recipe_source_item_id = $1 AND deleted_at IS NULL ORDER BY id",
    )
    .bind(item_id)
    .fetch_all(&mut *conn)
    .await?;
    for c in copies {
        stats += rebuild_one(conn, c).await?;
    }
    Ok(stats)
}

/// Re-expand a set of items (deduplicated), each followed by its copies.
pub async fn rebuild_items(
    conn: &mut PgConnection,
    item_ids: &[Uuid],
) -> Result<ExpandStats, AppError> {
    let mut ids = item_ids.to_vec();
    ids.sort();
    ids.dedup();
    let mut stats = ExpandStats::default();
    for id in ids {
        stats += rebuild_item(conn, id).await?;
    }
    Ok(stats)
}

/// The id of the most specific active packaging rule for one size, if any.
///
/// A rule matches when every match field it sets equals the size's item / the item's
/// menu category / the size label. Ranking: item beats category beats label, each
/// field set counting in that order, so
/// `item+label > item > category+label > category > label`; then `sort`, then age.
pub async fn best_rule_for(
    conn: &mut PgConnection,
    org_id: Uuid,
    item_id: Uuid,
    category_id: Option<Uuid>,
    size_label: &str,
) -> Result<Option<Uuid>, AppError> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM packaging_rules \
         WHERE org_id = $1 AND is_active \
           AND (match_item_id IS NULL OR match_item_id = $2) \
           AND (match_category_id IS NULL OR match_category_id = $3) \
           AND (match_size_label IS NULL OR match_size_label = $4) \
         ORDER BY (match_item_id IS NOT NULL) DESC, (match_category_id IS NOT NULL) DESC, \
                  (match_size_label IS NOT NULL) DESC, sort, created_at, id \
         LIMIT 1",
    )
    .bind(org_id)
    .bind(item_id)
    .bind(category_id)
    .bind(size_label)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(id)
}

async fn rebuild_one(conn: &mut PgConnection, item_id: Uuid) -> Result<ExpandStats, AppError> {
    let item: Option<(Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT org_id, category_id, recipe_source_item_id FROM menu_items \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(item_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((org_id, category_id, linked_to)) = item else {
        return Ok(ExpandStats::default());
    };

    let sizes: Vec<(Uuid, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, label, base_id FROM menu_item_sizes WHERE menu_item_id = $1 ORDER BY sort, label",
    )
    .bind(item_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut stats = ExpandStats::default();
    for (size_id, label, base_id) in sizes {
        stats.sizes_seen += 1;
        // (id, ingredient, qty, unit, source) currently stored.
        let current: Vec<(Uuid, Uuid, Decimal, String, Option<String>)> = sqlx::query_as(
            "SELECT id, ingredient_id, quantity, unit, source FROM recipe_lines \
             WHERE owner_type = 'item_size' AND owner_id = $1",
        )
        .bind(size_id)
        .fetch_all(&mut *conn)
        .await?;

        let (managed, wants) = if let Some(src) = linked_to {
            let rows: Vec<(Uuid, Decimal, String)> = sqlx::query_as(
                "SELECT rl.ingredient_id, rl.quantity, rl.unit FROM recipe_lines rl \
                 JOIN menu_item_sizes s ON s.id = rl.owner_id \
                 WHERE rl.owner_type = 'item_size' AND s.menu_item_id = $1 AND s.label = $2 \
                 ORDER BY rl.ingredient_id",
            )
            .bind(src)
            .bind(&label)
            .fetch_all(&mut *conn)
            .await?;
            let wants = rows
                .into_iter()
                .map(|(ingredient_id, quantity, unit)| Want {
                    ingredient_id,
                    quantity,
                    unit,
                    source: "linked",
                })
                .collect::<Vec<_>>();
            // A copy's lines are ALL managed by the link.
            (current, wants)
        } else {
            let (own, managed): (Vec<_>, Vec<_>) = current
                .into_iter()
                .partition(|r| matches!(r.4.as_deref(), None | Some("own")));
            let mut taken: std::collections::HashSet<Uuid> = own.iter().map(|r| r.1).collect();
            let mut wants: Vec<Want> = Vec::new();

            if let Some(base) = base_id {
                let rows: Vec<(Uuid, Decimal, String)> = sqlx::query_as(
                    "SELECT DISTINCT ON (bl.ingredient_id) bl.ingredient_id, bl.quantity, bl.unit \
                     FROM recipe_base_lines bl \
                     JOIN recipe_bases b ON b.id = bl.base_id \
                     WHERE bl.base_id = $1 AND b.is_active AND b.deleted_at IS NULL \
                       AND (bl.size_label IS NULL OR bl.size_label = $2) \
                     ORDER BY bl.ingredient_id, (bl.size_label IS NULL), bl.sort",
                )
                .bind(base)
                .bind(&label)
                .fetch_all(&mut *conn)
                .await?;
                for (ingredient_id, quantity, unit) in rows {
                    if taken.insert(ingredient_id) {
                        wants.push(Want {
                            ingredient_id,
                            quantity,
                            unit,
                            source: "base",
                        });
                    }
                }
            }

            if let Some(rule) = best_rule_for(conn, org_id, item_id, category_id, &label).await? {
                let rows: Vec<(Uuid, Decimal, String)> = sqlx::query_as(
                    "SELECT ingredient_id, quantity, unit FROM packaging_rule_lines \
                     WHERE rule_id = $1 ORDER BY sort, ingredient_id",
                )
                .bind(rule)
                .fetch_all(&mut *conn)
                .await?;
                for (ingredient_id, quantity, unit) in rows {
                    if taken.insert(ingredient_id) {
                        wants.push(Want {
                            ingredient_id,
                            quantity,
                            unit,
                            source: "rule",
                        });
                    }
                }
            }
            (managed, wants)
        };

        if apply_diff(conn, size_id, managed, wants).await? {
            stats.sizes_changed += 1;
        }
    }
    Ok(stats)
}

/// Make the managed rows of a size equal `wants` (keyed by ingredient). Returns
/// whether anything was written.
async fn apply_diff(
    conn: &mut PgConnection,
    size_id: Uuid,
    managed: Vec<(Uuid, Uuid, Decimal, String, Option<String>)>,
    wants: Vec<Want>,
) -> Result<bool, AppError> {
    let mut changed = false;
    let mut by_ing: std::collections::HashMap<Uuid, (Uuid, Decimal, String, Option<String>)> =
        managed
            .into_iter()
            .map(|(id, ing, q, u, s)| (ing, (id, q, u, s)))
            .collect();

    for w in wants {
        match by_ing.remove(&w.ingredient_id) {
            Some((id, q, u, s)) => {
                if q != w.quantity || u != w.unit || s.as_deref() != Some(w.source) {
                    sqlx::query(
                        "UPDATE recipe_lines SET quantity = $2, unit = $3, source = $4, updated_at = now() \
                         WHERE id = $1",
                    )
                    .bind(id)
                    .bind(w.quantity)
                    .bind(&w.unit)
                    .bind(w.source)
                    .execute(&mut *conn)
                    .await?;
                    changed = true;
                }
            }
            None => {
                sqlx::query(
                    "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit, source) \
                     VALUES ('item_size', $1, $2, $3, $4, $5)",
                )
                .bind(size_id)
                .bind(w.ingredient_id)
                .bind(w.quantity)
                .bind(&w.unit)
                .bind(w.source)
                .execute(&mut *conn)
                .await?;
                changed = true;
            }
        }
    }

    let stale: Vec<Uuid> = by_ing.into_values().map(|(id, ..)| id).collect();
    if !stale.is_empty() {
        sqlx::query("DELETE FROM recipe_lines WHERE id = ANY($1)")
            .bind(&stale)
            .execute(&mut *conn)
            .await?;
        changed = true;
    }
    Ok(changed)
}

/// A stable fingerprint of an item's recipe per size label: sorted
/// `label|ingredient|qty|unit` rows. Two items with equal fingerprints deduct the
/// same thing (lint F19, twin drift).
/// Only sizes whose label is in `labels` count.
pub async fn recipe_fingerprint(
    conn: &mut PgConnection,
    item_id: Uuid,
    labels: &[String],
) -> Result<String, AppError> {
    let rows: Vec<(String, Uuid, Decimal, String)> = sqlx::query_as(
        "SELECT s.label, rl.ingredient_id, rl.quantity, rl.unit FROM recipe_lines rl \
         JOIN menu_item_sizes s ON s.id = rl.owner_id \
         WHERE rl.owner_type = 'item_size' AND s.menu_item_id = $1 AND s.label = ANY($2) \
         ORDER BY s.label, rl.ingredient_id",
    )
    .bind(item_id)
    .bind(labels)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(l, i, q, u)| format!("{l}|{i}|{}|{u}", q.normalize()))
        .collect::<Vec<_>>()
        .join("\n"))
}
