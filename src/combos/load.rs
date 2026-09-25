//! Loading combos from SQL, once, for every reader: the dashboard CRUD, the
//! POS feed (`GET /menu-items?full=true`, `/catalog/sync`, `/sync/pull`), the
//! public menus and the order path. Plus the bridges to madar-catalog's pure
//! types (`ComboView`, `Window`, `LocalNow`).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    combos::types::{ChannelToggles, ComboChoice, ComboFeed, ComboSlot, SaleWindow, SizeSurcharge},
    errors::AppError,
};

/// A combo as stored: its item row, its catalogue price P (the `one_size`
/// row) and its slots and windows.
#[derive(Clone, Debug)]
pub struct ComboDef {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
    pub category_id: Option<Uuid>,
    pub description: Option<String>,
    pub description_translations: serde_json::Value,
    pub image_url: Option<String>,
    pub is_active: bool,
    /// The catalogue P (the `one_size` row's price).
    pub price: i32,
    /// The `one_size` row's id (its branch prices live in menu_price_overrides).
    pub size_id: Option<Uuid>,
    pub windows: Vec<SaleWindow>,
    pub slots: Vec<ComboSlot>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ComboDef {
    /// C1's fixed bundle: every slot has exactly one item choice, min == max.
    pub fn is_fixed(&self) -> bool {
        is_fixed(&self.slots)
    }

    /// madar-catalog's view at price `p` (the branch-effective P), with only
    /// the windows for `branch` (or every branch; `None` = the org's
    /// all-branch windows only).
    pub fn view(&self, p: i64, branch: Option<Uuid>) -> madar_catalog::combo::ComboView {
        madar_catalog::combo::ComboView {
            id: self.id.to_string(),
            price: p,
            is_active: self.is_active,
            slots: self.slots.iter().map(slot_view).collect(),
            windows: self
                .windows
                .iter()
                .filter(|w| w.branch_id.is_none() || (branch.is_some() && w.branch_id == branch))
                .map(window_view)
                .collect(),
        }
    }

    /// The `combo` object of a feed row for `branch`.
    pub fn feed(&self, sell: ChannelToggles, branch: Option<Uuid>) -> ComboFeed {
        ComboFeed {
            is_fixed: self.is_fixed(),
            sell,
            windows: self
                .windows
                .iter()
                .filter(|w| w.branch_id.is_none() || (branch.is_some() && w.branch_id == branch))
                .cloned()
                .collect(),
            slots: self.slots.clone(),
        }
    }
}

pub fn is_fixed(slots: &[ComboSlot]) -> bool {
    !slots.is_empty()
        && slots
            .iter()
            .all(|s| s.min == s.max && s.choices.len() == 1 && s.choices[0].menu_item_id.is_some())
}

pub fn slot_view(s: &ComboSlot) -> madar_catalog::combo::SlotView {
    madar_catalog::combo::SlotView {
        id: s.id.to_string(),
        name: s.name.clone(),
        sort: i64::from(s.sort),
        min: i64::from(s.min),
        max: i64::from(s.max),
        default_item_id: s.default_item_id.map(|i| i.to_string()),
        default_size_label: s.default_size_label.clone(),
        choices: s
            .choices
            .iter()
            .map(|c| madar_catalog::combo::ChoiceView {
                id: Some(c.id.to_string()),
                menu_item_id: c.menu_item_id.map(|i| i.to_string()),
                category_id: c.category_id.map(|i| i.to_string()),
                surcharge: i64::from(c.surcharge),
                included_size_label: c.included_size_label.clone(),
                size_surcharges: c
                    .size_surcharges
                    .iter()
                    .map(|z| madar_catalog::combo::SizeSurcharge {
                        size_label: z.size_label.clone(),
                        surcharge: i64::from(z.surcharge),
                    })
                    .collect(),
                sort: i64::from(c.sort),
            })
            .collect(),
    }
}

pub fn window_view(w: &SaleWindow) -> madar_catalog::sale_window::Window {
    madar_catalog::sale_window::Window {
        branch_id: w.branch_id.map(|b| b.to_string()),
        weekdays: u8::try_from(w.weekdays.clamp(0, 127)).unwrap_or(127),
        starts_at: w.starts_at.clone(),
        ends_at: w.ends_at.clone(),
        valid_from: w.valid_from.map(|d| d.format("%Y-%m-%d").to_string()),
        valid_to: w.valid_to.map(|d| d.format("%Y-%m-%d").to_string()),
    }
}

/// The wall clock at a branch (its effective time zone), or at the org's
/// zone when no branch is given.
pub async fn local_now(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<madar_catalog::sale_window::LocalNow, AppError> {
    let name =
        crate::tz::scope_tz_name(&mut *conn, branch_id.unwrap_or(Uuid::nil()), org_id).await?;
    let tz = crate::tz::parse(&name);
    let now = Utc::now().with_timezone(&tz);
    Ok(madar_catalog::sale_window::LocalNow::new(
        now.format("%Y-%m-%d").to_string(),
        now.format("%H:%M:%S").to_string(),
    ))
}

#[allow(clippy::type_complexity)]
type ItemRowT = (
    Uuid,
    Uuid,
    String,
    serde_json::Value,
    Option<Uuid>,
    Option<String>,
    serde_json::Value,
    Option<String>,
    bool,
    Option<i32>,
    Option<Uuid>,
    DateTime<Utc>,
    DateTime<Utc>,
);

/// The org's combos (not deleted), or only `ids`, in `name, id` order.
pub async fn load_combos(
    conn: &mut PgConnection,
    org_id: Uuid,
    ids: Option<&[Uuid]>,
) -> Result<Vec<ComboDef>, AppError> {
    let rows: Vec<ItemRowT> = sqlx::query_as(
        "SELECT mi.id, mi.org_id, mi.name, mi.name_translations, mi.category_id, mi.description, \
                mi.description_translations, mi.image_url, mi.is_active, z.price, z.id, \
                mi.created_at, GREATEST(mi.updated_at, c.updated_at) \
           FROM menu_items mi \
           JOIN menu_item_combos c ON c.menu_item_id = mi.id \
           LEFT JOIN menu_item_sizes z ON z.menu_item_id = mi.id AND z.label = 'one_size' \
          WHERE mi.org_id = $1 AND mi.kind = 'combo' AND mi.deleted_at IS NULL \
            AND ($2::uuid[] IS NULL OR mi.id = ANY($2)) \
          ORDER BY mi.name, mi.id",
    )
    .bind(org_id)
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let combo_ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let mut slots = slots_of(&mut *conn, &combo_ids).await?;
    let mut windows =
        crate::deals::load::windows_of(&mut *conn, "combo_item_id", &combo_ids).await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                org_id,
                name,
                name_translations,
                category_id,
                description,
                description_translations,
                image_url,
                is_active,
                price,
                size_id,
                created_at,
                updated_at,
            )| ComboDef {
                id,
                org_id,
                name,
                name_translations,
                category_id,
                description,
                description_translations,
                image_url,
                is_active,
                price: price.unwrap_or(0),
                size_id,
                windows: windows.remove(&id).unwrap_or_default(),
                slots: slots.remove(&id).unwrap_or_default(),
                created_at,
                updated_at,
            },
        )
        .collect())
}

/// The slots (with choices and size surcharges) of each combo, in slot
/// `sort, id` order and choice `sort, id` order.
pub async fn slots_of(
    conn: &mut PgConnection,
    combos: &[Uuid],
) -> Result<HashMap<Uuid, Vec<ComboSlot>>, AppError> {
    #[allow(clippy::type_complexity)]
    let slot_rows: Vec<(
        Uuid,
        Uuid,
        String,
        serde_json::Value,
        i32,
        i16,
        i16,
        Option<Uuid>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT combo_item_id, id, name, name_translations, sort, min_picks, max_picks, \
                default_item_id, default_size_label \
           FROM combo_slots WHERE combo_item_id = ANY($1) ORDER BY combo_item_id, sort, id",
    )
    .bind(combos)
    .fetch_all(&mut *conn)
    .await?;
    let slot_ids: Vec<Uuid> = slot_rows.iter().map(|r| r.1).collect();
    #[allow(clippy::type_complexity)]
    let choice_rows: Vec<(
        Uuid,
        Uuid,
        Option<Uuid>,
        Option<Uuid>,
        i32,
        Option<String>,
        i32,
        Option<String>,
        Option<serde_json::Value>,
    )> = sqlx::query_as(
        "SELECT c.slot_id, c.id, c.menu_item_id, c.category_id, c.surcharge, c.included_size_label, c.sort, \
                COALESCE(mi.name, cat.name), COALESCE(mi.name_translations, cat.name_translations) \
           FROM combo_slot_choices c \
           LEFT JOIN menu_items mi ON mi.id = c.menu_item_id \
           LEFT JOIN categories cat ON cat.id = c.category_id \
          WHERE c.slot_id = ANY($1) ORDER BY c.slot_id, c.sort, c.id",
    )
    .bind(&slot_ids)
    .fetch_all(&mut *conn)
    .await?;
    let choice_ids: Vec<Uuid> = choice_rows.iter().map(|r| r.1).collect();
    let surcharge_rows: Vec<(Uuid, String, i32)> = sqlx::query_as(
        "SELECT choice_id, size_label, surcharge FROM combo_choice_size_surcharges \
          WHERE choice_id = ANY($1) ORDER BY choice_id, size_label",
    )
    .bind(&choice_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut surcharges: HashMap<Uuid, Vec<SizeSurcharge>> = HashMap::new();
    for (choice, size_label, surcharge) in surcharge_rows {
        surcharges.entry(choice).or_default().push(SizeSurcharge {
            size_label,
            surcharge,
        });
    }
    let mut choices: HashMap<Uuid, Vec<ComboChoice>> = HashMap::new();
    for (slot, id, menu_item_id, category_id, surcharge, included_size_label, sort, name, tr) in
        choice_rows
    {
        choices.entry(slot).or_default().push(ComboChoice {
            id,
            menu_item_id,
            category_id,
            name: name.unwrap_or_default(),
            name_translations: tr.unwrap_or_else(|| serde_json::json!({})),
            surcharge,
            included_size_label,
            size_surcharges: surcharges.remove(&id).unwrap_or_default(),
            sort,
        });
    }
    let mut out: HashMap<Uuid, Vec<ComboSlot>> = HashMap::new();
    for (combo, id, name, name_translations, sort, min, max, default_item_id, default_size_label) in
        slot_rows
    {
        out.entry(combo).or_default().push(ComboSlot {
            id,
            name,
            name_translations,
            sort,
            min,
            max,
            default_item_id,
            default_size_label,
            choices: choices.remove(&id).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Every item a category choice admits: the org's live kind=item items of
/// that category, keyed by category.
pub async fn category_members(
    conn: &mut PgConnection,
    org_id: Uuid,
    categories: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Uuid>>, AppError> {
    if categories.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT category_id, id FROM menu_items \
          WHERE org_id = $1 AND category_id = ANY($2) AND kind = 'item' \
            AND deleted_at IS NULL AND is_active \
          ORDER BY category_id, name, id",
    )
    .bind(org_id)
    .bind(categories)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (c, i) in rows {
        out.entry(c).or_default().push(i);
    }
    Ok(out)
}

/// Every live "make it a meal" link of the org (C14): item → {combo, slot},
/// only where the combo is still a live combo.
pub async fn meal_links(
    conn: &mut PgConnection,
    org_id: Uuid,
) -> Result<HashMap<Uuid, crate::combos::types::MealLink>, AppError> {
    let rows: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT mi.id, mi.meal_combo_id, mi.meal_slot_id FROM menu_items mi \
           JOIN menu_items c ON c.id = mi.meal_combo_id AND c.kind = 'combo' AND c.deleted_at IS NULL \
          WHERE mi.org_id = $1 AND mi.deleted_at IS NULL AND mi.meal_slot_id IS NOT NULL",
    )
    .bind(org_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, combo_id, slot_id)| (id, crate::combos::types::MealLink { combo_id, slot_id }))
        .collect())
}

/// The feed's `combo` object for each of `ids` that is a live combo, with the
/// channel toggles resolved for `branch`.
pub async fn feeds_for(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch: Option<Uuid>,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, ComboFeed>, AppError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let defs = load_combos(&mut *conn, org_id, Some(ids)).await?;
    if defs.is_empty() {
        return Ok(HashMap::new());
    }
    let sell = crate::deals::load::channels_at(&mut *conn, org_id, branch).await?;
    Ok(defs.iter().map(|d| (d.id, d.feed(sell, branch))).collect())
}
