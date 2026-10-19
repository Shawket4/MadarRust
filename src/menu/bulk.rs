//! `POST /menu/bulk`: a whole menu in one request, for the self-serve import
//! (Madar `MENU_IMPORT_PLAN.md`).
//!
//! The body is a [`madar_menu::BulkMenu`] the dashboard built in the browser
//! (a Foodics export read by madar-menu's `foodics` reader, which this server
//! never compiles). The server runs madar-menu's [`madar_menu::validate`] and
//! then writes the menu in ONE transaction, in a fixed number of set-based
//! statements however big the menu is (ids are made here, so no row's id
//! depends on the order `RETURNING` hands them back).
//!
//! - `mode: add` writes into an org whose menu is empty (no live category, no
//!   live item, no active group), else 409 `MENU_NOT_EMPTY`.
//! - `mode: replace` first archives the menu there is: items and categories
//!   stamped `deleted_at` (their open price epochs closed), groups switched
//!   off — what the dashboard's own deletes do. Orders, tickets and reports
//!   keep every row: order lines carry their own name and price. The owner
//!   must send the sentence `madar_menu::replace_sentence` shows, checked
//!   here (`madar_menu::confirms_replace`), else 422
//!   `MENU_REPLACE_NOT_CONFIRMED`.
//! - `?dry_run=true` does all of it and rolls back: the preview gets the
//!   server's verdict, the database's included, and nothing is kept.
//!
//! Every real import is a `menu_imports` row; an org may run
//! [`IMPORTS_PER_DAY`] a day. The catalog revision and the POS changefeed
//! move by their triggers, as for every catalog write. No recipe lines are
//! written, and names are not translated: the Arabic is what the file had.

use std::collections::HashMap;

use actix_web::{HttpRequest, HttpResponse, web};
use madar_menu::{BulkMenu, BulkSummary};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::errors::{AppError, AppErrorResponse};
use crate::menu::bases::{claims_org, extract_claims};
use crate::permissions::checker::check_permission;

/// The largest body the route reads. A full 2,000-item menu with long
/// descriptions is well under it.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Real imports one org may run in 24 hours (dry runs are free).
pub const IMPORTS_PER_DAY: i64 = 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BulkMode {
    /// Into an empty menu.
    #[default]
    Add,
    /// Archive the menu there is, then write this one.
    Replace,
}

impl BulkMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Replace => "replace",
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BulkMenuRequest {
    pub menu: BulkMenu,
    #[serde(default)]
    pub mode: BulkMode,
    /// `replace` only: the sentence the owner typed
    /// (`madar_menu::replace_sentence`, either language).
    #[serde(default)]
    pub confirm: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct BulkQuery {
    /// Check and write, then roll back: nothing is kept.
    #[serde(default)]
    pub dry_run: bool,
}

/// What a `replace` archived.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct BulkArchived {
    pub categories: i64,
    pub items: i64,
    pub groups: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BulkMenuResponse {
    pub dry_run: bool,
    pub mode: BulkMode,
    /// What was written (or, on a dry run, would be).
    pub summary: BulkSummary,
    /// `replace` only.
    pub archived: Option<BulkArchived>,
    /// The `menu_imports` row; `None` on a dry run.
    pub import_id: Option<Uuid>,
}

/// The menu's rows with their ids, flattened column by column for `UNNEST`.
#[derive(Default)]
struct Rows {
    category_ids: Vec<Uuid>,
    category_names: Vec<String>,
    category_ar: Vec<Option<String>>,

    group_ids: Vec<Uuid>,
    group_names: Vec<String>,
    group_ar: Vec<Option<String>>,
    group_min: Vec<i32>,
    group_max: Vec<Option<i32>>,
    group_family: Vec<Option<String>>,

    option_group: Vec<Uuid>,
    option_names: Vec<String>,
    option_ar: Vec<Option<String>>,
    option_price: Vec<i32>,
    option_sort: Vec<i32>,
    option_active: Vec<bool>,
    option_source: Vec<String>,

    item_ids: Vec<Uuid>,
    item_category: Vec<Uuid>,
    item_names: Vec<String>,
    item_ar: Vec<Option<String>>,
    item_description: Vec<Option<String>>,
    item_description_ar: Vec<Option<String>>,
    item_price: Vec<i32>,
    item_active: Vec<bool>,

    size_item: Vec<Uuid>,
    size_label: Vec<String>,
    size_price: Vec<i32>,
    size_sort: Vec<i32>,
    size_active: Vec<bool>,

    link_item: Vec<Uuid>,
    link_group: Vec<Uuid>,
    link_sort: Vec<i32>,
    link_min: Vec<Option<i32>>,
    link_max: Vec<Option<i32>>,
    link_required: Vec<Option<bool>>,

    epoch_item: Vec<Uuid>,
    epoch_label: Vec<Option<String>>,
    epoch_price: Vec<i32>,
}

fn int(v: impl TryInto<i32>) -> Result<i32, AppError> {
    // validate() bounds every price and count far inside i32.
    v.try_into().map_err(|_| AppError::Internal)
}

fn text(s: &str) -> String {
    s.trim().to_string()
}

fn opt_text(s: Option<&String>) -> Option<String> {
    s.map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Rows {
    /// Lays a validated menu out as rows. Each item's optionals become its
    /// own private "Options" group, as the dashboard's item Options do.
    fn of(menu: &BulkMenu) -> Result<Self, AppError> {
        let mut r = Rows::default();
        let mut categories = HashMap::new();
        for c in &menu.categories {
            let id = Uuid::new_v4();
            categories.insert(c.key.trim(), id);
            r.category_ids.push(id);
            r.category_names.push(text(&c.name));
            r.category_ar.push(opt_text(c.name_ar.as_ref()));
        }

        let mut groups = HashMap::new();
        for g in &menu.groups {
            let id = Uuid::new_v4();
            let active = g.options.iter().filter(|o| o.is_active).count();
            groups.insert(g.key.trim(), (id, g, active));
            r.push_group(
                id,
                &g.name,
                g.name_ar.as_ref(),
                g.min,
                g.max,
                Some(g.family.as_str()),
            )?;
            for (sort, o) in g.options.iter().enumerate() {
                r.push_option(
                    id,
                    &o.name,
                    o.name_ar.as_ref(),
                    o.price,
                    sort,
                    o.is_active,
                    "addon",
                )?;
            }
        }

        for it in &menu.items {
            let id = Uuid::new_v4();
            let category = *categories
                .get(it.category.trim())
                .ok_or(AppError::Internal)?;
            r.item_ids.push(id);
            r.item_category.push(category);
            r.item_names.push(text(&it.name));
            r.item_ar.push(opt_text(it.name_ar.as_ref()));
            r.item_description.push(opt_text(it.description.as_ref()));
            r.item_description_ar
                .push(opt_text(it.description_ar.as_ref()));
            r.item_price.push(int(it.price)?);
            r.item_active.push(it.is_active);
            // The item's own price epoch, as `POST /menu-items` writes it.
            r.epoch_item.push(id);
            r.epoch_label.push(None);
            r.epoch_price.push(int(it.price)?);

            for (sort, z) in it.sizes.iter().enumerate() {
                r.size_item.push(id);
                r.size_label.push(text(&z.label));
                r.size_price.push(int(z.price)?);
                r.size_sort.push(int(sort)?);
                r.size_active.push(z.is_active);
                r.epoch_item.push(id);
                r.epoch_label.push(Some(text(&z.label)));
                r.epoch_price.push(int(z.price)?);
            }

            for (sort, l) in it.groups.iter().enumerate() {
                let (gid, g, active) = groups.get(l.group.trim()).ok_or(AppError::Internal)?;
                // An override only where the item differs from the group, as
                // the admin import writes it. The schema has no "unlimited"
                // override, so an item allowing every option says so with the
                // option count.
                let differs_min = l.min != g.min;
                let max = match (l.max, g.max) {
                    (a, b) if a == b => None,
                    (Some(m), _) => Some(int(m)?),
                    (None, _) => Some(int(*active)?.max(1)),
                };
                r.link_item.push(id);
                r.link_group.push(*gid);
                r.link_sort.push(int(sort)?);
                r.link_min
                    .push(if differs_min { Some(int(l.min)?) } else { None });
                r.link_max.push(max);
                r.link_required.push(differs_min.then_some(l.min > 0));
            }

            if !it.optionals.is_empty() {
                let gid = Uuid::new_v4();
                r.push_group(gid, "Options", None, 0, None, None)?;
                for (sort, o) in it.optionals.iter().enumerate() {
                    r.push_option(
                        gid,
                        &o.name,
                        o.name_ar.as_ref(),
                        o.price,
                        sort,
                        true,
                        "optional",
                    )?;
                }
                r.link_item.push(id);
                r.link_group.push(gid);
                r.link_sort.push(int(it.groups.len())?);
                r.link_min.push(None);
                r.link_max.push(None);
                r.link_required.push(None);
            }
        }
        Ok(r)
    }

    fn push_group(
        &mut self,
        id: Uuid,
        name: &str,
        name_ar: Option<&String>,
        min: u32,
        max: Option<u32>,
        family: Option<&str>,
    ) -> Result<(), AppError> {
        self.group_ids.push(id);
        self.group_names.push(text(name));
        self.group_ar.push(opt_text(name_ar));
        self.group_min.push(int(min)?);
        self.group_max.push(max.map(int).transpose()?);
        self.group_family.push(family.map(str::to_string));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_option(
        &mut self,
        group: Uuid,
        name: &str,
        name_ar: Option<&String>,
        price: i64,
        sort: usize,
        is_active: bool,
        source: &str,
    ) -> Result<(), AppError> {
        self.option_group.push(group);
        self.option_names.push(text(name));
        self.option_ar.push(opt_text(name_ar));
        self.option_price.push(int(price)?);
        self.option_sort.push(int(sort)?);
        self.option_active.push(is_active);
        self.option_source.push(source.to_string());
        Ok(())
    }
}

fn body_sha256(menu: &BulkMenu) -> Result<String, AppError> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(menu).map_err(|_| AppError::Internal)?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[utoipa::path(
    post,
    path = "/menu/bulk",
    tag = "menu",
    params(BulkQuery),
    request_body = BulkMenuRequest,
    responses(
        (status = 200, description = "Dry run: checked and rolled back", body = BulkMenuResponse),
        (status = 201, description = "The menu was written", body = BulkMenuResponse),
        (status = 409, description = "MENU_NOT_EMPTY (add into a menu that is not empty; `vars` counts it)"),
        (status = 422, description = "BULK_MENU_INVALID / BULK_MENU_UNSUPPORTED_VERSION (`vars` is madar-menu's BulkRefusal), MENU_REPLACE_NOT_CONFIRMED (`vars` has the sentence in EN and AR)"),
        (status = 429, description = "MENU_IMPORT_LIMIT (an org's imports a day) or RATE_LIMITED"),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn bulk_menu(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BulkQuery>,
    body: web::Json<BulkMenuRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let org_id = claims_org(&claims)?;
    let body = body.into_inner();
    let dry_run = query.dry_run;

    let mut cells = vec![
        ("categories", "create"),
        ("menu_items", "create"),
        ("menu_items", "update"),
    ];
    if body.mode == BulkMode::Replace {
        cells.extend([("categories", "delete"), ("menu_items", "delete")]);
    }
    for (resource, action) in cells {
        check_permission(pool.get_ref(), &claims, resource, action).await?;
    }

    let summary = madar_menu::validate(&body.menu).map_err(|r| AppError::CodedVars {
        status: 422,
        code: r.code(),
        reason: r.to_string(),
        vars: serde_json::to_value(&r).unwrap_or_default(),
    })?;

    let mut tx = pool.begin().await?;
    // One import at a time per org: the emptiness check and the writes below
    // must not interleave with another import's.
    sqlx::query!(
        r#"SELECT true AS "locked!" FROM pg_advisory_xact_lock(hashtextextended('menu_bulk:' || $1::uuid::text, 0))"#,
        org_id
    )
    .fetch_one(&mut *tx)
    .await?;

    if !dry_run {
        let today = sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM menu_imports
                WHERE org_id = $1 AND created_at > now() - interval '1 day'"#,
            org_id
        )
        .fetch_one(&mut *tx)
        .await?;
        if today >= IMPORTS_PER_DAY {
            return Err(AppError::TooManyRequests {
                code: "MENU_IMPORT_LIMIT",
                reason: format!("An organization may import a menu {IMPORTS_PER_DAY} times a day."),
            });
        }
    }

    let live = sqlx::query!(
        r#"SELECT
             (SELECT count(*) FROM categories WHERE org_id = $1 AND deleted_at IS NULL) AS "categories!",
             (SELECT count(*) FROM menu_items WHERE org_id = $1 AND deleted_at IS NULL) AS "items!",
             (SELECT count(*) FROM modifier_groups WHERE org_id = $1 AND is_active) AS "groups!""#,
        org_id
    )
    .fetch_one(&mut *tx)
    .await?;

    let archived = match body.mode {
        BulkMode::Add => {
            if live.categories + live.items + live.groups > 0 {
                return Err(AppError::CodedVars {
                    status: 409,
                    code: "MENU_NOT_EMPTY",
                    reason: "The menu already has categories, items or groups; replace it instead."
                        .into(),
                    vars: serde_json::json!({
                        "categories": live.categories,
                        "items": live.items,
                        "groups": live.groups,
                    }),
                });
            }
            None
        }
        BulkMode::Replace => {
            if !dry_run {
                let org_name =
                    sqlx::query_scalar!("SELECT name FROM organizations WHERE id = $1", org_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let typed = body.confirm.as_deref().unwrap_or("");
                if !madar_menu::confirms_replace(&org_name, typed) {
                    return Err(AppError::CodedVars {
                        status: 422,
                        code: "MENU_REPLACE_NOT_CONFIRMED",
                        reason: "Type the sentence shown to replace the whole menu.".into(),
                        vars: serde_json::json!({
                            "sentence_en": madar_menu::replace_sentence(&org_name, "en"),
                            "sentence_ar": madar_menu::replace_sentence(&org_name, "ar"),
                        }),
                    });
                }
            }
            Some(archive(&mut tx, org_id).await?)
        }
    };

    write(&mut tx, org_id, claims.user_id(), &Rows::of(&body.menu)?).await?;

    if dry_run {
        tx.rollback().await?;
        return Ok(HttpResponse::Ok().json(BulkMenuResponse {
            dry_run,
            mode: body.mode,
            summary,
            archived,
            import_id: None,
        }));
    }

    let import_id = sqlx::query_scalar!(
        "INSERT INTO menu_imports (org_id, user_id, mode, summary, archived, body_sha256)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        org_id,
        claims.user_id(),
        body.mode.as_str(),
        serde_json::to_value(summary).map_err(|_| AppError::Internal)?,
        archived.map(|a| serde_json::json!(a)),
        body_sha256(&body.menu)?,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(HttpResponse::Created().json(BulkMenuResponse {
        dry_run,
        mode: body.mode,
        summary,
        archived,
        import_id: Some(import_id),
    }))
}

/// The menu there is, put away the way the dashboard's deletes do it.
async fn archive(tx: &mut sqlx::PgConnection, org_id: Uuid) -> Result<BulkArchived, AppError> {
    sqlx::query!(
        "UPDATE menu_item_price_epochs e SET effective_until = now()
           FROM menu_items m
          WHERE e.menu_item_id = m.id AND m.org_id = $1 AND m.deleted_at IS NULL
            AND e.effective_until IS NULL",
        org_id
    )
    .execute(&mut *tx)
    .await?;
    let items = sqlx::query!(
        "UPDATE menu_items SET deleted_at = now() WHERE org_id = $1 AND deleted_at IS NULL",
        org_id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let categories = sqlx::query!(
        "UPDATE categories SET deleted_at = now() WHERE org_id = $1 AND deleted_at IS NULL",
        org_id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    // Off, not deleted: an option an old order line names keeps resolving.
    let groups = sqlx::query!(
        "UPDATE modifier_groups SET is_active = false, updated_at = now() WHERE org_id = $1 AND is_active",
        org_id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let count = |n: u64| i64::try_from(n).map_err(|_| AppError::Internal);
    Ok(BulkArchived {
        categories: count(categories)?,
        items: count(items)?,
        groups: count(groups)?,
    })
}

/// The new menu, one statement per table. Options go in before the item
/// links: a link's `included_option_ids` is filled from the group's options
/// by trigger.
async fn write(
    tx: &mut sqlx::PgConnection,
    org_id: Uuid,
    user_id: Uuid,
    r: &Rows,
) -> Result<(), AppError> {
    sqlx::query!(
        "INSERT INTO categories (id, org_id, name, name_translations, display_order)
         SELECT id, $1, name,
                CASE WHEN ar IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('ar', ar) END,
                (ord - 1)::int
           FROM UNNEST($2::uuid[], $3::text[], $4::text[]) WITH ORDINALITY AS t(id, name, ar, ord)",
        org_id,
        &r.category_ids,
        &r.category_names,
        &r.category_ar as &[Option<String>],
    )
    .execute(&mut *tx)
    .await?;

    // A one-pick group is `single`; a swap family (milk, beans) is inferred
    // from legacy_addon_type, as the admin import writes it.
    sqlx::query!(
        "INSERT INTO modifier_groups
             (id, org_id, name, name_translations, selection_type, min_selections,
              max_selections, is_required, legacy_addon_type, sort)
         SELECT id, $1, name,
                CASE WHEN ar IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('ar', ar) END,
                CASE WHEN mx = 1 THEN 'single' ELSE 'multi' END,
                mn, mx, mn > 0, fam, (ord - 1)::int
           FROM UNNEST($2::uuid[], $3::text[], $4::text[], $5::int4[], $6::int4[], $7::text[])
                WITH ORDINALITY AS t(id, name, ar, mn, mx, fam, ord)",
        org_id,
        &r.group_ids,
        &r.group_names,
        &r.group_ar as &[Option<String>],
        &r.group_min,
        &r.group_max as &[Option<i32>],
        &r.group_family as &[Option<String>],
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        "INSERT INTO modifier_options
             (org_id, group_id, name, name_translations, price, sort, is_active, legacy_source)
         SELECT $1, g, name,
                CASE WHEN ar IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('ar', ar) END,
                price, sort, active, src
           FROM UNNEST($2::uuid[], $3::text[], $4::text[], $5::int4[], $6::int4[], $7::bool[], $8::text[])
                AS t(g, name, ar, price, sort, active, src)",
        org_id,
        &r.option_group,
        &r.option_names,
        &r.option_ar as &[Option<String>],
        &r.option_price,
        &r.option_sort,
        &r.option_active,
        &r.option_source,
    )
    .execute(&mut *tx)
    .await?;

    // Each item is born with its hidden `one_size` row (trigger); its first
    // real active size below retires it (trigger), and base_price follows
    // the lowest active size (trigger).
    sqlx::query!(
        "INSERT INTO menu_items
             (id, org_id, category_id, name, name_translations, description,
              description_translations, base_price, is_active)
         SELECT id, $1, cat, name,
                CASE WHEN ar IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('ar', ar) END,
                descr,
                CASE WHEN dar IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('ar', dar) END,
                price, active
           FROM UNNEST($2::uuid[], $3::uuid[], $4::text[], $5::text[], $6::text[], $7::text[],
                       $8::int4[], $9::bool[])
                AS t(id, cat, name, ar, descr, dar, price, active)",
        org_id,
        &r.item_ids,
        &r.item_category,
        &r.item_names,
        &r.item_ar as &[Option<String>],
        &r.item_description as &[Option<String>],
        &r.item_description_ar as &[Option<String>],
        &r.item_price,
        &r.item_active,
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort, is_active)
         SELECT item, label, price, sort, active
           FROM UNNEST($1::uuid[], $2::text[], $3::int4[], $4::int4[], $5::bool[])
                AS t(item, label, price, sort, active)",
        &r.size_item,
        &r.size_label,
        &r.size_price,
        &r.size_sort,
        &r.size_active,
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        "INSERT INTO menu_item_price_epochs (menu_item_id, size_label, price, effective_from, changed_by)
         SELECT item, label, price, now(), $1
           FROM UNNEST($2::uuid[], $3::text[], $4::int4[]) AS t(item, label, price)",
        user_id,
        &r.epoch_item,
        &r.epoch_label as &[Option<String>],
        &r.epoch_price,
    )
    .execute(&mut *tx)
    .await?;

    // legacy_origin and included_option_ids are filled by trigger
    // (`mimg_fill_provenance`): slot / allowlist / options, every option.
    sqlx::query!(
        "INSERT INTO menu_item_modifier_groups
             (menu_item_id, group_id, sort, min_override, max_override, is_required_override)
         SELECT item, grp, sort, mn, mx, req
           FROM UNNEST($1::uuid[], $2::uuid[], $3::int4[], $4::int4[], $5::int4[], $6::bool[])
                AS t(item, grp, sort, mn, mx, req)",
        &r.link_item,
        &r.link_group,
        &r.link_sort,
        &r.link_min as &[Option<i32>],
        &r.link_max as &[Option<i32>],
        &r.link_required as &[Option<bool>],
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}
