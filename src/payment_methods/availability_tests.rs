//! B3 tests: payment method availability (TILLS_CONTRACT.md §8 B3, §2.3).
use actix_web::{App, http::StatusCode, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::models::UserRole;
use crate::payment_methods::{availability::*, routes};

struct Fx {
    org: Uuid,
    branch: Uuid,
    teller: Uuid,
    admin: Uuid,
    device: Uuid,
    cash: Uuid,
    card: Uuid,
    cib: Uuid,
    inactive: Uuid,
}

async fn method(pool: &PgPool, org: Uuid, name: &str, is_cash: bool, active: bool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active)
         VALUES ($1, $2, '{}', 'c', 'i', $3, $4) RETURNING id",
    )
    .bind(org).bind(name).bind(is_cash).bind(active).fetch_one(pool).await.unwrap()
}

async fn user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, $3, $3, 'h', $4::user_role)")
        .bind(id).bind(org).bind(format!("{id}@t.com")).bind(role).execute(pool).await.unwrap();
    id
}

async fn fixture(pool: &PgPool) -> Fx {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(org)
        .bind(format!("o-{org}"))
        .execute(pool)
        .await
        .unwrap();
    let branch = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(branch)
        .bind(org)
        .bind(format!("b-{branch}"))
        .execute(pool)
        .await
        .unwrap();
    let device = Uuid::new_v4();
    sqlx::query("INSERT INTO devices (id, org_id, branch_id, code) VALUES ($1, $2, $3, '36B')")
        .bind(device)
        .bind(org)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    Fx {
        org,
        branch,
        teller: user(pool, org, "teller").await,
        admin: user(pool, org, "org_admin").await,
        device,
        cash: method(pool, org, "cash", true, true).await,
        card: method(pool, org, "card", false, true).await,
        cib: method(pool, org, "CIB – counter", false, true).await,
        inactive: method(pool, org, "old", false, false).await,
    }
}

fn list(ids: &[Uuid]) -> AllowList {
    AllowList {
        restricted: true,
        payment_method_ids: ids.to_vec(),
    }
}

async fn names(pool: &PgPool, fx: &Fx, user: Option<Uuid>, device: Option<Uuid>) -> Vec<String> {
    effective_method_names(pool, fx.org, fx.branch, user, device)
        .await
        .unwrap()
}

#[sqlx::test]
async fn no_rows_means_unrestricted(pool: PgPool) {
    let fx = fixture(&pool).await;
    let all = vec!["cash".to_string(), "card".into(), "CIB – counter".into()];
    assert_eq!(names(&pool, &fx, None, None).await, all);
    assert_eq!(
        names(&pool, &fx, Some(fx.teller), Some(fx.device)).await,
        all
    );
    assert!(
        !is_method_available(&pool, fx.org, fx.branch, None, None, "old")
            .await
            .unwrap()
    );
    assert!(
        !is_method_available(&pool, fx.org, fx.branch, None, None, "nope")
            .await
            .unwrap()
    );
    let a = load_availability(&pool, fx.org, fx.branch).await.unwrap();
    assert!(!a.branch.restricted && a.users.is_empty() && a.devices.is_empty());
    let _ = (fx.cash, fx.card, fx.cib, fx.inactive, fx.admin);
}

#[sqlx::test]
async fn effective_methods_intersection_branch_user_device(pool: PgPool) {
    let fx = fixture(&pool).await;
    replace_list(
        &pool,
        fx.org,
        Owner::Branch,
        fx.branch,
        &list(&[fx.cash, fx.card, fx.cib, fx.inactive]),
    )
    .await
    .unwrap();
    replace_list(
        &pool,
        fx.org,
        Owner::User,
        fx.teller,
        &list(&[fx.cash, fx.cib, fx.card]),
    )
    .await
    .unwrap();
    replace_list(
        &pool,
        fx.org,
        Owner::Device,
        fx.device,
        &list(&[fx.cash, fx.cib]),
    )
    .await
    .unwrap();

    assert_eq!(
        names(&pool, &fx, None, None).await,
        ["cash", "card", "CIB – counter"]
    ); // inactive never
    assert_eq!(
        names(&pool, &fx, Some(fx.teller), Some(fx.device)).await,
        ["cash", "CIB – counter"]
    );
    replace_list(
        &pool,
        fx.org,
        Owner::User,
        fx.teller,
        &list(&[fx.card, fx.cash]),
    )
    .await
    .unwrap();
    assert_eq!(
        names(&pool, &fx, Some(fx.teller), Some(fx.device)).await,
        ["cash"]
    );
    assert_eq!(
        names(&pool, &fx, Some(fx.teller), None).await,
        ["cash", "card"]
    );
    assert!(
        !is_method_available(
            &pool,
            fx.org,
            fx.branch,
            Some(fx.teller),
            Some(fx.device),
            "card"
        )
        .await
        .unwrap()
    );
    assert!(
        is_method_available(&pool, fx.org, fx.branch, Some(fx.teller), None, "card")
            .await
            .unwrap()
    );
    // Another teller with no rows is unrestricted by user.
    let other = user(&pool, fx.org, "teller").await;
    assert_eq!(
        names(&pool, &fx, Some(other), Some(fx.device)).await,
        ["cash", "CIB – counter"]
    );

    let a = load_availability(&pool, fx.org, fx.branch).await.unwrap();
    assert!(a.branch.restricted);
    assert_eq!(a.users.len(), 1);
    assert_eq!(a.devices[0].device_id, fx.device);

    // restricted=false clears; idempotent.
    let off = AllowList {
        restricted: false,
        payment_method_ids: vec![],
    };
    for _ in 0..2 {
        let r = replace_list(&pool, fx.org, Owner::Device, fx.device, &off)
            .await
            .unwrap();
        assert_eq!(r, off);
    }
    assert_eq!(
        names(&pool, &fx, Some(fx.teller), Some(fx.device)).await,
        ["cash", "card"]
    );
}

#[sqlx::test]
async fn cross_org_ids_rejected(pool: PgPool) {
    let fx = fixture(&pool).await;
    let other = fixture(&pool).await;
    let e = replace_list(
        &pool,
        fx.org,
        Owner::Branch,
        fx.branch,
        &list(&[other.card]),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, crate::errors::AppError::BadRequest(_)));
    let e = replace_list(&pool, fx.org, Owner::User, other.teller, &list(&[fx.card]))
        .await
        .unwrap_err();
    assert!(matches!(e, crate::errors::AppError::NotFound(_)));
}

fn token(user: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&JwtSecret("secret".into()), user, Some(org), role, None, 24).unwrap()
}

#[sqlx::test]
async fn put_restricted_empty_is_400_and_permissions(pool: PgPool) {
    let fx = fixture(&pool).await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("secret".into())))
            .configure(routes::configure),
    )
    .await;
    let admin = token(fx.admin, fx.org, UserRole::OrgAdmin);
    let teller = token(fx.teller, fx.org, UserRole::Teller);
    let put = |tok: &str, path: String, body: serde_json::Value| {
        test::TestRequest::put()
            .uri(&path)
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .set_json(body)
            .to_request()
    };

    let r = test::call_service(
        &app,
        put(
            &admin,
            format!("/payment-methods/availability/branches/{}", fx.branch),
            serde_json::json!({"restricted": true, "payment_method_ids": []}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = test::read_body_json(r).await;
    assert!(body.to_string().contains(CODE_EMPTY_ALLOW_LIST));

    let r = test::call_service(
        &app,
        put(
            &teller,
            format!("/payment-methods/availability/users/{}", fx.teller),
            serde_json::json!({"restricted": true, "payment_method_ids": [fx.cash]}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    let r = test::call_service(
        &app,
        put(
            &admin,
            format!("/payment-methods/availability/devices/{}", fx.device),
            serde_json::json!({"restricted": true, "payment_method_ids": [fx.cib, fx.cib]}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let stored: AllowList = test::read_body_json(r).await;
    assert_eq!(stored, list(&[fx.cib]));

    let get = |uri: String| {
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(("Authorization", format!("Bearer {teller}")))
            .to_request()
    };
    let r = test::call_service(
        &app,
        get(format!(
            "/payment-methods/effective?branch_id={}&device_id={}",
            fx.branch, fx.device
        )),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let v: serde_json::Value = test::read_body_json(r).await;
    assert_eq!(v.as_array().unwrap().len(), 1);
    let r = test::call_service(
        &app,
        get(format!(
            "/payment-methods/availability?branch_id={}",
            fx.branch
        )),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let a: PaymentMethodAvailability = test::read_body_json(r).await;
    assert_eq!(a.devices.len(), 1);
}
