//! A combo's economics (C11, COMBOS_CONTRACT.md §2.2): its list value, cost,
//! margin and saving at a branch (or the org's catalogue), and the warnings
//! the editor shows. Warnings never block a save.
//!
//! - Each slot's candidates: its item choices (live, `kind = 'item'`) and every
//!   live item of its category choices, at the size the combo includes
//!   (`madar_catalog::combo::included_size`), priced for the branch.
//! - Default case: the slot's default item (else its first candidate) × `min`.
//! - `list_min` / `list_max`: the cheapest × `min` / the dearest × `max`.
//! - Worst cost: each slot's highest-cost candidate × `max`.
//! - Cost is the recipe rollup (`costing::service::sku_costs_for_items`) at
//!   the included size; an unknown or partial cost makes the figure `null`.

use std::collections::{HashMap, HashSet};

use rust_decimal::Decimal;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    combos::{
        load::{ComboDef, category_members, slot_view},
        types::{ChannelToggles, ComboEconomics, ComboSlot, ComboWarning},
    },
    errors::AppError,
    orders::catalog_view::Catalog,
};

/// What a combo's economics found: the panel, and which items can be sold
/// now at the branch (for `available_now`).
pub struct Analysis {
    pub economics: ComboEconomics,
    /// Live, active items not switched off at the branch.
    pub available: HashSet<Uuid>,
    /// Category → its live items.
    pub members: HashMap<Uuid, Vec<Uuid>>,
}

impl Analysis {
    /// A choice has something to sell now.
    pub fn choice_available(&self, c: &madar_catalog::combo::ChoiceView) -> bool {
        let parse = |s: &Option<String>| s.as_deref().and_then(|v| v.parse::<Uuid>().ok());
        if let Some(id) = parse(&c.menu_item_id) {
            return self.available.contains(&id);
        }
        parse(&c.category_id)
            .and_then(|cat| self.members.get(&cat))
            .is_some_and(|ms| ms.iter().any(|m| self.available.contains(m)))
    }
}

/// A fraction with exactly 4 decimals ("0.8333"), rounded half away from 0.
pub fn fraction(d: Decimal) -> String {
    format!(
        "{:.4}",
        d.round_dp_with_strategy(4, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
    )
}

fn margin(price: i64, cost: Option<i64>) -> Option<Decimal> {
    let cost = cost?;
    if price <= 0 {
        return None;
    }
    Some(Decimal::from(price - cost) / Decimal::from(price))
}

/// The org's minimum margin (C11), `None` = no warning.
pub async fn min_margin(
    conn: &mut PgConnection,
    org_id: Uuid,
) -> Result<Option<Decimal>, AppError> {
    Ok(sqlx::query_scalar::<_, Option<Decimal>>(
        "SELECT combo_min_margin FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(&mut *conn)
    .await?
    .flatten())
}

struct Candidate {
    item: Uuid,
    price: i64,
    cost: Option<i64>,
}

/// The economics of `slots` sold at P = `price`, at `branch` (or the org).
pub async fn analyse(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch: Option<Uuid>,
    price: i64,
    slots: &[ComboSlot],
) -> Result<Analysis, AppError> {
    let min = min_margin(&mut *conn, org_id).await?;

    let categories: Vec<Uuid> = slots
        .iter()
        .flat_map(|s| s.choices.iter().filter_map(|c| c.category_id))
        .collect();
    let members = category_members(&mut *conn, org_id, &categories).await?;
    let item_choices: Vec<Uuid> = slots
        .iter()
        .flat_map(|s| s.choices.iter().filter_map(|c| c.menu_item_id))
        .collect();
    // The item choices that are live, active items.
    let live: HashSet<Uuid> = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM menu_items WHERE org_id = $1 AND id = ANY($2) \
            AND kind = 'item' AND deleted_at IS NULL AND is_active",
    )
    .bind(org_id)
    .bind(&item_choices)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();

    let mut all: Vec<Uuid> = live.iter().copied().collect();
    all.extend(members.values().flatten().copied());
    all.sort();
    all.dedup();

    let mut catalog = Catalog::new(branch);
    catalog.ensure(&mut *conn, &all, &[]).await?;
    let available: HashSet<Uuid> = all
        .iter()
        .copied()
        .filter(|i| {
            catalog
                .item(*i)
                .and_then(|l| l.row.as_ref())
                .is_some_and(|r| !r.branch_disabled)
        })
        .collect();

    let costs: HashMap<(Uuid, String), Option<i64>> =
        crate::costing::service::sku_costs_for_items(&mut *conn, org_id, &all, branch)
            .await?
            .into_iter()
            .map(|c| {
                let v = if c.cost_missing { None } else { c.cost };
                ((c.menu_item_id, c.size_label), v)
            })
            .collect();

    let mut warnings: Vec<ComboWarning> = Vec::new();
    let mut warned: HashSet<(String, Uuid)> = HashSet::new();
    let mut warn =
        |warnings: &mut Vec<ComboWarning>, code: &str, key: Uuid, vars: serde_json::Value| {
            if warned.insert((code.to_string(), key)) {
                warnings.push(ComboWarning {
                    code: code.into(),
                    vars,
                });
            }
        };

    let (mut list_default, mut list_min, mut list_max) = (0i64, 0i64, 0i64);
    let (mut cost_default, mut cost_max) = (Some(0i64), Some(0i64));
    for slot in slots {
        for c in &slot.choices {
            if let Some(i) = c.menu_item_id
                && !live.contains(&i)
            {
                warn(
                    &mut warnings,
                    "CHOICE_INACTIVE",
                    i,
                    serde_json::json!({ "menu_item_id": i }),
                );
            }
        }
        let sv = slot_view(slot);
        let mut cands: Vec<Candidate> = Vec::new();
        for (c, cv) in slot.choices.iter().zip(&sv.choices) {
            let items: Vec<Uuid> = match (c.menu_item_id, c.category_id) {
                (Some(i), _) => vec![i],
                (None, Some(cat)) => members.get(&cat).cloned().unwrap_or_default(),
                _ => vec![],
            };
            for item in items {
                if !available.contains(&item) || cands.iter().any(|k| k.item == item) {
                    continue;
                }
                let view = catalog.view(item);
                let Ok(inc) = madar_catalog::combo::included_size(&view, cv) else {
                    continue;
                };
                let Ok(p) = madar_catalog::unit_price(&view.item, Some(&inc)) else {
                    continue;
                };
                let cost = costs.get(&(item, inc.clone())).copied().flatten();
                if cost.is_none() {
                    warn(
                        &mut warnings,
                        "COST_UNKNOWN",
                        item,
                        serde_json::json!({ "menu_item_id": item }),
                    );
                }
                cands.push(Candidate {
                    item,
                    price: p,
                    cost,
                });
            }
        }
        if cands.is_empty() {
            warn(
                &mut warnings,
                "SLOT_EMPTY_NOW",
                slot.id,
                serde_json::json!({ "slot_id": slot.id }),
            );
            continue;
        }
        let (lo, hi) = (i64::from(slot.min), i64::from(slot.max));
        let def = slot
            .default_item_id
            .and_then(|d| cands.iter().find(|c| c.item == d))
            .unwrap_or(&cands[0]);
        list_default += lo * def.price;
        list_min += lo * cands.iter().map(|c| c.price).min().unwrap_or(0);
        list_max += hi * cands.iter().map(|c| c.price).max().unwrap_or(0);
        cost_default = match (cost_default, def.cost) {
            (Some(a), Some(b)) => Some(a + lo * b),
            _ if lo == 0 => cost_default,
            _ => None,
        };
        let worst = if cands.iter().all(|c| c.cost.is_some()) {
            cands.iter().filter_map(|c| c.cost).max()
        } else {
            None
        };
        cost_max = match (cost_max, worst) {
            (Some(a), Some(b)) => Some(a + hi * b),
            _ => None,
        };
    }

    let margin_default = margin(price, cost_default);
    let margin_worst = margin(price, cost_max);
    if let (Some(min), Some(m)) = (min, margin_worst.or(margin_default))
        && m < min
    {
        warnings.insert(
            0,
            ComboWarning {
                code: "MARGIN_BELOW_MIN".into(),
                vars: serde_json::json!({ "margin": fraction(m), "min": fraction(min) }),
            },
        );
    }
    if price >= list_default {
        warnings.push(ComboWarning {
            code: "NO_SAVING".into(),
            vars: serde_json::json!({}),
        });
    }

    Ok(Analysis {
        economics: ComboEconomics {
            branch_id: branch,
            price,
            list_default,
            list_min,
            list_max,
            cost_default,
            cost_max,
            margin_default: margin_default.map(fraction),
            margin_worst: margin_worst.map(fraction),
            min_margin: min.map(fraction),
            saving_default: list_default - price,
            warnings,
        },
        available,
        members,
    })
}

/// P at `branch`: the combo's `one_size` price as that branch sells it.
pub async fn branch_price(
    conn: &mut PgConnection,
    def: &ComboDef,
    branch: Option<Uuid>,
) -> Result<(i64, bool), AppError> {
    let mut catalog = Catalog::new(branch);
    catalog.ensure(&mut *conn, &[def.id], &[]).await?;
    let view = catalog.view(def.id);
    let p = madar_catalog::unit_price(&view.item, Some("one_size")).unwrap_or(i64::from(def.price));
    let disabled = catalog
        .item(def.id)
        .and_then(|l| l.row.as_ref())
        .is_some_and(|r| r.branch_disabled);
    Ok((p, !disabled))
}

pub fn sell_of(t: ChannelToggles) -> madar_catalog::combo::Sell {
    madar_catalog::combo::Sell {
        pos: t.pos,
        qr: t.qr,
        online: t.online,
        delivery: t.delivery,
    }
}

/// On sale on the till now at `branch` (or org-level): active, the POS
/// channel on, a window open, every required slot with a choice to sell.
pub async fn available_now(
    conn: &mut PgConnection,
    def: &ComboDef,
    branch: Option<Uuid>,
    p: i64,
    branch_enabled: bool,
    analysis: &Analysis,
) -> Result<bool, AppError> {
    let sell = sell_of(crate::deals::load::channels_at(&mut *conn, def.org_id, branch).await?);
    let now = crate::combos::load::local_now(&mut *conn, def.org_id, branch).await?;
    let b = branch.map(|b| b.to_string());
    let at = madar_catalog::combo::Availability {
        channel: madar_catalog::combo::Channel::Pos,
        sell: &sell,
        branch_enabled,
        branch_id: b.as_deref(),
        now: &now,
    };
    Ok(
        madar_catalog::combo::available(&def.view(p, branch), &at, |c| {
            analysis.choice_available(c)
        })
        .is_ok(),
    )
}
