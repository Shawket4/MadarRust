//! The frozen snapshot — single source of truth for a delivery order's money,
//! cost, and inventory.
//!
//! * [`resolve_cart`] runs at **intake**: it server-prices each line using the
//!   org→branch→channel resolution chain, rolls up COGS at the current ingredient
//!   costs, and produces a [`CartSnapshot`] (priced lines) + a flat list of
//!   [`SnapshotDeduction`]s (the inventory plan). Both are stored verbatim on the
//!   `delivery_orders` row.
//! * [`apply_snapshot`] runs at **finalize**: it replays the frozen snapshot to
//!   insert a normal completed `orders` row — order lines, addons, optionals,
//!   payment, inventory movements (`record_movement`), and a minted `order_ref` —
//!   WITHOUT re-pricing or re-resolving anything. Menu/recipe/override edits made
//!   between intake and finalize cannot leak in, and neither does a tax change:
//!   the caller hands it the policy and figures frozen on the `delivery_orders`
//!   row, not the branch's policy of the day.
//!
//!   This is deliberately NOT `orders::handlers::create_order_inner`. The till's
//!   path re-prices — tax under the branch's CURRENT policy, deductions from the
//!   CURRENT recipe at CURRENT costs, optionals at catalog price — and cannot
//!   express `order_type`, the delivery fee or the delivery link at all, so a
//!   finalize through it would book a different sale from the one the customer
//!   agreed. Until the till's path can replay a frozen bill, the replay lives
//!   here and mirrors its inserts column for column.
//! * [`record_waste`] runs on **cancel with restore=false**: the food was made
//!   but not delivered, so the frozen plan is deducted from stock and logged as a
//!   `waste` movement.
//!
//! Money is integer piastres throughout.

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::AppError;
use crate::orders::component_resolve::{AddonInput, resolve_menu_item_configuration};

// ── Intake input ──────────────────────────────────────────────

/// One line of a public cart. Prices are NOT taken from the client — the server
/// resolves them. `addons` reuses [`AddonInput`] but its `unit_price` is ignored.
#[derive(Deserialize, Serialize, Clone, ToSchema)]
pub struct CartLineInput {
    pub menu_item_id: Uuid,
    #[serde(default)]
    pub size_label: Option<String>,
    pub quantity: i32,
    #[serde(default)]
    pub addons: Vec<AddonInput>,
    #[serde(default)]
    pub optional_field_ids: Vec<Uuid>,
    #[serde(default)]
    pub notes: Option<String>,
    /// A combo line's picks (§3.1); the server prices every part. Additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combo: Option<crate::combos::types::ComboInput>,
}

// ── Frozen snapshot shapes (serialised into delivery_orders.cart) ──

#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct SnapshotAddon {
    pub addon_item_id: Uuid,
    pub addon_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub unit_price: i32,
    pub quantity: i32,
    pub line_cost: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct SnapshotOptional {
    pub optional_field_id: Uuid,
    pub field_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub price: i32,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: Option<String>,
    pub ingredient_unit: Option<String>,
    pub quantity_used: Option<f64>,
    pub cost: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct SnapshotLine {
    pub menu_item_id: Uuid,
    pub item_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub size_label: Option<String>,
    pub unit_price: i32,
    pub quantity: i32,
    pub line_total: i32,
    pub notes: Option<String>,
    pub addons: Vec<SnapshotAddon>,
    pub optionals: Vec<SnapshotOptional>,
    pub line_cost: Option<i64>,
    pub unit_cost: Option<i64>,
    pub cost_missing: bool,
    /// Combos (additive; absent on a snapshot frozen before them): `item`, a
    /// combo's `combo` header (no money) or one of its `combo_part`s. The
    /// header comes first, then its parts in slot order.
    #[serde(default = "crate::combos::types::item_kind")]
    pub line_kind: String,
    /// The row id the order line takes at finalize (a header's parts point
    /// at it). `None` on a plain line: the database mints one.
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub combo_line_id: Option<Uuid>,
    #[serde(default)]
    pub combo_slot_id: Option<Uuid>,
    #[serde(default)]
    pub combo_slot_name: Option<String>,
    /// A header: P per combo unit.
    #[serde(default)]
    pub combo_unit_price: Option<i32>,
    /// A part: its share of P and its surcharges, whole line (its
    /// `line_total` is their sum; its add-ons and optionals are extra).
    #[serde(default)]
    pub combo_share: i32,
    #[serde(default)]
    pub combo_surcharge: i32,
    /// A plain line: what an applied deal took off `line_total`.
    #[serde(default)]
    pub deal_minor: i32,
    /// A part: the kitchen's combo tag (C12).
    #[serde(default)]
    pub combo: Option<crate::combos::types::KitchenComboTag>,
}

impl SnapshotLine {
    /// What the line adds to the subtotal: a part's add-ons and optionals are
    /// on top of its `line_total`; every other line's `line_total` already
    /// holds them.
    pub fn charged(&self) -> i32 {
        if self.line_kind == "combo_part" {
            let extras: i32 = self
                .addons
                .iter()
                .map(|a| a.unit_price * a.quantity)
                .sum::<i32>()
                + self.optionals.iter().map(|o| o.price).sum::<i32>();
            self.line_total + extras * self.quantity
        } else {
            self.line_total
        }
    }
}

/// A deal the server applied at intake (owner answer §11.2), booked at
/// finalize as `order_deals`.
#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct SnapshotDeal {
    pub deal_rule_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub times: i32,
    pub discount: i32,
    /// (index into `lines`, units, the cut off that line).
    pub lines: Vec<SnapshotDealLine>,
}

#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct SnapshotDealLine {
    pub line_index: usize,
    pub units: i32,
    pub discount: i32,
}

/// The priced line snapshot stored in `delivery_orders.cart`.
#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct CartSnapshot {
    pub lines: Vec<SnapshotLine>,
    /// The deals applied at intake (additive).
    #[serde(default)]
    pub deals: Vec<SnapshotDeal>,
}

/// One frozen inventory deduction. Stored (as a flat list) in
/// `delivery_orders.deductions_snapshot`; `line_index` ties each back to its
/// `cart.lines[i]` so finalize can attach per-line `deductions_snapshot` columns.
#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct SnapshotDeduction {
    pub line_index: usize,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: String,
    pub unit: String,
    pub quantity: f64,
    pub source: String,
    pub category: String,
    pub addon_item_id: Option<Uuid>,
    pub optional_field_id: Option<Uuid>,
    pub cost_per_unit: Option<f64>,
    pub line_cost: Option<i64>,
}

/// Output of [`resolve_cart`].
pub struct ResolvedCart {
    pub snapshot: CartSnapshot,
    pub deductions: Vec<SnapshotDeduction>,
    /// Sum of line totals (items + addons + optionals), pre-tax, pre-fee,
    /// after the deals.
    pub subtotal: i32,
    /// The same cart as the public cart quote shows it, line by input line.
    pub quote: crate::deals::types::CartQuote,
}

// ── Intake: resolve + freeze ──────────────────────────────────

/// Server-price and freeze a cart for a branch + channel. Rejects unknown,
/// deleted, or channel/branch-disabled items (the public menu would not have
/// offered them).
pub async fn resolve_cart(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    // The delivery channel for public/delivery orders, or `None` for dine-in
    // (POS / waiter tickets). When `None`, the `branch_channel_*_overrides`
    // joins bind `NULL::delivery_channel` and never match, so pricing/availability
    // fall back to the org→branch layer — exactly the dine-in behaviour.
    channel: Option<&str>,
    lines: &[CartLineInput],
    at: DateTime<Utc>,
) -> Result<ResolvedCart, AppError> {
    let sale = if channel.is_some() {
        madar_catalog::combo::Channel::Online
    } else {
        madar_catalog::combo::Channel::Pos
    };
    resolve_cart_as(pool, org_id, branch_id, channel, sale, lines, at).await
}

/// The cart as an order line the shared resolvers read.
fn as_input(line: &CartLineInput) -> crate::orders::handlers::OrderItemInput {
    crate::orders::handlers::OrderItemInput {
        menu_item_id: Some(line.menu_item_id),
        size_label: line.size_label.clone(),
        quantity: line.quantity,
        addons: line
            .addons
            .iter()
            .map(|a| AddonInput {
                unit_price: None,
                ..a.clone()
            })
            .collect(),
        optional_field_ids: line.optional_field_ids.clone(),
        notes: line.notes.clone(),
        combo: line.combo.clone(),
        ..Default::default()
    }
}

/// Stamp each deduction with its ingredient's cost at `at`.
async fn enrich_costs(
    pool: &PgPool,
    branch_id: Uuid,
    deductions: &mut [SnapshotDeduction],
    at: DateTime<Utc>,
) -> Result<(), AppError> {
    let ids: Vec<Uuid> = deductions
        .iter()
        .filter_map(|d| d.org_ingredient_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let costs = crate::costing::ingredient_costs_at(pool, branch_id, &ids, at).await?;
    for d in deductions {
        let c = d
            .org_ingredient_id
            .and_then(|id| costs.get(&id))
            .and_then(|c| c.to_f64());
        d.cost_per_unit = c;
        d.line_cost = c.map(|c| (d.quantity * c).round() as i64);
    }
    Ok(())
}

/// A delivery sub-channel switch-off of `item` (`None`: no override).
async fn channel_off(
    pool: &PgPool,
    branch_id: Uuid,
    channel: Option<&str>,
    item: Uuid,
) -> Result<bool, AppError> {
    let Some(ch) = channel else {
        return Ok(false);
    };
    let off: Option<Option<bool>> = sqlx::query_scalar(
        "SELECT is_available FROM branch_channel_menu_overrides \
          WHERE branch_id = $1 AND menu_item_id = $2 AND channel = $3::delivery_channel",
    )
    .bind(branch_id)
    .bind(item)
    .bind(ch)
    .fetch_optional(pool)
    .await?;
    Ok(off.flatten() == Some(false))
}

/// [`resolve_cart`] for a sale channel: `Online` (the storefront, with its
/// delivery sub-channel), `Qr` (a table's cart quote, dine-in prices) or
/// `Pos`. Combos are priced by `combos::order_line` (a public channel refuses
/// an unavailable one); on a public channel the best deals are applied.
pub async fn resolve_cart_as(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    channel: Option<&str>,
    sale_channel: madar_catalog::combo::Channel,
    lines: &[CartLineInput],
    at: DateTime<Utc>,
) -> Result<ResolvedCart, AppError> {
    use crate::delivery::{
        MAX_ADDON_QTY, MAX_CART_LINES, MAX_LINE_NOTES_LEN, MAX_LINE_QTY, MAX_SIZE_LABEL_LEN,
    };

    if lines.is_empty() {
        return Err(AppError::BadRequest("Cart is empty".into()));
    }
    if lines.len() > MAX_CART_LINES {
        return Err(AppError::BadRequest(format!(
            "A cart may contain at most {MAX_CART_LINES} items"
        )));
    }

    let mut snapshot_lines: Vec<SnapshotLine> = Vec::new();
    let mut all_deductions: Vec<SnapshotDeduction> = Vec::new();
    let mut subtotal: i32 = 0;
    let mut quoted: Vec<crate::deals::types::QuotedLine> = Vec::with_capacity(lines.len());
    // Plain lines a deal may take: (snapshot index, input index).
    let mut plain: Vec<(usize, usize)> = Vec::new();
    let inputs: Vec<crate::orders::handlers::OrderItemInput> = lines.iter().map(as_input).collect();
    let ctx = crate::combos::order_line::load_ctx(pool, org_id, branch_id, &inputs, at, true)
        .await?
        .ok_or(AppError::Internal)?;
    let mut catalog = crate::orders::catalog_view::Catalog::new(Some(branch_id));

    for (idx, line) in lines.iter().enumerate() {
        if ctx.is_combo(Some(line.menu_item_id)) {
            if line.quantity <= 0 || line.quantity > MAX_LINE_QTY {
                return Err(AppError::BadRequest(format!(
                    "Item quantity must be between 1 and {MAX_LINE_QTY}"
                )));
            }
            let (charged, q) = resolve_combo_line(
                pool,
                &mut catalog,
                &ctx,
                branch_id,
                channel,
                sale_channel,
                line,
                &inputs[idx],
                at,
                &mut snapshot_lines,
                &mut all_deductions,
            )
            .await?;
            subtotal = subtotal
                .checked_add(charged)
                .ok_or_else(|| AppError::BadRequest("Order total is too large".into()))?;
            quoted.push(crate::deals::types::QuotedLine {
                index: idx as i32,
                quantity: line.quantity,
                unit_price: 0,
                line_total: charged,
                deal_minor: 0,
                combo: Some(q),
            });
            continue;
        }
        let idx_in = idx;
        let idx = snapshot_lines.len();
        if line.quantity <= 0 || line.quantity > MAX_LINE_QTY {
            return Err(AppError::BadRequest(format!(
                "Item quantity must be between 1 and {MAX_LINE_QTY}"
            )));
        }
        if line
            .addons
            .iter()
            .any(|a| a.quantity <= 0 || a.quantity > MAX_ADDON_QTY)
        {
            return Err(AppError::BadRequest(format!(
                "Addon quantity must be between 1 and {MAX_ADDON_QTY}"
            )));
        }
        if let Some(notes) = &line.notes
            && notes.chars().count() > MAX_LINE_NOTES_LEN
        {
            return Err(AppError::BadRequest(format!(
                "Item notes must be at most {MAX_LINE_NOTES_LEN} characters"
            )));
        }
        if let Some(size) = &line.size_label
            && size.chars().count() > MAX_SIZE_LABEL_LEN
        {
            return Err(AppError::BadRequest("Invalid size".into()));
        }

        // Resolve org → branch → channel price + availability.
        #[allow(clippy::type_complexity)]
        let row: Option<(
            String,
            serde_json::Value,
            i32,
            Option<i32>,
            bool,
            Option<i32>,
            Option<bool>,
        )> = sqlx::query_as(
            r#"SELECT mi.name, mi.name_translations, mi.base_price,
                          bmo.price_override,
                          COALESCE(bmo.is_available, true) AS branch_available,
                          bcmo.price_override AS channel_price,
                          bcmo.is_available  AS channel_available
                   FROM menu_items mi
                   LEFT JOIN branch_menu_overrides bmo
                          ON bmo.menu_item_id = mi.id AND bmo.branch_id = $2
                   LEFT JOIN branch_channel_menu_overrides bcmo
                          ON bcmo.menu_item_id = mi.id AND bcmo.branch_id = $2
                         AND bcmo.channel = $3::delivery_channel
                   WHERE mi.id = $1 AND mi.org_id = $4 AND mi.deleted_at IS NULL"#,
        )
        .bind(line.menu_item_id)
        .bind(branch_id)
        .bind(channel)
        .bind(org_id)
        .fetch_optional(pool)
        .await?;

        let (
            item_name,
            name_translations,
            base_price,
            branch_price,
            branch_available,
            channel_price,
            channel_available,
        ) = row.ok_or_else(|| {
            AppError::NotFound(format!("Menu item {} not found", line.menu_item_id))
        })?;

        // channel availability overrides branch availability; either can disable.
        let available = channel_available.unwrap_or(branch_available);
        if !available {
            return Err(AppError::BadRequest(format!(
                "{} is not available for this channel",
                item_name
            )));
        }

        // Channel price wins over branch over catalog (first non-NULL).
        let effective_base = channel_price.or(branch_price).unwrap_or(base_price);

        let unit_price = match &line.size_label {
            Some(size) => {
                resolve_size_price(pool, branch_id, line.menu_item_id, size, effective_base).await?
            }
            None => effective_base,
        };

        // Reuse the shared config resolver for addons / optionals / recipe deductions
        // (branch-effective addon prices; no channel addon layer yet).
        let config = resolve_menu_item_configuration(
            pool,
            line.menu_item_id,
            line.size_label.clone(),
            line.quantity,
            &line.addons,
            &line.optional_field_ids,
            branch_id,
        )
        .await?;

        // Cost-enrich this line's deductions at the current ingredient costs.
        let mut line_deductions: Vec<SnapshotDeduction> = config
            .deductions
            .into_iter()
            .map(|d| SnapshotDeduction {
                line_index: idx,
                org_ingredient_id: d.org_ingredient_id,
                ingredient_name: d.ingredient_name,
                unit: d.unit,
                quantity: d.quantity,
                source: d.source,
                category: d.category,
                addon_item_id: d.addon_item_id,
                optional_field_id: d.optional_field_id,
                cost_per_unit: None,
                line_cost: None,
            })
            .collect();

        enrich_costs(pool, branch_id, &mut line_deductions, at).await?;

        let (line_cost, unit_cost, cost_missing) =
            rollup_line_cost(&line_deductions, line.quantity);

        // Addons: server prices, with per-addon COGS rolled from attributed deductions.
        let mut addons: Vec<SnapshotAddon> = config
            .addons
            .into_iter()
            .map(|a| {
                let line_cost = addon_cost(&line_deductions, a.addon_item_id);
                SnapshotAddon {
                    addon_item_id: a.addon_item_id,
                    addon_name: a.addon_name,
                    name_translations: a.name_translations,
                    unit_price: a.unit_price,
                    quantity: a.quantity,
                    line_cost,
                }
            })
            .collect();

        // Channel addon overrides layer on top of the branch-effective addon price:
        // a price_override replaces the charged price; is_available=false rejects it.
        if !addons.is_empty() {
            let addon_ids: Vec<Uuid> = addons.iter().map(|a| a.addon_item_id).collect();
            let rows: Vec<(Uuid, Option<i32>, Option<bool>)> = sqlx::query_as(
                "SELECT addon_item_id, price_override, is_available \
                 FROM branch_channel_addon_overrides \
                 WHERE branch_id = $1 AND channel = $2::delivery_channel AND addon_item_id = ANY($3)",
            )
            .bind(branch_id)
            .bind(channel)
            .bind(&addon_ids)
            .fetch_all(pool)
            .await?;
            let ovr: std::collections::HashMap<Uuid, (Option<i32>, Option<bool>)> =
                rows.into_iter().map(|(id, p, a)| (id, (p, a))).collect();
            for a in &mut addons {
                if let Some((price, avail)) = ovr.get(&a.addon_item_id) {
                    if *avail == Some(false) {
                        return Err(AppError::BadRequest(format!(
                            "{} is not available for this channel",
                            a.addon_name
                        )));
                    }
                    if let Some(p) = price {
                        a.unit_price = *p;
                    }
                }
            }
        }

        let optionals: Vec<SnapshotOptional> = config
            .optionals
            .into_iter()
            .map(|o| {
                let cost = match (o.quantity_used, o.org_ingredient_id) {
                    (Some(qty), Some(_)) => line_deductions
                        .iter()
                        .find(|d| d.optional_field_id == Some(o.optional_field_id))
                        .and_then(|d| d.cost_per_unit)
                        .map(|c| (qty * c).round() as i64),
                    _ => Some(0),
                };
                SnapshotOptional {
                    optional_field_id: o.optional_field_id,
                    field_name: o.field_name,
                    name_translations: o.name_translations,
                    price: o.price,
                    org_ingredient_id: o.org_ingredient_id,
                    ingredient_name: o.ingredient_name,
                    ingredient_unit: o.ingredient_unit,
                    quantity_used: o.quantity_used,
                    cost,
                }
            })
            .collect();

        // Money math in i64, then range-check back to i32. Even with the per-field
        // caps above this guards against a server-side price/quantity combination
        // overflowing the integer-piastre line total (release builds do not panic
        // on overflow — they would silently wrap and mis-price the order).
        let too_large = || AppError::BadRequest("Order total is too large".into());
        let addon_per_unit: i64 = addons
            .iter()
            .map(|a| a.unit_price as i64 * a.quantity as i64)
            .sum();
        let optional_per_unit: i64 = optionals.iter().map(|o| o.price as i64).sum();
        let line_total_i64 =
            (unit_price as i64 + addon_per_unit + optional_per_unit) * line.quantity as i64;
        let line_total = i32::try_from(line_total_i64).map_err(|_| too_large())?;
        subtotal = subtotal.checked_add(line_total).ok_or_else(too_large)?;

        snapshot_lines.push(SnapshotLine {
            menu_item_id: line.menu_item_id,
            item_name,
            name_translations,
            size_label: line.size_label.clone(),
            unit_price,
            quantity: line.quantity,
            line_total,
            notes: line.notes.clone(),
            addons,
            optionals,
            line_cost,
            unit_cost,
            cost_missing,
            line_kind: "item".into(),
            id: None,
            combo_line_id: None,
            combo_slot_id: None,
            combo_slot_name: None,
            combo_unit_price: None,
            combo_share: 0,
            combo_surcharge: 0,
            deal_minor: 0,
            combo: None,
        });
        all_deductions.extend(line_deductions);
        quoted.push(crate::deals::types::QuotedLine {
            index: idx_in as i32,
            quantity: line.quantity,
            unit_price,
            line_total,
            deal_minor: 0,
            combo: None,
        });
        plain.push((idx, idx_in));
    }
    let items_total = subtotal;

    // ── Deals (owner answer §11.2): on QR and online checkout the server
    // applies the best ones itself, off the plain lines before tax. ──
    let mut deals: Vec<SnapshotDeal> = Vec::new();
    let mut quoted_deals: Vec<crate::deals::types::QuotedDeal> = Vec::new();
    if sale_channel != madar_catalog::combo::Channel::Pos && !plain.is_empty() {
        let plain_lines: Vec<crate::deals::order::PlainLine> = plain
            .iter()
            .map(|&(sidx, _)| {
                let l = &snapshot_lines[sidx];
                crate::deals::order::PlainLine {
                    input_index: sidx,
                    menu_item_id: l.menu_item_id,
                    category_id: ctx.category_of(l.menu_item_id),
                    size_label: l.size_label.clone(),
                    unit_price: l.unit_price,
                    expected_unit_price: l.unit_price,
                    quantity: l.quantity,
                }
            })
            .collect();
        let mut conn = pool.acquire().await?;
        let apps = crate::deals::order::auto(&mut conn, &ctx, sale_channel, &plain_lines).await?;
        drop(conn);
        for app in apps {
            let mut snap_lines = Vec::with_capacity(app.lines.len());
            let mut quote_lines = Vec::with_capacity(app.lines.len());
            for &(sidx, units, cut, _) in &app.lines {
                let l = &mut snapshot_lines[sidx];
                l.deal_minor += cut;
                l.line_total -= cut;
                subtotal -= cut;
                let input = plain.iter().find(|p| p.0 == sidx).map_or(0, |p| p.1);
                if let Some(q) = quoted.iter_mut().find(|q| q.index == input as i32) {
                    q.deal_minor += cut;
                }
                snap_lines.push(SnapshotDealLine {
                    line_index: sidx,
                    units,
                    discount: cut,
                });
                quote_lines.push(crate::deals::types::DealLineInput {
                    line_index: input as i32,
                    units,
                });
            }
            quoted_deals.push(crate::deals::types::QuotedDeal {
                deal_rule_id: app.rule_id,
                name: app.name.clone(),
                name_translations: app.name_translations.clone(),
                times: app.times,
                discount: app.discount,
                lines: quote_lines,
            });
            deals.push(SnapshotDeal {
                deal_rule_id: app.rule_id,
                name: app.name,
                name_translations: app.name_translations,
                times: app.times,
                discount: app.discount,
                lines: snap_lines,
            });
        }
    }
    let deal_discount = items_total - subtotal;

    Ok(ResolvedCart {
        snapshot: CartSnapshot {
            lines: snapshot_lines,
            deals,
        },
        deductions: all_deductions,
        subtotal,
        quote: crate::deals::types::CartQuote {
            lines: quoted,
            items_total,
            deals: quoted_deals,
            deal_discount,
            total_after_deals: subtotal,
        },
    })
}

/// A combo line of the cart: its header and parts appended to `out` (their
/// deductions to `deductions`, costed at `at`). Returns what the line adds to
/// the subtotal and its quote.
#[allow(clippy::too_many_arguments)]
async fn resolve_combo_line(
    pool: &PgPool,
    catalog: &mut crate::orders::catalog_view::Catalog,
    ctx: &crate::combos::order_line::ComboCtx,
    branch_id: Uuid,
    channel: Option<&str>,
    sale_channel: madar_catalog::combo::Channel,
    line: &CartLineInput,
    input: &crate::orders::handlers::OrderItemInput,
    at: DateTime<Utc>,
    out: &mut Vec<SnapshotLine>,
    deductions: &mut Vec<SnapshotDeduction>,
) -> Result<(i32, crate::deals::types::QuotedCombo), AppError> {
    use crate::combos::{codes::refuse, order_line as ol};
    use serde_json::json;
    // The delivery sub-channel can switch the combo, or a pick, off.
    if channel_off(pool, branch_id, channel, line.menu_item_id).await? {
        return Err(refuse(
            "COMBO_UNAVAILABLE",
            json!({"combo_id": line.menu_item_id, "reason": "branch"}),
        ));
    }
    for p in line.combo.iter().flat_map(|c| c.picks.iter()) {
        if channel_off(pool, branch_id, channel, p.menu_item_id).await? {
            let item = ctx
                .items
                .get(&p.menu_item_id)
                .map(|f| f.name.clone())
                .unwrap_or_default();
            return Err(refuse(
                "COMBO_ITEM_UNAVAILABLE",
                json!({"menu_item_id": p.menu_item_id, "item": item}),
            ));
        }
    }
    let cl = ol::resolve(
        pool,
        catalog,
        ctx,
        input,
        ol::Sale {
            channel: sale_channel,
            prices: crate::orders::handlers::ClientPrices::Ignore,
            strict: true,
            judge_availability: true,
        },
    )
    .await?;

    let h = &cl.header;
    out.push(SnapshotLine {
        menu_item_id: line.menu_item_id,
        item_name: h.item_name.clone(),
        name_translations: h.name_translations.clone(),
        size_label: None,
        unit_price: 0,
        quantity: h.quantity,
        line_total: 0,
        notes: h.notes.clone(),
        addons: Vec::new(),
        optionals: Vec::new(),
        line_cost: Some(0),
        unit_cost: Some(0),
        cost_missing: false,
        line_kind: "combo".into(),
        id: h.combo.id,
        combo_line_id: None,
        combo_slot_id: None,
        combo_slot_name: None,
        combo_unit_price: h.combo.unit_price,
        combo_share: 0,
        combo_surcharge: 0,
        deal_minor: 0,
        combo: None,
    });

    let mut charged: i64 = 0;
    for part in &cl.parts {
        let sidx = out.len();
        let mut line_deductions: Vec<SnapshotDeduction> = part
            .deductions
            .iter()
            .map(|d| SnapshotDeduction {
                line_index: sidx,
                org_ingredient_id: d.org_ingredient_id,
                ingredient_name: d.ingredient_name.clone(),
                unit: d.unit.clone(),
                quantity: d.quantity,
                source: d.source.clone(),
                category: d.category.clone(),
                addon_item_id: d.addon_item_id,
                optional_field_id: d.optional_field_id,
                cost_per_unit: None,
                line_cost: None,
            })
            .collect();
        enrich_costs(pool, branch_id, &mut line_deductions, at).await?;
        let (line_cost, unit_cost, cost_missing) =
            rollup_line_cost(&line_deductions, part.quantity);
        let addons: Vec<SnapshotAddon> = part
            .addons
            .iter()
            .map(|a| SnapshotAddon {
                addon_item_id: a.addon_item_id,
                addon_name: a.addon_name.clone(),
                name_translations: a.name_translations.clone(),
                unit_price: a.unit_price,
                quantity: a.quantity,
                line_cost: addon_cost(&line_deductions, a.addon_item_id),
            })
            .collect();
        let optionals: Vec<SnapshotOptional> = part
            .optionals
            .iter()
            .map(|o| SnapshotOptional {
                optional_field_id: o.optional_field_id,
                field_name: o.field_name.clone(),
                name_translations: o.name_translations.clone(),
                price: o.price,
                org_ingredient_id: o.org_ingredient_id,
                ingredient_name: o.ingredient_name.clone(),
                ingredient_unit: o.ingredient_unit.clone(),
                quantity_used: o.quantity_used,
                cost: match (o.quantity_used, o.org_ingredient_id) {
                    (Some(qty), Some(_)) => line_deductions
                        .iter()
                        .find(|d| d.optional_field_id == Some(o.optional_field_id))
                        .and_then(|d| d.cost_per_unit)
                        .map(|c| (qty * c).round() as i64),
                    _ => Some(0),
                },
            })
            .collect();
        charged += i64::from(part.charged_subtotal());
        out.push(SnapshotLine {
            menu_item_id: part.menu_item_id.unwrap_or_default(),
            item_name: part.item_name.clone(),
            name_translations: part.name_translations.clone(),
            size_label: part.size_label.clone(),
            unit_price: part.unit_price,
            quantity: part.quantity,
            line_total: part.combo.share + part.combo.surcharge,
            notes: part.notes.clone(),
            addons,
            optionals,
            line_cost,
            unit_cost,
            cost_missing,
            line_kind: "combo_part".into(),
            id: part.combo.id,
            combo_line_id: part.combo.header_id,
            combo_slot_id: part.combo.slot_id,
            combo_slot_name: part.combo.slot_name.clone(),
            combo_unit_price: None,
            combo_share: part.combo.share,
            combo_surcharge: part.combo.surcharge,
            deal_minor: 0,
            combo: part.combo.tag.clone(),
        });
        deductions.extend(line_deductions);
    }
    let charged = i32::try_from(charged)
        .map_err(|_| AppError::BadRequest("Order total is too large".into()))?;
    let q = &cl.quote;
    let parse = |s: &str| Uuid::parse_str(s).unwrap_or_default();
    Ok((
        charged,
        crate::deals::types::QuotedCombo {
            price: q.price as i32,
            unit_total: q.unit_total as i32,
            saving_unit: q.saving_unit as i32,
            parts: q
                .parts
                .iter()
                .map(|p| crate::deals::types::QuotedComboPart {
                    slot_id: parse(&p.slot_id),
                    menu_item_id: parse(&p.menu_item_id),
                    size_label: p.size_label.clone(),
                    quantity: p.quantity as i32,
                    unit_price: p.unit_price as i32,
                    combo_share: p.combo_share as i32,
                    combo_surcharge: p.combo_surcharge as i32,
                    line_total: p.line_total as i32,
                    addons_total: p.addons_total as i32,
                })
                .collect(),
        },
    ))
}

/// branch-size override > catalog-size override > the branch/channel-effective base.
///
/// The size label arrives from an untrusted client. Comparing on `label::text`
/// (rather than casting the inbound string to ``) means an unknown
/// size is a clean 400 instead of a Postgres enum-cast 500. A size the item does
/// not actually offer is rejected — it must exist as an active catalog size.
async fn resolve_size_price(
    pool: &PgPool,
    branch_id: Uuid,
    menu_item_id: Uuid,
    size: &str,
    effective_base: i32,
) -> Result<i32, AppError> {
    let catalog: Option<Option<i32>> = sqlx::query_scalar(
        "SELECT price_override FROM item_sizes \
         WHERE menu_item_id = $1 AND label::text = $2 AND is_active = true",
    )
    .bind(menu_item_id)
    .bind(size)
    .fetch_optional(pool)
    .await?;
    let Some(catalog_override) = catalog else {
        return Err(AppError::BadRequest(format!("Unknown size '{size}'")));
    };
    let branch_size: Option<i32> = sqlx::query_scalar(
        "SELECT price_override FROM branch_menu_size_overrides \
         WHERE branch_id = $1 AND menu_item_id = $2 AND size_label::text = $3",
    )
    .bind(branch_id)
    .bind(menu_item_id)
    .bind(size)
    .fetch_optional(pool)
    .await?;
    Ok(branch_size.or(catalog_override).unwrap_or(effective_base))
}

/// Full line COGS, recipe-scope unit cost, and a cost-missing flag — mirrors the
/// POS `summarize_line_costs` rollup for one line's deductions.
fn rollup_line_cost(
    deductions: &[SnapshotDeduction],
    quantity: i32,
) -> (Option<i64>, Option<i64>, bool) {
    let cost_missing = deductions.is_empty() || deductions.iter().any(|d| d.line_cost.is_none());
    let line_cost = if cost_missing {
        None
    } else {
        Some(
            deductions
                .iter()
                .filter_map(|d| d.cost_per_unit.map(|c| c * d.quantity))
                .sum::<f64>()
                .round() as i64,
        )
    };
    let recipe: Vec<&SnapshotDeduction> = deductions
        .iter()
        .filter(|d| d.source == "drink_recipe" || d.source.starts_with("addon_swap:"))
        .collect();
    let unit_cost = if recipe.is_empty() || recipe.iter().any(|d| d.cost_per_unit.is_none()) {
        None
    } else {
        Some(
            (recipe
                .iter()
                .map(|d| d.cost_per_unit.unwrap() * d.quantity)
                .sum::<f64>()
                / quantity.max(1) as f64)
                .round() as i64,
        )
    };
    (line_cost, unit_cost, cost_missing)
}

/// Additive-addon COGS: sum the deductions attributed to this addon. `None` when
/// the addon has no ingredient rows or any attributed cost is unknown (matches POS).
fn addon_cost(deductions: &[SnapshotDeduction], addon_item_id: Uuid) -> Option<i64> {
    let entries: Vec<&SnapshotDeduction> = deductions
        .iter()
        .filter(|d| d.source == "addon" && d.addon_item_id == Some(addon_item_id))
        .collect();
    if entries.is_empty() || entries.iter().any(|d| d.cost_per_unit.is_none()) {
        None
    } else {
        Some(
            entries
                .iter()
                .map(|d| d.cost_per_unit.unwrap() * d.quantity)
                .sum::<f64>()
                .round() as i64,
        )
    }
}

// ── Finalize: replay snapshot into a real orders row ──────────

/// Everything the sale is booked from. Every money figure here is FROZEN — read
/// off the `delivery_orders` row, never computed at finalize — and the orders
/// row CHECKs that they agree with each other, so a caller that invents one is
/// refused rather than recorded.
pub struct FinalizeCtx<'a> {
    pub branch_id: Uuid,
    pub till_id: Uuid,
    pub teller_id: Uuid,
    pub payment_method: &'a str,
    pub is_cash: bool,
    pub created_at: DateTime<Utc>,
    pub subtotal: i32,
    /// Inside `total_amount` when `tax_inclusive`, added to it otherwise.
    pub tax_amount: i32,
    /// The policy this bill was priced under, recorded alongside the figures it
    /// produced. See `orders.tax_rate_applied`. For a delivery the service
    /// charge pair is zero — dine-in only, by ruling — and `orders` CHECKs it.
    pub service_charge_amount: i32,
    pub tax_rate_applied: rust_decimal::Decimal,
    pub service_charge_rate_applied: rust_decimal::Decimal,
    pub tax_inclusive: bool,
    pub delivery_fee: i32,
    /// `subtotal - discount_amount + service_charge_amount + delivery_fee`, plus
    /// `tax_amount` when exclusive. The quote's own `total`.
    pub total_amount: i32,
    /// Frozen channel discount (item subtotal only). `discount_amount` is 0
    /// when none.
    pub discount_id: Option<Uuid>,
    pub discount_type: Option<&'a str>,
    pub discount_value: rust_decimal::Decimal,
    pub discount_amount: i32,
    pub customer_name: Option<&'a str>,
    /// Whose sale this is: the delivery order's customer, resolved through the
    /// merge chain as the row is written. Soft, like every customer reference.
    pub customer_id: Option<Uuid>,
    pub notes: Option<&'a str>,
    /// `'delivery'` — for every online channel, pickup included: a pickup is
    /// a delivery channel, not a takeaway (see `orders.order_type`). Carried
    /// rather than hard-coded so the row states its kind where it is written.
    pub order_type: &'a str,
    /// The quote this sale settles. `orders.delivery_order_id`; the read model
    /// hydrates channel and coordinates through it.
    pub delivery_order_id: Option<Uuid>,
}

/// The sale a snapshot became: its id and the reference printed on the receipt.
/// Deliberately not the full `Order` read model — the row is read back through
/// the orders API like any other sale, and returning only the keys keeps this
/// replay from having to track every column that model grows.
pub struct MaterializedOrder {
    pub id: Uuid,
    pub order_ref: String,
}

/// Replay the frozen snapshot into a normal completed `orders` row inside an
/// existing transaction. Returns the created sale plus any oversold warnings.
pub async fn apply_snapshot(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ctx: &FinalizeCtx<'_>,
    lines: &[SnapshotLine],
    deals: &[SnapshotDeal],
    deductions: &[SnapshotDeduction],
) -> Result<(MaterializedOrder, Vec<String>), AppError> {
    let order_number: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(order_number), 0) + 1 FROM orders WHERE till_id = $1",
    )
    .bind(ctx.till_id)
    .fetch_one(&mut **tx)
    .await?;

    // Mint a normal order_ref off the per-(branch, business_date) counter — same
    // infra POS orders use (the delivery_ref was minted separately at intake).
    let (branch_code, biz_date): (String, chrono::NaiveDate) = sqlx::query_as(
        "SELECT b.code, ($1::timestamptz AT TIME ZONE COALESCE(b.timezone, o.timezone)::text)::date
         FROM branches b JOIN organizations o ON o.id = b.org_id WHERE b.id = $2",
    )
    .bind(ctx.created_at)
    .bind(ctx.branch_id)
    .fetch_one(&mut **tx)
    .await?;
    let ref_seq: i32 = sqlx::query_scalar(
        "INSERT INTO order_ref_counters (branch_id, business_date, last_seq)
         VALUES ($1, $2, 1)
         ON CONFLICT (branch_id, business_date)
         DO UPDATE SET last_seq = order_ref_counters.last_seq + 1
         RETURNING last_seq",
    )
    .bind(ctx.branch_id)
    .bind(biz_date)
    .fetch_one(&mut **tx)
    .await?;
    let order_ref = format!(
        "{}-{}-{:04}",
        branch_code,
        madar_time::yymmdd(biz_date),
        ref_seq
    );

    // `service_charge_taxable_applied` is left NULL on purpose. The quote does
    // not record it — with the service-charge rate pinned at zero the flag
    // cannot influence a figure, and a column that records a policy that
    // cannot apply is one that lies the day someone reads it. Writing the
    // branch's flag of THIS moment would be the one act of pricing this replay
    // must not do.
    let order_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO orders
            (branch_id, till_id, teller_id, order_number,
             payment_method, subtotal,
             discount_type, discount_value, discount_amount, discount_id,
             tax_amount, total_amount, tip_amount, status,
             customer_name, notes, created_at, order_ref,
             price_flagged, price_expected_total, tip_is_cash,
             order_type, delivery_fee, delivery_order_id,
             service_charge_amount, tax_rate_applied,
             service_charge_rate_applied, tax_inclusive, customer_id)
        VALUES ($1, $2, $3, $4, $5, $6,
                $15::discount_type, $16, $17, $18,
                $7, $8, 0, 'completed',
                $9, $10, $11, $12,
                false, $8, NULL,
                $19, $13, $14,
                $20, $21, $22, $23,
                COALESCE((SELECT customers_resolve(b.org_id, $24) FROM branches b WHERE b.id = $1), $24))
        RETURNING id
        "#,
    )
    .bind(ctx.branch_id)
    .bind(ctx.till_id)
    .bind(ctx.teller_id)
    .bind(order_number)
    .bind(ctx.payment_method)
    .bind(ctx.subtotal)
    .bind(ctx.tax_amount)
    .bind(ctx.total_amount)
    .bind(ctx.customer_name)
    .bind(ctx.notes)
    .bind(ctx.created_at)
    .bind(&order_ref)
    .bind(ctx.delivery_fee)
    .bind(ctx.delivery_order_id)
    .bind(ctx.discount_type) // $15
    .bind(ctx.discount_value) // $16
    .bind(ctx.discount_amount) // $17
    .bind(ctx.discount_id) // $18
    .bind(ctx.order_type) // $19
    .bind(ctx.service_charge_amount) // $20
    .bind(ctx.tax_rate_applied) // $21
    .bind(ctx.service_charge_rate_applied) // $22
    .bind(ctx.tax_inclusive) // $23
    .bind(ctx.customer_id) // $24
    .fetch_one(&mut **tx)
    .await?;
    let order = MaterializedOrder {
        id: order_id,
        order_ref,
    };

    sqlx::query(
        "INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1, $2, $3, $4)",
    )
    .bind(order.id)
    .bind(ctx.payment_method)
    .bind(ctx.total_amount)
    .bind(ctx.is_cash)
    .execute(&mut **tx)
    .await?;

    let mut warnings: Vec<String> = Vec::new();
    // The stored row of each snapshot line, for the deals that name them.
    let mut row_of: std::collections::HashMap<usize, Uuid> = std::collections::HashMap::new();

    for (idx, line) in lines.iter().enumerate() {
        let line_deductions: Vec<&SnapshotDeduction> =
            deductions.iter().filter(|d| d.line_index == idx).collect();
        let snapshot_json = serde_json::to_value(&line_deductions)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));

        let item_id: Uuid = sqlx::query_scalar(
            r#"INSERT INTO order_items
                (order_id, menu_item_id, item_name, name_translations, size_label,
                 unit_price, quantity, line_total, notes, deductions_snapshot,
                 line_cost, unit_cost, cost_missing, price_flagged,
                 id, line_kind, combo_line_id, combo_slot_id, combo_slot_name,
                 combo_unit_price, combo_share, combo_surcharge, deal_minor)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, false,
                       COALESCE($14, gen_random_uuid()), $15, $16, $17, $18, $19, $20, $21, $22)
               RETURNING id"#,
        )
        .bind(order.id)
        .bind(line.menu_item_id)
        .bind(&line.item_name)
        .bind(&line.name_translations)
        .bind(&line.size_label)
        .bind(line.unit_price)
        .bind(line.quantity)
        .bind(line.line_total)
        .bind(&line.notes)
        .bind(snapshot_json)
        .bind(line.line_cost)
        .bind(line.unit_cost)
        .bind(line.cost_missing)
        .bind(line.id)
        .bind(&line.line_kind)
        .bind(line.combo_line_id)
        .bind(line.combo_slot_id)
        .bind(&line.combo_slot_name)
        .bind(line.combo_unit_price)
        .bind(line.combo_share)
        .bind(line.combo_surcharge)
        .bind(line.deal_minor)
        .fetch_one(&mut **tx)
        .await?;
        row_of.insert(idx, item_id);

        for addon in &line.addons {
            let addon_line = addon.unit_price * addon.quantity * line.quantity;
            sqlx::query(
                "INSERT INTO order_item_addons \
                    (order_item_id, addon_item_id, addon_name, name_translations, unit_price, quantity, line_total, line_cost) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(item_id)
            .bind(addon.addon_item_id)
            .bind(&addon.addon_name)
            .bind(&addon.name_translations)
            .bind(addon.unit_price)
            .bind(addon.quantity)
            .bind(addon_line)
            .bind(addon.line_cost)
            .execute(&mut **tx)
            .await?;
        }

        for opt in &line.optionals {
            sqlx::query(
                "INSERT INTO order_item_optionals \
                    (order_item_id, optional_field_id, field_name, name_translations, price, \
                     org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted, cost) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            )
            .bind(item_id)
            .bind(opt.optional_field_id)
            .bind(&opt.field_name)
            .bind(&opt.name_translations)
            .bind(opt.price)
            .bind(opt.org_ingredient_id)
            .bind(&opt.ingredient_name)
            .bind(&opt.ingredient_unit)
            .bind(opt.quantity_used)
            .bind(opt.cost)
            .execute(&mut **tx)
            .await?;
        }

        apply_one_deductions(
            tx,
            ctx.branch_id,
            order.id,
            ctx.teller_id,
            &line_deductions,
            &mut warnings,
        )
        .await?;
    }

    // The deals applied at intake, as the till's are booked.
    if !deals.is_empty() {
        let org_id: Uuid = sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
            .bind(ctx.branch_id)
            .fetch_one(&mut **tx)
            .await?;
        let priced: Vec<crate::deals::order::PricedDeal> = deals
            .iter()
            .map(|d| crate::deals::order::PricedDeal {
                rule_id: d.deal_rule_id,
                name: d.name.clone(),
                name_translations: d.name_translations.clone(),
                times: d.times,
                discount: d.discount,
                discount_server: Some(d.discount),
                lines: d
                    .lines
                    .iter()
                    .map(|l| (l.line_index, l.units, l.discount, l.discount))
                    .collect(),
            })
            .collect();
        crate::deals::order::insert(tx, org_id, order.id, &priced, &row_of).await?;
    }

    Ok((order, warnings))
}

/// Post a line's frozen ingredient plan to the ledger as `sale` movements
/// (negative stock allowed but flagged), mirroring the POS path exactly.
async fn apply_one_deductions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    branch_id: Uuid,
    order_id: Uuid,
    teller_id: Uuid,
    deductions: &[&SnapshotDeduction],
    warnings: &mut Vec<String>,
) -> Result<(), AppError> {
    for d in deductions {
        let Some(ing_id) = d.org_ingredient_id else {
            continue;
        };
        let posted = crate::inventory::movements::record_movement(
            &mut **tx,
            crate::inventory::movements::MovementParams {
                branch_id,
                org_ingredient_id: ing_id,
                movement_type: "sale",
                quantity: -d.quantity,
                unit_cost: d.cost_per_unit.map(|c| c.round() as i64),
                reason: None,
                source_type: Some("order"),
                source_id: Some(order_id),
                note: None,
                created_by: Some(teller_id),
            },
        )
        .await?;
        if posted.below_zero {
            warnings.push(format!(
                "{} is oversold — stock is now {:.3} {}",
                d.ingredient_name, posted.balance_after, d.unit
            ));
        }
    }
    Ok(())
}

// ── What the kitchen would read ───────────────────────────────

/// The slim kitchen display lines for a frozen cart — the same projection the
/// till's `create_order_inner` builds for a counter order (addons as
/// modifiers, `2× Extra shot` when repeated; no prices), so a delivery round
/// renders on the KDS exactly like a till round. Server-minted ids: an online
/// order is never projected offline, so there is no client id to honour.
///
/// Built for the confirm step, which cannot fire it yet: `kitchen_tickets`
/// names its source by `order_id` or `open_ticket_id` (exactly one, CHECKed)
/// and a delivery order has neither until the door. See `staff::set_status`.
pub fn kitchen_lines(cart: &CartSnapshot) -> Vec<crate::kitchen::KitchenLine> {
    cart.lines
        .iter()
        // A combo's header is never fired; each part goes to its station,
        // tagged with the combo (C12).
        .filter(|line| line.line_kind != "combo")
        .map(|line| {
            let modifiers = line
                .addons
                .iter()
                .map(|a| {
                    if a.quantity > 1 {
                        format!("{}× {}", a.quantity, a.addon_name)
                    } else {
                        a.addon_name.clone()
                    }
                })
                .collect();
            crate::kitchen::KitchenLine {
                combo: line.combo.clone(),
                menu_item_id: Some(line.menu_item_id),
                name: line.item_name.clone(),
                qty: line.quantity,
                size_label: line.size_label.clone(),
                modifiers,
                notes: line.notes.clone(),
                kitchen_item_id: None,
                // A delivery order has no bill lines to void one of.
                open_ticket_item_id: None,
            }
        })
        .collect()
}

/// Cancel-with-waste: the food was made but not delivered. Deduct the frozen plan
/// from stock (it was never deducted — no orders row existed) and log each as a
/// `waste` movement against the delivery order. Best-effort per ingredient.
pub async fn record_waste(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    branch_id: Uuid,
    delivery_order_id: Uuid,
    deductions: &[SnapshotDeduction],
    created_by: Uuid,
) -> Result<(), AppError> {
    for d in deductions {
        let Some(ing_id) = d.org_ingredient_id else {
            continue;
        };
        crate::inventory::movements::record_movement(
            &mut **tx,
            crate::inventory::movements::MovementParams {
                branch_id,
                org_ingredient_id: ing_id,
                movement_type: "waste",
                quantity: -d.quantity,
                unit_cost: d.cost_per_unit.map(|c| c.round() as i64),
                reason: Some("order_cancelled"),
                source_type: Some("delivery_order"),
                source_id: Some(delivery_order_id),
                note: Some("Delivery order cancelled — made, not restocked"),
                created_by: Some(created_by),
            },
        )
        .await?;
    }
    Ok(())
}
