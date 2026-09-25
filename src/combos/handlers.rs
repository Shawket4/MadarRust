//! Combo endpoints (§2.2 as adjusted by §11). Reading uses `menu.items.read`;
//! writing `menu.combos.edit`; selling a combo needs nothing beyond selling.

use std::collections::{HashMap, HashSet};

use actix_web::{HttpRequest, HttpResponse, web};
use rust_decimal::Decimal;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    authz::{Cap, require::require},
    combos::{
        economics::{self, fraction},
        load::{ComboDef, load_combos},
        types::{
            BranchChannelOverride, ChannelOverride, ChannelToggles, Combo, ComboChoice,
            ComboEconomics, ComboEconomicsRequest, ComboGetQuery, ComboListQuery, ComboSettings,
            ComboSettingsWrite, ComboSlot, ComboSlotWrite, ComboSummary, ComboWrite, MealLinkWrite,
            PaginatedCombos, SaleWindow,
        },
    },
    errors::{AppError, AppErrorResponse},
    menu::{
        bases::{claims_org, extract_claims},
        cache::MenuCache,
    },
};

type Cache = Option<web::Data<MenuCache>>;

fn invalidate(cache: &Cache, org: Uuid) {
    if let Some(c) = cache {
        c.invalidate(org);
    }
}

fn slot_invalid(index: usize, field: &str) -> AppError {
    AppError::CodedVars {
        status: 400,
        code: "COMBO_SLOT_INVALID",
        reason: format!("Check slot {} ({field}).", index + 1),
        vars: serde_json::json!({ "slot_index": index, "field": field }),
    }
}

fn window_invalid(index: usize, field: &str, code: &'static str) -> AppError {
    AppError::CodedVars {
        status: 400,
        code,
        reason: format!("Check window {} ({field}).", index + 1),
        vars: serde_json::json!({ "window_index": index, "field": field }),
    }
}

fn parse_time(s: &str) -> Option<chrono::NaiveTime> {
    chrono::NaiveTime::parse_from_str(s, "%H:%M")
        .or_else(|_| chrono::NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .ok()
}

/// Windows are valid for `org` (combos: COMBO_SLOT_INVALID; deals: DEAL_INVALID).
pub(crate) async fn validate_windows(
    conn: &mut PgConnection,
    org: Uuid,
    windows: &[SaleWindow],
    code: &'static str,
) -> Result<(), AppError> {
    for (i, w) in windows.iter().enumerate() {
        if !(1..=127).contains(&w.weekdays) {
            return Err(window_invalid(i, "weekdays", code));
        }
        let s = w.starts_at.as_deref().map(|t| (t, parse_time(t)));
        let e = w.ends_at.as_deref().map(|t| (t, parse_time(t)));
        if let Some((_, None)) = s {
            return Err(window_invalid(i, "starts_at", code));
        }
        if let Some((_, None)) = e {
            return Err(window_invalid(i, "ends_at", code));
        }
        match (s, e) {
            (Some(_), None) => return Err(window_invalid(i, "ends_at", code)),
            (None, Some(_)) => return Err(window_invalid(i, "starts_at", code)),
            (Some((_, a)), Some((_, b))) if a == b => {
                return Err(window_invalid(i, "ends_at", code));
            }
            _ => {}
        }
        if let (Some(f), Some(t)) = (w.valid_from, w.valid_to)
            && f > t
        {
            return Err(window_invalid(i, "valid_to", code));
        }
        if let Some(b) = w.branch_id {
            let ok: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM branches WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL)",
            )
            .bind(b)
            .bind(org)
            .fetch_one(&mut *conn)
            .await?;
            if !ok {
                return Err(window_invalid(i, "branch_id", code));
            }
        }
    }
    Ok(())
}

/// Replace the windows of a combo or a deal (`column` = `combo_item_id` or
/// `deal_rule_id`).
pub(crate) async fn write_windows(
    conn: &mut PgConnection,
    org: Uuid,
    column: &'static str,
    owner: Uuid,
    windows: &[SaleWindow],
) -> Result<(), AppError> {
    debug_assert!(matches!(column, "combo_item_id" | "deal_rule_id"));
    sqlx::query(&format!("DELETE FROM sale_windows WHERE {column} = $1"))
        .bind(owner)
        .execute(&mut *conn)
        .await?;
    for (i, w) in windows.iter().enumerate() {
        sqlx::query(&format!(
            "INSERT INTO sale_windows (org_id, {column}, branch_id, weekdays, starts_at, ends_at, \
                                       valid_from, valid_to, sort) \
             VALUES ($1, $2, $3, $4, $5::time, $6::time, $7, $8, $9)"
        ))
        .bind(org)
        .bind(owner)
        .bind(w.branch_id)
        .bind(w.weekdays)
        .bind(w.starts_at.as_deref())
        .bind(w.ends_at.as_deref())
        .bind(w.valid_from)
        .bind(w.valid_to)
        .bind(i as i32)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Every check a combo write passes before anything is written (§2.7).
async fn validate(conn: &mut PgConnection, org: Uuid, body: &ComboWrite) -> Result<(), AppError> {
    if body.name.trim().is_empty() {
        return Err(AppError::BadRequest("A combo needs a name".into()));
    }
    if body.price < 0 {
        return Err(AppError::BadRequest("The price can't be negative".into()));
    }
    if body.slots.is_empty() {
        return Err(AppError::Coded {
            status: 400,
            code: "COMBO_SLOTS_REQUIRED",
            reason: "Add at least one slot.".into(),
        });
    }
    // The items the slots name: live items of this org, never a combo.
    let ids: Vec<Uuid> = body
        .slots
        .iter()
        .flat_map(|s| {
            s.choices
                .iter()
                .filter_map(|c| c.menu_item_id)
                .chain(s.default_item_id)
        })
        .collect();
    let items: HashMap<Uuid, (String, Option<Uuid>)> =
        sqlx::query_as::<_, (Uuid, String, Option<Uuid>)>(
            "SELECT id, kind, category_id FROM menu_items \
              WHERE org_id = $1 AND id = ANY($2) AND deleted_at IS NULL",
        )
        .bind(org)
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|(id, k, c)| (id, (k, c)))
        .collect();
    let cats: Vec<Uuid> = body
        .slots
        .iter()
        .flat_map(|s| s.choices.iter().filter_map(|c| c.category_id))
        .collect();
    let known_cats: HashSet<Uuid> = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM categories WHERE org_id = $1 AND id = ANY($2) AND deleted_at IS NULL",
    )
    .bind(org)
    .bind(&cats)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();

    for (i, s) in body.slots.iter().enumerate() {
        if s.name.trim().is_empty() {
            return Err(slot_invalid(i, "name"));
        }
        if !(0..=10).contains(&s.min) {
            return Err(slot_invalid(i, "min"));
        }
        if !(1..=10).contains(&s.max) {
            return Err(slot_invalid(i, "max"));
        }
        if s.min > s.max {
            return Err(slot_invalid(i, "min"));
        }
        if s.choices.is_empty() {
            return Err(slot_invalid(i, "choices"));
        }
        let mut seen_items = HashSet::new();
        let mut seen_cats = HashSet::new();
        for c in &s.choices {
            if c.surcharge < 0 || c.size_surcharges.iter().any(|z| z.surcharge < 0) {
                return Err(slot_invalid(i, "surcharge"));
            }
            match (c.menu_item_id, c.category_id) {
                (Some(item), None) => {
                    let Some((kind, _)) = items.get(&item) else {
                        return Err(AppError::CodedVars {
                            status: 400,
                            code: "COMBO_CHOICE_NOT_ALLOWED",
                            reason: "That item can't be chosen here.".into(),
                            vars: serde_json::json!({ "slot_index": i, "menu_item_id": item }),
                        });
                    };
                    if kind == "combo" {
                        return Err(AppError::CodedVars {
                            status: 400,
                            code: "COMBO_NESTED",
                            reason: "A combo can't contain another combo.".into(),
                            vars: serde_json::json!({ "slot_index": i, "menu_item_id": item }),
                        });
                    }
                    if !seen_items.insert(item) {
                        return Err(slot_invalid(i, "choices"));
                    }
                }
                (None, Some(cat)) => {
                    if !known_cats.contains(&cat) {
                        return Err(AppError::CodedVars {
                            status: 400,
                            code: "COMBO_CHOICE_NOT_ALLOWED",
                            reason: "That category can't be chosen here.".into(),
                            vars: serde_json::json!({ "slot_index": i, "category_id": cat }),
                        });
                    }
                    if !seen_cats.insert(cat) {
                        return Err(slot_invalid(i, "choices"));
                    }
                }
                _ => return Err(slot_invalid(i, "choices")),
            }
        }
        if let Some(d) = s.default_item_id {
            let admitted = match items.get(&d) {
                Some((kind, cat)) if kind == "item" => s.choices.iter().any(|c| {
                    c.menu_item_id == Some(d)
                        || (c.menu_item_id.is_none()
                            && c.category_id.is_some()
                            && c.category_id == *cat)
                }),
                _ => false,
            };
            if !admitted {
                return Err(slot_invalid(i, "default_item_id"));
            }
        }
    }
    validate_windows(&mut *conn, org, &body.windows, "COMBO_SLOT_INVALID").await
}

/// Write the slots (diffed in place by id), choices, size surcharges and
/// windows of combo `id`.
async fn write_children(
    conn: &mut PgConnection,
    org: Uuid,
    id: Uuid,
    body: &ComboWrite,
) -> Result<(), AppError> {
    let existing: HashSet<Uuid> =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM combo_slots WHERE combo_item_id = $1")
            .bind(id)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    let kept: HashSet<Uuid> = body
        .slots
        .iter()
        .filter_map(|s| s.id)
        .filter(|i| existing.contains(i))
        .collect();
    let gone: Vec<Uuid> = existing.difference(&kept).copied().collect();
    if !gone.is_empty() {
        // An item's "make it a meal" pointing at a deleted slot is unlinked
        // first (the FK's SET NULL alone would trip the meal guard).
        sqlx::query(
            "UPDATE menu_items SET meal_combo_id = NULL, meal_slot_id = NULL WHERE meal_slot_id = ANY($1)",
        )
        .bind(&gone)
        .execute(&mut *conn)
        .await?;
        sqlx::query("DELETE FROM combo_slots WHERE id = ANY($1)")
            .bind(&gone)
            .execute(&mut *conn)
            .await?;
    }
    for s in &body.slots {
        let slot_id = write_slot(&mut *conn, org, id, s, &kept).await?;
        write_choices(&mut *conn, org, slot_id, s).await?;
    }
    // A meal link whose slot no longer admits its item is dropped.
    sqlx::query(
        "UPDATE menu_items mi SET meal_combo_id = NULL, meal_slot_id = NULL \
          WHERE mi.meal_combo_id = $1 \
            AND NOT EXISTS (SELECT 1 FROM combo_slot_choices c \
                             WHERE c.slot_id = mi.meal_slot_id \
                               AND (c.menu_item_id = mi.id \
                                    OR (c.menu_item_id IS NULL AND c.category_id = mi.category_id)))",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    write_windows(&mut *conn, org, "combo_item_id", id, &body.windows).await
}

async fn write_slot(
    conn: &mut PgConnection,
    org: Uuid,
    combo: Uuid,
    s: &ComboSlotWrite,
    kept: &HashSet<Uuid>,
) -> Result<Uuid, AppError> {
    if let Some(sid) = s.id.filter(|i| kept.contains(i)) {
        sqlx::query(
            "UPDATE combo_slots SET name = $2, name_translations = $3, sort = $4, min_picks = $5, \
                    max_picks = $6, default_item_id = $7, default_size_label = $8, updated_at = now() \
              WHERE id = $1",
        )
        .bind(sid)
        .bind(s.name.trim())
        .bind(&s.name_translations)
        .bind(s.sort)
        .bind(s.min)
        .bind(s.max)
        .bind(s.default_item_id)
        .bind(s.default_size_label.as_deref())
        .execute(&mut *conn)
        .await?;
        Ok(sid)
    } else {
        Ok(sqlx::query_scalar(
            "INSERT INTO combo_slots (org_id, combo_item_id, name, name_translations, sort, min_picks, \
                                      max_picks, default_item_id, default_size_label) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
        )
        .bind(org)
        .bind(combo)
        .bind(s.name.trim())
        .bind(&s.name_translations)
        .bind(s.sort)
        .bind(s.min)
        .bind(s.max)
        .bind(s.default_item_id)
        .bind(s.default_size_label.as_deref())
        .fetch_one(&mut *conn)
        .await?)
    }
}

async fn write_choices(
    conn: &mut PgConnection,
    org: Uuid,
    slot: Uuid,
    s: &ComboSlotWrite,
) -> Result<(), AppError> {
    let existing: HashSet<Uuid> =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM combo_slot_choices WHERE slot_id = $1")
            .bind(slot)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    let kept: HashSet<Uuid> = s
        .choices
        .iter()
        .filter_map(|c| c.id)
        .filter(|i| existing.contains(i))
        .collect();
    let gone: Vec<Uuid> = existing.difference(&kept).copied().collect();
    sqlx::query("DELETE FROM combo_slot_choices WHERE id = ANY($1)")
        .bind(&gone)
        .execute(&mut *conn)
        .await?;
    for c in &s.choices {
        let cid: Uuid = if let Some(cid) = c.id.filter(|i| kept.contains(i)) {
            sqlx::query(
                "UPDATE combo_slot_choices SET menu_item_id = $2, category_id = $3, surcharge = $4, \
                        included_size_label = $5, sort = $6 WHERE id = $1",
            )
            .bind(cid)
            .bind(c.menu_item_id)
            .bind(c.category_id)
            .bind(c.surcharge)
            .bind(c.included_size_label.as_deref())
            .bind(c.sort)
            .execute(&mut *conn)
            .await?;
            cid
        } else {
            sqlx::query_scalar(
                "INSERT INTO combo_slot_choices (org_id, slot_id, menu_item_id, category_id, surcharge, \
                                                 included_size_label, sort) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
            )
            .bind(org)
            .bind(slot)
            .bind(c.menu_item_id)
            .bind(c.category_id)
            .bind(c.surcharge)
            .bind(c.included_size_label.as_deref())
            .bind(c.sort)
            .fetch_one(&mut *conn)
            .await?
        };
        sqlx::query("DELETE FROM combo_choice_size_surcharges WHERE choice_id = $1")
            .bind(cid)
            .execute(&mut *conn)
            .await?;
        for z in &c.size_surcharges {
            sqlx::query(
                "INSERT INTO combo_choice_size_surcharges (choice_id, org_id, size_label, surcharge) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (choice_id, size_label) DO UPDATE SET surcharge = EXCLUDED.surcharge",
            )
            .bind(cid)
            .bind(org)
            .bind(&z.size_label)
            .bind(z.surcharge)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// The full `Combo` of a stored combo, priced for `branch` (or the org).
pub async fn combo_full(
    conn: &mut PgConnection,
    def: ComboDef,
    branch: Option<Uuid>,
) -> Result<Combo, AppError> {
    let (p, enabled) = economics::branch_price(&mut *conn, &def, branch).await?;
    let analysis = economics::analyse(&mut *conn, def.org_id, branch, p, &def.slots).await?;
    let available_now =
        economics::available_now(&mut *conn, &def, branch, p, enabled, &analysis).await?;
    Ok(Combo {
        id: def.id,
        kind: "combo".into(),
        is_fixed: def.is_fixed(),
        name: def.name,
        name_translations: def.name_translations,
        category_id: def.category_id,
        description: def.description,
        description_translations: def.description_translations,
        image_url: def.image_url,
        is_active: def.is_active,
        price: def.price,
        windows: def.windows,
        slots: def.slots,
        available_now,
        economics: analysis.economics,
        created_at: def.created_at,
        updated_at: def.updated_at,
    })
}

async fn load_one(conn: &mut PgConnection, org: Uuid, id: Uuid) -> Result<ComboDef, AppError> {
    load_combos(&mut *conn, org, Some(&[id]))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::NotFound("Combo not found".into()))
}

#[utoipa::path(
    get,
    path = "/combos",
    tag = "menu",
    params(ComboListQuery),
    responses((status = 200, description = "The org's combos", body = PaginatedCombos), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_combos(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ComboListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuItemsRead, None).await?;
    let org = claims_org(&claims)?;
    let per_page = query.per_page.unwrap_or(50).clamp(1, 200);
    let page = query.page.unwrap_or(1).max(1);
    let q = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(|q| {
            format!(
                "%{}%",
                q.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        });
    let mut conn = pool.acquire().await?;
    let filter = "FROM menu_items mi \
         WHERE mi.org_id = $1 AND mi.kind = 'combo' AND mi.deleted_at IS NULL \
           AND ($2::text IS NULL OR mi.name ILIKE $2 OR mi.name_translations->>'ar' ILIKE $2) \
           AND ($3::uuid IS NULL OR mi.category_id = $3) \
           AND ($4::bool IS NULL OR mi.is_active = $4)";
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) {filter}"))
        .bind(org)
        .bind(q.as_deref())
        .bind(query.category_id)
        .bind(query.is_active)
        .fetch_one(&mut *conn)
        .await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(&format!(
        "SELECT mi.id {filter} ORDER BY mi.name, mi.id LIMIT $5 OFFSET $6"
    ))
    .bind(org)
    .bind(q.as_deref())
    .bind(query.category_id)
    .bind(query.is_active)
    .bind(per_page)
    .bind((page - 1) * per_page)
    .fetch_all(&mut *conn)
    .await?;
    let mut data = Vec::with_capacity(ids.len());
    for def in load_combos(&mut conn, org, Some(&ids)).await? {
        let p = i64::from(def.price);
        let analysis = economics::analyse(&mut conn, org, None, p, &def.slots).await?;
        let available_now =
            economics::available_now(&mut conn, &def, None, p, true, &analysis).await?;
        data.push(ComboSummary {
            id: def.id,
            is_fixed: def.is_fixed(),
            slot_count: def.slots.len() as i64,
            window_count: def.windows.len() as i64,
            name: def.name,
            name_translations: def.name_translations,
            image_url: def.image_url,
            category_id: def.category_id,
            price: def.price,
            is_active: def.is_active,
            available_now,
            margin_default: analysis.economics.margin_default,
            warning_count: analysis.economics.warnings.len() as i64,
        });
    }
    Ok(HttpResponse::Ok().json(PaginatedCombos {
        data,
        total,
        page,
        per_page,
        total_pages: (total + per_page - 1) / per_page,
    }))
}

#[utoipa::path(
    post,
    path = "/combos",
    tag = "menu",
    request_body = ComboWrite,
    responses((status = 201, description = "The combo, created in one transaction", body = Combo), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_combo(
    req: HttpRequest,
    pool: crate::db::Db,
    cache: Cache,
    body: web::Json<ComboWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuCombosEdit, None).await?;
    let org = claims_org(&claims)?;
    let mut tx = pool.begin().await?;
    validate(&mut tx, org, &body).await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, category_id, name, name_translations, description, \
                                 description_translations, base_price, is_active, kind) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'combo') RETURNING id",
    )
    .bind(org)
    .bind(body.category_id)
    .bind(body.name.trim())
    .bind(&body.name_translations)
    .bind(body.description.as_deref())
    .bind(&body.description_translations)
    .bind(body.price)
    .bind(body.is_active)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE menu_item_sizes SET price = $2 WHERE menu_item_id = $1 AND label = 'one_size'",
    )
    .bind(id)
    .bind(body.price)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO menu_item_combos (menu_item_id, org_id) VALUES ($1, $2)")
        .bind(id)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    write_children(&mut tx, org, id, &body).await?;
    tx.commit().await?;
    invalidate(&cache, org);
    let mut conn = pool.acquire().await?;
    let def = load_one(&mut conn, org, id).await?;
    Ok(HttpResponse::Created().json(combo_full(&mut conn, def, None).await?))
}

#[utoipa::path(
    get,
    path = "/combos/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "The combo's menu item id"), ComboGetQuery),
    responses((status = 200, description = "The combo with its economics", body = Combo), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_combo(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    query: web::Query<ComboGetQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuItemsRead, query.branch_id).await?;
    let org = claims_org(&claims)?;
    let mut conn = pool.acquire().await?;
    let def = load_one(&mut conn, org, *id).await?;
    Ok(HttpResponse::Ok().json(combo_full(&mut conn, def, query.branch_id).await?))
}

#[utoipa::path(
    put,
    path = "/combos/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "The combo's menu item id")),
    request_body = ComboWrite,
    responses((status = 200, description = "The combo; slots and choices diffed in place by id", body = Combo), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_combo(
    req: HttpRequest,
    pool: crate::db::Db,
    cache: Cache,
    id: web::Path<Uuid>,
    body: web::Json<ComboWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuCombosEdit, None).await?;
    let org = claims_org(&claims)?;
    let id = *id;
    let mut tx = pool.begin().await?;
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT mi.id FROM menu_items mi JOIN menu_item_combos c ON c.menu_item_id = mi.id \
          WHERE mi.id = $1 AND mi.org_id = $2 AND mi.kind = 'combo' AND mi.deleted_at IS NULL \
          FOR UPDATE OF mi",
    )
    .bind(id)
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?;
    if found.is_none() {
        return Err(AppError::NotFound("Combo not found".into()));
    }
    validate(&mut tx, org, &body).await?;
    sqlx::query(
        "UPDATE menu_items SET category_id = $2, name = $3, name_translations = $4, description = $5, \
                description_translations = $6, base_price = $7, is_active = $8, updated_at = now() \
          WHERE id = $1",
    )
    .bind(id)
    .bind(body.category_id)
    .bind(body.name.trim())
    .bind(&body.name_translations)
    .bind(body.description.as_deref())
    .bind(&body.description_translations)
    .bind(body.price)
    .bind(body.is_active)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort, is_active) \
         VALUES ((md5($1::text || ':one_size'))::uuid, $1, 'one_size', $2, 0, true) \
         ON CONFLICT (menu_item_id, label) DO UPDATE SET price = EXCLUDED.price",
    )
    .bind(id)
    .bind(body.price)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE menu_item_combos SET updated_at = now() WHERE menu_item_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    write_children(&mut tx, org, id, &body).await?;
    tx.commit().await?;
    invalidate(&cache, org);
    let mut conn = pool.acquire().await?;
    let def = load_one(&mut conn, org, id).await?;
    Ok(HttpResponse::Ok().json(combo_full(&mut conn, def, None).await?))
}

/// A draft's slots as stored slots (ids kept when given).
fn draft_slots(w: &ComboWrite) -> Vec<ComboSlot> {
    w.slots
        .iter()
        .map(|s| ComboSlot {
            id: s.id.unwrap_or_else(Uuid::new_v4),
            name: s.name.clone(),
            name_translations: s.name_translations.clone(),
            sort: s.sort,
            min: s.min,
            max: s.max,
            default_item_id: s.default_item_id,
            default_size_label: s.default_size_label.clone(),
            choices: s
                .choices
                .iter()
                .map(|c| ComboChoice {
                    id: c.id.unwrap_or_else(Uuid::new_v4),
                    menu_item_id: c.menu_item_id,
                    category_id: c.category_id,
                    name: String::new(),
                    name_translations: serde_json::json!({}),
                    surcharge: c.surcharge,
                    included_size_label: c.included_size_label.clone(),
                    size_surcharges: c.size_surcharges.clone(),
                    sort: c.sort,
                })
                .collect(),
        })
        .collect()
}

#[utoipa::path(
    post,
    path = "/combos/economics",
    tag = "menu",
    request_body = ComboEconomicsRequest,
    responses((status = 200, description = "The editor's live panel; nothing is saved", body = ComboEconomics), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn combo_economics(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<ComboEconomicsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuItemsRead, body.branch_id).await?;
    let org = claims_org(&claims)?;
    let mut conn = pool.acquire().await?;
    let slots = draft_slots(&body.combo);
    let a = economics::analyse(
        &mut conn,
        org,
        body.branch_id,
        i64::from(body.combo.price),
        &slots,
    )
    .await?;
    Ok(HttpResponse::Ok().json(a.economics))
}

fn meal_invalid(reason: &str) -> AppError {
    AppError::Coded {
        status: 400,
        code: "MEAL_TARGET_INVALID",
        reason: reason.into(),
    }
}

#[utoipa::path(
    put,
    path = "/menu-items/{id}/meal",
    tag = "menu",
    params(("id" = Uuid, Path, description = "A kind=item menu item")),
    request_body = MealLinkWrite,
    responses((status = 204, description = "Linked (or unlinked when both fields are null)"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_meal(
    req: HttpRequest,
    pool: crate::db::Db,
    cache: Cache,
    id: web::Path<Uuid>,
    body: web::Json<Option<MealLinkWrite>>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuCombosEdit, None).await?;
    let org = claims_org(&claims)?;
    let id = *id;
    let link = body.into_inner().unwrap_or_default();
    let mut tx = pool.begin().await?;
    let item: Option<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT kind, category_id FROM menu_items WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?;
    let (kind, category) = item.ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    match (link.combo_id, link.slot_id) {
        (None, None) => {}
        (Some(combo), Some(slot)) => {
            if kind != "item" {
                return Err(meal_invalid("Only an item can be made a meal."));
            }
            let admits: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM combo_slots s \
                           JOIN menu_items c ON c.id = s.combo_item_id \
                           JOIN combo_slot_choices ch ON ch.slot_id = s.id \
                          WHERE s.id = $1 AND s.combo_item_id = $2 AND c.org_id = $3 \
                            AND c.kind = 'combo' AND c.deleted_at IS NULL \
                            AND (ch.menu_item_id = $4 \
                                 OR (ch.menu_item_id IS NULL AND ch.category_id = $5)))",
            )
            .bind(slot)
            .bind(combo)
            .bind(org)
            .bind(id)
            .bind(category)
            .fetch_one(&mut *tx)
            .await?;
            if !admits {
                return Err(meal_invalid("That combo has no slot for this item."));
            }
        }
        _ => {
            return Err(meal_invalid(
                "Give both the combo and its slot, or neither.",
            ));
        }
    }
    sqlx::query(
        "UPDATE menu_items SET meal_combo_id = $2, meal_slot_id = $3, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(link.combo_id)
    .bind(link.slot_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    invalidate(&cache, org);
    Ok(HttpResponse::NoContent().finish())
}

async fn settings_of(conn: &mut PgConnection, org: Uuid) -> Result<ComboSettings, AppError> {
    let min = economics::min_margin(&mut *conn, org).await?;
    let (channels, _) = crate::deals::load::channel_settings(&mut *conn, org, None).await?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(Uuid, Option<bool>, Option<bool>, Option<bool>, Option<bool>)> = sqlx::query_as(
        "SELECT o.branch_id, o.sell_pos, o.sell_qr, o.sell_online, o.sell_delivery \
               FROM combo_channel_branch_overrides o JOIN branches b ON b.id = o.branch_id \
              WHERE o.org_id = $1 ORDER BY b.name, o.branch_id",
    )
    .bind(org)
    .fetch_all(&mut *conn)
    .await?;
    Ok(ComboSettings {
        min_margin: min.map(fraction),
        channels,
        branch_overrides: rows
            .into_iter()
            .map(|(branch_id, pos, qr, online, delivery)| {
                let sell = ChannelOverride {
                    pos,
                    qr,
                    online,
                    delivery,
                };
                BranchChannelOverride {
                    branch_id,
                    sell,
                    effective: crate::deals::load::resolve(channels, Some(sell)),
                }
            })
            .collect(),
    })
}

#[utoipa::path(
    get,
    path = "/settings/combos",
    tag = "menu",
    responses((status = 200, description = "The minimum margin and the channel toggles", body = ComboSettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_settings(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::OrgSettingsRead, None).await?;
    let org = claims_org(&claims)?;
    let mut conn = pool.acquire().await?;
    Ok(HttpResponse::Ok().json(settings_of(&mut conn, org).await?))
}

#[utoipa::path(
    put,
    path = "/settings/combos",
    tag = "menu",
    request_body = ComboSettingsWrite,
    responses((status = 200, description = "The settings as saved", body = ComboSettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_settings(
    req: HttpRequest,
    pool: crate::db::Db,
    cache: Cache,
    body: web::Json<ComboSettingsWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuCombosEdit, None).await?;
    let org = claims_org(&claims)?;
    let min: Option<Decimal> = match body.min_margin.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s) => {
            let d: Decimal = s.parse().map_err(|_| {
                AppError::BadRequest("min_margin must be a decimal fraction".into())
            })?;
            if d < Decimal::ZERO || d > Decimal::ONE {
                return Err(AppError::BadRequest(
                    "min_margin must be between 0 and 1".into(),
                ));
            }
            Some(d.round_dp(4))
        }
    };
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE organizations SET combo_min_margin = $2 WHERE id = $1")
        .bind(org)
        .bind(min)
        .execute(&mut *tx)
        .await?;
    if let Some(c) = body.channels {
        write_org_channels(&mut tx, org, c).await?;
    }
    tx.commit().await?;
    invalidate(&cache, org);
    let mut conn = pool.acquire().await?;
    Ok(HttpResponse::Ok().json(settings_of(&mut conn, org).await?))
}

async fn write_org_channels(
    conn: &mut PgConnection,
    org: Uuid,
    c: ChannelToggles,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO combo_channel_settings (org_id, sell_pos, sell_qr, sell_online, sell_delivery) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (org_id) DO UPDATE SET sell_pos = EXCLUDED.sell_pos, sell_qr = EXCLUDED.sell_qr, \
                sell_online = EXCLUDED.sell_online, sell_delivery = EXCLUDED.sell_delivery, updated_at = now()",
    )
    .bind(org)
    .bind(c.pos)
    .bind(c.qr)
    .bind(c.online)
    .bind(c.delivery)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn branch_of_org(conn: &mut PgConnection, org: Uuid, branch: Uuid) -> Result<(), AppError> {
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM branches WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL)",
    )
    .bind(branch)
    .bind(org)
    .fetch_one(&mut *conn)
    .await?;
    if ok {
        Ok(())
    } else {
        Err(AppError::NotFound("Branch not found".into()))
    }
}

#[utoipa::path(
    put,
    path = "/settings/combos/branches/{branch_id}",
    tag = "menu",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = ChannelOverride,
    responses((status = 204, description = "The branch's channel override saved (all null = inherit)"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_branch_channels(
    req: HttpRequest,
    pool: crate::db::Db,
    cache: Cache,
    branch_id: web::Path<Uuid>,
    body: web::Json<ChannelOverride>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(
        pool.get_ref(),
        &claims,
        Cap::MenuCombosEdit,
        Some(*branch_id),
    )
    .await?;
    let org = claims_org(&claims)?;
    let mut conn = pool.acquire().await?;
    branch_of_org(&mut conn, org, *branch_id).await?;
    let o = *body;
    if o.pos.is_none() && o.qr.is_none() && o.online.is_none() && o.delivery.is_none() {
        sqlx::query("DELETE FROM combo_channel_branch_overrides WHERE branch_id = $1")
            .bind(*branch_id)
            .execute(&mut *conn)
            .await?;
    } else {
        sqlx::query(
            "INSERT INTO combo_channel_branch_overrides \
                    (branch_id, org_id, sell_pos, sell_qr, sell_online, sell_delivery) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (branch_id) DO UPDATE SET sell_pos = EXCLUDED.sell_pos, \
                    sell_qr = EXCLUDED.sell_qr, sell_online = EXCLUDED.sell_online, \
                    sell_delivery = EXCLUDED.sell_delivery, updated_at = now()",
        )
        .bind(*branch_id)
        .bind(org)
        .bind(o.pos)
        .bind(o.qr)
        .bind(o.online)
        .bind(o.delivery)
        .execute(&mut *conn)
        .await?;
    }
    invalidate(&cache, org);
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    delete,
    path = "/settings/combos/branches/{branch_id}",
    tag = "menu",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 204, description = "The branch inherits the org's toggles"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_branch_channels(
    req: HttpRequest,
    pool: crate::db::Db,
    cache: Cache,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(
        pool.get_ref(),
        &claims,
        Cap::MenuCombosEdit,
        Some(*branch_id),
    )
    .await?;
    let org = claims_org(&claims)?;
    let mut conn = pool.acquire().await?;
    branch_of_org(&mut conn, org, *branch_id).await?;
    sqlx::query("DELETE FROM combo_channel_branch_overrides WHERE branch_id = $1")
        .bind(*branch_id)
        .execute(&mut *conn)
        .await?;
    invalidate(&cache, org);
    Ok(HttpResponse::NoContent().finish())
}
