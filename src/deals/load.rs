//! Loading deal rules (and the channel toggles that gate them) from SQL: one
//! loader for the dashboard CRUD, the `deal_rule` feed rows, the public menus
//! and the order path.

use std::collections::HashMap;

use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    combos::types::{ChannelOverride, ChannelToggles, SaleWindow},
    deals::types::{DealBranchOverride, DealPoolEntry, DealRule},
    errors::AppError,
};

type RuleRow = (
    Uuid,
    String,
    Value,
    String,
    i16,
    Option<i32>,
    Option<i16>,
    Option<i16>,
    Option<i16>,
    i32,
    bool,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
);

/// The org's deal rules (not soft-deleted), or only `ids` when given, in
/// `sort, name, id` order, with their pools, windows and branch overrides.
pub async fn load_rules(
    conn: &mut PgConnection,
    org_id: Uuid,
    ids: Option<&[Uuid]>,
) -> Result<Vec<DealRule>, AppError> {
    let rows: Vec<RuleRow> = sqlx::query_as(
        "SELECT id, name, name_translations, kind, qty, price, get_qty, get_percent, max_per_order, \
                sort, is_active, created_at, updated_at \
           FROM deal_rules \
          WHERE org_id = $1 AND deleted_at IS NULL AND ($2::uuid[] IS NULL OR id = ANY($2)) \
          ORDER BY sort, name, id",
    )
    .bind(org_id)
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let rule_ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();

    #[allow(clippy::type_complexity)]
    let items: Vec<(Uuid, String, Option<Uuid>, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT deal_rule_id, role, menu_item_id, category_id, size_label FROM deal_rule_items \
          WHERE deal_rule_id = ANY($1) ORDER BY deal_rule_id, role, sort, id",
    )
    .bind(&rule_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut pools: HashMap<Uuid, (Vec<DealPoolEntry>, Vec<DealPoolEntry>)> = HashMap::new();
    for (rule, role, menu_item_id, category_id, size_label) in items {
        let e = DealPoolEntry {
            menu_item_id,
            category_id,
            size_label,
        };
        let slot = pools.entry(rule).or_default();
        if role == "reward" {
            slot.1.push(e);
        } else {
            slot.0.push(e);
        }
    }

    let mut windows = windows_of(&mut *conn, "deal_rule_id", &rule_ids).await?;

    let overrides: Vec<(Uuid, Uuid, bool)> = sqlx::query_as(
        "SELECT deal_rule_id, branch_id, is_active FROM deal_rule_branch_overrides \
          WHERE deal_rule_id = ANY($1) ORDER BY deal_rule_id, branch_id",
    )
    .bind(&rule_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut by_rule: HashMap<Uuid, Vec<DealBranchOverride>> = HashMap::new();
    for (rule, branch_id, is_active) in overrides {
        by_rule.entry(rule).or_default().push(DealBranchOverride {
            branch_id,
            is_active,
        });
    }

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                name,
                name_translations,
                kind,
                qty,
                price,
                get_qty,
                get_percent,
                max_per_order,
                sort,
                is_active,
                created_at,
                updated_at,
            )| {
                let (pool, reward_pool) = pools.remove(&id).unwrap_or_default();
                DealRule {
                    id,
                    name,
                    name_translations,
                    kind,
                    qty,
                    price,
                    get_qty,
                    get_percent,
                    max_per_order,
                    sort,
                    is_active,
                    pool,
                    reward_pool,
                    windows: windows.remove(&id).unwrap_or_default(),
                    branch_overrides: by_rule.remove(&id).unwrap_or_default(),
                    sell: None,
                    created_at,
                    updated_at,
                }
            },
        )
        .collect())
}

/// The windows of every owner in `owners`, keyed by owner, in `sort, id` order.
/// `column` is `combo_item_id` or `deal_rule_id`.
pub async fn windows_of(
    conn: &mut PgConnection,
    column: &'static str,
    owners: &[Uuid],
) -> Result<HashMap<Uuid, Vec<SaleWindow>>, AppError> {
    debug_assert!(matches!(column, "combo_item_id" | "deal_rule_id"));
    let sql = format!(
        "SELECT {column}, id, branch_id, weekdays, to_char(starts_at, 'HH24:MI'), to_char(ends_at, 'HH24:MI'), \
                valid_from, valid_to \
           FROM sale_windows WHERE {column} = ANY($1) ORDER BY {column}, sort, id"
    );
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        Uuid,
        Uuid,
        Option<Uuid>,
        i16,
        Option<String>,
        Option<String>,
        Option<chrono::NaiveDate>,
        Option<chrono::NaiveDate>,
    )> = sqlx::query_as(&sql)
        .bind(owners)
        .fetch_all(&mut *conn)
        .await?;
    let mut out: HashMap<Uuid, Vec<SaleWindow>> = HashMap::new();
    for (owner, id, branch_id, weekdays, starts_at, ends_at, valid_from, valid_to) in rows {
        out.entry(owner).or_default().push(SaleWindow {
            id: Some(id),
            branch_id,
            weekdays,
            starts_at,
            ends_at,
            valid_from,
            valid_to,
        });
    }
    Ok(out)
}

/// The org's channel toggles (§11.1), and the branch's override when given.
pub async fn channel_settings(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<(ChannelToggles, Option<ChannelOverride>), AppError> {
    let org: Option<(bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT sell_pos, sell_qr, sell_online, sell_delivery FROM combo_channel_settings WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_optional(&mut *conn)
    .await?;
    let org = org
        .map(|(pos, qr, online, delivery)| ChannelToggles {
            pos,
            qr,
            online,
            delivery,
        })
        .unwrap_or_default();
    let Some(branch_id) = branch_id else {
        return Ok((org, None));
    };
    #[allow(clippy::type_complexity)]
    let over: Option<(Option<bool>, Option<bool>, Option<bool>, Option<bool>)> = sqlx::query_as(
        "SELECT sell_pos, sell_qr, sell_online, sell_delivery FROM combo_channel_branch_overrides \
          WHERE branch_id = $1 AND org_id = $2",
    )
    .bind(branch_id)
    .bind(org_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok((
        org,
        over.map(|(pos, qr, online, delivery)| ChannelOverride {
            pos,
            qr,
            online,
            delivery,
        }),
    ))
}

/// The org's toggles with the branch's override applied.
pub fn resolve(org: ChannelToggles, over: Option<ChannelOverride>) -> ChannelToggles {
    let Some(o) = over else { return org };
    ChannelToggles {
        pos: o.pos.unwrap_or(org.pos),
        qr: o.qr.unwrap_or(org.qr),
        online: o.online.unwrap_or(org.online),
        delivery: o.delivery.unwrap_or(org.delivery),
    }
}

/// The channel toggles in effect at a branch.
pub async fn channels_at(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<ChannelToggles, AppError> {
    let (org, over) = channel_settings(conn, org_id, branch_id).await?;
    Ok(resolve(org, over))
}

/// A rule as one branch sees it: `is_active` after the branch's override,
/// only its windows for that branch or every branch, no override list.
pub fn for_branch(mut rule: DealRule, branch_id: Uuid, sell: ChannelToggles) -> DealRule {
    if let Some(o) = rule
        .branch_overrides
        .iter()
        .find(|o| o.branch_id == branch_id)
    {
        rule.is_active = rule.is_active && o.is_active;
    }
    rule.branch_overrides.clear();
    rule.windows
        .retain(|w| w.branch_id.is_none() || w.branch_id == Some(branch_id));
    rule.sell = Some(sell);
    rule
}

/// The `deal_rule` feed rows for a device of `branch_id`.
pub async fn feed_rows(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch_id: Uuid,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Value>, AppError> {
    let sell = channels_at(&mut *conn, org_id, Some(branch_id)).await?;
    let rules = load_rules(&mut *conn, org_id, Some(ids)).await?;
    let mut out = HashMap::with_capacity(rules.len());
    for rule in rules {
        let id = rule.id;
        let row = serde_json::to_value(for_branch(rule, branch_id, sell))
            .map_err(|_| AppError::Internal)?;
        out.insert(id, row);
    }
    Ok(out)
}
