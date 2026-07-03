//! Stable-id backfill: legacy menu/recipe/modifier tables → the new unified schema.
//!
//! Run AFTER `20260703100000_menu_unification_expand.sql`. Populates:
//!   menu_item_sizes, modifier_groups, modifier_options, menu_item_modifier_groups,
//!   recipe_lines, menu_price_overrides, catalog_revision.
//!
//! INVARIANTS (see MadarRust/CONTRACT.md):
//!   * modifier_options.id == old addon_items.id / menu_item_optional_fields.id, so
//!     order_item_addons.addon_item_id and order_item_optionals.optional_field_id
//!     (immutable order history) keep resolving. We copy the source id verbatim.
//!   * Order history (order_items / order_item_addons / order_item_optionals /
//!     order_line_bundle_components / *_cost / size_label) is NEVER touched.
//!   * Recipe lines become id-keyed (ingredient_id FK), fixing the rename-orphan bug.
//!     Legacy rows keyed only by ingredient NAME are resolved by (org_id, name); a row
//!     whose name can't be resolved is reported, not silently dropped.
//!   * Money stays integer piastres. Unknown cost stays NULL.
//!
//! Idempotent: clears this org's rows in the new tables first, then rebuilds. Wrapped
//! in one transaction; `--dry-run` rolls back. Reuses [`BackfillScope`] from the
//! recipe-units backfill.

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::recipes::backfill::BackfillScope;

/// One legacy row that could not be faithfully migrated. Carries enough context to
/// fix by hand (or to prove a clean run when the list is empty).
pub struct Unmigratable {
    /// machine-readable category, e.g. "recipe.ingredient_unresolved".
    pub kind: String,
    /// source table + primary key.
    pub source: String,
    /// human-readable explanation.
    pub detail: String,
}

/// Counts of rows written to each new table + the unmigratable report.
#[derive(Default)]
pub struct UnificationSummary {
    pub sizes_copied: u64,
    pub one_size_synth: u64,
    pub groups_created: u64,
    pub options_created: u64,
    pub item_group_attaches: u64,
    pub recipe_lines: u64,
    pub price_overrides: u64,
    pub unmigratable: Vec<Unmigratable>,
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

pub async fn backfill_menu_unification(
    pool: &PgPool,
    scope: BackfillScope,
    dry_run: bool,
) -> Result<UnificationSummary, AppError> {
    let org = resolve_org(pool, scope).await?;
    let mut tx = pool.begin().await?;
    let mut s = UnificationSummary::default();

    // ── Idempotency: clear this org's rows in the new tables (reverse-dependency) ──
    for sql in [
        "DELETE FROM recipe_lines rl WHERE \
           (rl.owner_type='item_size' AND rl.owner_id IN \
             (SELECT z.id FROM menu_item_sizes z JOIN menu_items m ON m.id=z.menu_item_id WHERE m.org_id=$1)) \
           OR (rl.owner_type='modifier_option' AND rl.owner_id IN \
             (SELECT o.id FROM modifier_options o JOIN modifier_groups g ON g.id=o.group_id WHERE g.org_id=$1))",
        "DELETE FROM menu_price_overrides p WHERE \
           (p.target_type='menu_item_size' AND p.target_id IN \
             (SELECT z.id FROM menu_item_sizes z JOIN menu_items m ON m.id=z.menu_item_id WHERE m.org_id=$1)) \
           OR (p.target_type='modifier_option' AND p.target_id IN \
             (SELECT o.id FROM modifier_options o JOIN modifier_groups g ON g.id=o.group_id WHERE g.org_id=$1))",
        "DELETE FROM menu_item_modifier_groups WHERE menu_item_id IN (SELECT id FROM menu_items WHERE org_id=$1)",
        "DELETE FROM modifier_options WHERE group_id IN (SELECT id FROM modifier_groups WHERE org_id=$1)",
        "DELETE FROM menu_item_sizes WHERE menu_item_id IN (SELECT id FROM menu_items WHERE org_id=$1)",
        "DELETE FROM modifier_groups WHERE org_id=$1",
    ] {
        sqlx::query(sql).bind(org).execute(&mut *tx).await?;
    }

    // ── Step A: menu_item_sizes ────────────────────────────────────────────────
    // A1 — copy the existing per-item dictionary verbatim (preserve id). Clamp any
    //      stray negative price to 0 (reported below) so the CHECK never aborts.
    s.sizes_copied = sqlx::query(
        "INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort, is_active) \
         SELECT z.id, z.menu_item_id, z.label, GREATEST(z.price_override,0), 0, z.is_active \
         FROM item_sizes z JOIN menu_items m ON m.id=z.menu_item_id WHERE m.org_id=$1",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // A2 — synthesize a 'one_size' row for every item with no size rows (incl.
    //      soft-deleted, so historical (menu_item_id,'one_size') SKUs stay resolvable).
    s.one_size_synth = sqlx::query(
        "INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort, is_active) \
         SELECT (md5(m.id::text || ':one_size'))::uuid, m.id, 'one_size', GREATEST(m.base_price,0), 0, true \
         FROM menu_items m \
         WHERE m.org_id=$1 AND NOT EXISTS (SELECT 1 FROM item_sizes z WHERE z.menu_item_id=m.id)",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // ── Step B: modifier_groups ────────────────────────────────────────────────
    // B1 — one reusable group per (org, addon_items.type). selection_type='single'
    //      iff at least one slot for the type exists and none has max_selections<>1.
    let g1 = sqlx::query(
        "INSERT INTO modifier_groups \
           (id, org_id, name, name_translations, selection_type, min_selections, max_selections, is_required, sort, is_active, legacy_addon_type) \
         SELECT (md5($1::text || ':addon:' || a.type))::uuid, $1, a.type, '{}'::jsonb, \
           CASE WHEN EXISTS (SELECT 1 FROM menu_item_addon_slots sl JOIN menu_items m2 ON m2.id=sl.menu_item_id \
                             WHERE m2.org_id=$1 AND sl.addon_type=a.type) \
                 AND NOT EXISTS (SELECT 1 FROM menu_item_addon_slots sl JOIN menu_items m2 ON m2.id=sl.menu_item_id \
                             WHERE m2.org_id=$1 AND sl.addon_type=a.type AND sl.max_selections IS DISTINCT FROM 1) \
                THEN 'single' ELSE 'multi' END, \
           0, NULL, false, 0, true, a.type \
         FROM addon_items a WHERE a.org_id=$1 GROUP BY a.type",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // B2 — one per-item 'Options' group for items that have optional fields.
    let g2 = sqlx::query(
        "INSERT INTO modifier_groups \
           (id, org_id, name, name_translations, selection_type, min_selections, max_selections, is_required, sort, is_active, legacy_addon_type) \
         SELECT (md5(m.id::text || ':options'))::uuid, $1, 'Options', '{}'::jsonb, 'multi', 0, NULL, false, 100, true, NULL \
         FROM menu_items m WHERE m.org_id=$1 \
           AND EXISTS (SELECT 1 FROM menu_item_optional_fields f WHERE f.menu_item_id=m.id)",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    s.groups_created = g1 + g2;

    // ── Step C: modifier_options (STABLE ids) ──────────────────────────────────
    // C1 — from addon_items (option.id = addon_items.id).
    let o1 = sqlx::query(
        "INSERT INTO modifier_options \
           (id, group_id, name, name_translations, price, sort, is_default, is_active, replaces_ingredient_id, legacy_source, created_at, updated_at) \
         SELECT a.id, (md5($1::text || ':addon:' || a.type))::uuid, a.name, a.name_translations, \
                GREATEST(a.default_price,0), 0, false, a.is_active, NULL, 'addon', a.created_at, a.updated_at \
         FROM addon_items a WHERE a.org_id=$1",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // C2 — from menu_item_optional_fields (option.id = optional_field.id).
    let o2 = sqlx::query(
        "INSERT INTO modifier_options \
           (id, group_id, name, name_translations, price, sort, is_default, is_active, replaces_ingredient_id, legacy_source, created_at, updated_at) \
         SELECT f.id, (md5(f.menu_item_id::text || ':options'))::uuid, f.name, f.name_translations, \
                GREATEST(f.price,0), 0, false, f.is_active, NULL, 'optional', f.created_at, f.updated_at \
         FROM menu_item_optional_fields f JOIN menu_items m ON m.id=f.menu_item_id WHERE m.org_id=$1",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    s.options_created = o1 + o2;

    // ── Step D: recipe_lines (id-keyed; resolve ingredient by id else by name) ──
    // D1 — menu_item_recipes → owner item_size (join size dictionary by label).
    let d1 = sqlx::query(
        "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit, created_at, updated_at) \
         SELECT 'item_size', ms.id, COALESCE(r.org_ingredient_id, oi.id), r.quantity_used, r.ingredient_unit, r.created_at, r.updated_at \
         FROM menu_item_recipes r \
         JOIN menu_items m ON m.id=r.menu_item_id AND m.org_id=$1 \
         JOIN menu_item_sizes ms ON ms.menu_item_id=r.menu_item_id AND ms.label=r.size_label \
         LEFT JOIN org_ingredients oi ON oi.org_id=$1 AND oi.name=r.ingredient_name AND oi.deleted_at IS NULL \
         WHERE COALESCE(r.org_ingredient_id, oi.id) IS NOT NULL AND r.quantity_used >= 0 \
         ON CONFLICT (owner_type, owner_id, ingredient_id) DO NOTHING",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // D2 — addon_item_ingredients → owner modifier_option (owner_id = addon_item_id).
    let d2 = sqlx::query(
        "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit, created_at, updated_at) \
         SELECT 'modifier_option', ai.addon_item_id, COALESCE(ai.org_ingredient_id, oi.id), ai.quantity_used, ai.ingredient_unit, ai.created_at, ai.updated_at \
         FROM addon_item_ingredients ai \
         JOIN addon_items a ON a.id=ai.addon_item_id AND a.org_id=$1 \
         LEFT JOIN org_ingredients oi ON oi.org_id=$1 AND oi.name=ai.ingredient_name AND oi.deleted_at IS NULL \
         WHERE COALESCE(ai.org_ingredient_id, oi.id) IS NOT NULL AND ai.quantity_used >= 0 \
         ON CONFLICT (owner_type, owner_id, ingredient_id) DO NOTHING",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // D3 — inline optional recipe → owner modifier_option (owner_id = optional_field.id).
    //      size scoping (size_label) is not representable on an option; reported below.
    let d3 = sqlx::query(
        "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit, created_at, updated_at) \
         SELECT 'modifier_option', f.id, COALESCE(f.org_ingredient_id, oi.id), f.quantity_used, f.ingredient_unit, f.created_at, f.updated_at \
         FROM menu_item_optional_fields f \
         JOIN menu_items m ON m.id=f.menu_item_id AND m.org_id=$1 \
         LEFT JOIN org_ingredients oi ON oi.org_id=$1 AND oi.name=f.ingredient_name AND oi.deleted_at IS NULL \
         WHERE f.ingredient_name IS NOT NULL AND f.quantity_used IS NOT NULL AND f.quantity_used >= 0 \
           AND COALESCE(f.org_ingredient_id, oi.id) IS NOT NULL \
         ON CONFLICT (owner_type, owner_id, ingredient_id) DO NOTHING",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    s.recipe_lines = d1 + d2 + d3;

    // ── Step E: menu_item_modifier_groups (attach groups to items) ─────────────
    // E1 — from slots (min/max/required overrides; allowlist → included_option_ids).
    let e1 = sqlx::query(
        "INSERT INTO menu_item_modifier_groups \
           (menu_item_id, group_id, sort, min_override, max_override, is_required_override, included_option_ids, legacy_origin) \
         SELECT sl.menu_item_id, g.id, 0, sl.min_selections, sl.max_selections, sl.is_required, \
           CASE WHEN EXISTS (SELECT 1 FROM menu_item_allowed_addons al WHERE al.menu_item_id=sl.menu_item_id) \
                THEN ARRAY(SELECT al.addon_item_id FROM menu_item_allowed_addons al \
                           JOIN addon_items a2 ON a2.id=al.addon_item_id \
                           WHERE al.menu_item_id=sl.menu_item_id AND a2.type=sl.addon_type ORDER BY al.sort_order) \
                ELSE NULL END, 'slot' \
         FROM menu_item_addon_slots sl \
         JOIN menu_items m ON m.id=sl.menu_item_id AND m.org_id=$1 \
         JOIN modifier_groups g ON g.id=(md5($1::text || ':addon:' || sl.addon_type))::uuid \
         ON CONFLICT (menu_item_id, group_id) DO NOTHING",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // E2 — from allowlist rows whose type has NO slot on that item (attach with
    //      group defaults; included = the allowlisted subset of that type).
    let e2 = sqlx::query(
        "INSERT INTO menu_item_modifier_groups \
           (menu_item_id, group_id, sort, min_override, max_override, is_required_override, included_option_ids, legacy_origin) \
         SELECT al.menu_item_id, (md5($1::text || ':addon:' || a2.type))::uuid, 0, NULL, NULL, NULL, \
                ARRAY(SELECT al2.addon_item_id FROM menu_item_allowed_addons al2 \
                      JOIN addon_items a3 ON a3.id=al2.addon_item_id \
                      WHERE al2.menu_item_id=al.menu_item_id AND a3.type=a2.type ORDER BY al2.sort_order), 'allowlist' \
         FROM menu_item_allowed_addons al \
         JOIN addon_items a2 ON a2.id=al.addon_item_id AND a2.org_id=$1 \
         JOIN menu_items m ON m.id=al.menu_item_id \
         WHERE NOT EXISTS (SELECT 1 FROM menu_item_addon_slots sl \
                           WHERE sl.menu_item_id=al.menu_item_id AND sl.addon_type=a2.type) \
         GROUP BY al.menu_item_id, a2.type \
         ON CONFLICT (menu_item_id, group_id) DO NOTHING",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // E3 — attach each item's 'Options' group.
    let e3 = sqlx::query(
        "INSERT INTO menu_item_modifier_groups \
           (menu_item_id, group_id, sort, min_override, max_override, is_required_override, included_option_ids, legacy_origin) \
         SELECT m.id, (md5(m.id::text || ':options'))::uuid, 100, NULL, NULL, NULL, NULL, 'options' \
         FROM menu_items m WHERE m.org_id=$1 \
           AND EXISTS (SELECT 1 FROM menu_item_optional_fields f WHERE f.menu_item_id=m.id) \
         ON CONFLICT (menu_item_id, group_id) DO NOTHING",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    s.item_group_attaches = e1 + e2 + e3;

    // ── Step F: menu_price_overrides (merge 5 legacy override tables) ──────────
    let mut po: u64 = 0;
    // F1a — branch_menu_overrides price → the item's one_size row (size-less items).
    po += sqlx::query(
        "INSERT INTO menu_price_overrides (scope, branch_id, channel, target_type, target_id, price, is_available) \
         SELECT 'branch', bmo.branch_id, NULL, 'menu_item_size', ms.id, bmo.price_override, NULL \
         FROM branch_menu_overrides bmo \
         JOIN menu_items m ON m.id=bmo.menu_item_id AND m.org_id=$1 \
         JOIN menu_item_sizes ms ON ms.menu_item_id=bmo.menu_item_id AND ms.label='one_size' \
         WHERE bmo.price_override IS NOT NULL \
         ON CONFLICT (target_type, target_id, branch_id) WHERE scope='branch' \
           DO UPDATE SET price=EXCLUDED.price, updated_at=NOW()",
    )
    .bind(org).execute(&mut *tx).await?.rows_affected();
    // F1b — branch_menu_overrides is_available=false → all sizes of the item.
    po += sqlx::query(
        "INSERT INTO menu_price_overrides (scope, branch_id, channel, target_type, target_id, price, is_available) \
         SELECT 'branch', bmo.branch_id, NULL, 'menu_item_size', ms.id, NULL, false \
         FROM branch_menu_overrides bmo \
         JOIN menu_items m ON m.id=bmo.menu_item_id AND m.org_id=$1 \
         JOIN menu_item_sizes ms ON ms.menu_item_id=bmo.menu_item_id \
         WHERE bmo.is_available = false \
         ON CONFLICT (target_type, target_id, branch_id) WHERE scope='branch' \
           DO UPDATE SET is_available=EXCLUDED.is_available, updated_at=NOW()",
    )
    .bind(org).execute(&mut *tx).await?.rows_affected();
    // F2 — branch_menu_size_overrides → per-size price.
    po += sqlx::query(
        "INSERT INTO menu_price_overrides (scope, branch_id, channel, target_type, target_id, price, is_available) \
         SELECT 'branch', bms.branch_id, NULL, 'menu_item_size', ms.id, bms.price_override, NULL \
         FROM branch_menu_size_overrides bms \
         JOIN menu_items m ON m.id=bms.menu_item_id AND m.org_id=$1 \
         JOIN menu_item_sizes ms ON ms.menu_item_id=bms.menu_item_id AND ms.label=bms.size_label \
         ON CONFLICT (target_type, target_id, branch_id) WHERE scope='branch' \
           DO UPDATE SET price=EXCLUDED.price, updated_at=NOW()",
    )
    .bind(org).execute(&mut *tx).await?.rows_affected();
    // F3 — branch_addon_overrides → modifier_option (price + availability in one row).
    po += sqlx::query(
        "INSERT INTO menu_price_overrides (scope, branch_id, channel, target_type, target_id, price, is_available) \
         SELECT 'branch', bao.branch_id, NULL, 'modifier_option', bao.addon_item_id, bao.price_override, \
                CASE WHEN bao.is_available=false THEN false ELSE NULL END \
         FROM branch_addon_overrides bao JOIN addon_items a ON a.id=bao.addon_item_id AND a.org_id=$1 \
         WHERE bao.price_override IS NOT NULL OR bao.is_available=false \
         ON CONFLICT (target_type, target_id, branch_id) WHERE scope='branch' \
           DO UPDATE SET price=EXCLUDED.price, is_available=EXCLUDED.is_available, updated_at=NOW()",
    )
    .bind(org).execute(&mut *tx).await?.rows_affected();
    // F4a — branch_channel_menu_overrides price → one_size.
    po += sqlx::query(
        "INSERT INTO menu_price_overrides (scope, branch_id, channel, target_type, target_id, price, is_available) \
         SELECT 'branch_channel', bcmo.branch_id, bcmo.channel, 'menu_item_size', ms.id, bcmo.price_override, NULL \
         FROM branch_channel_menu_overrides bcmo \
         JOIN menu_items m ON m.id=bcmo.menu_item_id AND m.org_id=$1 \
         JOIN menu_item_sizes ms ON ms.menu_item_id=bcmo.menu_item_id AND ms.label='one_size' \
         WHERE bcmo.price_override IS NOT NULL \
         ON CONFLICT (target_type, target_id, branch_id, channel) WHERE scope='branch_channel' \
           DO UPDATE SET price=EXCLUDED.price, updated_at=NOW()",
    )
    .bind(org).execute(&mut *tx).await?.rows_affected();
    // F4b — branch_channel_menu_overrides availability (tri-state) → all sizes.
    po += sqlx::query(
        "INSERT INTO menu_price_overrides (scope, branch_id, channel, target_type, target_id, price, is_available) \
         SELECT 'branch_channel', bcmo.branch_id, bcmo.channel, 'menu_item_size', ms.id, NULL, bcmo.is_available \
         FROM branch_channel_menu_overrides bcmo \
         JOIN menu_items m ON m.id=bcmo.menu_item_id AND m.org_id=$1 \
         JOIN menu_item_sizes ms ON ms.menu_item_id=bcmo.menu_item_id \
         WHERE bcmo.is_available IS NOT NULL \
         ON CONFLICT (target_type, target_id, branch_id, channel) WHERE scope='branch_channel' \
           DO UPDATE SET is_available=EXCLUDED.is_available, updated_at=NOW()",
    )
    .bind(org).execute(&mut *tx).await?.rows_affected();
    // F5 — branch_channel_addon_overrides → modifier_option.
    po += sqlx::query(
        "INSERT INTO menu_price_overrides (scope, branch_id, channel, target_type, target_id, price, is_available) \
         SELECT 'branch_channel', bcao.branch_id, bcao.channel, 'modifier_option', bcao.addon_item_id, \
                bcao.price_override, bcao.is_available \
         FROM branch_channel_addon_overrides bcao JOIN addon_items a ON a.id=bcao.addon_item_id AND a.org_id=$1 \
         WHERE bcao.price_override IS NOT NULL OR bcao.is_available IS NOT NULL \
         ON CONFLICT (target_type, target_id, branch_id, channel) WHERE scope='branch_channel' \
           DO UPDATE SET price=EXCLUDED.price, is_available=EXCLUDED.is_available, updated_at=NOW()",
    )
    .bind(org).execute(&mut *tx).await?.rows_affected();
    s.price_overrides = po;

    // ── Step G: catalog_revision ───────────────────────────────────────────────
    sqlx::query("INSERT INTO catalog_revision (org_id, revision) VALUES ($1, 1) ON CONFLICT (org_id) DO NOTHING")
        .bind(org)
        .execute(&mut *tx)
        .await?;

    // ── Reports: collect every row that could not be faithfully migrated ────────
    collect_reports(&mut tx, org, &mut s).await?;

    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(s)
}

/// A report query returning `(source_id_text, detail_text)`, mapped to one
/// [`Unmigratable`] each with the given `kind`.
async fn push_report(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: Uuid,
    kind: &str,
    sql: &str,
    out: &mut Vec<Unmigratable>,
) -> Result<(), AppError> {
    let rows: Vec<(String, String)> = sqlx::query_as(sql).bind(org).fetch_all(&mut **tx).await?;
    for (source, detail) in rows {
        out.push(Unmigratable {
            kind: kind.to_string(),
            source,
            detail,
        });
    }
    Ok(())
}

async fn collect_reports(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: Uuid,
    s: &mut UnificationSummary,
) -> Result<(), AppError> {
    // Recipe ingredient unresolvable by id OR name → no recipe_line was written.
    push_report(tx, org, "recipe.ingredient_unresolved",
        "SELECT 'menu_item_recipes:'||r.id::text, \
                'item '||r.menu_item_id::text||' size '||r.size_label||' ingredient '''||r.ingredient_name||'''' \
         FROM menu_item_recipes r JOIN menu_items m ON m.id=r.menu_item_id AND m.org_id=$1 \
         LEFT JOIN org_ingredients oi ON oi.org_id=$1 AND oi.name=r.ingredient_name AND oi.deleted_at IS NULL \
         WHERE r.org_ingredient_id IS NULL AND oi.id IS NULL", &mut s.unmigratable).await?;
    // Recipe size_label not present in the size dictionary → recipe line skipped.
    push_report(tx, org, "recipe.size_unmatched",
        "SELECT 'menu_item_recipes:'||r.id::text, \
                'item '||r.menu_item_id::text||' has no size labelled '''||r.size_label||'''' \
         FROM menu_item_recipes r JOIN menu_items m ON m.id=r.menu_item_id AND m.org_id=$1 \
         WHERE NOT EXISTS (SELECT 1 FROM menu_item_sizes ms WHERE ms.menu_item_id=r.menu_item_id AND ms.label=r.size_label)",
        &mut s.unmigratable).await?;
    // Negative recipe quantity → skipped (invalid; violates recipe_lines qty>=0). Zero is
    // allowed (swap marker), so only strictly-negative rows are reported here.
    push_report(tx, org, "recipe.negative_qty",
        "SELECT 'menu_item_recipes:'||r.id::text, 'quantity '||r.quantity_used::text||' '||r.ingredient_unit \
         FROM menu_item_recipes r JOIN menu_items m ON m.id=r.menu_item_id AND m.org_id=$1 WHERE r.quantity_used < 0 \
         UNION ALL \
         SELECT 'addon_item_ingredients:'||ai.id::text, 'quantity '||ai.quantity_used::text||' '||ai.ingredient_unit \
         FROM addon_item_ingredients ai JOIN addon_items a ON a.id=ai.addon_item_id AND a.org_id=$1 WHERE ai.quantity_used < 0",
        &mut s.unmigratable).await?;
    // Addon ingredient unresolvable → no recipe_line for that addon option.
    push_report(tx, org, "addon.ingredient_unresolved",
        "SELECT 'addon_item_ingredients:'||ai.id::text, 'addon '||ai.addon_item_id::text||' ingredient '''||ai.ingredient_name||'''' \
         FROM addon_item_ingredients ai JOIN addon_items a ON a.id=ai.addon_item_id AND a.org_id=$1 \
         LEFT JOIN org_ingredients oi ON oi.org_id=$1 AND oi.name=ai.ingredient_name AND oi.deleted_at IS NULL \
         WHERE ai.org_ingredient_id IS NULL AND oi.id IS NULL", &mut s.unmigratable).await?;
    // Optional-with-recipe whose ingredient is unresolvable.
    push_report(tx, org, "optional.ingredient_unresolved",
        "SELECT 'menu_item_optional_fields:'||f.id::text, 'optional '''||f.name||''' ingredient '''||COALESCE(f.ingredient_name,'')||'''' \
         FROM menu_item_optional_fields f JOIN menu_items m ON m.id=f.menu_item_id AND m.org_id=$1 \
         LEFT JOIN org_ingredients oi ON oi.org_id=$1 AND oi.name=f.ingredient_name AND oi.deleted_at IS NULL \
         WHERE f.ingredient_name IS NOT NULL AND f.quantity_used IS NOT NULL AND f.org_ingredient_id IS NULL AND oi.id IS NULL",
        &mut s.unmigratable).await?;
    // Size-scoped optional deduction: size scoping is lost on the option (review).
    push_report(tx, org, "optional.size_scoped",
        "SELECT 'menu_item_optional_fields:'||f.id::text, 'optional '''||f.name||''' was scoped to size '''||f.size_label||''' — deduction now applies to all sizes' \
         FROM menu_item_optional_fields f JOIN menu_items m ON m.id=f.menu_item_id AND m.org_id=$1 \
         WHERE f.size_label IS NOT NULL AND f.ingredient_name IS NOT NULL", &mut s.unmigratable).await?;
    // branch_menu_overrides price on a SIZED item → not applied (no one_size target).
    push_report(tx, org, "branch_menu.price_on_sized_item",
        "SELECT 'branch_menu_overrides:'||bmo.branch_id::text||':'||bmo.menu_item_id::text, \
                'price '||bmo.price_override::text||' not applied — item is multi-size (no one_size target)' \
         FROM branch_menu_overrides bmo JOIN menu_items m ON m.id=bmo.menu_item_id AND m.org_id=$1 \
         WHERE bmo.price_override IS NOT NULL \
           AND NOT EXISTS (SELECT 1 FROM menu_item_sizes ms WHERE ms.menu_item_id=bmo.menu_item_id AND ms.label='one_size')",
        &mut s.unmigratable).await?;
    // branch_channel_menu_overrides price on a SIZED item → not applied.
    push_report(tx, org, "branch_channel_menu.price_on_sized_item",
        "SELECT 'branch_channel_menu_overrides:'||bcmo.branch_id::text||':'||bcmo.menu_item_id::text||':'||bcmo.channel::text, \
                'channel price '||bcmo.price_override::text||' not applied — item is multi-size' \
         FROM branch_channel_menu_overrides bcmo JOIN menu_items m ON m.id=bcmo.menu_item_id AND m.org_id=$1 \
         WHERE bcmo.price_override IS NOT NULL \
           AND NOT EXISTS (SELECT 1 FROM menu_item_sizes ms WHERE ms.menu_item_id=bcmo.menu_item_id AND ms.label='one_size')",
        &mut s.unmigratable).await?;
    // branch_menu_size_overrides whose size_label is not in the dictionary.
    push_report(tx, org, "branch_menu_size.size_unmatched",
        "SELECT 'branch_menu_size_overrides:'||bms.branch_id::text||':'||bms.menu_item_id::text||':'||bms.size_label, \
                'no size labelled '''||bms.size_label||''' on item '||bms.menu_item_id::text \
         FROM branch_menu_size_overrides bms JOIN menu_items m ON m.id=bms.menu_item_id AND m.org_id=$1 \
         WHERE NOT EXISTS (SELECT 1 FROM menu_item_sizes ms WHERE ms.menu_item_id=bms.menu_item_id AND ms.label=bms.size_label)",
        &mut s.unmigratable).await?;
    // Negative catalog prices we clamped to 0.
    push_report(tx, org, "size.negative_price_clamped",
        "SELECT 'item_sizes:'||z.id::text, 'price_override '||z.price_override::text||' clamped to 0' \
         FROM item_sizes z JOIN menu_items m ON m.id=z.menu_item_id AND m.org_id=$1 WHERE z.price_override < 0",
        &mut s.unmigratable).await?;
    // Items that relied on the legacy implicit "offer all org addons" default (no slots,
    // no allowlist). By design the new model is explicit-attachment, so these get NO
    // auto-attached groups; deployed clients keep their behavior via the compat shim
    // (empty slots/allowlist). Informational — one summary row per org, not a failure.
    push_report(tx, org, "info.implicit_all_addons",
        "SELECT 'org:'||$1::text, count(*)::text||' menu item(s) had no addon slots/allowlist and relied on the implicit all-org-addons default — not auto-attached (author explicitly; shim preserves old-client behavior)' \
         FROM menu_items m \
         WHERE m.org_id=$1 AND m.deleted_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM menu_item_addon_slots sl WHERE sl.menu_item_id=m.id) \
           AND NOT EXISTS (SELECT 1 FROM menu_item_allowed_addons al WHERE al.menu_item_id=m.id) \
           AND EXISTS (SELECT 1 FROM addon_items a WHERE a.org_id=$1) \
         HAVING count(*) > 0", &mut s.unmigratable).await?;
    // menu_item_addon_overrides (per-item ingredient swaps) — no reusable-option home; manual review.
    push_report(tx, org, "addon_override.manual_review",
        "SELECT 'menu_item_addon_overrides:'||o.id::text, \
                'per-item swap on item '||o.menu_item_id::text||' addon '||o.addon_item_id::text||' — re-model as modifier_option.replaces_ingredient_id' \
         FROM menu_item_addon_overrides o JOIN menu_items m ON m.id=o.menu_item_id AND m.org_id=$1",
        &mut s.unmigratable).await?;
    Ok(())
}
