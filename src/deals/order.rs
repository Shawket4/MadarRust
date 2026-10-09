//! Deals on an order (COMBOS_CONTRACT.md §3.1, §5; owner answers §11.2).
//!
//! - The till: the teller applied each deal on named units; the server prices
//!   exactly those units with `madar_catalog::deal::price_application`. Live a
//!   deal that does not hold is `409 DEAL_NOT_ELIGIBLE` and the capability
//!   `orders.deals.apply` is required; on replay the till's discount is kept,
//!   the server's verdict is stored beside it and a difference is flagged.
//! - QR and online: [`auto`] applies the best deals itself
//!   (`madar_catalog::deal::auto_apply`) over the plain lines.
//!
//! Only plain lines take part: never a combo header or part, a reward or a
//! staff drink. A deal covers the item's size price only; add-ons pay.

use std::collections::HashMap;

use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use madar_catalog::deal as md;

use crate::{
    combos::{codes::refuse, order_line::ComboCtx, types::ChannelToggles},
    deals::types::{DealApplicationInput, DealRule, OrderDeal, OrderDealLine},
    errors::AppError,
};

/// madar-catalog's view of a rule (already resolved for the branch).
pub fn view_of(r: &DealRule) -> md::DealView {
    let entry = |e: &crate::deals::types::DealPoolEntry| md::PoolEntry {
        menu_item_id: e.menu_item_id.map(|i| i.to_string()),
        category_id: e.category_id.map(|i| i.to_string()),
        size_label: e.size_label.clone(),
    };
    md::DealView {
        id: r.id.to_string(),
        name: r.name.clone(),
        kind: r.kind.clone(),
        qty: i64::from(r.qty),
        price: r.price.map(i64::from),
        get_qty: r.get_qty.map(i64::from),
        get_percent: r.get_percent.map(i64::from),
        max_per_order: r.max_per_order.map(i64::from),
        sort: i64::from(r.sort),
        is_active: r.is_active,
        pool: r.pool.iter().map(entry).collect(),
        reward_pool: r.reward_pool.iter().map(entry).collect(),
        windows: r
            .windows
            .iter()
            .map(crate::combos::load::window_view)
            .collect(),
    }
}

/// One plain line of the order as a deal sees it.
#[derive(Clone, Debug)]
pub struct PlainLine {
    /// Index into the order's `items[]`.
    pub input_index: usize,
    pub menu_item_id: Uuid,
    pub category_id: Option<Uuid>,
    pub size_label: Option<String>,
    /// One unit at its size, no add-ons, as charged.
    pub unit_price: i32,
    /// The same at the catalogue's price (the server's verdict uses it).
    pub expected_unit_price: i32,
    pub quantity: i32,
}

fn deal_lines(lines: &[PlainLine], expected: bool) -> Vec<md::DealLine> {
    lines
        .iter()
        .map(|l| md::DealLine {
            line_index: l.input_index,
            menu_item_id: l.menu_item_id.to_string(),
            category_id: l.category_id.map(|c| c.to_string()),
            size_label: l.size_label.clone(),
            unit_price: i64::from(if expected {
                l.expected_unit_price
            } else {
                l.unit_price
            }),
            quantity: i64::from(l.quantity),
        })
        .collect()
}

/// A deal as it will be stored.
#[derive(Clone, Debug)]
pub struct PricedDeal {
    pub rule_id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
    pub times: i32,
    /// What came off the lines (the till's figure on a replay).
    pub discount: i32,
    pub discount_server: Option<i32>,
    /// (input index, units, discount as charged, the server's cut).
    pub lines: Vec<(usize, i32, i32, i32)>,
}

/// The rules of the org by id, each resolved for the branch (deleted ones
/// too, by name only, so a replay of a deal deleted since keeps its name).
async fn rules_for(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch_id: Uuid,
    sell: ChannelToggles,
    ids: &[Uuid],
) -> Result<
    (
        HashMap<Uuid, DealRule>,
        HashMap<Uuid, (String, serde_json::Value)>,
    ),
    AppError,
> {
    let live = crate::deals::load::load_rules(&mut *conn, org_id, Some(ids)).await?;
    let names: Vec<(Uuid, String, serde_json::Value)> = sqlx::query_as(
        "SELECT id, name, name_translations FROM deal_rules WHERE org_id = $1 AND id = ANY($2)",
    )
    .bind(org_id)
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok((
        live.into_iter()
            .map(|r| (r.id, crate::deals::load::for_branch(r, branch_id, sell)))
            .collect(),
        names.into_iter().map(|(i, n, t)| (i, (n, t))).collect(),
    ))
}

/// Price the deals a till applied. Returns the priced deals and the replay
/// flags they raise. `may_apply`: the author holds `orders.deals.apply`.
pub async fn price_applied(
    conn: &mut PgConnection,
    ctx: &ComboCtx,
    inputs: &[DealApplicationInput],
    lines: &[PlainLine],
    replay: bool,
    may_apply: bool,
) -> Result<(Vec<PricedDeal>, Vec<String>), AppError> {
    let mut flags: Vec<String> = Vec::new();
    if inputs.is_empty() {
        return Ok((Vec::new(), flags));
    }
    if !may_apply {
        if !replay {
            return Err(AppError::Forbidden(
                "Applying a deal needs the orders.deals.apply permission".into(),
            ));
        }
        flags.push("orders.deals.apply".into());
    }

    // One unit counts toward one deal: the claims against each line.
    let mut claimed: HashMap<usize, i32> = HashMap::new();
    for a in inputs {
        for l in &a.lines {
            if l.line_index < 0 || l.units < 1 {
                if replay {
                    continue;
                }
                return Err(refuse(
                    "DEAL_NOT_ELIGIBLE",
                    json!({"deal_rule_id": a.deal_rule_id, "reason": "count"}),
                ));
            }
            *claimed.entry(l.line_index as usize).or_default() += l.units;
        }
    }
    for (idx, units) in &claimed {
        let have = lines
            .iter()
            .find(|l| l.input_index == *idx)
            .map_or(0, |l| l.quantity);
        let named_plain = lines.iter().any(|l| l.input_index == *idx);
        if named_plain && *units > have && !replay {
            return Err(refuse("DEAL_UNITS_OVERLAP", json!({"line_index": idx})));
        }
    }

    let ids: Vec<Uuid> = inputs.iter().map(|a| a.deal_rule_id).collect();
    let (rules, names) = rules_for(conn, ctx.org_id, ctx.branch_id, ctx.sell, &ids).await?;

    let mut free_charged = deal_lines(lines, false);
    let mut free_expected = deal_lines(lines, true);
    let mut used: Vec<md::UsedDeal> = Vec::new();
    let mut out = Vec::with_capacity(inputs.len());
    for a in inputs {
        let units: Vec<md::LineUnits> = a
            .lines
            .iter()
            .filter(|l| l.line_index >= 0 && l.units >= 1)
            .map(|l| md::LineUnits {
                line_index: l.line_index as usize,
                units: i64::from(l.units),
            })
            .collect();
        let dctx = md::DealContext {
            branch_id: Some(ctx.branch_id.to_string()),
            now: ctx.now.clone(),
            used: used.clone(),
        };
        // The server's verdict: the rule over these units at catalogue prices.
        let verdict: Result<md::Application, &'static str> = match rules.get(&a.deal_rule_id) {
            None => Err("inactive"),
            Some(_) if !ctx.sell.pos => Err("channel"),
            Some(r) => md::price_application(&view_of(r), &free_expected, &units, &dctx)
                .map_err(|e| e.token()),
        };
        let (name, name_translations) = rules
            .get(&a.deal_rule_id)
            .map(|r| (r.name.clone(), r.name_translations.clone()))
            .or_else(|| names.get(&a.deal_rule_id).cloned())
            .unwrap_or_else(|| ("Deal".into(), json!({})));

        let priced = match (&verdict, replay) {
            (Err(reason), false) => {
                return Err(refuse(
                    "DEAL_NOT_ELIGIBLE",
                    json!({"deal_rule_id": a.deal_rule_id, "reason": reason}),
                ));
            }
            (Ok(app), false) => {
                // Live: the catalogue prices the line, so charged = expected.
                let lines_out = app
                    .lines
                    .iter()
                    .map(|l| {
                        (
                            l.line_index,
                            l.units as i32,
                            l.discount as i32,
                            l.discount as i32,
                        )
                    })
                    .collect();
                PricedDeal {
                    rule_id: a.deal_rule_id,
                    name,
                    name_translations,
                    times: app.times as i32,
                    discount: app.discount as i32,
                    discount_server: Some(app.discount as i32),
                    lines: lines_out,
                }
            }
            (verdict, true) => {
                // Replay: the till's discount stands, split over its lines by
                // the server's per-line cuts (else by the units' value).
                let server = verdict.as_ref().ok();
                if server.is_none() {
                    flags.push("orders.deals.apply:not_eligible".into());
                }
                let till = a
                    .discount
                    .unwrap_or_else(|| server.map_or(0, |s| s.discount as i32))
                    .max(0);
                if server.is_some_and(|s| s.discount as i32 != till) {
                    flags.push("orders.deals.apply:mismatch".into());
                }
                let mut per_line: Vec<(usize, i32)> = Vec::new();
                for u in &units {
                    match per_line.iter_mut().find(|p| p.0 == u.line_index) {
                        Some(p) => p.1 += u.units as i32,
                        None => per_line.push((u.line_index, u.units as i32)),
                    }
                }
                per_line.sort_by_key(|p| p.0);
                let server_cut = |idx: usize| -> i32 {
                    server
                        .and_then(|s| s.lines.iter().find(|l| l.line_index == idx))
                        .map_or(0, |l| l.discount as i32)
                };
                let weights: Vec<i64> = if server.is_some_and(|s| s.discount > 0) {
                    per_line
                        .iter()
                        .map(|p| i64::from(server_cut(p.0)))
                        .collect()
                } else {
                    per_line
                        .iter()
                        .map(|p| {
                            let price = lines
                                .iter()
                                .find(|l| l.input_index == p.0)
                                .map_or(0, |l| l.unit_price);
                            i64::from(price) * i64::from(p.1)
                        })
                        .collect()
                };
                let split = madar_money::alloc::split(i64::from(till), &weights);
                PricedDeal {
                    rule_id: a.deal_rule_id,
                    name,
                    name_translations,
                    times: server.map_or(a.times.max(1), |s| s.times as i32),
                    discount: till,
                    discount_server: server.map(|s| s.discount as i32),
                    lines: per_line
                        .iter()
                        .zip(split)
                        .map(|(p, cut)| (p.0, p.1, cut as i32, server_cut(p.0)))
                        .collect(),
                }
            }
        };

        // Consume the units, and count the application toward max_per_order.
        for (idx, units, _, _) in &priced.lines {
            for free in [&mut free_charged, &mut free_expected] {
                if let Some(l) = free.iter_mut().find(|l| l.line_index == *idx) {
                    l.quantity = (l.quantity - i64::from(*units)).max(0);
                }
            }
        }
        match used
            .iter_mut()
            .find(|u| u.deal_id == a.deal_rule_id.to_string())
        {
            Some(u) => u.times += i64::from(priced.times),
            None => used.push(md::UsedDeal {
                deal_id: a.deal_rule_id.to_string(),
                times: i64::from(priced.times),
            }),
        }
        out.push(priced);
    }
    flags.dedup();
    Ok((out, flags))
}

/// QR and online checkout (§11.2): the best deals, applied by the server,
/// again and again until none is left. Empty when the channel is off.
pub async fn auto(
    conn: &mut PgConnection,
    ctx: &ComboCtx,
    channel: madar_catalog::combo::Channel,
    lines: &[PlainLine],
) -> Result<Vec<PricedDeal>, AppError> {
    if lines.is_empty() || !ctx.sell().get(channel) {
        return Ok(Vec::new());
    }
    let all = crate::deals::load::load_rules(&mut *conn, ctx.org_id, None).await?;
    if all.is_empty() {
        return Ok(Vec::new());
    }
    let rules: Vec<DealRule> = all
        .into_iter()
        .map(|r| crate::deals::load::for_branch(r, ctx.branch_id, ctx.sell))
        .collect();
    let views: Vec<md::DealView> = rules.iter().map(view_of).collect();
    let dctx = md::DealContext {
        branch_id: Some(ctx.branch_id.to_string()),
        now: ctx.now.clone(),
        used: Vec::new(),
    };
    let apps = md::auto_apply(&views, &deal_lines(lines, false), &dctx);
    Ok(apps
        .into_iter()
        .filter_map(|app| {
            let r = rules.iter().find(|r| r.id.to_string() == app.deal_id)?;
            Some(PricedDeal {
                rule_id: r.id,
                name: r.name.clone(),
                name_translations: r.name_translations.clone(),
                times: app.times as i32,
                discount: app.discount as i32,
                discount_server: Some(app.discount as i32),
                lines: app
                    .lines
                    .iter()
                    .map(|l| {
                        (
                            l.line_index,
                            l.units as i32,
                            l.discount as i32,
                            l.discount as i32,
                        )
                    })
                    .collect(),
            })
        })
        .collect())
}

/// Write an order's deals, their lines naming the stored rows
/// (`row_of[input index]`). Returns them as `OrderFull.deals` reads them.
pub async fn insert(
    conn: &mut PgConnection,
    org_id: Uuid,
    order_id: Uuid,
    deals: &[PricedDeal],
    row_of: &HashMap<usize, Uuid>,
) -> Result<Vec<OrderDeal>, AppError> {
    let mut out = Vec::with_capacity(deals.len());
    for d in deals {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO order_deals (org_id, order_id, deal_rule_id, deal_name, name_translations, \
                                      times, discount, discount_server) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
        )
        .bind(org_id)
        .bind(order_id)
        .bind(d.rule_id)
        .bind(&d.name)
        .bind(&d.name_translations)
        .bind(d.times as i16)
        .bind(d.discount)
        .bind(d.discount_server)
        .fetch_one(&mut *conn)
        .await?;
        let mut lines = Vec::with_capacity(d.lines.len());
        for (idx, units, discount, _) in &d.lines {
            let Some(item) = row_of.get(idx) else {
                continue;
            };
            sqlx::query(
                "INSERT INTO order_deal_lines (order_deal_id, order_item_id, org_id, units, discount) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(id)
            .bind(item)
            .bind(org_id)
            .bind(*units as i16)
            .bind(*discount)
            .execute(&mut *conn)
            .await?;
            lines.push(OrderDealLine {
                order_item_id: *item,
                units: *units,
                discount: *discount,
            });
        }
        out.push(OrderDeal {
            id,
            deal_rule_id: d.rule_id,
            name: d.name.clone(),
            name_translations: d.name_translations.clone(),
            times: d.times,
            discount: d.discount,
            discount_server: d.discount_server,
            lines,
        });
    }
    Ok(out)
}

/// The deals of each order, for `OrderFull.deals` (reads and the pull feed).
pub async fn of_orders(
    conn: &mut PgConnection,
    order_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<OrderDeal>>, AppError> {
    let mut by_order: HashMap<Uuid, Vec<OrderDeal>> = HashMap::new();
    if order_ids.is_empty() {
        return Ok(by_order);
    }
    #[allow(clippy::type_complexity)]
    let rows: Vec<(Uuid, Uuid, Uuid, String, serde_json::Value, i16, i32, Option<i32>)> =
        sqlx::query_as(
            "SELECT id, order_id, deal_rule_id, deal_name, name_translations, times, discount, discount_server \
               FROM order_deals WHERE order_id = ANY($1) ORDER BY order_id, created_at, id",
        )
        .bind(order_ids)
        .fetch_all(&mut *conn)
        .await?;
    if rows.is_empty() {
        return Ok(by_order);
    }
    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let line_rows: Vec<(Uuid, Uuid, i16, i32)> = sqlx::query_as(
        "SELECT order_deal_id, order_item_id, units, discount FROM order_deal_lines \
          WHERE order_deal_id = ANY($1) ORDER BY order_deal_id, order_item_id",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut lines: HashMap<Uuid, Vec<OrderDealLine>> = HashMap::new();
    for (d, item, units, discount) in line_rows {
        lines.entry(d).or_default().push(OrderDealLine {
            order_item_id: item,
            units: i32::from(units),
            discount,
        });
    }
    for (id, order_id, deal_rule_id, name, tr, times, discount, discount_server) in rows {
        by_order.entry(order_id).or_default().push(OrderDeal {
            id,
            deal_rule_id,
            name,
            name_translations: tr,
            times: i32::from(times),
            discount,
            discount_server,
            lines: lines.remove(&id).unwrap_or_default(),
        });
    }
    Ok(by_order)
}
