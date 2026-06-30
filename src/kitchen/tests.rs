use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::kitchen::stations::KitchenStation;
use crate::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn org_admin(uid: Uuid, org: Uuid) -> String {
    create_token(&secret(), uid, Some(org), UserRole::OrgAdmin, None, 24).unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1,'Org',$2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1,$2,'Branch')")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_category(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1,$2,'Cat')")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_item(pool: &PgPool, org: Uuid, category: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price) VALUES ($1,$2,$3,'I',100)")
        .bind(id).bind(org).bind(category).execute(pool).await.unwrap();
    id
}
async fn seed_station(
    pool: &PgPool,
    org: Uuid,
    branch: Uuid,
    name: &str,
    is_default: bool,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO kitchen_stations (org_id, branch_id, name, is_default) VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(org).bind(branch).bind(name).bind(is_default).fetch_one(pool).await.unwrap()
}
async fn grant(pool: &PgPool, role: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ($1::user_role, 'kitchen_stations'::permission_resource, $2::permission_action, true) ON CONFLICT DO NOTHING",
    )
    .bind(role).bind(action).execute(pool).await.unwrap();
}

/// Routing precedence (frozen at fire time): item override > category rule >
/// branch default > none.
#[sqlx::test]
async fn resolve_station_precedence(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, Some(cat)).await;
    let grill = seed_station(&pool, org, branch, "Grill", true).await; // default
    let bar = seed_station(&pool, org, branch, "Bar", false).await;

    // Category rule: cat → Grill.
    sqlx::query("INSERT INTO category_station_routes (branch_id, category_id, station_id) VALUES ($1,$2,$3)")
        .bind(branch).bind(cat).bind(grill).execute(&pool).await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    // Category rule applies (no item override yet).
    let s = crate::kitchen::resolve_station(&mut tx, branch, Some(item))
        .await
        .unwrap();
    assert_eq!(s, Some(grill), "category rule routes to Grill");

    // Item override wins.
    sqlx::query("INSERT INTO menu_item_station_routes (branch_id, menu_item_id, station_id) VALUES ($1,$2,$3)")
        .bind(branch).bind(item).bind(bar).execute(&mut *tx).await.unwrap();
    let s = crate::kitchen::resolve_station(&mut tx, branch, Some(item))
        .await
        .unwrap();
    assert_eq!(s, Some(bar), "item override beats the category rule");

    // An uncategorised, unrouted item falls to the branch default station.
    let other = seed_item(&pool, org, None).await;
    let s = crate::kitchen::resolve_station(&mut tx, branch, Some(other))
        .await
        .unwrap();
    assert_eq!(s, Some(grill), "default station catches unrouted items");
    tx.commit().await.unwrap();
}

/// Station CRUD via the API + one-default-per-branch invariant.
#[sqlx::test]
async fn station_crud_and_single_default(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(crate::kitchen::routes::configure),
    )
    .await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    grant(&pool, "org_admin", "create").await;
    grant(&pool, "org_admin", "read").await;
    let t = org_admin(Uuid::new_v4(), org);

    for (name, def) in [("Grill", true), ("Bar", true)] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/kitchen/stations")
                .insert_header(("Authorization", format!("Bearer {t}")))
                .set_json(
                    &serde_json::json!({ "branch_id": branch, "name": name, "is_default": def }),
                )
                .to_request(),
        )
        .await;
        assert!(
            resp.status().is_success(),
            "create {name}: {:?}",
            resp.status()
        );
    }

    // The second default demoted the first → exactly one default.
    let defaults: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kitchen_stations WHERE branch_id=$1 AND is_default AND deleted_at IS NULL")
        .bind(branch).fetch_one(&pool).await.unwrap();
    assert_eq!(defaults, 1);

    let list = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/kitchen/stations?branch_id={branch}"))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .to_request(),
    )
    .await;
    let stations: Vec<KitchenStation> = test::read_body_json(list).await;
    assert_eq!(stations.len(), 2);
}
