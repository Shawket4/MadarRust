//! The `/sync/pull` gaps offline plan B closes (TILLS_VERIFICATION "Before/with
//! B"): addon items, effective permissions, delivery prep minutes, order lines
//! for reprint, per-channel menu prices — and the payment-availability event on
//! its own realtime topic.
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::sync::pull::{PullRequest, pull_core};

struct Shop {
    org: Uuid,
    branch: Uuid,
    teller: Uuid,
}

async fn shop(pool: &PgPool) -> Shop {
    // Teller defaults beyond the core reads, so a test can revoke one.
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) VALUES
            ('teller', 'orders', 'delete', true), ('teller', 'refunds', 'read', true)
         ON CONFLICT (role, resource, action) DO UPDATE SET granted = true",
    )
    .execute(pool)
    .await
    .unwrap();
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

fn row<'a>(
    resp: &'a madar_rust::sync::pull::PullResponse,
    ty: &str,
    id: Uuid,
) -> Option<&'a Value> {
    resp.data
        .get(ty)?
        .iter()
        .find(|r| r["id"] == id.to_string())
}

fn change<'a>(
    resp: &'a madar_rust::sync::pull::PullResponse,
    ty: &str,
    id: Uuid,
) -> Option<&'a madar_rust::sync::pull::PullChange> {
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
    sqlx::raw_sql(include_str!("../deploy/menu_unification_shim.sql"))
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
    // Plus the reads a teller always holds (core capabilities).
    let core = madar_rust::authz::core_set(madar_rust::authz::RoleKind::Teller);
    let mut expected: Vec<String> = role_granted
        .clone()
        .into_iter()
        .chain(
            core.iter()
                .filter_map(|c| c.meta().legacy.map(|(r, a)| format!("{r}:{a}"))),
        )
        .collect();
    expected.sort();
    expected.dedup();
    assert_eq!(
        perms, expected,
        "no override: exactly the role's grants and core reads, granted only"
    );

    // Revoke one for this person: it leaves their list through an incremental change.
    let revoked = "orders:delete".to_string();
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
    use madar_rust::realtime::event::Topic;
    assert_eq!(
        madar_rust::payment_methods::availability::AVAILABILITY_TOPIC,
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

fn paged(
    branch: Uuid,
    size: i64,
    cursor: Option<madar_rust::sync::pull::SnapshotCursor>,
) -> PullRequest {
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
    let ledger_on_p1: usize = madar_rust::sync::pull::LEDGER_TYPES
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
            p.types.iter().all(|t| madar_rust::sync::pull::is_ledger(t)),
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
        "SELECT resource::text, action::text FROM role_permissions WHERE role = 'teller' AND granted AND NOT (resource::text || ':' || action::text = ANY(ARRAY['addon_items:read','branches:read','categories:read','menu_items:read','discounts:read','floor_plan:read','orders:read','payment_methods:read','tills:read','open_tickets:read','table_transfers:read'])) ORDER BY 1, 2 LIMIT 1",
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
    // A role grant change re-projects every holder of the role (the new model's
    // emitter); a pinned override only means their answer did not change.
    let _ = pinned;
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

fn test_secret() -> madar_rust::auth::jwt::JwtSecret {
    madar_rust::auth::jwt::JwtSecret("gaps-secret".into())
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
            .app_data(web::Data::new(
                madar_rust::realtime::hub::BranchEventHub::new(),
            ))
            .configure(madar_rust::tills::routes::configure)
            .configure(madar_rust::sync::routes::configure),
    )
    .await;
    let token = madar_rust::auth::jwt::create_token(
        &test_secret(),
        s.teller,
        Some(s.org),
        madar_rust::models::UserRole::Teller,
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
        .get(madar_rust::sync::handlers::SYNC_SEQ_HEADER)
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

// ── Custom (untyped) modifier groups after the contract shim ─────────────────
//
// A group created with no `legacy_addon_type` is a custom group for new clients
// only. The shim's `addon_items` view still lists its options (it is where the
// order path prices every option), but with `type` NULL, which the old addon
// wire (`addon_type: String`) cannot carry. Live (T1 rig, 2026-09-26) that NULL
// reached `addon_items_by_ids` and every `/sync/pull` of the branch answered
// 500 "decoding column addon_type: unexpected null".

macro_rules! custom_group_app {
    ($pool:expr) => {
        actix_web::test::init_service(
            actix_web::App::new()
                .app_data(actix_web::web::Data::new($pool.clone()))
                .app_data(actix_web::web::Data::new(test_secret()))
                .app_data(actix_web::web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .configure(madar_rust::menu::routes::configure)
                .configure(madar_rust::sync::routes::configure)
                .configure(madar_rust::costing::routes::configure)
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::delivery::routes::configure),
        )
        .await
    };
}

async fn http<S>(
    app: &S,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    use actix_web::test::TestRequest;
    let mut r = match method {
        "GET" => TestRequest::get(),
        "POST" => TestRequest::post(),
        "PATCH" => TestRequest::patch(),
        "PUT" => TestRequest::put(),
        other => panic!("unsupported method {other}"),
    }
    .uri(uri);
    if let Some(t) = token {
        r = r.insert_header(("Authorization", format!("Bearer {t}")));
    }
    if let Some(b) = body {
        r = r.set_json(b);
    }
    let resp = actix_web::test::call_service(app, r.to_request()).await;
    let status = resp.status().as_u16();
    let bytes = actix_web::test::read_body(resp).await;
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

/// The shop's owner (org admin, default role grants) on a database the contract
/// shim has been applied to, as on the box. Returns the owner's token.
async fn shimmed_owner(pool: &PgPool, s: &Shop) -> String {
    let owner: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) VALUES ($1, 'Owner', $2, 'x', 'org_admin') RETURNING id",
    )
    .bind(s.org)
    .bind(format!("{}@gaps.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../deploy/menu_unification_shim.sql"))
        .execute(pool)
        .await
        .unwrap();
    madar_rust::auth::jwt::create_token(
        &test_secret(),
        owner,
        Some(s.org),
        madar_rust::models::UserRole::OrgAdmin,
        None,
        1,
    )
    .unwrap()
}

/// `POST /modifier-groups` then `POST /modifier-groups/{gid}/options`, as the
/// dashboard does. Returns (group, option).
async fn group_with_option<S>(app: &S, token: &str, group: Value, option: Value) -> (Uuid, Uuid)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let (st, g) = http(app, "POST", "/modifier-groups", Some(token), Some(group)).await;
    assert_eq!(st, 201, "{g}");
    let gid: Uuid = g["id"].as_str().unwrap().parse().unwrap();
    let (st, o) = http(
        app,
        "POST",
        &format!("/modifier-groups/{gid}/options"),
        Some(token),
        Some(option),
    )
    .await;
    assert_eq!(st, 201, "{o}");
    (gid, o["id"].as_str().unwrap().parse().unwrap())
}

/// The `id`s of a JSON array of objects.
fn ids_in(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("not an array: {v}"))
        .iter()
        .filter_map(|r| r["id"].as_str().or(r["addon_item_id"].as_str()))
        .map(str::to_string)
        .collect()
}

/// The pull's change for (`ty`, `id`), from an HTTP answer.
fn change_in<'a>(pull: &'a Value, ty: &str, id: Uuid) -> Option<&'a Value> {
    pull["changes"]
        .as_array()?
        .iter()
        .find(|c| c["type"] == ty && c["id"] == id.to_string())
}

#[sqlx::test]
async fn a_custom_group_with_no_legacy_type_keeps_the_pull_and_the_addon_lists_answering(
    pool: PgPool,
) {
    let s = shop(&pool).await;
    let token = shimmed_owner(&pool, &s).await;
    sqlx::query(
        "INSERT INTO branch_delivery_settings (branch_id, pickup_enabled) VALUES ($1, true)",
    )
    .bind(s.branch)
    .execute(&pool)
    .await
    .unwrap();
    let app = custom_group_app!(pool);
    let pull = json_pull(s.branch);

    let (st, first) = http(&app, "POST", "/sync/pull", Some(&token), Some(pull.clone())).await;
    assert_eq!(st, 200, "{first}");
    let cursor = first["next"].as_i64().unwrap();

    // The owner's custom group: effect omitted, no legacy type → NULL type.
    let (gid, custom) = group_with_option(
        &app,
        &token,
        serde_json::json!({"name": "Toppings", "selection_type": "multi"}),
        serde_json::json!({"name": "Caramel drizzle", "price": 500}),
    )
    .await;
    let legacy: Option<String> =
        sqlx::query_scalar("SELECT legacy_addon_type FROM modifier_groups WHERE id = $1")
            .bind(gid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(legacy, None, "a custom group has no legacy lineage");
    // A typed group beside it: old tills see this one.
    let (_, typed) = group_with_option(
        &app,
        &token,
        serde_json::json!({"name": "Extras", "selection_type": "multi", "legacy_addon_type": "extra"}),
        serde_json::json!({"name": "Extra shot", "price": 700}),
    )
    .await;

    // The incremental pull answers; the custom option's feed row is not a
    // broken upsert but the delete the pull sends for anything it does not
    // project (a till never held it), and the typed one arrives whole.
    let (st, inc) = http(
        &app,
        "POST",
        &format!("/sync/pull?since={cursor}"),
        Some(&token),
        Some(pull.clone()),
    )
    .await;
    assert_eq!(st, 200, "the branch keeps syncing: {inc}");
    let c = change_in(&inc, "addon_item", custom).expect("the option's feed row is answered");
    assert_eq!(c["op"], "delete", "{c}");
    assert!(c.get("data").is_none_or(Value::is_null), "{c}");
    let t = change_in(&inc, "addon_item", typed).expect("the typed option rides the feed");
    assert_eq!(t["op"], "upsert");
    assert_eq!(t["data"]["addon_type"], "extra");
    assert_eq!(t["data"]["default_price"], 700);

    // A full snapshot answers too, without the custom option, and its checksum
    // counts exactly the rows it ships.
    let (st, full) = http(&app, "POST", "/sync/pull", Some(&token), Some(pull.clone())).await;
    assert_eq!(st, 200, "{full}");
    let rows = full["data"]["addon_item"].as_array().unwrap();
    let shipped = ids_in(&full["data"]["addon_item"]);
    assert!(shipped.contains(&typed.to_string()));
    assert!(
        !shipped.contains(&custom.to_string()),
        "invisible to old tills"
    );
    let pairs: Vec<(String, i64)> = rows
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_string(),
                r["seq"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(full["checksums"]["addon_item"]["count"], pairs.len() as i64);
    assert_eq!(
        full["checksums"]["addon_item"]["checksum"],
        madar_rust::sync::pull::checksum::checksum_of(&pairs)
    );

    // The legacy addon lists answer, without it.
    for uri in [
        format!("/addon-items?org_id={}", s.org),
        format!("/addon-items?org_id={}&branch_id={}", s.org, s.branch),
        format!("/costing/addon-items?org_id={}", s.org),
    ] {
        let (st, list) = http(&app, "GET", &uri, Some(&token), None).await;
        assert_eq!(st, 200, "{uri}: {list}");
        let ids = ids_in(&list);
        assert!(ids.contains(&typed.to_string()), "{uri}: {list}");
        assert!(!ids.contains(&custom.to_string()), "{uri}: {list}");
    }
    for uri in [
        format!("/addon-items?org_id={}&page=1&per_page=10", s.org),
        format!("/addon-items/catalog?org_id={}", s.org),
    ] {
        let (st, page) = http(&app, "GET", &uri, Some(&token), None).await;
        assert_eq!(st, 200, "{uri}: {page}");
        assert_eq!(ids_in(&page["data"]), vec![typed.to_string()], "{uri}");
        assert_eq!(page["total"], 1, "{uri}: the count agrees with the rows");
    }
    // The storefront's org-wide add-on list (the legacy shape) too.
    let (st, menu) = http(
        &app,
        "GET",
        &format!(
            "/public/branches/{}/menu?channel=pickup&preview=true",
            s.branch
        ),
        None,
        None,
    )
    .await;
    assert_eq!(st, 200, "{menu}");
    let addons = ids_in(&menu["addons"]);
    assert!(addons.contains(&typed.to_string()), "{menu}");
    assert!(!addons.contains(&custom.to_string()), "{menu}");

    // Giving the group a legacy type later makes its option an ordinary addon
    // (an upsert), and clearing it again retires it (a delete).
    let at = full["next"].as_i64().unwrap();
    let (st, g) = http(
        &app,
        "PATCH",
        &format!("/modifier-groups/{gid}"),
        Some(&token),
        Some(serde_json::json!({"legacy_addon_type": "extra"})),
    )
    .await;
    assert_eq!(st, 200, "{g}");
    let (st, inc) = http(
        &app,
        "POST",
        &format!("/sync/pull?since={at}"),
        Some(&token),
        Some(pull.clone()),
    )
    .await;
    assert_eq!(st, 200, "{inc}");
    let c = change_in(&inc, "addon_item", custom).expect("typed now: re-emitted");
    assert_eq!(c["op"], "upsert", "{c}");
    assert_eq!(c["data"]["addon_type"], "extra");
    let at = inc["next"].as_i64().unwrap();
    let (st, g) = http(
        &app,
        "PATCH",
        &format!("/modifier-groups/{gid}"),
        Some(&token),
        Some(serde_json::json!({"legacy_addon_type": null})),
    )
    .await;
    assert_eq!(st, 200, "{g}");
    assert_eq!(g["legacy_addon_type"], Value::Null);
    let (st, inc) = http(
        &app,
        "POST",
        &format!("/sync/pull?since={at}"),
        Some(&token),
        Some(pull),
    )
    .await;
    assert_eq!(st, 200, "{inc}");
    let c = change_in(&inc, "addon_item", custom).expect("untyped again: retired");
    assert_eq!(c["op"], "delete", "{c}");
    let (st, list) = http(
        &app,
        "GET",
        &format!("/addon-items?org_id={}", s.org),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st, 200, "{list}");
    assert!(!ids_in(&list).contains(&custom.to_string()));
}

/// The order path is where a custom option must stay VISIBLE: a new till
/// sells it from the unified groups and sends its id as an addon. Its NULL
/// type must neither 500 the line that chooses it nor any line of an item whose
/// recipe shares an ingredient with it (the swap-candidate lookup reads every
/// option carrying the recipe's ingredients).
#[sqlx::test]
async fn a_custom_option_with_no_legacy_type_still_prices_on_the_order_path(pool: PgPool) {
    let s = shop(&pool).await;
    let token = shimmed_owner(&pool, &s).await;
    let app = custom_group_app!(pool);

    let espresso: Uuid = sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit, cost_per_unit) VALUES ($1, 'Espresso', 'g', 1) RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let cat: Uuid = sqlx::query_scalar(
        "INSERT INTO categories (org_id, name) VALUES ($1, 'Coffee') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let latte: Uuid = sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, category_id, name, base_price) VALUES ($1, $2, 'Latte', 6000) RETURNING id",
    )
    .bind(s.org)
    .bind(cat)
    .fetch_one(&pool)
    .await
    .unwrap();
    let size: Uuid = sqlx::query_scalar("SELECT id FROM menu_item_sizes WHERE menu_item_id = $1")
        .bind(latte)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) VALUES ('item_size', $1, $2, 18, 'g')")
        .bind(size)
        .bind(espresso)
        .execute(&pool)
        .await
        .unwrap();

    // "Extra shot" in a custom group, deducting the latte's own ingredient.
    let (gid, shot) = group_with_option(
        &app,
        &token,
        serde_json::json!({"name": "Strength", "selection_type": "multi"}),
        serde_json::json!({"name": "Extra shot", "price": 1500}),
    )
    .await;
    let (st, r) = http(
        &app,
        "PUT",
        &format!("/modifier-options/{shot}/recipe"),
        Some(&token),
        Some(serde_json::json!([{"ingredient_id": espresso, "quantity": 9, "unit": "g"}])),
    )
    .await;
    assert_eq!(st, 200, "{r}");
    let (st, r) = http(
        &app,
        "PUT",
        &format!("/menu-items/{latte}/modifier-groups"),
        Some(&token),
        Some(serde_json::json!({"groups": [{"group_id": gid, "sort": 0}]})),
    )
    .await;
    assert_eq!(st, 200, "{r}");

    use madar_rust::orders::component_resolve::{AddonInput, resolve_menu_item_configuration};
    // A plain latte: the custom option is a swap candidate of its espresso.
    let plain = resolve_menu_item_configuration(&pool, latte, None, 1, &[], &[], s.branch)
        .await
        .expect("a latte still sells");
    assert_eq!(plain.addon_line, 0);
    // A latte with the extra shot: charged, and its 9 g deducted over the 18 g.
    let with = resolve_menu_item_configuration(
        &pool,
        latte,
        None,
        1,
        &[AddonInput {
            addon_item_id: shot,
            quantity: 1,
            unit_price: None,
        }],
        &[],
        s.branch,
    )
    .await
    .expect("the custom option sells");
    assert_eq!(with.addon_line, 1500);
    let espresso_g: f64 = with
        .deductions
        .iter()
        .filter(|d| d.org_ingredient_id == Some(espresso))
        .map(|d| d.quantity)
        .sum();
    assert_eq!(espresso_g, 27.0);

    // The recipe preview a till asks for answers too.
    let (st, preview) = http(
        &app,
        "POST",
        "/orders/preview-recipe",
        Some(&token),
        Some(serde_json::json!({
            "menu_item_id": latte,
            "size_label": null,
            "addons": [{"addon_item_id": shot, "quantity": 1}],
            "optional_field_ids": []
        })),
    )
    .await;
    assert_eq!(st, 200, "{preview}");
    let from_addon: f64 = preview
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["source"] == "addon")
        .map(|l| l["quantity"].as_f64().unwrap())
        .sum();
    assert_eq!(from_addon, 9.0, "{preview}");
}

fn json_pull(branch: Uuid) -> Value {
    serde_json::json!({ "branch_id": branch })
}
