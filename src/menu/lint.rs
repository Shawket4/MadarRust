//! Menu modeling lint (`GET /menu/lint`): pure read-only checks for menu states the
//! dashboard allows but the POS, the legacy shim views or the deduction engine
//! silently mishandle. Rules follow MENU_MODELING_AUDIT.md §3 (F1–F11, F13–F15).
//!
//! Every rule reads the UNIFIED tables (`modifier_groups`, `modifier_options`,
//! `menu_item_modifier_groups`, `menu_item_sizes`, `recipe_lines`) — on prod the
//! legacy relations are views over exactly these, so a rule and the resolver see the
//! same data. Only live rows count: non-deleted active items, active sizes, active
//! groups and options, non-deleted active ingredients.
//!
//! The swap vocabulary matches the resolver (`component_resolve.rs`): a group with
//! `effect = 'swaps'` and a `swap_category_id` swaps the recipe line in that category;
//! otherwise a group whose `legacy_addon_type` is `milk_type` / `coffee_type` swaps the
//! recipe line whose ingredient category slug is `milk` / `coffee_bean`.
//!
//! [`lint_org`] is the engine (callable from tests and, later, readiness); the handler
//! only authorizes and serializes.

use actix_web::HttpMessage;
use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum LintSeverity {
    Error,
    Warn,
}

/// One finding. `entity_type` is `attachment` | `item` | `option` | `ingredient`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LintIssue {
    /// Audit rule id, e.g. `F4`.
    pub rule: String,
    pub severity: LintSeverity,
    pub entity_type: String,
    pub entity_id: Uuid,
    pub entity_name: String,
    pub message: String,
    /// Set when the finding is about one size of an item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_label: Option<String>,
    /// The menu item the finding is about, when it is item-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<Uuid>,
    /// The modifier group the finding is about, when it is group-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct LintQuery {
    /// Organization to lint.
    pub org_id: Uuid,
    /// Only findings about this modifier group (for the group editor).
    #[serde(default)]
    pub group_id: Option<Uuid>,
}

/// (entity_id, entity_name, size_label, message, item_id, group_id)
type Row = (
    Uuid,
    String,
    Option<String>,
    String,
    Option<Uuid>,
    Option<Uuid>,
);

struct Rule {
    id: &'static str,
    severity: LintSeverity,
    entity_type: &'static str,
    sql: &'static str,
}

// ── shared SQL fragments (inlined via concat!) ─────────────────────────────
// Swap groups of the org with the ingredient slug they swap.
macro_rules! swap_groups {
    () => {
        "swap AS (
            SELECT g.id, g.name, g.legacy_addon_type,
                   COALESCE(c.slug, CASE g.legacy_addon_type WHEN 'milk_type' THEN 'milk'
                                                             ELSE 'coffee_bean' END) AS slug
              FROM modifier_groups g
              LEFT JOIN ingredient_categories c
                     ON c.id = g.swap_category_id AND g.effect = 'swaps'
             WHERE g.org_id = $1 AND g.is_active
               AND (c.id IS NOT NULL OR g.legacy_addon_type IN ('milk_type', 'coffee_type')))"
    };
}
// Live (item, size, attachment) triples for swap groups.
macro_rules! swap_sizes {
    () => {
        "att AS (
            SELECT i.id AS item_id, i.name AS item_name, s.id AS size_id, s.label AS size_label,
                   m.included_option_ids, sw.id AS group_id, sw.name AS group_name, sw.slug
              FROM menu_items i
              JOIN menu_item_sizes s ON s.menu_item_id = i.id AND s.is_active
              JOIN menu_item_modifier_groups m ON m.menu_item_id = i.id
              JOIN swap sw ON sw.id = m.group_id
             WHERE i.org_id = $1 AND i.deleted_at IS NULL AND i.is_active)"
    };
}
// Recipe lines with their ingredient's slug.
macro_rules! lines {
    () => {
        "ln AS (
            SELECT rl.owner_type, rl.owner_id, rl.ingredient_id, rl.unit, rl.quantity,
                   oi.name AS ing_name, oi.unit::text AS ing_unit, ic.slug
              FROM recipe_lines rl
              JOIN org_ingredients oi ON oi.id = rl.ingredient_id
              LEFT JOIN ingredient_categories ic ON ic.id = oi.category_id
             WHERE oi.org_id = $1)"
    };
}

const RULES: &[Rule] = &[
    Rule {
        id: "F1",
        severity: LintSeverity::Error,
        entity_type: "attachment",
        sql: "SELECT m.id, i.name || ' / ' || g.name, NULL::text,
                     'Attachment has no legacy_origin: old tills do not show \"' || g.name
                     || '\" as a slot on \"' || i.name || '\" and do not enforce required',
                     i.id, g.id
                FROM menu_item_modifier_groups m
                JOIN menu_items i ON i.id = m.menu_item_id
                JOIN modifier_groups g ON g.id = m.group_id
               WHERE i.org_id = $1 AND i.deleted_at IS NULL AND m.legacy_origin IS NULL
               ORDER BY i.name, g.name",
    },
    Rule {
        id: "F2",
        severity: LintSeverity::Error,
        entity_type: "attachment",
        sql: "SELECT m.id, i.name || ' / ' || g.name, NULL::text,
                     'Item-private options group \"' || g.name || '\" is attached as '
                     || COALESCE(m.legacy_origin, 'NULL')
                     || ', not options: its optionals are neither charged nor deducted on old tills',
                     i.id, g.id
                FROM menu_item_modifier_groups m
                JOIN menu_items i ON i.id = m.menu_item_id
                JOIN modifier_groups g ON g.id = m.group_id
               WHERE i.org_id = $1 AND i.deleted_at IS NULL
                 AND g.legacy_addon_type IS NULL
                 AND m.legacy_origin IS DISTINCT FROM 'options'
               ORDER BY i.name, g.name",
    },
    Rule {
        id: "F3",
        severity: LintSeverity::Error,
        entity_type: "item",
        sql: "SELECT i.id, i.name, NULL::text,
                     'Some option groups offer all options (no list) while others are restricted: '
                     || 'old tills hide the unrestricted groups'' options ('
                     || string_agg(g.name, ', ' ORDER BY g.name) FILTER (WHERE m.included_option_ids IS NULL)
                     || ')',
                     i.id, NULL::uuid
                FROM menu_items i
                JOIN menu_item_modifier_groups m ON m.menu_item_id = i.id
                JOIN modifier_groups g ON g.id = m.group_id AND g.legacy_addon_type IS NOT NULL
               WHERE i.org_id = $1 AND i.deleted_at IS NULL
               GROUP BY i.id, i.name
              HAVING bool_or(m.included_option_ids IS NULL) AND bool_or(m.included_option_ids IS NOT NULL)
               ORDER BY i.name",
    },
    Rule {
        id: "F4",
        severity: LintSeverity::Error,
        entity_type: "item",
        sql: concat!(
            "WITH ",
            swap_groups!(),
            ", ",
            swap_sizes!(),
            ", ",
            lines!(),
            "
            SELECT a.item_id, a.item_name, a.size_label,
                   '\"' || a.group_name || '\" swaps the drink''s ' || a.slug
                   || ' but this recipe has no ' || a.slug
                   || ' line: no swap, no default, full option price charged',
                   a.item_id, a.group_id
              FROM att a
             WHERE NOT EXISTS (SELECT 1 FROM ln
                                WHERE ln.owner_type = 'item_size' AND ln.owner_id = a.size_id
                                  AND ln.slug = a.slug)
             ORDER BY a.item_name, a.size_label, a.group_name"
        ),
    },
    Rule {
        id: "F5",
        severity: LintSeverity::Error,
        entity_type: "item",
        sql: concat!(
            "WITH ",
            swap_groups!(),
            ", ",
            swap_sizes!(),
            ", ",
            lines!(),
            "
            SELECT DISTINCT a.item_id, a.item_name, a.size_label,
                   'The recipe''s ' || b.ing_name || ' is not offered by \"' || a.group_name
                   || '\": nothing is preselected and every sale counts as a swap',
                   a.item_id, a.group_id
              FROM att a
              JOIN ln b ON b.owner_type = 'item_size' AND b.owner_id = a.size_id AND b.slug = a.slug
             WHERE NOT EXISTS (
                     SELECT 1 FROM modifier_options o
                       JOIN ln r ON r.owner_type = 'modifier_option' AND r.owner_id = o.id
                      WHERE o.group_id = a.group_id AND o.is_active
                        AND (a.included_option_ids IS NULL OR o.id = ANY(a.included_option_ids))
                        AND r.ingredient_id = b.ingredient_id)
             ORDER BY a.item_name, a.size_label"
        ),
    },
    Rule {
        id: "F6",
        severity: LintSeverity::Error,
        entity_type: "option",
        sql: concat!(
            "WITH ",
            swap_groups!(),
            "
            SELECT o.id, sw.name || ' / ' || o.name, NULL::text,
                   'Swap option has no recipe line: the swap fails, the base is still deducted and the full price charged',
                   NULL::uuid, sw.id
              FROM modifier_options o
              JOIN swap sw ON sw.id = o.group_id
             WHERE o.is_active
               AND NOT EXISTS (SELECT 1 FROM recipe_lines rl
                                WHERE rl.owner_type = 'modifier_option' AND rl.owner_id = o.id)
             ORDER BY sw.name, o.name"
        ),
    },
    Rule {
        id: "F7",
        severity: LintSeverity::Error,
        entity_type: "option",
        sql: concat!(
            "WITH ",
            swap_groups!(),
            "
            SELECT o.id, sw.name || ' / ' || o.name, NULL::text,
                   'Swap option has ' || count(*) || ' recipe lines; only the first by ingredient name ('
                   || min(oi.name) || ') is used as the replacement',
                   NULL::uuid, sw.id
              FROM modifier_options o
              JOIN swap sw ON sw.id = o.group_id
              JOIN recipe_lines rl ON rl.owner_type = 'modifier_option' AND rl.owner_id = o.id
              JOIN org_ingredients oi ON oi.id = rl.ingredient_id
             WHERE o.is_active
             GROUP BY o.id, sw.id, sw.name, o.name
            HAVING count(*) > 1
             ORDER BY sw.name, o.name"
        ),
    },
    Rule {
        id: "F8",
        severity: LintSeverity::Error,
        entity_type: "option",
        sql: concat!(
            "WITH ",
            swap_groups!(),
            ", ",
            lines!(),
            "
            SELECT o.id, sw.name || ' / ' || o.name, NULL::text,
                   'Swap option ingredient ' || r.ing_name || ' is in category '
                   || COALESCE(r.slug, 'none') || ', not ' || sw.slug || ': the base line is never replaced',
                   NULL::uuid, sw.id
              FROM modifier_options o
              JOIN swap sw ON sw.id = o.group_id
              JOIN ln r ON r.owner_type = 'modifier_option' AND r.owner_id = o.id
             WHERE o.is_active AND r.slug IS DISTINCT FROM sw.slug
             ORDER BY sw.name, o.name"
        ),
    },
    Rule {
        id: "F9",
        severity: LintSeverity::Warn,
        entity_type: "ingredient",
        sql: "SELECT oi.id, oi.name, NULL::text,
                     'No ' || CASE ic.slug WHEN 'milk' THEN 'milk' ELSE 'coffee' END
                     || ' option uses this ingredient: it can never be chosen on the POS',
                     NULL::uuid, NULL::uuid
                FROM org_ingredients oi
                JOIN ingredient_categories ic ON ic.id = oi.category_id
               WHERE oi.org_id = $1 AND oi.deleted_at IS NULL AND oi.is_active
                 AND ic.slug IN ('milk', 'coffee_bean')
                 AND NOT EXISTS (
                       SELECT 1 FROM recipe_lines rl
                         JOIN modifier_options o ON o.id = rl.owner_id AND o.is_active
                         JOIN modifier_groups g ON g.id = o.group_id AND g.is_active
                        WHERE rl.owner_type = 'modifier_option' AND rl.ingredient_id = oi.id
                          AND (g.legacy_addon_type = CASE ic.slug WHEN 'milk' THEN 'milk_type' ELSE 'coffee_type' END
                               OR (g.effect = 'swaps' AND g.swap_category_id = oi.category_id)))
               ORDER BY oi.name",
    },
    Rule {
        id: "F10",
        severity: LintSeverity::Error,
        entity_type: "item",
        sql: concat!(
            "WITH ",
            swap_groups!(),
            ", ",
            swap_sizes!(),
            ", ",
            lines!(),
            ",
            fam(u, f) AS (VALUES ('g','mass'),('kg','mass'),('ml','volume'),('l','volume'),('pcs','count')),
            first_line AS (
                SELECT DISTINCT ON (r.owner_id) r.owner_id, r.ing_name, r.unit, r.ingredient_id
                  FROM ln r WHERE r.owner_type = 'modifier_option'
                 ORDER BY r.owner_id, r.ing_name, r.ingredient_id),
            -- swaps: base line unit vs the option's replacement line unit
            swaps AS (
                SELECT a.item_id, a.item_name, a.size_label, a.group_id,
                       'Choosing \"' || o.name || '\" swaps ' || b.ing_name || ' (' || b.unit || ') for '
                       || fl.ing_name || ' (' || fl.unit || '): incompatible units, nothing is deducted' AS msg
                  FROM att a
                  JOIN ln b ON b.owner_type = 'item_size' AND b.owner_id = a.size_id AND b.slug = a.slug
                  JOIN modifier_options o ON o.group_id = a.group_id AND o.is_active
                       AND (a.included_option_ids IS NULL OR o.id = ANY(a.included_option_ids))
                  JOIN first_line fl ON fl.owner_id = o.id
                  LEFT JOIN fam fb ON fb.u = lower(b.unit)
                  LEFT JOIN fam fr ON fr.u = lower(fl.unit)
                 WHERE fb.f IS DISTINCT FROM fr.f
                   -- re-picking the recipe's own ingredient is not a swap
                   AND fl.ingredient_id <> b.ingredient_id),
            -- follows: an additive option line in milk/coffee follows the drink's ingredient
            follows AS (
                SELECT i.id AS item_id, i.name AS item_name, s.label AS size_label, g.id AS group_id,
                       '\"' || o.name || '\" adds ' || r.ing_name || ' (' || r.unit || ') which follows '
                       || b.ing_name || ' (' || b.unit || '): incompatible units, left undeducted as authored' AS msg
                  FROM menu_items i
                  JOIN menu_item_sizes s ON s.menu_item_id = i.id AND s.is_active
                  JOIN menu_item_modifier_groups m ON m.menu_item_id = i.id
                  JOIN modifier_groups g ON g.id = m.group_id AND g.is_active
                       AND g.legacy_addon_type IS NOT NULL
                       AND g.legacy_addon_type NOT IN ('milk_type', 'coffee_type')
                       AND g.effect <> 'swaps'
                  JOIN modifier_options o ON o.group_id = g.id AND o.is_active
                       AND (m.included_option_ids IS NULL OR o.id = ANY(m.included_option_ids))
                  JOIN ln r ON r.owner_type = 'modifier_option' AND r.owner_id = o.id
                       AND r.slug IN ('milk', 'coffee_bean')
                  JOIN ln b ON b.owner_type = 'item_size' AND b.owner_id = s.id AND b.slug = r.slug
                       AND b.ingredient_id <> r.ingredient_id
                  LEFT JOIN fam fb ON fb.u = lower(b.unit)
                  LEFT JOIN fam fr ON fr.u = lower(r.unit)
                 WHERE i.org_id = $1 AND i.deleted_at IS NULL AND i.is_active
                   AND fb.f IS DISTINCT FROM fr.f)
            SELECT DISTINCT item_id, item_name, size_label, msg, item_id, group_id FROM swaps
            UNION
            SELECT DISTINCT item_id, item_name, size_label, msg, item_id, group_id FROM follows
             ORDER BY 2, 3, 4"
        ),
    },
    Rule {
        id: "F11",
        severity: LintSeverity::Warn,
        entity_type: "option",
        sql: concat!(
            "WITH ",
            lines!(),
            ",
            fam(u, f) AS (VALUES ('g','mass'),('kg','mass'),('ml','volume'),('l','volume'),('pcs','count'))
            SELECT o.id, g.name || ' / ' || o.name, NULL::text,
                   CASE WHEN fl.f IS DISTINCT FROM fi.f
                        THEN 'Recipe line ' || r.quantity::text || ' ' || r.unit || ' of ' || r.ing_name
                             || ' is not in the ingredient''s unit family (' || r.ing_unit || ')'
                        ELSE 'Recipe line ' || r.quantity::text || ' ' || r.unit || ' of ' || r.ing_name
                             || ' is not in the ingredient''s unit (' || r.ing_unit || ')'
                   END,
                   NULL::uuid, g.id
              FROM ln r
              JOIN modifier_options o ON o.id = r.owner_id AND o.is_active
              JOIN modifier_groups g ON g.id = o.group_id AND g.is_active
              LEFT JOIN fam fl ON fl.u = lower(r.unit)
              LEFT JOIN fam fi ON fi.u = lower(r.ing_unit)
             WHERE r.owner_type = 'modifier_option' AND lower(r.unit) <> lower(r.ing_unit)
             ORDER BY g.name, o.name, r.ing_name"
        ),
    },
    Rule {
        id: "F13",
        severity: LintSeverity::Error,
        entity_type: "ingredient",
        sql: "SELECT oi.id, oi.name, NULL::text,
                     'Options of type ' || g.legacy_addon_type || ' carrying this ingredient have different prices ('
                     || string_agg(g.name || ' / ' || o.name || ' ' || o.price, ', ' ORDER BY o.price, g.name, o.name)
                     || '): the swap price base is ambiguous',
                     NULL::uuid, NULL::uuid
                FROM modifier_options o
                JOIN modifier_groups g ON g.id = o.group_id AND g.is_active
                     AND g.legacy_addon_type IN ('milk_type', 'coffee_type')
                JOIN recipe_lines rl ON rl.owner_type = 'modifier_option' AND rl.owner_id = o.id
                JOIN org_ingredients oi ON oi.id = rl.ingredient_id
               WHERE g.org_id = $1 AND o.is_active
               GROUP BY oi.id, oi.name, g.legacy_addon_type
              HAVING count(DISTINCT o.price) > 1
               ORDER BY oi.name",
    },
    Rule {
        id: "F14",
        severity: LintSeverity::Warn,
        entity_type: "ingredient",
        sql: "SELECT oi.id, oi.name, NULL::text,
                     'Looks like packaging but is in category ' || COALESCE(ic.slug, 'none')
                     || ', not packaging: dine-in orders still deduct it',
                     NULL::uuid, NULL::uuid
                FROM org_ingredients oi
                LEFT JOIN ingredient_categories ic ON ic.id = oi.category_id
               WHERE oi.org_id = $1 AND oi.deleted_at IS NULL AND oi.is_active
                 AND oi.name ~* '\\m(cups?|lids?|straws?|sleeves?)\\M'
                 AND ic.slug IS DISTINCT FROM 'packaging'
               ORDER BY oi.name",
    },
    Rule {
        id: "F15",
        severity: LintSeverity::Warn,
        entity_type: "item",
        sql: concat!(
            "WITH ",
            lines!(),
            ",
            sized AS (
                SELECT i.id AS item_id, i.name AS item_name, s.id AS size_id, s.label, s.sort,
                       (SELECT count(*) FROM ln WHERE ln.owner_type = 'item_size' AND ln.owner_id = s.id) AS n_lines,
                       EXISTS (SELECT 1 FROM ln WHERE ln.owner_type = 'item_size' AND ln.owner_id = s.id
                                                  AND ln.slug = 'packaging') AS has_pack
                  FROM menu_items i
                  JOIN menu_item_sizes s ON s.menu_item_id = i.id AND s.is_active
                 WHERE i.org_id = $1 AND i.deleted_at IS NULL AND i.is_active
                   AND (SELECT count(*) FROM menu_item_sizes z WHERE z.menu_item_id = i.id AND z.is_active) > 1)
            SELECT a.item_id, a.item_name, a.label,
                   CASE WHEN a.n_lines = 0 THEN 'This size has no recipe lines: nothing is deducted'
                        ELSE 'Other sizes deduct packaging but this one does not' END,
                   a.item_id, NULL::uuid
              FROM sized a
             WHERE a.n_lines = 0
                OR (NOT a.has_pack AND EXISTS (SELECT 1 FROM sized b
                                                WHERE b.item_id = a.item_id AND b.has_pack))
             ORDER BY a.item_name, a.sort, a.label"
        ),
    },
];

/// Run every rule for one org. Findings are ordered by rule, then as each rule orders them.
pub async fn lint_org(pool: &PgPool, org_id: Uuid) -> Result<Vec<LintIssue>, AppError> {
    lint_org_group(pool, org_id, None).await
}

/// [`lint_org`] restricted to findings about one modifier group (`group_id = gid`).
pub async fn lint_org_group(
    pool: &PgPool,
    org_id: Uuid,
    group_id: Option<Uuid>,
) -> Result<Vec<LintIssue>, AppError> {
    let mut out = Vec::new();
    for rule in RULES {
        let rows: Vec<Row> = sqlx::query_as(rule.sql)
            .bind(org_id)
            .fetch_all(pool)
            .await?;
        out.extend(
            rows.into_iter()
                .filter(|row| group_id.is_none() || row.5 == group_id)
                .map(
                    |(entity_id, entity_name, size_label, message, item_id, group_id)| LintIssue {
                        rule: rule.id.to_string(),
                        severity: rule.severity,
                        entity_type: rule.entity_type.to_string(),
                        entity_id,
                        entity_name,
                        message,
                        size_label,
                        item_id,
                        group_id,
                    },
                ),
        );
    }
    Ok(out)
}

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

#[utoipa::path(
    get,
    path = "/menu/lint",
    tag = "menu",
    params(LintQuery),
    responses(
        (status = 200, description = "Menu modeling findings for the org (empty = clean)", body = [LintIssue]),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_menu_lint(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<LintQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;
    let issues = lint_org_group(pool.get_ref(), query.org_id, query.group_id).await?;
    Ok(HttpResponse::Ok().json(issues))
}

