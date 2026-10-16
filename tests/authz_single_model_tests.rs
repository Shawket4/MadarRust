//! DB audit Phase 4: the server decides from ONE model (architecture E), and
//! the shapes old tills read are unchanged.
//!
//! - `GET /auth/permissions` keeps its exact cell list and item shape
//!   (pinned in `tests/fixtures/authz/auth_permissions_cells.txt`).
//! - The POS teller row keeps its keys; `permissions` is the same sorted
//!   `resource:action` strings, now computed from the new model only.
//! - A check reads nothing from `role_permissions` / `permissions`.
//! - The exhaustive reconciliation finds no difference on a mirrored org and
//!   finds a planted one.

use std::collections::BTreeSet;

use actix_web::{App, test, web};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{Claims, JwtSecret};
use madar_rust::authz::{CAPS, RoleKind};
use madar_rust::errors::AppError;
use madar_rust::models::UserRole;
use madar_rust::permissions::checker::check_permission;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

async fn org(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO organizations (name, slug) VALUES ('Org', $1) RETURNING id")
        .bind(format!("o-{}", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn branch(pool: &PgPool, org: Uuid) -> Uuid {
    sqlx::query_scalar("INSERT INTO branches (org_id, name) VALUES ($1, $2) RETURNING id")
        .bind(org)
        .bind(format!("B {}", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, role, email, password_hash, pin_hash)
         VALUES ($1, $2, $3, $4::user_role, $5, 'h', 'h')",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role}-{}", &id.to_string()[..6]))
    .bind(role)
    .bind(format!("{id}@t.com"))
    .execute(pool)
    .await
    .unwrap();
    id
}

fn claims(user: Uuid, org: Uuid, role: UserRole) -> Claims {
    Claims {
        sub: user.to_string(),
        org_id: Some(org.to_string()),
        role,
        branch_id: None,
        exp: usize::MAX / 2,
        iat: chrono::Utc::now().timestamp() as usize,
    }
}

fn pinned_cells() -> Vec<String> {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/authz/auth_permissions_cells.txt"
    ))
    .unwrap()
    .lines()
    .filter(|l| !l.starts_with('#') && !l.is_empty())
    .map(str::to_string)
    .collect()
}

/// The contract a pre-0.8 tablet reads: `{"permissions": [{resource, action,
/// granted}, ...]}`, every pinned cell in the pinned order (the CRUD grid, the
/// extra grants, then the `shifts:*` mirror of `tills:*`), nothing else.
#[sqlx::test]
async fn auth_permissions_keeps_its_old_shape(pool: PgPool) {
    let o = org(&pool).await;
    let teller = user(&pool, o, "teller").await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(madar_rust::auth::routes::configure),
    )
    .await;
    let token =
        madar_rust::auth::jwt::create_token(&secret(), teller, Some(o), UserRole::Teller, None, 24)
            .unwrap();
    let req = test::TestRequest::get()
        .uri("/auth/permissions")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: Value = test::read_body_json(resp).await;
    let top: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(top, vec!["permissions"]);
    let items = body["permissions"].as_array().unwrap();
    let cells: Vec<String> = items
        .iter()
        .map(|i| {
            let keys: BTreeSet<&String> = i.as_object().unwrap().keys().collect();
            assert_eq!(
                keys.into_iter().cloned().collect::<Vec<_>>(),
                vec!["action", "granted", "resource"]
            );
            assert!(i["granted"].is_boolean());
            format!(
                "{}:{}",
                i["resource"].as_str().unwrap(),
                i["action"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(cells, pinned_cells());
    // The mirror still mirrors.
    let granted = |cell: &str| {
        items
            .iter()
            .find(|i| format!("{}:{}", i["resource"].as_str().unwrap(), i["action"].as_str().unwrap()) == cell)
            .map(|i| i["granted"].as_bool().unwrap())
            .unwrap()
    };
    for a in ["create", "read", "update", "delete"] {
        assert_eq!(granted(&format!("shifts:{a}")), granted(&format!("tills:{a}")));
    }
    assert!(granted("orders:create"), "a teller rings up");
    assert!(!granted("payroll:read"), "a teller sees no salaries");
}

/// The teller row of the POS feed: the same keys as before Phase 4 (additive
/// only), and `permissions` is the sorted list of granted cells.
#[sqlx::test]
async fn the_teller_row_keeps_its_keys(pool: PgPool) {
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let teller = user(&pool, o, "teller").await;
    let req = madar_rust::sync::pull::PullRequest {
        branch_id: b,
        device_id: None,
        types: Some(vec!["teller".into()]),
        limit: None,
        ledger_page_size: None,
        snapshot_cursor: None,
    };
    let resp = madar_rust::sync::pull::pull_core(&pool, o, &req, None)
        .await
        .unwrap();
    let row = resp.data["teller"]
        .iter()
        .find(|r| r["id"] == teller.to_string())
        .expect("the teller is on the feed")
        .clone();
    let keys: BTreeSet<String> = row.as_object().unwrap().keys().cloned().collect();
    let pinned: BTreeSet<String> = [
        "id",
        "user_id",
        "name",
        "role",
        "is_active",
        "permissions",
        "capabilities",
        "limits",
        "ask_manager",
        "is_owner",
        "role_kinds",
        "authz_epoch",
        "seq",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(keys, pinned);
    assert_eq!(row["role"], "teller");
    let perms: Vec<String> = row["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect();
    let mut sorted = perms.clone();
    sorted.sort();
    assert_eq!(perms, sorted, "sorted, as old tills expect");
    assert!(perms.contains(&"orders:create".to_string()));
    assert!(!perms.contains(&"payroll:read".to_string()));
    assert!(perms.iter().all(|p| !p.starts_with("shifts:")), "the feed never carried the mirror");
}

/// With the mirror switched off and the legacy tables emptied (and a stray
/// grant planted in them), a check answers exactly as before: nothing legacy
/// is read.
#[sqlx::test]
async fn a_check_reads_nothing_from_the_legacy_tables(pool: PgPool) {
    let o = org(&pool).await;
    let teller = user(&pool, o, "teller").await;
    let c = claims(teller, o, UserRole::Teller);
    assert!(check_permission(&pool, &c, "orders", "create").await.is_ok());
    assert!(check_permission(&pool, &c, "payroll", "read").await.is_err());

    sqlx::query("ALTER TABLE role_permissions DISABLE TRIGGER USER")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE permissions DISABLE TRIGGER USER")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM role_permissions").execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, 'payroll', 'read', true)",
    )
    .bind(teller)
    .execute(&pool)
    .await
    .unwrap();

    assert!(check_permission(&pool, &c, "orders", "create").await.is_ok());
    assert!(matches!(
        check_permission(&pool, &c, "payroll", "read").await,
        Err(AppError::Forbidden(_))
    ));
}

/// A token whose account row is gone gets the spec's role default (what the
/// legacy role table answered that case with), never more.
#[sqlx::test]
async fn a_token_with_no_user_row_gets_the_spec_role_default(pool: PgPool) {
    let o = org(&pool).await;
    let ghost = claims(Uuid::new_v4(), o, UserRole::Teller);
    assert!(check_permission(&pool, &ghost, "orders", "create").await.is_ok());
    assert!(check_permission(&pool, &ghost, "payroll", "read").await.is_err());
}

/// A teller-granted legacy cell that is not core for tellers (so removing the
/// role grant really removes it).
fn removable_teller_cell() -> (&'static str, &'static str, i16) {
    CAPS.iter()
        .find_map(|m| {
            let (r, a) = m.legacy?;
            // Not one the fixture's own overrides touch.
            let free = !matches!((r, a), ("orders", "update") | ("reports", "read"));
            (free && m.defaults.contains(RoleKind::Teller) && !m.core.contains(RoleKind::Teller))
                .then_some((r, a, m.cap.id() as i16))
        })
        .expect("some configurable teller cell")
}

#[sqlx::test]
async fn the_reconciliation_is_empty_on_a_mirrored_org_and_finds_a_planted_difference(
    pool: PgPool,
) {
    let o = org(&pool).await;
    let b1 = branch(&pool, o).await;
    let b2 = branch(&pool, o).await;
    let _owner = user(&pool, o, "org_admin").await;
    let mgr = user(&pool, o, "branch_manager").await;
    let teller = user(&pool, o, "teller").await;
    let waiter = user(&pool, o, "waiter").await;
    let _kitchen = user(&pool, o, "kitchen").await;
    for (u, b) in [(mgr, b1), (waiter, b2)] {
        sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
            .bind(u)
            .bind(b)
            .execute(&pool)
            .await
            .unwrap();
    }
    // An allow, a deny, and a deny on a core cell (which the new model ignores
    // by design: rule 5).
    for (u, r, a, g) in [
        (teller, "reports", "read", true),
        (teller, "orders", "update", false),
        (waiter, "menu_items", "read", false),
    ] {
        sqlx::query(
            "INSERT INTO permissions (user_id, resource, action, granted)
             VALUES ($1, $2::permission_resource, $3::permission_action, $4)",
        )
        .bind(u)
        .bind(r)
        .bind(a)
        .bind(g)
        .execute(&pool)
        .await
        .unwrap();
    }
    let report = madar_rust::authz::reconcile::run(&pool, Some(o)).await.unwrap();
    let res = &report.orgs[0];
    assert_eq!(res.compared, 5);
    // 5 people x every capability x (anywhere + 2 branches).
    assert_eq!(res.cells, 5 * CAPS.len() * 3);
    assert!(res.diffs.is_empty(), "{:#?}", res.diffs);
    assert_eq!(res.old_hash, res.new_hash);

    // Plant a difference the mirror would never make: remove the teller role's
    // grant behind the trigger's back.
    let (r, a, cap) = removable_teller_cell();
    sqlx::query(
        "DELETE FROM org_role_grants g USING org_roles x
          WHERE g.org_role_id = x.id AND x.org_id = $1 AND x.key = 'teller' AND g.capability_id = $2",
    )
    .bind(o)
    .bind(cap)
    .execute(&pool)
    .await
    .unwrap();
    let report = madar_rust::authz::reconcile::run(&pool, Some(o)).await.unwrap();
    let diffs = &report.orgs[0].diffs;
    assert!(!diffs.is_empty(), "the planted {r}:{a} difference is found");
    assert!(
        diffs
            .iter()
            .all(|d| d.user_id == teller && d.old && !d.new && d.cap == CAPS.iter().find(|m| m.cap.id() as i16 == cap).unwrap().key),
        "{diffs:#?}"
    );
    assert_eq!(diffs.len(), 3, "anywhere and both branches");
}
