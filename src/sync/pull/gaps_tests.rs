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
    let org: Uuid = sqlx::query_scalar("INSERT INTO organizations (name, slug) VALUES ('Gaps Org', $1) RETURNING id")
        .bind(format!("gaps-{}", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap();
    let branch: Uuid = sqlx::query_scalar("INSERT INTO branches (org_id, name, code) VALUES ($1, 'Gaps', 'GAPS') RETURNING id")
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
    Shop { org, branch, teller }
}

fn req(branch: Uuid) -> PullRequest {
    PullRequest { branch_id: branch, device_id: None, types: None, limit: None }
}

fn row<'a>(resp: &'a super::PullResponse, ty: &str, id: Uuid) -> Option<&'a Value> {
    resp.data.get(ty)?.iter().find(|r| r["id"] == id.to_string())
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
    assert!(full.checksums.contains_key("addon_item"), "a state type is checksummed");

    // A branch override: new price, then switched off — each an incremental change.
    sqlx::query("INSERT INTO branch_addon_overrides (branch_id, addon_item_id, price_override) VALUES ($1, $2, 1800)")
        .bind(s.branch)
        .bind(addon)
        .execute(&pool)
        .await
        .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next).await.unwrap();
    let c = change(&inc, "addon_item", addon).expect("override re-emits the addon");
    assert_eq!(c.data.as_ref().unwrap()["default_price"], 1800);
    sqlx::query("UPDATE branch_addon_overrides SET is_available = false WHERE branch_id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next).await.unwrap();
    assert_eq!(change(&inc2, "addon_item", addon).unwrap().data.as_ref().unwrap()["is_available"], false);

    // An ingredient edit re-emits; a delete is a delete.
    sqlx::query("UPDATE addon_item_ingredients SET quantity_used = 0.3 WHERE addon_item_id = $1")
        .bind(addon)
        .execute(&pool)
        .await
        .unwrap();
    let inc3 = pull_core(&pool, s.org, &req(s.branch), inc2.next).await.unwrap();
    assert!(change(&inc3, "addon_item", addon).is_some());
    sqlx::query("DELETE FROM addon_items WHERE id = $1").bind(addon).execute(&pool).await.unwrap();
    let inc4 = pull_core(&pool, s.org, &req(s.branch), inc3.next).await.unwrap();
    assert_eq!(change(&inc4, "addon_item", addon).unwrap().op, "delete");
}

/// After the menu-unification contract shim the legacy addon relations are VIEWS
/// (no row triggers): the unified tables' emitters carry `addon_item` instead.
#[sqlx::test]
async fn an_addon_item_rides_the_feed_after_the_contract_shim(pool: PgPool) {
    let s = shop(&pool).await;
    sqlx::raw_sql(include_str!("../../../deploy/menu_unification_shim.sql")).execute(&pool).await.unwrap();
    let kind: String = sqlx::query_scalar("SELECT relkind::text FROM pg_class WHERE relname = 'addon_items'")
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
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next).await.unwrap();
    let c = change(&inc, "addon_item", opt).expect("a new addon option emits");
    assert_eq!(c.op, "upsert");
    assert_eq!(c.data.as_ref().unwrap()["default_price"], 1500);
    assert_eq!(c.data.as_ref().unwrap()["addon_type"], "milk_type");

    // Its recipe line (the ingredient it uses), under a rename of that ingredient.
    let ing: Uuid = sqlx::query_scalar("INSERT INTO org_ingredients (org_id, name, unit) VALUES ($1, 'Oat', 'l') RETURNING id")
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
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next).await.unwrap();
    assert_eq!(change(&inc2, "addon_item", opt).unwrap().data.as_ref().unwrap()["ingredients"][0]["ingredient_name"], "Oat");
    sqlx::query("UPDATE org_ingredients SET name = 'Oat drink' WHERE id = $1").bind(ing).execute(&pool).await.unwrap();
    let inc3 = pull_core(&pool, s.org, &req(s.branch), inc2.next).await.unwrap();
    assert_eq!(
        change(&inc3, "addon_item", opt).expect("an ingredient rename re-emits").data.as_ref().unwrap()["ingredients"][0]["ingredient_name"],
        "Oat drink"
    );

    // A branch override switches it off at this branch.
    sqlx::query("INSERT INTO menu_price_overrides (scope, branch_id, target_type, target_id, price, is_available) VALUES ('branch', $1, 'modifier_option', $2, 1800, false)")
        .bind(s.branch)
        .bind(opt)
        .execute(&pool)
        .await
        .unwrap();
    let inc4 = pull_core(&pool, s.org, &req(s.branch), inc3.next).await.unwrap();
    let d = change(&inc4, "addon_item", opt).expect("override re-emits").data.clone().unwrap();
    assert_eq!((d["default_price"].clone(), d["is_available"].clone()), (1800.into(), false.into()));

    // Deleting the option retires it; deleting a whole group retires its options.
    sqlx::query("DELETE FROM modifier_options WHERE id = $1").bind(opt).execute(&pool).await.unwrap();
    let inc5 = pull_core(&pool, s.org, &req(s.branch), inc4.next).await.unwrap();
    assert_eq!(change(&inc5, "addon_item", opt).unwrap().op, "delete");
    let opt2: Uuid = sqlx::query_scalar("INSERT INTO modifier_options (group_id, name, legacy_source) VALUES ($1, 'Soy', 'addon') RETURNING id")
        .bind(group)
        .fetch_one(&pool)
        .await
        .unwrap();
    let inc6 = pull_core(&pool, s.org, &req(s.branch), inc5.next).await.unwrap();
    assert_eq!(change(&inc6, "addon_item", opt2).unwrap().op, "upsert");
    sqlx::query("DELETE FROM modifier_groups WHERE id = $1").bind(group).execute(&pool).await.unwrap();
    let inc7 = pull_core(&pool, s.org, &req(s.branch), inc6.next).await.unwrap();
    assert_eq!(change(&inc7, "addon_item", opt2).expect("the group delete retires its options").op, "delete");

    // And a full snapshot agrees with the incremental feed.
    let again = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    assert!(row(&again, "addon_item", opt2).is_none());
}

#[sqlx::test]
async fn a_tellers_effective_permissions_ride_the_feed(pool: PgPool) {
    let s = shop(&pool).await;
    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    let t = row(&full, "teller", s.teller).expect("teller projected");
    let perms: Vec<String> = t["permissions"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect();
    let role_granted: Vec<String> = sqlx::query_scalar(
        "SELECT resource::text || ':' || action::text FROM role_permissions WHERE role = 'teller' AND granted ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!role_granted.is_empty());
    assert_eq!(perms, role_granted, "no override: exactly the role's grants, granted only");

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
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next).await.unwrap();
    let data = change(&inc, "teller", s.teller).expect("override re-emits the teller").data.clone().unwrap();
    assert!(!data["permissions"].as_array().unwrap().iter().any(|p| p == revoked.as_str()));

    // A role default changing re-emits every user of that role.
    let (r2, a2) = role_granted[1].split_once(':').unwrap();
    sqlx::query("UPDATE role_permissions SET granted = false WHERE role = 'teller' AND resource = $1::permission_resource AND action = $2::permission_action")
        .bind(r2)
        .bind(a2)
        .execute(&pool)
        .await
        .unwrap();
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next).await.unwrap();
    let data2 = change(&inc2, "teller", s.teller).expect("role default re-emits").data.clone().unwrap();
    assert!(!data2["permissions"].as_array().unwrap().iter().any(|p| p == role_granted[1].as_str()));
}

#[sqlx::test]
async fn branch_settings_carry_the_delivery_prep_minutes(pool: PgPool) {
    let s = shop(&pool).await;
    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    assert_eq!(row(&full, "branch_settings", s.branch).unwrap()["delivery_prep_minutes"], 20, "the table default");
    sqlx::query("INSERT INTO branch_delivery_settings (branch_id, prep_time_minutes) VALUES ($1, 35)")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next).await.unwrap();
    assert_eq!(change(&inc, "branch_settings", s.branch).unwrap().data.as_ref().unwrap()["delivery_prep_minutes"], 35);
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
    for banned in ["deductions_snapshot", "line_cost", "unit_cost", "cost_missing", "cost"] {
        assert!(!all.iter().any(|k| k == banned), "no `{banned}` on the wire");
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
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next).await.unwrap();
    let r = change(&inc, "refund", refund).expect("refund change").data.clone().unwrap();
    assert_eq!(r["issued_by_name"], "Sara");
    assert_eq!(r["lines"][0]["item_name"], "Latte");

    // A till keeps what its Z report needs.
    let t = row(&full, "till", till).unwrap();
    for k in ["opening_cash", "opening_cash_was_edited", "closing_cash_system", "status"] {
        assert!(t.get(k).is_some(), "till carries `{k}`");
    }
    assert_eq!(t["reconciliation"], serde_json::json!([]), "an open till has no close lines yet");

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
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next).await.unwrap();
    let tr = change(&inc2, "till", till).expect("the close re-emits the till").data.clone().unwrap();
    assert_eq!(tr["reconciliation"][0]["method"], "Cash");
    assert_eq!(tr["reconciliation"][0]["status"], "checked");
}

#[sqlx::test]
async fn a_menu_item_lists_only_the_channel_prices_that_differ(pool: PgPool) {
    let s = shop(&pool).await;
    let item: Uuid = sqlx::query_scalar("INSERT INTO menu_items (org_id, name) VALUES ($1, 'Latte') RETURNING id")
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
    assert_eq!(row(&full, "menu_item", item).unwrap()["channel_prices"], serde_json::json!({}));

    sqlx::query(
        "INSERT INTO menu_price_overrides (scope, channel, target_type, target_id, price) VALUES ('channel', 'outside', 'menu_item_size', $1, 1300)",
    )
    .bind(size)
    .execute(&pool)
    .await
    .unwrap();
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next).await.unwrap();
    let cp = change(&inc, "menu_item", item).expect("a channel override re-emits").data.clone().unwrap()["channel_prices"].clone();
    assert_eq!(cp["outside"]["sizes"][size.to_string()]["price"], 1300);
    assert!(cp.get("in_mall").is_none(), "an unchanged channel is not listed");
}

#[test]
fn payment_availability_has_its_own_topic() {
    use crate::realtime::event::Topic;
    assert_eq!(crate::payment_methods::availability::AVAILABILITY_TOPIC, Topic::PaymentMethods);
    assert_eq!(Topic::parse("payment_methods"), Some(Topic::PaymentMethods));
    assert_eq!(Topic::PaymentMethods.permission(), Some(("payment_methods", "read")));
    assert!(Topic::ALL.contains(&Topic::PaymentMethods));
    // Old subscriptions are unaffected: every earlier topic still parses as before.
    for t in ["delivery", "tickets", "kitchen", "orders", "floor", "bookings", "tills", "sync"] {
        assert_eq!(Topic::parse(t).unwrap().as_str(), t);
    }
}
