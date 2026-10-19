//! Shop readiness — can this shop sell yet, and what exactly stops it.
//!
//! Replaces the counted onboarding checklist (`onboarding.rs`, now a frozen shim
//! for released Flutter dashboards). Nothing is stored: every read derives the
//! answer from the shop's live rows, so it can never disagree with the shop.
//!
//! A check is `blocking` when a cashier cannot ring up and take payment without
//! it, `advice` otherwise. `ready` = every blocking check passes; `live` = the
//! shop has an order. The POS checks apply only to an org with the `pos` module
//! (a Dawam-only org gets none); the recipe and cost checks only to a plan with
//! `inventory`. The menu advice is the menu lint (`menu/lint.rs`), so the Setup
//! page and the menu editor never disagree.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::guards::require_same_org,
    authz::Cap,
    errors::AppError,
    menu::lint::{LintIssue, LintSeverity, lint_org},
    permissions::checker::check_permission,
};

use super::handlers::extract_claims;

/// At most this many named problems per check; the rest are counted in `more`.
const SHOWN: usize = 20;

/// Lint rules about attachments, which matter on every plan. The others are
/// about recipes, swaps and stock, which only a plan with inventory uses.
const PLAN_FREE_RULES: [&str; 3] = ["F1", "F2", "F3"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CheckLevel {
    /// The POS cannot sell until it passes.
    Blocking,
    /// Worth doing; never stops a sale.
    Advice,
}

/// One named thing a check found wrong: a branch, an item, an attachment, an ingredient.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReadinessProblem {
    /// `branch` | `item` | `attachment` | `option` | `ingredient`.
    pub entity_type: String,
    pub entity_id: Uuid,
    pub name: String,
    /// The menu lint rule (`F1`…) when the problem comes from the lint.
    pub rule: Option<String>,
    /// The lint's sentence, English, for the menu-lint problems only.
    pub message: Option<String>,
    /// The item to open, when the problem is about one.
    pub item_id: Option<Uuid>,
    /// The modifier group to open, when the problem is about one.
    pub group_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReadinessCheck {
    /// Stable key the dashboards switch on, never localized: `branch`, `menu`,
    /// `payment`, `sign_in`, `tablet`, `items_no_category`, `items_zero_price`,
    /// `required_group_empty`, `menu_lint`, `recipes`, `ingredient_costs`, `team`, `receipt`.
    pub key: String,
    pub level: CheckLevel,
    pub ok: bool,
    /// How many things the check counted: problems when it names them, else the
    /// things found (branches, sellable items, people).
    pub count: i64,
    /// What `count` is out of, when that reads better as a share (recipes: of active items).
    pub total: Option<i64>,
    /// Up to 20 named problems.
    pub problems: Vec<ReadinessProblem>,
    /// Problems past the first 20.
    pub more: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Readiness {
    pub org_id: Uuid,
    /// Every blocking check passes: a cashier can ring up and take payment.
    pub ready: bool,
    /// The shop has taken an order.
    pub live: bool,
    pub first_order_at: Option<DateTime<Utc>>,
    /// Blocking checks first, then advice. Empty for an org without the POS.
    pub checks: Vec<ReadinessCheck>,
}

// ── GET /orgs/{id}/readiness ──────────────────────────────────

#[utoipa::path(
    get,
    path = "/orgs/{id}/readiness",
    tag = "orgs",
    params(("id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Whether the shop can sell, and what stops it", body = Readiness), crate::errors::AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_readiness(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let org_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "read").await?;
    require_same_org(&claims, Some(org_id))?;
    Ok(HttpResponse::Ok().json(load(pool.get_ref(), org_id).await?))
}

/// The readiness of one org. Public for the tests and the onboarding shim.
pub async fn load(pool: &PgPool, org_id: Uuid) -> Result<Readiness, AppError> {
    let org = sqlx::query!(
        r#"SELECT modules, logo_url IS NOT NULL AS "logo_set!" FROM organizations WHERE id = $1"#,
        org_id
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Organization not found".into()))?;
    let (modules, logo_set) = (org.modules, org.logo_set);

    let first_order_at: Option<DateTime<Utc>> = sqlx::query_scalar!(
        "SELECT min(f.created_at) FROM branches b
           CROSS JOIN LATERAL (SELECT o.created_at FROM orders o WHERE o.branch_id = b.id
                                ORDER BY o.created_at LIMIT 1) f
          WHERE b.org_id = $1",
        org_id
    )
    .fetch_one(pool)
    .await?;

    let checks = if modules.iter().any(|m| m == "pos") {
        let inventory =
            crate::plan::org_has_feature(pool, org_id, madar_plan::Feature::Inventory).await?;
        pos_checks(pool, org_id, inventory, logo_set).await?
    } else {
        Vec::new()
    };

    Ok(Readiness {
        org_id,
        ready: checks.iter().all(|c| c.ok || c.level == CheckLevel::Advice),
        live: first_order_at.is_some(),
        first_order_at,
        checks,
    })
}

async fn pos_checks(
    pool: &PgPool,
    org_id: Uuid,
    inventory: bool,
    logo_set: bool,
) -> Result<Vec<ReadinessCheck>, AppError> {
    use CheckLevel::{Advice, Blocking};

    let branches: Vec<(Uuid, String)> = sqlx::query!(
        "SELECT id, name FROM branches
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL ORDER BY name",
        org_id
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (r.id, r.name))
    .collect();

    // Items on a tile: active, in an active category (the POS feed sends nothing else).
    let sellable: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM menu_items i
           JOIN categories c ON c.id = i.category_id AND c.is_active AND c.deleted_at IS NULL
          WHERE i.org_id = $1 AND i.deleted_at IS NULL AND i.is_active"#,
        org_id
    )
    .fetch_one(pool)
    .await?;

    // A branch takes payment when a tender is left after its allow-list (the
    // order path's rule; a person's or a tablet's own list narrows it further).
    let mut no_tender = Vec::new();
    for (id, name) in &branches {
        let names = crate::payment_methods::availability::effective_method_names(
            pool, org_id, *id, None, None,
        )
        .await?;
        if names.is_empty() {
            no_tender.push(branch_problem(*id, name));
        }
    }

    // A branch can sign in when someone with a PIN may sign in, open a till and
    // sell there: the PIN login's own test (auth/handlers.rs).
    let holders: Vec<Uuid> = sqlx::query_scalar(&format!(
        "SELECT u.id FROM users u WHERE {}",
        crate::auth::handlers::PIN_HOLDER_FILTER
    ))
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    let mut no_signer = Vec::new();
    for (id, name) in &branches {
        let mut any = false;
        for user in &holders {
            let eff = crate::authz::require::effective(pool, *user, Some(*id)).await?;
            if eff.can(Cap::PosSignIn) && eff.can(Cap::TillOpen) && eff.can(Cap::OrdersCreate) {
                any = true;
                break;
            }
        }
        if !any {
            no_signer.push(branch_problem(*id, name));
        }
    }

    // A tablet is bound by an activation code (a `devices` row) or by the
    // owner's login on a build that never registers one (seen in `client_seen`).
    let no_tablet = problems(
        "branch",
        sqlx::query_as!(
            Named,
            "SELECT b.id, b.name FROM branches b
          WHERE b.org_id = $1 AND b.is_active AND b.deleted_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM devices d
                             WHERE d.branch_id = b.id AND d.kind = 'pos'
                               AND d.retired_at IS NULL AND d.plan_inactive IS NOT TRUE)
            AND NOT EXISTS (SELECT 1 FROM client_seen s
                             WHERE s.org_id = $1 AND s.branch_id = b.id AND s.client LIKE 'pos/%')
          ORDER BY b.name",
            org_id
        )
        .fetch_all(pool)
        .await?,
    );

    let no_category = problems(
        "item",
        sqlx::query_as!(
            Named,
            "SELECT i.id, i.name FROM menu_items i
           LEFT JOIN categories c ON c.id = i.category_id AND c.is_active AND c.deleted_at IS NULL
          WHERE i.org_id = $1 AND i.deleted_at IS NULL AND i.is_active AND c.id IS NULL
          ORDER BY i.name",
            org_id
        )
        .fetch_all(pool)
        .await?,
    );

    let zero_price = problems(
        "item",
        sqlx::query_as!(
            Named,
            "SELECT i.id, i.name FROM menu_items i
          WHERE i.org_id = $1 AND i.deleted_at IS NULL AND i.is_active AND i.base_price = 0
            AND NOT EXISTS (SELECT 1 FROM menu_item_sizes s
                             WHERE s.menu_item_id = i.id AND s.is_active AND s.price > 0)
          ORDER BY i.name",
            org_id
        )
        .fetch_all(pool)
        .await?,
    );

    let lint = lint_org(pool, org_id).await?;
    let empty_required: Vec<ReadinessProblem> = lint
        .iter()
        .filter(|i| i.rule == "F16")
        .map(lint_problem)
        .collect();
    let menu_lint: Vec<ReadinessProblem> = lint
        .iter()
        .filter(|i| i.severity == LintSeverity::Error && i.rule != "F16")
        .filter(|i| inventory || PLAN_FREE_RULES.contains(&i.rule.as_str()))
        .map(lint_problem)
        .collect();

    let team: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM users
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL
            AND role <> 'super_admin' AND NOT is_guest_principal"#,
        org_id
    )
    .fetch_one(pool)
    .await?;

    let mut checks = vec![
        present("branch", Blocking, branches.len() as i64),
        present("menu", Blocking, sellable),
        per_branch("payment", &branches, no_tender),
        per_branch("sign_in", &branches, no_signer),
        per_branch("tablet", &branches, no_tablet),
        listed("items_no_category", Advice, no_category, None),
        listed("items_zero_price", Advice, zero_price, None),
        listed("required_group_empty", Advice, empty_required, None),
        listed("menu_lint", Advice, menu_lint, None),
    ];

    if inventory {
        let active: i64 = sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM menu_items WHERE org_id = $1 AND deleted_at IS NULL AND is_active"#,
            org_id
        )
        .fetch_one(pool)
        .await?;
        // An item is costed when every size it sells has a recipe.
        let no_recipe = problems(
            "item",
            sqlx::query_as!(
                Named,
                "SELECT i.id, i.name FROM menu_items i
              WHERE i.org_id = $1 AND i.deleted_at IS NULL AND i.is_active
                AND (NOT EXISTS (SELECT 1 FROM menu_item_sizes s
                                  WHERE s.menu_item_id = i.id AND s.is_active)
                     OR EXISTS (SELECT 1 FROM menu_item_sizes s
                                 WHERE s.menu_item_id = i.id AND s.is_active
                                   AND NOT EXISTS (SELECT 1 FROM recipe_lines r
                                                    WHERE r.owner_type = 'item_size'
                                                      AND r.owner_id = s.id)))
              ORDER BY i.name",
                org_id
            )
            .fetch_all(pool)
            .await?,
        );
        let free_ingredients = problems(
            "ingredient",
            sqlx::query_as!(
                Named,
                "SELECT oi.id, oi.name FROM org_ingredients oi
              WHERE oi.org_id = $1 AND oi.deleted_at IS NULL AND oi.is_active
                AND oi.archived_at IS NULL AND COALESCE(oi.cost_per_unit, 0) = 0
                AND EXISTS (SELECT 1 FROM recipe_lines r WHERE r.ingredient_id = oi.id)
              ORDER BY oi.name",
                org_id
            )
            .fetch_all(pool)
            .await?,
        );
        checks.push(listed("recipes", Advice, no_recipe, Some(active)));
        checks.push(listed("ingredient_costs", Advice, free_ingredients, None));
    }

    checks.push(present("team", Advice, team - 1).with_count(team));
    checks.push(present("receipt", Advice, i64::from(logo_set)));
    Ok(checks)
}

impl ReadinessCheck {
    fn with_count(mut self, count: i64) -> Self {
        self.count = count;
        self
    }
}

/// Passes when something exists; `count` is how many.
fn present(key: &str, level: CheckLevel, found: i64) -> ReadinessCheck {
    ReadinessCheck {
        key: key.into(),
        level,
        ok: found > 0,
        count: found.max(0),
        total: None,
        problems: Vec::new(),
        more: 0,
    }
}

/// Passes when nothing is named.
fn listed(
    key: &str,
    level: CheckLevel,
    mut problems: Vec<ReadinessProblem>,
    total: Option<i64>,
) -> ReadinessCheck {
    let count = problems.len() as i64;
    problems.truncate(SHOWN);
    ReadinessCheck {
        key: key.into(),
        level,
        ok: count == 0,
        count,
        total,
        more: count - problems.len() as i64,
        problems,
    }
}

/// A blocking check over the branches: fails with no branch at all, or naming
/// each branch that fails it.
fn per_branch(
    key: &str,
    branches: &[(Uuid, String)],
    failing: Vec<ReadinessProblem>,
) -> ReadinessCheck {
    let mut c = listed(
        key,
        CheckLevel::Blocking,
        failing,
        Some(branches.len() as i64),
    );
    c.ok = c.ok && !branches.is_empty();
    c
}

/// An (id, name) row of a check's query.
struct Named {
    id: Uuid,
    name: String,
}

/// The rows of one check's query as problems of one type.
fn problems(entity_type: &str, rows: Vec<Named>) -> Vec<ReadinessProblem> {
    rows.into_iter()
        .map(|r| ReadinessProblem {
            entity_type: entity_type.into(),
            item_id: (entity_type == "item").then_some(r.id),
            entity_id: r.id,
            name: r.name,
            rule: None,
            message: None,
            group_id: None,
        })
        .collect()
}

fn branch_problem(id: Uuid, name: &str) -> ReadinessProblem {
    ReadinessProblem {
        entity_type: "branch".into(),
        entity_id: id,
        name: name.into(),
        rule: None,
        message: None,
        item_id: None,
        group_id: None,
    }
}

fn lint_problem(i: &LintIssue) -> ReadinessProblem {
    ReadinessProblem {
        entity_type: i.entity_type.clone(),
        entity_id: i.entity_id,
        name: i.entity_name.clone(),
        rule: Some(i.rule.clone()),
        message: Some(i.message.clone()),
        item_id: i.item_id,
        group_id: i.group_id,
    }
}
