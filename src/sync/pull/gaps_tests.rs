//! The `/sync/pull` gaps offline plan B closes (TILLS_VERIFICATION "Before/with
//! B"): addon items, effective permissions, delivery prep minutes, order lines
//! for reprint, per-channel menu prices — and the payment-availability event on
//! its own realtime topic.
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::{PullRequest, pull_core};

struct Shop {
    org: Uuid,
    branch: Uuid,
    teller: Uuid,
}

async fn shop(pool: &PgPool) -> Shop {
    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug) VALUES ('Gaps Org', $1) RETURNING id",
    )
    .bind(format!("gaps-{}", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    let branch: Uuid = sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, code) VALUES ($1, 'Gaps', 'GAPS') RETURNING id",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    let teller: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) VALUES ($1, 'Sara', $2, 'x', 'teller') RETURNING id",
    )
    .bind(org)
    .bind(format!("{}@gaps.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    Shop {
        org,
        branch,
        teller,
    }
}

fn req(branch: Uuid) -> PullRequest {
    PullRequest {
        branch_id: branch,
        device_id: None,
        types: None,
        limit: None,
        ledger_page_size: None,
        snapshot_cursor: None,
    }
}

fn row<'a>(resp: &'a super::PullResponse, ty: &str, id: Uuid) -> Option<&'a Value> {
    resp.data
        .get(ty)?
        .iter()
        .find(|r| r["id"] == id.to_string())
}

fn change<'a>(resp: &'a super::PullResponse, ty: &str, id: Uuid) -> Option<&'a super::PullChange> {
    resp.changes.iter().find(|c| c.ty == ty && c.id == id)
}

/// Every key at any depth of `v`.
fn keys(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                out.push(k.clone());
                keys(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| keys(x, out)),
        _ => {}
    }
}

#[sqlx::test]
async fn an_addon_item_rides_the_feed_branch_effective(pool: PgPool) {
    let s = shop(&pool).await;
    let addon: Uuid = sqlx::query_scalar(
        "INSERT INTO addon_items (org_id, name, type, default_price) VALUES ($1, 'Oat milk', 'milk', 1500) RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO addon_item_ingredients (addon_item_id, quantity_used, ingredient_name, ingredient_unit) VALUES ($1, 0.25, 'Oat', 'l')")
        .bind(addon)
        .execute(&pool)
        .await
        .unwrap();

    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    let a = row(&full, "addon_item", addon).expect("the addon is in the snapshot");
    assert_eq!(a["default_price"], 1500);
    assert_eq!(a["is_available"], true);
    assert_eq!(a["ingredients"][0]["ingredient_name"], "Oat");
    assert!(a.get("org_id").is_none());
    assert!(
        full.checksums.contains_key("addon_item"),
        "a state type is checksummed"
    );

    // A branch override: new price, then switched off — each an incremental change.
    sqlx::query("INSERT INTO branch_addon_overrides (branch_id, addon_item_id, price_override) VALUES ($1, $2, 1800)")
        .bind(s.branch)
        .bind(addon)
        .execute(&pool)
        .await
        .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next)
        .await
        .unwrap();
    let c = change(&inc, "addon_item", addon).expect("override re-emits the addon");
    assert_eq!(c.data.as_ref().unwrap()["default_price"], 1800);
    sqlx::query("UPDATE branch_addon_overrides SET is_available = false WHERE branch_id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next)
        .await
        .unwrap();
    assert_eq!(
        change(&inc2, "addon_item", addon)
            .unwrap()
            .data
            .as_ref()
            .unwrap()["is_available"],
        false
    );

    // An ingredient edit re-emits; a delete is a delete.
    sqlx::query("UPDATE addon_item_ingredients SET quantity_used = 0.3 WHERE addon_item_id = $1")
        .bind(addon)
        .execute(&pool)
        .await
        .unwrap();
    let inc3 = pull_core(&pool, s.org, &req(s.branch), inc2.next)
        .await
        .unwrap();
    assert!(change(&inc3, "addon_item", addon).is_some());
    sqlx::query("DELETE FROM addon_items WHERE id = $1")
        .bind(addon)
        .execute(&pool)
        .await
        .unwrap();
    let inc4 = pull_core(&pool, s.org, &req(s.branch), inc3.next)
        .await
        .unwrap();
    assert_eq!(change(&inc4, "addon_item", addon).unwrap().op, "delete");
}

/// After the menu-unification contract shim the legacy addon relations are VIEWS
/// (no row triggers): the unified tables' emitters carry `addon_item` instead.
#[sqlx::test]
async fn an_addon_item_rides_the_feed_after_the_contract_shim(pool: PgPool) {
    let s = shop(&pool).await;
    sqlx::raw_sql(include_str!("../../../deploy/menu_unification_shim.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let kind: String =
        sqlx::query_scalar("SELECT relkind::text FROM pg_class WHERE relname = 'addon_items'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kind, "v", "the shim turned the legacy table into a view");

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_groups (org_id, name, legacy_addon_type) VALUES ($1, 'Milk', 'milk_type') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    let opt: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_options (group_id, name, price, legacy_source) VALUES ($1, 'Oat milk', 1500, 'addon') RETURNING id",
    )
    .bind(group)
    .fetch_one(&pool)
    .await
    .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next)
        .await
        .unwrap();
    let c = change(&inc, "addon_item", opt).expect("a new addon option emits");
    assert_eq!(c.op, "upsert");
    assert_eq!(c.data.as_ref().unwrap()["default_price"], 1500);
    assert_eq!(c.data.as_ref().unwrap()["addon_type"], "milk_type");

    // Its recipe line (the ingredient it uses), under a rename of that ingredient.
    let ing: Uuid = sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit) VALUES ($1, 'Oat', 'l') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) VALUES ('modifier_option', $1, $2, 0.25, 'l')")
        .bind(opt)
        .bind(ing)
        .execute(&pool)
        .await
        .unwrap();
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next)
        .await
        .unwrap();
    assert_eq!(
        change(&inc2, "addon_item", opt)
            .unwrap()
            .data
            .as_ref()
            .unwrap()["ingredients"][0]["ingredient_name"],
        "Oat"
    );
    sqlx::query("UPDATE org_ingredients SET name = 'Oat drink' WHERE id = $1")
        .bind(ing)
        .execute(&pool)
        .await
        .unwrap();
    let inc3 = pull_core(&pool, s.org, &req(s.branch), inc2.next)
        .await
        .unwrap();
    assert_eq!(
        change(&inc3, "addon_item", opt)
            .expect("an ingredient rename re-emits")
            .data
            .as_ref()
            .unwrap()["ingredients"][0]["ingredient_name"],
        "Oat drink"
    );

    // A branch override switches it off at this branch.
    sqlx::query("INSERT INTO menu_price_overrides (scope, branch_id, target_type, target_id, price, is_available) VALUES ('branch', $1, 'modifier_option', $2, 1800, false)")
        .bind(s.branch)
        .bind(opt)
        .execute(&pool)
        .await
        .unwrap();
    let inc4 = pull_core(&pool, s.org, &req(s.branch), inc3.next)
        .await
        .unwrap();
    let d = change(&inc4, "addon_item", opt)
        .expect("override re-emits")
        .data
        .clone()
        .unwrap();
    assert_eq!(
        (d["default_price"].clone(), d["is_available"].clone()),
        (1800.into(), false.into())
    );

    // Deleting the option retires it; deleting a whole group retires its options.
    sqlx::query("DELETE FROM modifier_options WHERE id = $1")
        .bind(opt)
        .execute(&pool)
        .await
        .unwrap();
    let inc5 = pull_core(&pool, s.org, &req(s.branch), inc4.next)
        .await
        .unwrap();
    assert_eq!(change(&inc5, "addon_item", opt).unwrap().op, "delete");
    let opt2: Uuid = sqlx::query_scalar("INSERT INTO modifier_options (group_id, name, legacy_source) VALUES ($1, 'Soy', 'addon') RETURNING id")
        .bind(group)
        .fetch_one(&pool)
        .await
        .unwrap();
    let inc6 = pull_core(&pool, s.org, &req(s.branch), inc5.next)
        .await
        .unwrap();
    assert_eq!(change(&inc6, "addon_item", opt2).unwrap().op, "upsert");
    sqlx::query("DELETE FROM modifier_groups WHERE id = $1")
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    let inc7 = pull_core(&pool, s.org, &req(s.branch), inc6.next)
        .await
        .unwrap();
    assert_eq!(
        change(&inc7, "addon_item", opt2)
            .expect("the group delete retires its options")
            .op,
        "delete"
    );

    // And a full snapshot agrees with the incremental feed.
    let again = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    assert!(row(&again, "addon_item", opt2).is_none());
}

#[sqlx::test]
async fn a_tellers_effective_permissions_ride_the_feed(pool: PgPool) {
    let s = shop(&pool).await;
    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    let t = row(&full, "teller", s.teller).expect("teller projected");
    let perms: Vec<String> = t["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect();
    let role_granted: Vec<String> = sqlx::query_scalar(
        "SELECT resource::text || ':' || action::text FROM role_permissions WHERE role = 'teller' AND granted ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!role_granted.is_empty());
    assert_eq!(
        perms, role_granted,
        "no override: exactly the role's grants, granted only"
    );

    // Revoke one for this person: it leaves their list through an incremental change.
    let revoked = role_granted[0].clone();
    let (res, act) = revoked.split_once(':').unwrap();
    sqlx::query("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, $2::permission_resource, $3::permission_action, false)")
        .bind(s.teller)
        .bind(res)
        .bind(act)
        .execute(&pool)
        .await
        .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next)
        .await
        .unwrap();
    let data = change(&inc, "teller", s.teller)
        .expect("override re-emits the teller")
        .data
        .clone()
        .unwrap();
    assert!(
        !data["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == revoked.as_str())
    );

    // A role default changing re-emits every user of that role.
    let (r2, a2) = role_granted[1].split_once(':').unwrap();
    sqlx::query("UPDATE role_permissions SET granted = false WHERE role = 'teller' AND resource = $1::permission_resource AND action = $2::permission_action")
        .bind(r2)
        .bind(a2)
        .execute(&pool)
        .await
        .unwrap();
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next)
        .await
        .unwrap();
    let data2 = change(&inc2, "teller", s.teller)
        .expect("role default re-emits")
        .data
        .clone()
        .unwrap();
    assert!(
        !data2["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == role_granted[1].as_str())
    );
}

#[sqlx::test]
async fn branch_settings_carry_the_delivery_prep_minutes(pool: PgPool) {
    let s = shop(&pool).await;
    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    assert_eq!(
        row(&full, "branch_settings", s.branch).unwrap()["delivery_prep_minutes"],
        20,
        "the table default"
    );
    sqlx::query(
        "INSERT INTO branch_delivery_settings (branch_id, prep_time_minutes) VALUES ($1, 35)",
    )
    .bind(s.branch)
    .execute(&pool)
    .await
    .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next)
        .await
        .unwrap();
    assert_eq!(
        change(&inc, "branch_settings", s.branch)
            .unwrap()
            .data
            .as_ref()
            .unwrap()["delivery_prep_minutes"],
        35
    );
}

/// The branch reads a POS used to poll (stations, routing mode, delivery
/// settings, the loyalty programme) ride the settings row, and each source
/// table re-emits it.
#[sqlx::test]
async fn branch_settings_carry_the_branch_reads_a_till_used_to_poll(pool: PgPool) {
    let s = shop(&pool).await;
    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    let row0 = row(&full, "branch_settings", s.branch).unwrap().clone();
    assert_eq!(
        row0["kitchen_routing_effective"], "till",
        "auto with no station"
    );
    assert_eq!(row0["kitchen_stations"], serde_json::json!([]));
    assert!(row0["delivery"].is_null(), "no delivery row: the defaults");
    assert!(row0["loyalty"].is_null(), "no programme");
    assert!(
        row0["tax_policy"]["tax_rate"].is_number(),
        "the effective tax policy"
    );

    // An org's tax rate change reaches every branch that inherits it.
    sqlx::query("UPDATE branches SET tax_rate = NULL WHERE id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let base = pull_core(&pool, s.org, &req(s.branch), full.next)
        .await
        .unwrap();
    sqlx::query("UPDATE organizations SET tax_rate = 0.05 WHERE id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let taxed = pull_core(&pool, s.org, &req(s.branch), base.next)
        .await
        .unwrap();
    let data = change(&taxed, "branch_settings", s.branch)
        .expect("an org tax change re-emits")
        .data
        .clone()
        .unwrap();
    assert_eq!(data["tax_policy"]["tax_rate"].as_f64(), Some(0.05));
    sqlx::query("UPDATE organizations SET name = name || ' ' WHERE id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let quiet = pull_core(&pool, s.org, &req(s.branch), taxed.next)
        .await
        .unwrap();
    assert!(
        change(&quiet, "branch_settings", s.branch).is_none(),
        "an unrelated org edit does not"
    );
    let full = taxed;

    let station: Uuid = sqlx::query_scalar(
        "INSERT INTO kitchen_stations (org_id, branch_id, name) VALUES ($1, $2, 'Grill') RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next)
        .await
        .unwrap();
    let data = change(&inc, "branch_settings", s.branch)
        .expect("a station re-emits")
        .data
        .clone()
        .unwrap();
    assert_eq!(
        data["kitchen_routing_effective"], "kds",
        "auto with a station"
    );
    assert_eq!(data["kitchen_stations"][0]["id"], station.to_string());
    assert_eq!(data["kitchen_stations"][0]["name"], "Grill");

    sqlx::query("INSERT INTO branch_delivery_settings (branch_id, in_mall_enabled, in_mall_override) VALUES ($1, true, 'closed')")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next)
        .await
        .unwrap();
    let data = change(&inc2, "branch_settings", s.branch)
        .expect("delivery re-emits")
        .data
        .clone()
        .unwrap();
    assert_eq!(data["delivery"]["in_mall_enabled"], true);
    assert_eq!(data["delivery"]["in_mall_override"], "closed");

    // The org programme, then the branch's own override wins.
    sqlx::query("INSERT INTO loyalty_settings (org_id, enabled, program_name) VALUES ($1, true, 'Org club')")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let inc3 = pull_core(&pool, s.org, &req(s.branch), inc2.next)
        .await
        .unwrap();
    let data = change(&inc3, "branch_settings", s.branch)
        .expect("an org programme re-emits")
        .data
        .clone()
        .unwrap();
    assert_eq!(data["loyalty"]["program_name"], "Org club");
    sqlx::query("INSERT INTO loyalty_settings (org_id, branch_id, enabled, mode, program_name) VALUES ($1, $2, false, 'visits', 'Branch club')")
        .bind(s.org)
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let inc4 = pull_core(&pool, s.org, &req(s.branch), inc3.next)
        .await
        .unwrap();
    let data = change(&inc4, "branch_settings", s.branch)
        .expect("a branch programme re-emits")
        .data
        .clone()
        .unwrap();
    assert_eq!(data["loyalty"]["program_name"], "Branch club");
    assert_eq!(data["loyalty"]["enabled"], false);
    assert_eq!(data["loyalty"]["mode"], "visits");
}

#[sqlx::test]
async fn an_order_carries_its_lines_and_drawer_fields_but_no_cost(pool: PgPool) {
    let s = shop(&pool).await;
    let till: Uuid = sqlx::query_scalar("INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1, $2, 'open', 0) RETURNING id")
        .bind(s.branch)
        .bind(s.teller)
        .fetch_one(&pool)
        .await
        .unwrap();
    let key = Uuid::new_v4();
    let order: Uuid = sqlx::query_scalar(
        "INSERT INTO orders (branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount,
                             tip_amount, tip_payment_method, tip_is_cash, idempotency_key, discount_type, discount_value, discount_amount)
         VALUES ($1, $2, $3, 1, 'Card', 'GAPS-1', 2000, 1800, 300, 'Cash', true, $4, 'percentage', 0.1, 200) RETURNING id",
    )
    .bind(s.branch)
    .bind(till)
    .bind(s.teller)
    .bind(key)
    .fetch_one(&pool)
    .await
    .unwrap();
    let item: Uuid = sqlx::query_scalar(
        "INSERT INTO order_items (order_id, item_name, unit_price, quantity, line_total, line_cost, unit_cost, cost_missing,
                                  deductions_snapshot) VALUES ($1, 'Latte', 1000, 2, 2000, 640, 320, false, '[{\"x\":1}]') RETURNING id",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1, 'Card', 1800, false)")
        .bind(order)
        .execute(&pool)
        .await
        .unwrap();
    let _ = item;

    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    let o = row(&full, "order", order).expect("order projected");
    assert_eq!(o["idempotency_key"], key.to_string());
    assert_eq!(o["tip_is_cash"], true);
    assert_eq!(o["tip_payment_method"], "Cash");
    assert_eq!(o["discount_value"], 10, "the legacy integer spelling");
    assert_eq!(o["shift_id"], till.to_string(), "decodes as OrderFull");
    assert_eq!(o["payment_legs"][0]["is_cash"], false);
    assert_eq!(o["items"][0]["item_name"], "Latte");
    assert_eq!(o["items"][0]["quantity"], 2);
    let mut all = Vec::new();
    keys(o, &mut all);
    for banned in [
        "deductions_snapshot",
        "line_cost",
        "unit_cost",
        "cost_missing",
        "cost",
    ] {
        assert!(
            !all.iter().any(|k| k == banned),
            "no `{banned}` on the wire"
        );
    }

    // A refund carries its lines and who issued it.
    let refund: Uuid = sqlx::query_scalar(
        "INSERT INTO order_refunds (org_id, branch_id, order_id, till_id, amount, method, is_cash, reason, issued_by)
         VALUES ($1, $2, $3, $4, 500, 'Cash', true, 'goodwill', $5) RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .bind(order)
    .bind(till)
    .bind(s.teller)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO order_refund_lines (org_id, refund_id, order_item_id, quantity, amount) VALUES ($1, $2, $3, 1, 500)")
        .bind(s.org)
        .bind(refund)
        .bind(item)
        .execute(&pool)
        .await
        .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next)
        .await
        .unwrap();
    let r = change(&inc, "refund", refund)
        .expect("refund change")
        .data
        .clone()
        .unwrap();
    assert_eq!(r["issued_by_name"], "Sara");
    assert_eq!(r["lines"][0]["item_name"], "Latte");

    // A till keeps what its Z report needs.
    let t = row(&full, "till", till).unwrap();
    for k in [
        "opening_cash",
        "opening_cash_was_edited",
        "closing_cash_system",
        "status",
    ] {
        assert!(t.get(k).is_some(), "till carries `{k}`");
    }
    assert_eq!(
        t["reconciliation"],
        serde_json::json!([]),
        "an open till has no close lines yet"
    );

    // Closed with a per-method reconciliation: the lines ride the till row.
    sqlx::query("UPDATE tills SET status = 'closed', closed_at = now(), closing_cash_declared = 0, closing_cash_system = 0, reconciliation_status = 'clean' WHERE id = $1")
        .bind(till)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO till_reconciliations (till_id, method, is_cash, system_total, current_system_total, status, declared_amount)
                 VALUES ($1, 'Cash', true, 0, 0, 'checked', 0)")
        .bind(till)
        .execute(&pool)
        .await
        .unwrap();
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next)
        .await
        .unwrap();
    let tr = change(&inc2, "till", till)
        .expect("the close re-emits the till")
        .data
        .clone()
        .unwrap();
    assert_eq!(tr["reconciliation"][0]["method"], "Cash");
    assert_eq!(tr["reconciliation"][0]["status"], "checked");
}

#[sqlx::test]
async fn a_menu_item_lists_only_the_channel_prices_that_differ(pool: PgPool) {
    let s = shop(&pool).await;
    let item: Uuid = sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, name) VALUES ($1, 'Latte') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let size: Uuid = sqlx::query_scalar("INSERT INTO menu_item_sizes (menu_item_id, label, price) VALUES ($1, 'M', 1000) RETURNING id")
        .bind(item)
        .fetch_one(&pool)
        .await
        .unwrap();
    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    assert_eq!(
        row(&full, "menu_item", item).unwrap()["channel_prices"],
        serde_json::json!({})
    );

    sqlx::query(
        "INSERT INTO menu_price_overrides (scope, channel, target_type, target_id, price) VALUES ('channel', 'outside', 'menu_item_size', $1, 1300)",
    )
    .bind(size)
    .execute(&pool)
    .await
    .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next)
        .await
        .unwrap();
    let cp = change(&inc, "menu_item", item)
        .expect("a channel override re-emits")
        .data
        .clone()
        .unwrap()["channel_prices"]
        .clone();
    assert_eq!(cp["outside"]["sizes"][size.to_string()]["price"], 1300);
    assert!(
        cp.get("in_mall").is_none(),
        "an unchanged channel is not listed"
    );
}

#[test]
fn payment_availability_has_its_own_topic() {
    use crate::realtime::event::Topic;
    assert_eq!(
        crate::payment_methods::availability::AVAILABILITY_TOPIC,
        Topic::PaymentMethods
    );
    assert_eq!(Topic::parse("payment_methods"), Some(Topic::PaymentMethods));
    assert_eq!(
        Topic::PaymentMethods.permission(),
        Some(("payment_methods", "read"))
    );
    assert!(Topic::ALL.contains(&Topic::PaymentMethods));
    // Old subscriptions are unaffected: every earlier topic still parses as before.
    for t in [
        "delivery", "tickets", "kitchen", "orders", "floor", "bookings", "tills", "sync",
    ] {
        assert_eq!(Topic::parse(t).unwrap().as_str(), t);
    }
}

// ── Offline B phase 1 follow-ups: paging, horizons, role scoping ───────────

async fn seed_orders(pool: &PgPool, s: &Shop, till: Uuid, n: usize) -> Vec<Uuid> {
    let mut out = Vec::new();
    for i in 0..n {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO orders (branch_id, till_id, teller_id, order_number, payment_method, subtotal, total_amount, idempotency_key, order_ref)
             VALUES ($1, $2, $3, $4, 'cash', 100, 100, $5, 'PAGE-' || $4::text) RETURNING id",
        )
        .bind(s.branch)
        .bind(till)
        .bind(s.teller)
        .bind(i as i32 + 1)
        .bind(Uuid::new_v4())
        .fetch_one(pool)
        .await
        .unwrap();
        out.push(id);
    }
    out
}

fn paged(branch: Uuid, size: i64, cursor: Option<super::SnapshotCursor>) -> PullRequest {
    PullRequest {
        branch_id: branch,
        device_id: None,
        types: None,
        limit: None,
        ledger_page_size: Some(size),
        snapshot_cursor: cursor,
    }
}

/// A paged full snapshot is the same snapshot as the unpaged one: every ledger
/// row exactly once, state types and checksums on page one, one horizon; a row
/// changed while paging moves to the incremental pull; a till closed while
/// paging keeps its rows in the later pages.
#[sqlx::test]
async fn a_paged_snapshot_is_the_unpaged_snapshot(pool: PgPool) {
    let s = shop(&pool).await;
    let till: Uuid = sqlx::query_scalar("INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1, $2, 'open', 0) RETURNING id")
        .bind(s.branch)
        .bind(s.teller)
        .fetch_one(&pool)
        .await
        .unwrap();
    let orders = seed_orders(&pool, &s, till, 350).await;
    // The feed has not seen these change for days: only the OPEN till keeps them.
    sqlx::query(
        "UPDATE sync_changes SET changed_at = now() - interval '5 days' WHERE branch_id = $1",
    )
    .bind(s.branch)
    .execute(&pool)
    .await
    .unwrap();
    let whole = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    assert_eq!(whole.data["order"].len(), 350);

    let p1 = pull_core(&pool, s.org, &paged(s.branch, 100, None), None)
        .await
        .unwrap();
    assert!(p1.full && p1.has_more);
    assert!(
        p1.types.iter().any(|t| t == "category"),
        "state types on page one"
    );
    assert!(!p1.checksums.is_empty() && p1.asset_bundle.is_some());
    let ledger_on_p1: usize = super::LEDGER_TYPES
        .iter()
        .map(|t| p1.data.get(*t).map(Vec::len).unwrap_or(0))
        .sum();
    assert_eq!(ledger_on_p1, 100);
    let mut cursor = p1
        .snapshot_cursor
        .clone()
        .expect("a cursor for the next page");
    assert_eq!(p1.next, Some(cursor.horizon));

    // Mid-paging: close the till, and change one order already sent.
    sqlx::query("UPDATE tills SET status = 'closed', closed_at = now(), closing_cash_declared = 0 WHERE id = $1")
        .bind(till)
        .execute(&pool)
        .await
        .unwrap();
    let late = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM orders WHERE till_id = $1 ORDER BY order_number DESC LIMIT 1",
    )
    .bind(till)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE orders SET notes = 'changed while paging' WHERE id = $1")
        .bind(late)
        .execute(&pool)
        .await
        .unwrap();

    let mut seen: Vec<String> = p1
        .data
        .get("order")
        .map(|v| {
            v.iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    let mut pages = 1;
    loop {
        let p = pull_core(
            &pool,
            s.org,
            &paged(s.branch, 100, Some(cursor.clone())),
            None,
        )
        .await
        .unwrap();
        pages += 1;
        assert!(
            p.checksums.is_empty() && p.asset_bundle.is_none(),
            "page {pages}: first-page extras only once"
        );
        assert!(
            p.types.iter().all(|t| super::is_ledger(t)),
            "later pages carry ledger types only"
        );
        assert_eq!(
            p.next,
            Some(cursor.horizon),
            "one horizon for the whole snapshot"
        );
        seen.extend(
            p.data
                .get("order")
                .map(|v| {
                    v.iter()
                        .map(|r| r["id"].as_str().unwrap().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        match p.snapshot_cursor {
            Some(c) => {
                assert!(p.has_more);
                assert!(c.after_seq > cursor.after_seq);
                cursor = c;
            }
            None => {
                assert!(!p.has_more);
                break;
            }
        }
    }
    let mut want: Vec<String> = orders.iter().map(|o| o.to_string()).collect();
    want.retain(|o| *o != late.to_string());
    let mut got = seen.clone();
    got.sort();
    got.dedup();
    assert_eq!(got.len(), seen.len(), "no row twice");
    want.sort();
    assert_eq!(
        got, want,
        "every unchanged row of the closed-while-paging till, once"
    );
    let inc = pull_core(&pool, s.org, &req(s.branch), Some(cursor.horizon))
        .await
        .unwrap();
    assert!(
        change(&inc, "order", late).is_some(),
        "the row changed while paging arrives incrementally"
    );
    assert!(change(&inc, "till", till).is_some());
    // Old clients are unchanged: paging is opt-in and only for full pulls.
    assert!(
        pull_core(
            &pool,
            s.org,
            &paged(s.branch, 100, None),
            Some(cursor.horizon)
        )
        .await
        .is_err()
    );
}

/// Role defaults are global (no org on `role_permissions`); a change re-projects
/// only the users whose EFFECTIVE permission it can move — one with a per-user
/// override for that permission is left alone — once per statement.
#[sqlx::test]
async fn a_role_default_reprojects_only_users_it_can_affect(pool: PgPool) {
    let s = shop(&pool).await;
    let pinned: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) VALUES ($1, 'Pinned', $2, 'x', 'teller') RETURNING id",
    )
    .bind(s.org)
    .bind(format!("{}@gaps.test", Uuid::new_v4()))
    .fetch_one(&pool)
    .await
    .unwrap();
    let (res, act): (String, String) = sqlx::query_as(
        "SELECT resource::text, action::text FROM role_permissions WHERE role = 'teller' AND granted ORDER BY 1, 2 LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, $2::permission_resource, $3::permission_action, true)")
        .bind(pinned)
        .bind(&res)
        .bind(&act)
        .execute(&pool)
        .await
        .unwrap();
    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    // One statement changing several roles' rows at once (including the teller default).
    sqlx::query("UPDATE role_permissions SET granted = NOT granted WHERE role IN ('teller', 'waiter') AND resource = $1::permission_resource AND action = $2::permission_action")
        .bind(&res)
        .bind(&act)
        .execute(&pool)
        .await
        .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next)
        .await
        .unwrap();
    assert!(
        change(&inc, "teller", s.teller).is_some(),
        "a user on the role default is re-projected"
    );
    assert!(
        change(&inc, "teller", pinned).is_none(),
        "a user whose override pins that permission is not"
    );
    let data = change(&inc, "teller", s.teller)
        .unwrap()
        .data
        .clone()
        .unwrap();
    assert!(
        !data["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == format!("{res}:{act}").as_str())
    );
    // Insert and delete statements fire too.
    sqlx::query("DELETE FROM role_permissions WHERE role = 'teller' AND resource = $1::permission_resource AND action = $2::permission_action")
        .bind(&res)
        .bind(&act)
        .execute(&pool)
        .await
        .unwrap();
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next)
        .await
        .unwrap();
    assert!(change(&inc2, "teller", s.teller).is_some());
    assert!(change(&inc2, "teller", pinned).is_none());
}

fn test_secret() -> crate::auth::jwt::JwtSecret {
    crate::auth::jwt::JwtSecret("gaps-secret".into())
}

/// The Z report names the feed horizon its figures include, and a replayed op's
/// answer names the horizon that includes the op — the two numbers a device
/// compares with its cursor instead of guessing by time.
#[sqlx::test]
async fn the_report_and_replay_answers_carry_feed_horizons(pool: PgPool) {
    use actix_web::{App, test, web};
    let s = shop(&pool).await;
    sqlx::query("UPDATE users SET pin_hash = 'x' WHERE id = $1")
        .bind(s.teller)
        .execute(&pool)
        .await
        .unwrap();
    for action in ["read", "update"] {
        sqlx::query("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, 'tills', $2::permission_action, true) ON CONFLICT DO NOTHING")
            .bind(s.teller)
            .bind(action)
            .execute(&pool)
            .await
            .unwrap();
    }
    let till: Uuid = sqlx::query_scalar("INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1, $2, 'open', 0) RETURNING id")
        .bind(s.branch)
        .bind(s.teller)
        .fetch_one(&pool)
        .await
        .unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(test_secret()))
            .app_data(web::Data::new(crate::realtime::hub::BranchEventHub::new()))
            .configure(crate::tills::routes::configure)
            .configure(crate::sync::routes::configure),
    )
    .await;
    let token = crate::auth::jwt::create_token(
        &test_secret(),
        s.teller,
        Some(s.org),
        crate::models::UserRole::Teller,
        Some(s.branch),
        1,
    )
    .unwrap();
    let head = |p: &PgPool| {
        let p = p.clone();
        let b = s.branch;
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(max(seq), 0) FROM sync_changes WHERE branch_id = $1",
            )
            .bind(b)
            .fetch_one(&p)
            .await
            .unwrap()
        }
    };

    let op = serde_json::json!({"op": "cash_movement", "teller_id": s.teller, "till_id": till,
        "request": {"amount": 500, "note": "float", "client_ref": Uuid::new_v4()}});
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&op)
            .to_request(),
    )
    .await;
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = test::read_body(resp).await;
    assert!(
        status.is_success(),
        "{status} {}",
        String::from_utf8_lossy(&body)
    );
    let seq: i64 = headers
        .get(crate::sync::handlers::SYNC_SEQ_HEADER)
        .expect("a replay answer names its horizon")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        seq,
        head(&pool).await,
        "every change the op made is at or below it"
    );
    let movement_seq: i64 = sqlx::query_scalar(
        "SELECT seq FROM sync_changes WHERE branch_id = $1 AND type = 'cash_movement'",
    )
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(movement_seq <= seq);

    let report: serde_json::Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri(&format!("/tills/{till}/report"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(
        report["as_of_seq"].as_i64(),
        Some(head(&pool).await),
        "the report includes the feed up to its horizon"
    );
    assert_eq!(report["cash_movements_net"].as_i64(), Some(500));
}
