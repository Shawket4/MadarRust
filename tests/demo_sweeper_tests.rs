//! The demo sweeper (src/demo/sweeper.rs) purges an expired DEMO org with
//! everything in it.

use sqlx::PgPool;
use uuid::Uuid;

/// An expired demo org that has a user. Creating the user gives the org access
/// rows (role assignments, its roles), whose authz triggers bump the org's
/// epoch when they are deleted. Left to the org's cascade, that bump inserts an
/// epoch for an org that is already gone and fails its FK, so the purge used
/// to fail for every demo org a visitor had signed up to.
#[sqlx::test]
async fn an_expired_demo_org_with_a_user_is_purged(pool: PgPool) {
    let (org, branch, user) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug, is_demo, demo_expires_at)
              VALUES ('{org}', 'Demo', 'demo-{org}', true, now() - interval '1 day');
         INSERT INTO branches (id, org_id, name, code) VALUES ('{branch}', '{org}', 'B', 'DEMO');
         INSERT INTO users (id, org_id, name, email, password_hash, role)
              VALUES ('{user}', '{org}', 'Visitor', 'v-{user}@demo.test', 'x', 'org_admin');"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let access: i64 = sqlx::query_scalar("SELECT count(*) FROM role_assignments WHERE org_id = $1")
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(access > 0, "the fixture needs the org's access rows");

    assert_eq!(madar_rust::demo::sweeper::gc_expired(&pool).await.unwrap(), 1);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}
