//! Manual customers: the dashboard surface, the till's queued ops, and the feed.

use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::realtime::hub::BranchEventHub;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch')")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'h', $5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role}-{id}"))
    .bind(format!("{id}@t.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::customers::routes::configure)
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    };
}

async fn call(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    req: test::TestRequest,
    bearer: &str,
) -> (StatusCode, Value) {
    let resp = test::call_service(
        app,
        req.insert_header(("Authorization", format!("Bearer {bearer}")))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn order_row(pool: &PgPool, branch: Uuid, teller: Uuid, total: i32) -> Uuid {
    let till: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO orders (branch_id, teller_id, till_id, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) \
         VALUES ($1, $2, $3, $4, 0, $4, 'completed', 1, 'cash', $5) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .bind(till)
    .bind(total)
    .bind(format!("R-{}", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap()
}

#[::core::prelude::v1::test]
fn phone_keys_fold_the_country_code() {
    // The key is the canonical form now (`crate::phone`): E.164 digits, no plus.
    use madar_rust::customers::handlers::phone_key;
    assert_eq!(
        phone_key("+20 100 123 4567").as_deref(),
        Some("201001234567")
    );
    assert_eq!(phone_key("00201001234567").as_deref(), Some("201001234567"));
    assert_eq!(phone_key("0100-123-4567").as_deref(), Some("201001234567"));
    assert_eq!(phone_key(" - "), None);
}

/// The owner manages customers in the dashboard; a phone is one person, and
/// only `customers.view` reads the list at all.
#[sqlx::test]
async fn the_dashboard_lists_edits_and_refuses_a_second_holder_of_a_phone(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let owner = seed_user(&pool, org, "org_admin").await;
    let teller = seed_user(&pool, org, "teller").await;
    let kitchen = seed_user(&pool, org, "kitchen").await;
    let t_owner = token(owner, org, UserRole::OrgAdmin);

    let (s, c) = call(
        &app,
        test::TestRequest::post()
            .uri("/customers")
            .set_json(json!({"name": " Mona ", "phone": "+20 100 123 4567", "notes": "oat milk"})),
        &t_owner,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    assert_eq!(c["customer"]["name"], "Mona");
    let mona = c["customer"]["id"].as_str().unwrap().to_string();

    let (s, c) = call(
        &app,
        test::TestRequest::post()
            .uri("/customers")
            .set_json(json!({"name": "Mona again", "phone": "01001234567"})),
        &t_owner,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(c["code"], "CUSTOMER_PHONE_EXISTS");

    let (s, list) = call(
        &app,
        test::TestRequest::get().uri("/customers?q=1234567"),
        &token(teller, org, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1, "found by phone digits");

    let (s, _) = call(
        &app,
        test::TestRequest::get().uri("/customers"),
        &token(kitchen, org, UserRole::Kitchen),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "no customers.view, no customer data"
    );

    let (s, _) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/customers/{mona}"))
            .set_json(json!({"name": "Mona S."})),
        &token(teller, org, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "editing is customers.edit");

    let (s, c) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/customers/{mona}"))
            .set_json(json!({"name": "Mona S.", "notes": ""})),
        &t_owner,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["customer"]["name"], "Mona S.");
    assert!(c["customer"]["notes"].is_null(), "an empty note clears it");
    assert_eq!(c["customer"]["phone"], "+20 100 123 4567");

    // Erasing is customers.erase: the owner's alone by default, never a
    // manager's, even though a manager edits and merges.
    let manager = seed_user(&pool, org, "branch_manager").await;
    let (s, _) = call(
        &app,
        test::TestRequest::post().uri(&format!("/customers/{mona}/erase")),
        &token(manager, org, UserRole::BranchManager),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "a manager may not erase a customer"
    );

    let (s, _) = call(
        &app,
        test::TestRequest::post().uri(&format!("/customers/{mona}/erase")),
        &t_owner,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (name, phone): (String, Option<String>) =
        sqlx::query_as("SELECT name, phone FROM customers WHERE id = $1::uuid")
            .bind(&mona)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (name.as_str(), phone),
        ("", None),
        "PDPL erase keeps no PII"
    );
}

/// Offline, two tills add the same phone. The second create is stored merged
/// into the first, an order naming the second lands on the first, and a
/// dashboard merge moves a duplicate's history.
#[sqlx::test]
async fn queued_customers_dedupe_by_phone_and_merges_move_history(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let owner = seed_user(&pool, org, "org_admin").await;
    let teller = seed_user(&pool, org, "teller").await;
    let kitchen = seed_user(&pool, org, "kitchen").await;
    let bearer = token(teller, org, UserRole::Teller);
    let create = |actor: Uuid, id: Uuid, name: &str, phone: Option<&str>| {
        json!({"op": "create_customer", "teller_id": actor,
               "request": {"id": id, "name": name, "phone": phone, "branch_id": branch}})
    };
    let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

    let (s, r) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(create(teller, a, "Omar", Some("0111 222 3333"))),
        &bearer,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["status"], "created");
    let (_, r) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(create(teller, a, "Omar", Some("0111 222 3333"))),
        &bearer,
    )
    .await;
    assert_eq!(r["status"], "existing", "a re-flushed create is idempotent");

    let (_, r) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(create(teller, b, "Omar K", Some("+201112223333"))),
        &bearer,
    )
    .await;
    assert_eq!(r["status"], "merged");
    assert_eq!(r["merged_into"], a.to_string());

    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(create(kitchen, Uuid::new_v4(), "X", None)),
        &bearer,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "kitchen does not hold customers.create"
    );

    // A sale attached to the merged id lands on the live customer.
    let o1 = order_row(&pool, branch, teller, 5000).await;
    let (s, r) = call(
        &app,
        test::TestRequest::post().uri("/sync/replay").set_json(
            json!({"op": "attach_customer", "teller_id": teller, "order_id": o1, "customer_id": b}),
        ),
        &bearer,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["customer_id"], a.to_string());

    // A plain duplicate the dashboard merges.
    call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(create(teller, c, "Omar Khaled", None)),
        &bearer,
    )
    .await;
    let o2 = order_row(&pool, branch, teller, 3000).await;
    let mut conn = pool.acquire().await.unwrap();
    madar_rust::customers::handlers::attach_to_order(&mut conn, org, o2, Some(c))
        .await
        .unwrap();
    drop(conn);

    let t_owner = token(owner, org, UserRole::OrgAdmin);
    let (s, d) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/customers/{c}/merge"))
            .set_json(json!({"into": a})),
        &t_owner,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["customer"]["orders_count"], 2);
    assert_eq!(d["customer"]["total_spent"], 8000);
    assert_eq!(d["recent_orders"].as_array().unwrap().len(), 2);
    assert_eq!(d["merged_from"].as_array().unwrap().len(), 2);

    let (s, d) = call(
        &app,
        test::TestRequest::get().uri(&format!("/customers/{c}")),
        &t_owner,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(d["customer"]["id"], a.to_string(), "a merged id resolves");
    assert_eq!(d["resolved_from"], c.to_string());

    // The feed lists the live customer at the branch and retires the merged ones.
    let ops: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT entity_id, op FROM sync_changes WHERE branch_id = $1 AND type = 'customer'",
    )
    .bind(branch)
    .fetch_all(&pool)
    .await
    .unwrap();
    let op_of = |id: Uuid| ops.iter().find(|(e, _)| *e == id).map(|(_, o)| o.as_str());
    assert_eq!(op_of(a), Some("upsert"));
    assert_eq!(op_of(b), Some("delete"));
    assert_eq!(op_of(c), Some("delete"));
}

/// The till's usual path: the sale itself names the customer.
#[sqlx::test]
async fn a_queued_sale_carries_its_customer(pool: PgPool) {
    let app = app!(pool);
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let item: Uuid = sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, name, base_price) VALUES ($1, 'Latte', 500) RETURNING id",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let till: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(&pool)
    .await
    .unwrap();
    let bearer = token(teller, org, UserRole::Teller);
    let customer = Uuid::new_v4();
    let (s, r) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(json!({
                "op": "create_customer", "teller_id": teller,
                "request": {"id": customer, "name": "Hana", "branch_id": branch}
            })),
        &bearer,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, r) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(json!({
                "op": "create_order", "teller_id": teller,
                "request": {
                    "branch_id": branch, "till_id": till, "payment_method": "cash",
                    "items": [{"menu_item_id": item, "quantity": 1}],
                    "customer_id": customer
                }
            })),
        &bearer,
    )
    .await;
    assert!(s.is_success(), "{s} {r}");
    let on_order: Option<Uuid> =
        sqlx::query_scalar("SELECT customer_id FROM orders WHERE branch_id = $1")
            .bind(branch)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(on_order, Some(customer));
}
