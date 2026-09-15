//! Phase 2: the new tables follow every legacy write, and the two models agree.

use sqlx::PgPool;
use uuid::Uuid;

use super::shadow::{Mode, compare_all, observe_in};
use crate::errors::AppError;

async fn org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("o-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org)
        .bind(format!("B {id}"))
        .execute(pool)
        .await
        .unwrap();
    id
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

async fn seed(pool: &PgPool) {
    crate::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
}

async fn scalar(pool: &PgPool, sql: &str, id: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn the_new_model_follows_every_legacy_write(pool: PgPool) {
    seed(&pool).await;
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let owner = user(&pool, o, "org_admin").await;
    let mgr = user(&pool, o, "branch_manager").await;
    let teller = user(&pool, o, "teller").await;

    // Five system roles, grants seeded from the global defaults.
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM org_roles WHERE org_id = $1 AND is_system",
            o
        )
        .await,
        5
    );
    let owner_flag: bool = sqlx::query_scalar("SELECT is_owner FROM users WHERE id = $1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(owner_flag);

    // A branch assignment reaches the manager's role assignment.
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(mgr)
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM role_assignment_branches rab JOIN role_assignments ra ON ra.id = rab.assignment_id WHERE ra.user_id = $1 AND ra.revoked_at IS NULL", mgr).await,
        1
    );

    // A legacy override becomes a user override, and its removal revokes it.
    sqlx::query("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, 'orders', 'delete', false)")
        .bind(teller)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM user_overrides WHERE user_id = $1 AND revoked_at IS NULL AND effect = 'deny'", teller).await, 1);
    sqlx::query("UPDATE permissions SET granted = true WHERE user_id = $1")
        .bind(teller)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM user_overrides WHERE user_id = $1 AND revoked_at IS NULL AND effect = 'allow'", teller).await, 1);
    sqlx::query("DELETE FROM permissions WHERE user_id = $1")
        .bind(teller)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM user_overrides WHERE user_id = $1 AND revoked_at IS NULL",
            teller
        )
        .await,
        0
    );

    // A role change moves the system assignment.
    sqlx::query("UPDATE users SET role = 'waiter' WHERE id = $1")
        .bind(teller)
        .execute(&pool)
        .await
        .unwrap();
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT r.kind::text FROM role_assignments ra JOIN org_roles r ON r.id = ra.org_role_id
          WHERE ra.user_id = $1 AND ra.revoked_at IS NULL",
    )
    .bind(teller)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(kinds, vec!["waiter".to_string()]);

    // A global default change reaches the org's system role.
    let before = scalar(&pool, "SELECT COUNT(*) FROM org_role_grants g JOIN org_roles r ON r.id = g.org_role_id WHERE r.org_id = $1 AND r.key = 'waiter'", o).await;
    sqlx::query("INSERT INTO role_permissions (role, resource, action, granted) VALUES ('waiter', 'refunds', 'read', true) ON CONFLICT (role, resource, action) DO UPDATE SET granted = true")
        .execute(&pool)
        .await
        .unwrap();
    let after = scalar(&pool, "SELECT COUNT(*) FROM org_role_grants g JOIN org_roles r ON r.id = g.org_role_id WHERE r.org_id = $1 AND r.key = 'waiter'", o).await;
    assert_eq!(after, before + 1);

    // Every write bumped the epoch.
    assert!(scalar(&pool, "SELECT epoch FROM authz_epoch WHERE org_id = $1", o).await > 1);
    let _ = owner;
}

#[sqlx::test]
async fn both_models_agree_for_every_person_and_cell(pool: PgPool) {
    seed(&pool).await;
    let o = org(&pool).await;
    let people = [
        user(&pool, o, "org_admin").await,
        user(&pool, o, "branch_manager").await,
        user(&pool, o, "teller").await,
        user(&pool, o, "waiter").await,
        user(&pool, o, "kitchen").await,
    ];
    // Overrides both ways on non-core cells, the shapes prod holds.
    for (who, r, a, g) in [
        (people[2], "orders", "delete", false),
        (people[2], "reports", "read", true),
        (people[1], "payroll", "read", true),
        (people[3], "open_tickets", "delete", false),
        (people[4], "inventory", "read", true),
        (people[0], "reports", "update", true),
    ] {
        sqlx::query("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, $2::permission_resource, $3::permission_action, $4)")
            .bind(who)
            .bind(r)
            .bind(a)
            .bind(g)
            .execute(&pool)
            .await
            .unwrap();
    }
    let mismatches = compare_all(&pool).await.unwrap();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

#[sqlx::test]
async fn a_core_deny_is_the_one_explained_difference_and_enforce_serves_the_new_answer(
    pool: PgPool,
) {
    seed(&pool).await;
    let o = org(&pool).await;
    let teller = user(&pool, o, "teller").await;
    sqlx::query("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, 'orders', 'create', false)")
        .bind(teller)
        .execute(&pool)
        .await
        .unwrap();
    let m = compare_all(&pool).await.unwrap();
    assert_eq!(m.len(), 1);
    assert!(m[0].explained.is_some());

    let mut conn = pool.acquire().await.unwrap();
    let legacy = Err(AppError::Forbidden("denied".into()));
    assert!(
        observe_in(Mode::Shadow, &mut conn, teller, "orders", "create", legacy)
            .await
            .is_err()
    );
    let legacy = Err(AppError::Forbidden("denied".into()));
    assert!(
        observe_in(Mode::Enforce, &mut conn, teller, "orders", "create", legacy)
            .await
            .is_ok()
    );
    assert!(
        observe_in(Mode::Enforce, &mut conn, teller, "payroll", "read", Ok(()))
            .await
            .is_err()
    );
}
