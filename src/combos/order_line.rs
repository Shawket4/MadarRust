//! A combo line of an order (COMBOS_CONTRACT.md §3): one header plus one part
//! per pick, priced by `madar_catalog::combo::quote`.
//!
//! One resolver for every intake that sells a combo: the till's live sale
//! and its offline replay (`orders::create_order_inner`), a ticket's fire and
//! settle (`tickets`), the public QR and online intakes, and the public cart
//! quote. Each part goes through the order path's own per-line resolver
//! (`resolve_order_line_in`), so its size, add-ons, recipe deduction and cost
//! are exactly those of the same item sold alone; only its money is
//! overridden: `line_total = combo_share + combo_surcharge`.
//!
//! - Live (`ClientPrices::Ignore`): the server prices every part; a bad pick
//!   is a coded refusal.
//! - Replay / settle (`AsCharged`): the till's P, shares, surcharges and add-on
//!   prices are recorded as charged, the server's quote is the expectation,
//!   and nothing is ever refused: missing picks take the slot defaults
//!   (`menu.combos:unpicked`), invalid picks are priced under a relaxed rule
//!   (`menu.combos:picks_invalid`).
//! - Availability: a public intake refuses (`COMBO_UNAVAILABLE`,
//!   `COMBO_ITEM_UNAVAILABLE`); the till flags (`menu.combos:unavailable`),
//!   as it does an item switched off at the branch.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use madar_catalog::combo::{self as mc, Channel, ComboRefusal};

use crate::{
    combos::{
        codes::refuse,
        load::ComboDef,
        types::{ChannelToggles, ComboInput, ComboPickInput, KitchenComboTag},
    },
    errors::AppError,
    orders::{
        catalog_view::Catalog,
        handlers::{ClientPrices, OrderItemInput, ResolvedItem, resolve_order_line_in},
    },
};

/// What an order line is (`order_items.line_kind`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineKind {
    #[default]
    Item,
    /// A combo's header: the combo item, no money.
    Header,
    /// One chosen item of a combo.
    Part,
}

impl LineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Header => "combo",
            Self::Part => "combo_part",
        }
    }
}

/// A resolved line's combo figures (all zero / `None` on a plain line).
#[derive(Clone, Debug, Default)]
pub struct LineCombo {
    pub kind: LineKind,
    /// The row's id, chosen before the insert (a header's parts point at it;
    /// parts are given ascending ids in slot order).
    pub id: Option<Uuid>,
    /// A part: its header.
    pub header_id: Option<Uuid>,
    pub slot_id: Option<Uuid>,
    pub slot_name: Option<String>,
    /// A header: P per combo unit, as charged.
    pub unit_price: Option<i32>,
    /// A part: its share of P and its surcharges, whole line, as charged.
    pub share: i32,
    pub surcharge: i32,
    /// A part: the server's share + surcharge, whole line (the expectation).
    pub expected: i32,
    /// A part: the kitchen's tag (C12).
    pub tag: Option<KitchenComboTag>,
}

/// What the order path knows about one menu item for combos and deals.
#[derive(Clone, Debug)]
pub struct ItemFacts {
    pub kind: String,
    pub category_id: Option<Uuid>,
    pub is_active: bool,
    /// Not switched off at the branch.
    pub branch_on: bool,
    pub name: String,
}

/// Everything a sale's combo and deal lines are resolved against, loaded
/// once per order.
pub struct ComboCtx {
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub combos: HashMap<Uuid, ComboDef>,
    pub items: HashMap<Uuid, ItemFacts>,
    /// A category's live kind=item items (category choices).
    pub members: HashMap<Uuid, Vec<Uuid>>,
    pub sell: ChannelToggles,
    /// The branch's wall clock at the sale.
    pub now: madar_catalog::sale_window::LocalNow,
}

impl ComboCtx {
    pub fn is_combo(&self, id: Option<Uuid>) -> bool {
        id.and_then(|i| self.items.get(&i))
            .is_some_and(|f| f.kind == "combo")
    }

    pub fn category_of(&self, id: Uuid) -> Option<Uuid> {
        self.items.get(&id).and_then(|f| f.category_id)
    }

    fn item_on(&self, id: Uuid) -> bool {
        self.items
            .get(&id)
            .is_some_and(|f| f.kind == "item" && f.is_active && f.branch_on)
    }

    /// A slot choice still has something to sell at this branch.
    pub fn choice_available(&self, c: &mc::ChoiceView) -> bool {
        let parse = |s: &Option<String>| s.as_deref().and_then(|v| Uuid::parse_str(v).ok());
        if let Some(id) = parse(&c.menu_item_id) {
            return self.item_on(id);
        }
        parse(&c.category_id)
            .and_then(|cat| self.members.get(&cat))
            .is_some_and(|m| m.iter().any(|i| self.item_on(*i)))
    }

    pub fn sell(&self) -> mc::Sell {
        mc::Sell {
            pos: self.sell.pos,
            qr: self.sell.qr,
            online: self.sell.online,
            delivery: self.sell.delivery,
        }
    }
}

/// The branch's wall clock at `at` (its effective time zone).
pub async fn local_at(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch_id: Uuid,
    at: DateTime<Utc>,
) -> Result<madar_catalog::sale_window::LocalNow, AppError> {
    let name = crate::tz::scope_tz_name(&mut *conn, branch_id, org_id).await?;
    let tz = crate::tz::parse(&name);
    let local = at.with_timezone(&tz);
    Ok(madar_catalog::sale_window::LocalNow::new(
        local.format("%Y-%m-%d").to_string(),
        local.format("%H:%M:%S").to_string(),
    ))
}

type FactRow = (Uuid, String, Option<Uuid>, bool, bool, String);

async fn facts(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch_id: Uuid,
    ids: &[Uuid],
    categories: &[Uuid],
) -> Result<Vec<FactRow>, AppError> {
    Ok(sqlx::query_as(
        "SELECT mi.id, mi.kind, mi.category_id, mi.is_active, COALESCE(bmo.is_available, true), mi.name \
           FROM menu_items mi \
           LEFT JOIN branch_menu_overrides bmo ON bmo.menu_item_id = mi.id AND bmo.branch_id = $2 \
          WHERE mi.org_id = $1 AND mi.deleted_at IS NULL \
            AND (mi.id = ANY($3) OR (mi.category_id = ANY($4) AND mi.kind = 'item'))",
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(ids)
    .bind(categories)
    .fetch_all(&mut *conn)
    .await?)
}

/// The context for `items`, or `None` when no line names a combo and `force`
/// is false (the fast path: a sale with no combo pays one query). `force`
/// loads the item facts anyway (deals need the categories).
pub async fn load_ctx(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    items: &[OrderItemInput],
    at: DateTime<Utc>,
    force: bool,
) -> Result<Option<ComboCtx>, AppError> {
    let mut ids: Vec<Uuid> = Vec::new();
    for it in items {
        ids.extend(it.menu_item_id);
        if let Some(c) = &it.combo {
            ids.extend(c.picks.iter().map(|p| p.menu_item_id));
        }
    }
    ids.sort();
    ids.dedup();
    let mut conn = pool.acquire().await?;
    let rows = facts(&mut conn, org_id, branch_id, &ids, &[]).await?;
    let combo_ids: Vec<Uuid> = rows
        .iter()
        .filter(|r| r.1 == "combo")
        .map(|r| r.0)
        .collect();
    if combo_ids.is_empty() && !force {
        return Ok(None);
    }
    let mut items_map: HashMap<Uuid, ItemFacts> = HashMap::new();
    let mut put =
        |rows: Vec<FactRow>, members: &mut HashMap<Uuid, Vec<Uuid>>, cats: &HashSet<Uuid>| {
            for (id, kind, category_id, is_active, branch_on, name) in rows {
                if let Some(c) = category_id
                    && kind == "item"
                    && cats.contains(&c)
                {
                    let m = members.entry(c).or_default();
                    if !m.contains(&id) {
                        m.push(id);
                    }
                }
                items_map.insert(
                    id,
                    ItemFacts {
                        kind,
                        category_id,
                        is_active,
                        branch_on,
                        name,
                    },
                );
            }
        };
    let mut members: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    put(rows, &mut members, &HashSet::new());

    let defs = if combo_ids.is_empty() {
        Vec::new()
    } else {
        crate::combos::load::load_combos(&mut conn, org_id, Some(&combo_ids)).await?
    };
    // Every choice item, default item and category member of these combos.
    let mut more: Vec<Uuid> = Vec::new();
    let mut cats: HashSet<Uuid> = HashSet::new();
    for d in &defs {
        for s in &d.slots {
            more.extend(s.default_item_id);
            for c in &s.choices {
                more.extend(c.menu_item_id);
                cats.extend(c.category_id);
            }
        }
    }
    if !more.is_empty() || !cats.is_empty() {
        let cat_list: Vec<Uuid> = cats.iter().copied().collect();
        let rows = facts(&mut conn, org_id, branch_id, &more, &cat_list).await?;
        put(rows, &mut members, &cats);
    }
    for m in members.values_mut() {
        m.sort_by(|a, b| {
            let na = items_map.get(a).map(|f| f.name.as_str()).unwrap_or("");
            let nb = items_map.get(b).map(|f| f.name.as_str()).unwrap_or("");
            na.cmp(nb).then(a.cmp(b))
        });
    }
    let sell = crate::deals::load::channels_at(&mut conn, org_id, Some(branch_id)).await?;
    let now = local_at(&mut conn, org_id, branch_id, at).await?;
    Ok(Some(ComboCtx {
        org_id,
        branch_id,
        combos: defs.into_iter().map(|d| (d.id, d)).collect(),
        items: items_map,
        members,
        sell,
        now,
    }))
}

/// A resolved combo line.
pub struct ComboLine {
    /// The header, then the parts in slot order.
    pub header: ResolvedItem,
    pub parts: Vec<ResolvedItem>,
    /// Replay flags this line raised (`menu.combos:*`).
    pub flags: Vec<String>,
    /// The line must be flagged whatever its figures (unavailable, unpicked,
    /// invalid picks, P differs from the server's).
    pub force_flag: bool,
    /// The server's quote (under the relaxed rule when the picks were invalid).
    pub quote: mc::ComboQuote,
    /// The line as it should be stored for a later settle: P, every pick's
    /// size, share, surcharge and add-on prices filled in as resolved.
    pub frozen: OrderItemInput,
}

/// Where the combo is sold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sale {
    pub channel: Channel,
    pub prices: ClientPrices,
    /// Refuse a bad line (live, public) rather than flag it (replay, settle).
    pub strict: bool,
}

impl Sale {
    fn public(&self) -> bool {
        self.channel != Channel::Pos
    }
}

/// The coded refusal of a pick the rule rejects.
pub fn refusal_of(r: &ComboRefusal, def: &ComboDef, ctx: &ComboCtx) -> AppError {
    let slot_name = |id: &str| {
        def.slots
            .iter()
            .find(|s| s.id.to_string() == id)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    };
    match r {
        ComboRefusal::TooFew { slot_id, min, got } => refuse(
            "COMBO_SLOT_TOO_FEW",
            json!({"slot_id": slot_id, "min": min, "got": got, "slot": slot_name(slot_id)}),
        ),
        ComboRefusal::BadQuantity { slot_id } => {
            let min = def
                .slots
                .iter()
                .find(|s| s.id.to_string() == *slot_id)
                .map_or(1, |s| s.min.max(1));
            refuse(
                "COMBO_SLOT_TOO_FEW",
                json!({"slot_id": slot_id, "min": min, "got": 0, "slot": slot_name(slot_id)}),
            )
        }
        ComboRefusal::TooMany { slot_id, max, got } => refuse(
            "COMBO_SLOT_TOO_MANY",
            json!({"slot_id": slot_id, "max": max, "got": got, "slot": slot_name(slot_id)}),
        ),
        ComboRefusal::UnknownSlot { slot_id } => refuse(
            "COMBO_CHOICE_NOT_ALLOWED",
            json!({"slot_id": slot_id, "menu_item_id": null}),
        ),
        ComboRefusal::NotAllowed {
            slot_id,
            menu_item_id,
        } => refuse(
            "COMBO_CHOICE_NOT_ALLOWED",
            json!({"slot_id": slot_id, "menu_item_id": menu_item_id}),
        ),
        ComboRefusal::Price { menu_item_id, .. } => {
            let item = Uuid::parse_str(menu_item_id)
                .ok()
                .and_then(|i| ctx.items.get(&i))
                .map(|f| f.name.clone())
                .unwrap_or_default();
            refuse(
                "COMBO_ITEM_UNAVAILABLE",
                json!({"menu_item_id": menu_item_id, "item": item}),
            )
        }
    }
}

/// The picks an unpicked line gets: each required slot's default (else its
/// first item choice, else the first item of its first category choice), `min`
/// units, at the slot's default size.
fn default_picks(def: &ComboDef, ctx: &ComboCtx) -> Vec<ComboPickInput> {
    let mut out = Vec::new();
    for s in &def.slots {
        if s.min < 1 {
            continue;
        }
        let item = s
            .default_item_id
            .or_else(|| s.choices.iter().find_map(|c| c.menu_item_id))
            .or_else(|| {
                s.choices
                    .iter()
                    .filter_map(|c| c.category_id)
                    .find_map(|cat| ctx.members.get(&cat).and_then(|m| m.first().copied()))
            });
        if let Some(menu_item_id) = item {
            out.push(ComboPickInput {
                slot_id: s.id,
                menu_item_id,
                size_label: s.default_size_label.clone(),
                quantity: i32::from(s.min),
                addons: Vec::new(),
                optional_field_ids: Vec::new(),
                notes: None,
                share: None,
                surcharge: None,
            });
        }
    }
    out
}

/// The combo's rule loosened so that every given pick is admitted and every
/// count allowed: how a replayed line with picks the rule now rejects is still
/// priced (and flagged) rather than refused.
fn relaxed(view: &mc::ComboView, picks: &[mc::PickIn]) -> mc::ComboView {
    let mut v = view.clone();
    for s in &mut v.slots {
        s.min = 0;
        s.max = i64::MAX / 4;
    }
    for p in picks {
        let slot = match v.slots.iter_mut().position(|s| s.id == p.slot_id) {
            Some(i) => &mut v.slots[i],
            None => {
                v.slots.push(mc::SlotView {
                    id: p.slot_id.clone(),
                    sort: i64::MAX / 4,
                    min: 0,
                    max: i64::MAX / 4,
                    ..Default::default()
                });
                v.slots.last_mut().expect("pushed")
            }
        };
        if mc::choice_for(slot, &p.view.item.id, p.category_id.as_deref()).is_none() {
            slot.choices.push(mc::ChoiceView {
                menu_item_id: Some(p.view.item.id.clone()),
                ..Default::default()
            });
        }
    }
    v
}

/// Resolve one combo line (`input.menu_item_id` is a combo of `ctx`).
pub async fn resolve(
    pool: &PgPool,
    catalog: &mut Catalog,
    ctx: &ComboCtx,
    input: &OrderItemInput,
    sale: Sale,
) -> Result<ComboLine, AppError> {
    let combo_id = input
        .menu_item_id
        .ok_or_else(|| AppError::BadRequest("Each line item must have a menu_item_id".into()))?;
    let def = ctx
        .combos
        .get(&combo_id)
        .ok_or_else(|| AppError::NotFound(format!("Menu item {combo_id} not found")))?;
    if input.quantity <= 0 {
        return Err(AppError::BadRequest("Item quantity must be > 0".into()));
    }
    let n = input.quantity;
    catalog.ensure_on(pool, &[combo_id], &[]).await?;
    let (name, name_translations, p_server, disabled) =
        crate::orders::handlers::catalog_unit_price_loaded(catalog, combo_id, Some("one_size"))?;

    let mut flags: Vec<String> = Vec::new();
    let mut force_flag = false;
    let view = def.view(i64::from(p_server), Some(ctx.branch_id));

    // ── Availability ──
    let sell = ctx.sell();
    let branch = ctx.branch_id.to_string();
    let at = mc::Availability {
        channel: sale.channel,
        sell: &sell,
        branch_enabled: !disabled,
        branch_id: Some(&branch),
        now: &ctx.now,
    };
    if let Err(why) = mc::available(&view, &at, |c| ctx.choice_available(c)) {
        if sale.public() {
            return Err(refuse(
                "COMBO_UNAVAILABLE",
                json!({"combo_id": combo_id, "reason": why.token()}),
            ));
        }
        flags.push("menu.combos:unavailable".into());
        force_flag = true;
    }

    // ── Picks ──
    let mut picks: Vec<ComboPickInput> = input
        .combo
        .as_ref()
        .map(|c| c.picks.clone())
        .unwrap_or_default();
    if picks.is_empty() {
        if sale.strict {
            return Err(refuse(
                "COMBO_PICKS_REQUIRED",
                json!({"combo_id": combo_id}),
            ));
        }
        picks = default_picks(def, ctx);
        flags.push("menu.combos:unpicked".into());
        force_flag = true;
    }
    let mut invalid = false;
    for p in &picks {
        let f = ctx.items.get(&p.menu_item_id);
        if f.is_some_and(|f| f.kind == "combo") {
            if sale.strict {
                return Err(refuse(
                    "COMBO_NESTED",
                    json!({"slot_id": p.slot_id, "menu_item_id": p.menu_item_id}),
                ));
            }
            invalid = true;
        }
        if !ctx.item_on(p.menu_item_id) {
            if sale.public() {
                let item = f.map(|f| f.name.clone()).unwrap_or_default();
                return Err(refuse(
                    "COMBO_ITEM_UNAVAILABLE",
                    json!({"menu_item_id": p.menu_item_id, "item": item}),
                ));
            }
            if !flags.iter().any(|f| f == "menu.combos:unavailable") {
                flags.push("menu.combos:unavailable".into());
            }
            force_flag = true;
        }
    }
    let pick_ids: Vec<Uuid> = picks.iter().map(|p| p.menu_item_id).collect();
    let addon_ids: Vec<Uuid> = picks
        .iter()
        .flat_map(|p| p.addons.iter().map(|a| a.addon_item_id))
        .collect();
    catalog.ensure_on(pool, &pick_ids, &addon_ids).await?;
    for p in &picks {
        if catalog
            .item(p.menu_item_id)
            .and_then(|i| i.row.as_ref())
            .is_none()
        {
            return Err(AppError::NotFound(format!(
                "Menu item {} not found",
                p.menu_item_id
            )));
        }
    }
    let pick_ins: Vec<mc::PickIn> = picks
        .iter()
        .map(|p| mc::PickIn {
            slot_id: p.slot_id.to_string(),
            view: catalog.view(p.menu_item_id),
            category_id: ctx.category_of(p.menu_item_id).map(|c| c.to_string()),
            selection: crate::orders::component_resolve::selection_of(
                p.size_label.as_deref(),
                &p.addons,
                &p.optional_field_ids,
            ),
            quantity: i64::from(p.quantity),
        })
        .collect();

    let quote = match mc::quote(&view, &pick_ins, i64::from(n)) {
        Ok(q) if !invalid => q,
        Ok(_) => mc::quote(&relaxed(&view, &pick_ins), &pick_ins, i64::from(n))
            .map_err(|r| refusal_of(&r, def, ctx))?,
        Err(r) if sale.strict => return Err(refusal_of(&r, def, ctx)),
        Err(r @ ComboRefusal::Price { .. }) => return Err(refusal_of(&r, def, ctx)),
        Err(_) => {
            invalid = true;
            mc::quote(&relaxed(&view, &pick_ins), &pick_ins, i64::from(n))
                .map_err(|r| refusal_of(&r, def, ctx))?
        }
    };
    if invalid {
        flags.push("menu.combos:picks_invalid".into());
        force_flag = true;
    }

    // ── Ids: the header, then the parts ascending in slot order ──
    let header_id = Uuid::new_v4();
    let mut part_ids: Vec<Uuid> = (0..quote.parts.len()).map(|_| Uuid::new_v4()).collect();
    part_ids.sort();
    let tag = KitchenComboTag {
        line_id: header_id,
        name: name.clone(),
        name_translations: name_translations.clone(),
    };

    // ── Parts: each through the order path's own resolver ──
    let mut parts: Vec<ResolvedItem> = Vec::with_capacity(quote.parts.len());
    let mut frozen_picks: Vec<ComboPickInput> = Vec::with_capacity(quote.parts.len());
    for (k, pq) in quote.parts.iter().enumerate() {
        let pick = &picks[pq.pick_index];
        let synthetic = OrderItemInput {
            menu_item_id: Some(pick.menu_item_id),
            size_label: Some(pq.size_label.clone()),
            quantity: pq.quantity as i32,
            addons: pick.addons.clone(),
            optional_field_ids: pick.optional_field_ids.clone(),
            notes: pick.notes.clone(),
            ..Default::default()
        };
        let mut r = resolve_order_line_in(pool, catalog, &synthetic, sale.prices).await?;
        let (share, surcharge) = match sale.prices {
            ClientPrices::Ignore => (pq.combo_share as i32, pq.combo_surcharge as i32),
            ClientPrices::AsCharged => (
                pick.share.map_or(pq.combo_share as i32, |s| s * n),
                pick.surcharge.map_or(pq.combo_surcharge as i32, |s| s * n),
            ),
        };
        if share < 0 || surcharge < 0 {
            return Err(AppError::BadRequest(
                "A combo pick's share or surcharge can never be below nothing.".into(),
            ));
        }
        let slot = def.slots.iter().find(|s| s.id == pick.slot_id);
        r.combo = LineCombo {
            kind: LineKind::Part,
            id: Some(part_ids[k]),
            header_id: Some(header_id),
            slot_id: Some(pick.slot_id),
            slot_name: slot.map(|s| s.name.clone()),
            unit_price: None,
            share,
            surcharge,
            expected: pq.line_total as i32,
            tag: Some(tag.clone()),
        };
        let mut fp = pick.clone();
        fp.size_label = Some(pq.size_label.clone());
        fp.share = Some(share / n);
        fp.surcharge = Some(surcharge / n);
        for (a, ra) in fp.addons.iter_mut().zip(r.addons.iter()) {
            a.unit_price = Some(ra.unit_price);
        }
        frozen_picks.push(fp);
        parts.push(r);
    }

    // ── The header: the combo, no money ──
    let p_charged = match sale.prices {
        ClientPrices::Ignore => p_server,
        ClientPrices::AsCharged => input.unit_price.unwrap_or(p_server),
    };
    if p_charged != p_server {
        force_flag = true;
    }
    let header = ResolvedItem {
        expected_unit_price: 0,
        expected_addon_per_unit: 0,
        branch_disabled: disabled,
        menu_item_id: Some(combo_id),
        item_name: name,
        name_translations,
        size_label: None,
        unit_price: 0,
        price_flagged: false,
        is_reward: false,
        reward_covered: 0,
        reward_units: 0,
        quantity: n,
        notes: input.notes.clone(),
        addons: Vec::new(),
        optionals: Vec::new(),
        deductions: Vec::new(),
        staff: None,
        combo: LineCombo {
            kind: LineKind::Header,
            id: Some(header_id),
            unit_price: Some(p_charged),
            ..Default::default()
        },
        deal_minor: 0,
    };

    let mut frozen = input.clone();
    frozen.unit_price = Some(p_charged);
    frozen.staff_drink = None;
    frozen.combo = Some(ComboInput {
        picks: frozen_picks,
    });

    Ok(ComboLine {
        header,
        parts,
        flags,
        force_flag,
        quote,
        frozen,
    })
}

/// Flag a resolved combo line: when any part's charged figure differs from
/// the server's, or the line was forced, the header and every part are
/// `price_flagged` and `menu.combos:price_mismatch` is added (once) to `flags`
/// when the figures themselves differ. Returns (charged, expected) totals.
pub fn settle_flags(line: &mut ComboLine) -> (i32, i32) {
    let mut charged = 0;
    let mut expected = 0;
    let mut differs = line.header.combo.unit_price != Some(line.quote.price as i32);
    for p in &line.parts {
        let c = p.charged_subtotal();
        let e = p.expected_subtotal();
        charged += c;
        expected += e;
        differs |= c != e;
    }
    if differs && !line.flags.iter().any(|f| f == "menu.combos:price_mismatch") {
        line.flags.push("menu.combos:price_mismatch".into());
    }
    let flag = differs || line.force_flag;
    line.header.price_flagged = flag;
    for p in &mut line.parts {
        p.price_flagged = flag;
    }
    (charged, expected)
}
