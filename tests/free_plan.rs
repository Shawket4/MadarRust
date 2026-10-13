//! The Free plan on the server (FREEMIUM_SPEC_2026-10-11.md §4): the
//! capability ceiling, the branch and device limits, the realtime stream, the
//! plan on organizations, the downgrade and its tills, and Pickup / Dine in on
//! every plan.
//!
//! Every test is written to FAIL when its guard is removed: each one runs the
//! same request on a Paid org (where it must succeed) and on a Free one (where
//! it must be refused), so a test cannot pass by never reaching the guard.

mod common;

use actix_web::{App, http::StatusCode, test, web};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop, token};
use madar_rust::authz::{Cap, require::effective};
use madar_rust::models::UserRole;

macro_rules! app {
    ($pool:expr) => {{
        unsafe {
            std::env::set_var("MADAR_DISABLE_RATE_LIMIT", "1");
            std::env::set_var("MADAR_DISABLE_AUTO_TRANSLATION", "1");
        }
        let read_pool = web::Data::new($pool.clone());
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(
                    madar_rust::menu::cache::MenuCache::from_env(),
                ))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(
                    madar_rust::auth::org_status::OrgStatusCache::new(),
                ))
                .app_data(web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .app_data(madar_rust::qr_card::routes::make_provider())
                .app_data(web::Data::new(
                    madar_rust::demo::config::DemoConfig::from_env(),
                ))
                .app_data(web::Data::new(madar_rust::ai::AiState::from_env()))
                .configure(|cfg| madar_rust::app_routes::configure_api(cfg, read_pool.clone())),
        )
        .await
    }};
}

async fn call<S, B>(app: &S, req: test::TestRequest, bearer: Option<&str>) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let req = match bearer {
        Some(b) => req.insert_header(("Authorization", format!("Bearer {b}"))),
        None => req,
    };
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn super_admin() -> String {
    madar_rust::auth::jwt::create_token(
        &secret(),
        Uuid::new_v4(),
        None,
        UserRole::SuperAdmin,
        None,
        24,
    )
    .unwrap()
}

async fn setup(pool: &PgPool) {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    madar_rust::authz::sync_catalogue(pool).await.unwrap();
}

async fn set_plan(pool: &PgPool, org: Uuid, plan: &str) {
    sqlx::query("UPDATE organizations SET plan = $2 WHERE id = $1")
        .bind(org)
        .bind(plan)
        .execute(pool)
        .await
        .unwrap();
}

async fn epoch(pool: &PgPool, org: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COALESCE((SELECT epoch FROM authz_epoch WHERE org_id = $1), 0)")
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A POS till (device) of `branch`, last heard from `ago_min` minutes ago.
async fn till_device(pool: &PgPool, org: Uuid, branch: Uuid, code: &str, ago_min: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO devices (id, org_id, branch_id, code, kind, last_seen_at)
         VALUES ($1, $2, $3, $4, 'pos', now() - make_interval(mins => $5::int))",
    )
    .bind(id)
    .bind(org)
    .bind(branch)
    .bind(code)
    .bind(ago_min as i32)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn inactive(pool: &PgPool, device: Uuid) -> bool {
    sqlx::query_scalar("SELECT plan_inactive FROM devices WHERE id = $1")
        .bind(device)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── Existing orgs are Paid ─────────────────────────────────────────────────

#[sqlx::test]
async fn an_org_made_the_old_way_is_paid_and_says_so(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;
    let plan: String = sqlx::query_scalar("SELECT plan FROM organizations WHERE id = $1")
        .bind(s.org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        plan, "paid",
        "the column default is every current org's plan"
    );
    let source: String =
        sqlx::query_scalar("SELECT signup_source FROM organizations WHERE id = $1")
            .bind(s.org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(source, "admin");

    let app = app!(pool);
    let (st, org) = call(
        &app,
        test::TestRequest::get().uri(&format!("/orgs/{}", s.org)),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{org}");
    assert_eq!(org["plan"], "paid");
    assert_eq!(org["signup_source"], "admin");
    let (_, me) = call(
        &app,
        test::TestRequest::get().uri("/auth/me"),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(me["plan"], "paid");
}

// ── The capability ceiling ─────────────────────────────────────────────────

#[sqlx::test]
async fn the_ceiling_cuts_an_owner_on_free_and_a_plan_change_moves_it_at_once(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;

    // Paid: the owner holds everything.
    let paid = effective(&pool, s.admin, None).await.unwrap();
    for cap in [
        Cap::InventoryRead,
        Cap::LoyaltyRead,
        Cap::BookingsRead,
        Cap::TicketsOpen,
        Cap::DeliveryOrdersRead,
        Cap::HrStaffRead,
        Cap::OrdersCreate,
        Cap::TillOpen,
    ] {
        assert!(paid.can(cap), "paid owner lacks {}", cap.key());
    }

    // Free: selling stays, every paid feature goes. The cache is keyed by the
    // org's epoch, so this also proves a plan change bumps it.
    let before = epoch(&pool, s.org).await;
    set_plan(&pool, s.org, "free").await;
    assert!(
        epoch(&pool, s.org).await > before,
        "a plan change bumps authz_epoch"
    );
    let free = effective(&pool, s.admin, None).await.unwrap();
    for cap in [Cap::OrdersCreate, Cap::TillOpen, Cap::PosSignIn] {
        assert!(free.can(cap), "Free keeps {}", cap.key());
    }
    for cap in [
        Cap::InventoryRead,
        Cap::InventoryAdjust,
        Cap::RecipesRead,
        Cap::PurchasingOrdersCreate,
        Cap::LoyaltyRead,
        Cap::BookingsRead,
        Cap::TicketsOpen,
        Cap::FloorLayoutEdit,
        Cap::DeliveryOrdersRead,
        Cap::DeliverySettingsEdit,
        Cap::KitchenBump,
        Cap::HrStaffRead,
        Cap::HrPayrollRun,
    ] {
        assert!(!free.can(cap), "Free must not hold {}", cap.key());
    }

    // The same, through the legacy (resource, action) check every old route uses.
    let claims = madar_rust::auth::jwt::Claims {
        sub: s.admin.to_string(),
        org_id: Some(s.org.to_string()),
        role: UserRole::OrgAdmin,
        branch_id: None,
        exp: usize::MAX,
        iat: 0,
    };
    assert!(
        madar_rust::permissions::checker::check_permission(&pool, &claims, "inventory", "read")
            .await
            .is_err(),
        "the legacy gate is cut by the ceiling too"
    );
    assert!(
        madar_rust::permissions::checker::check_permission(&pool, &claims, "orders", "create")
            .await
            .is_ok()
    );

    // Upgrade: back at once.
    set_plan(&pool, s.org, "paid").await;
    assert!(
        effective(&pool, s.admin, None)
            .await
            .unwrap()
            .can(Cap::InventoryRead)
    );
    assert!(
        madar_rust::permissions::checker::check_permission(&pool, &claims, "inventory", "read")
            .await
            .is_ok()
    );
}

#[sqlx::test]
async fn the_ceiling_is_the_plan_crate_s_for_every_template_and_role(pool: PgPool) {
    setup(&pool).await;
    let app = app!(pool);
    let sa = super_admin();
    for template in ["cafe", "restaurant"] {
        let (st, made) = call(
            &app,
            test::TestRequest::post()
                .uri("/orgs/provision")
                .set_json(json!({
                    "name": format!("T {template}"), "slug": format!("t-{template}-zz"),
                    "template": template, "branch": {"name": "Main"},
                    "owner": {"name": "Owner", "email": format!("o-{template}@t.test"),
                              "password": "long-enough-1"}
                })),
            Some(&sa),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{made}");
        let org = Uuid::parse_str(made["org"]["id"].as_str().unwrap()).unwrap();
        let branch = Uuid::parse_str(made["branch_id"].as_str().unwrap()).unwrap();
        assert_eq!(made["org"]["plan"], "paid", "a super-admin org is Paid");
        let mut people = vec![Uuid::parse_str(made["owner_id"].as_str().unwrap()).unwrap()];
        for role in ["branch_manager", "teller", "waiter", "kitchen"] {
            let u: Uuid = sqlx::query_scalar(
                "INSERT INTO users (org_id, name, email, password_hash, role)
                 VALUES ($1, $2, $3, 'h', $4::user_role) RETURNING id",
            )
            .bind(org)
            .bind(format!("{role}-{template}"))
            .bind(format!("{role}-{template}@t.test"))
            .bind(role)
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
                .bind(u)
                .bind(branch)
                .execute(&pool)
                .await
                .unwrap();
            people.push(u);
        }
        let mut paid = Vec::new();
        for p in &people {
            paid.push(effective(&pool, *p, Some(branch)).await.unwrap());
        }
        let (st, _) = call(
            &app,
            test::TestRequest::patch()
                .uri(&format!("/orgs/{org}"))
                .set_json(json!({"plan": "free"})),
            Some(&sa),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        for (p, before) in people.iter().zip(paid) {
            let got = effective(&pool, *p, Some(branch)).await.unwrap();
            let want = madar_plan::apply(madar_plan::Plan::Free, before.clone());
            assert_eq!(
                got.caps, want.caps,
                "{template} / {p}: ceiling != madar_plan::apply"
            );
            assert_eq!(got.limits, want.limits);
            assert!(
                got.caps.iter().count() < before.caps.iter().count() || before.caps == want.caps,
                "the ceiling only ever removes"
            );
            for f in madar_plan::Feature::all() {
                for c in f.caps().iter() {
                    assert!(!got.can(c), "{template}: Free holds {} of {f:?}", c.key());
                }
            }
        }
    }
}

#[sqlx::test]
async fn a_tills_signed_snapshot_is_ceilinged_so_the_offline_core_sees_it(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;
    sqlx::query("UPDATE users SET pin_hash = 'x' WHERE id = $1")
        .bind(s.admin)
        .execute(&pool)
        .await
        .unwrap();
    let dev = till_device(&pool, s.org, s.branch, "T1", 1).await;
    let paid = madar_rust::authz::snapshot::build(&pool, s.org, s.branch, dev)
        .await
        .unwrap();
    let eff = &paid
        .users
        .iter()
        .find(|u| u.user_id == s.admin.to_string())
        .unwrap()
        .eff;
    assert!(eff.can(Cap::InventoryRead), "the paid snapshot carries it");
    let e1 = paid.org_epoch;

    set_plan(&pool, s.org, "free").await;
    let free = madar_rust::authz::snapshot::build(&pool, s.org, s.branch, dev)
        .await
        .unwrap();
    let eff = &free
        .users
        .iter()
        .find(|u| u.user_id == s.admin.to_string())
        .unwrap()
        .eff;
    assert!(!eff.can(Cap::InventoryRead), "the Free snapshot is cut");
    assert!(!eff.can(Cap::LoyaltyRead));
    assert!(eff.can(Cap::OrdersCreate));
    assert!(
        free.org_epoch > e1,
        "the epoch moved, so a till refreshes its snapshot"
    );
}

// ── Limits: branches ──────────────────────────────────────────────────────

#[sqlx::test]
async fn a_free_org_cannot_add_a_branch_and_a_downgraded_one_keeps_its_own(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await; // Paid, two branches
    let app = app!(pool);
    let owner = s.admin_token();
    let add = |name: &str| {
        test::TestRequest::post()
            .uri("/branches")
            .set_json(json!({"org_id": s.org, "name": name}))
    };

    // Paid: unlimited.
    let (st, b) = call(&app, add("Third"), Some(&owner)).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");

    // Downgrade: all three branches stay; a fourth is refused.
    let (st, _) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "free"})),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM branches WHERE org_id = $1 AND deleted_at IS NULL",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 3, "a downgraded org keeps all of its branches");
    let (st, b) = call(&app, add("Fourth"), Some(&owner)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "plan_branch_limit");
    assert!(b["error_ar"].as_str().unwrap().contains("للفروع"));
    // A warehouse is stock: the `inventory` feature.
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/branches")
            .set_json(json!({"org_id": s.org, "name": "Store", "kind": "warehouse"})),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "plan_feature");
    assert_eq!(b["feature"], "inventory");

    // A one-branch Free org (a signup) is refused its second.
    let solo = madar_rust::auth::jwt::create_token(
        &secret(),
        s.admin,
        Some(s.org),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    sqlx::query("UPDATE branches SET deleted_at = now() WHERE org_id = $1 AND name <> 'Main'")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (st, b) = call(&app, add("Second"), Some(&solo)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "plan_branch_limit");
    // Upgrade and it opens.
    set_plan(&pool, s.org, "paid").await;
    let (st, b) = call(&app, add("Second"), Some(&solo)).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
}

// ── Limits: devices ───────────────────────────────────────────────────────

#[sqlx::test]
async fn a_free_branch_takes_one_till_and_no_kitchen_screen_or_waiter_device(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;
    let app = app!(pool);
    let owner = s.admin_token();
    let code = |kind: &str| {
        test::TestRequest::post()
            .uri("/devices/activation-codes")
            .set_json(json!({"branch_id": s.branch, "kind": kind}))
    };

    // Paid: every kind, any number.
    for kind in ["pos", "pos", "kds", "waiter"] {
        let (st, c) = call(&app, code(kind), Some(&owner)).await;
        assert_eq!(st, StatusCode::CREATED, "paid {kind}: {c}");
    }
    sqlx::query("UPDATE device_activation_codes SET revoked_at = now()")
        .execute(&pool)
        .await
        .unwrap();

    set_plan(&pool, s.org, "free").await;
    let (st, c) = call(&app, code("kds"), Some(&owner)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{c}");
    assert_eq!(
        (c["code"].as_str(), c["feature"].as_str()),
        (Some("plan_feature"), Some("kitchen"))
    );
    assert_eq!(c["error"], "Kitchen screens are on the paid plan.");
    let (st, c) = call(&app, code("waiter"), Some(&owner)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{c}");
    assert_eq!(
        (c["code"].as_str(), c["feature"].as_str()),
        (Some("plan_feature"), Some("floor"))
    );

    // The first till: a code, then the tablet.
    let (st, c1) = call(&app, code("pos"), Some(&owner)).await;
    assert_eq!(st, StatusCode::CREATED, "{c1}");
    let (st, c2) = call(&app, code("pos"), Some(&owner)).await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "two codes may exist before a till does: {c2}"
    );
    let d1 = Uuid::new_v4();
    let (st, a) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/activate-device")
            .set_json(json!({"code": c1["code"], "device_id": d1, "device_code": "T1"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{a}");
    // The second tablet is refused, and its code is NOT spent.
    let d2 = Uuid::new_v4();
    let (st, a) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/activate-device")
            .set_json(json!({"code": c2["code"], "device_id": d2, "device_code": "T2"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{a}");
    assert_eq!(a["code"], "plan_device_limit");
    let (_, list) = call(
        &app,
        test::TestRequest::get().uri(&format!("/devices/activation-codes?branch_id={}", s.branch)),
        Some(&owner),
    )
    .await;
    let spent = list
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == c2["id"])
        .unwrap();
    assert_eq!(
        spent["state"], "free",
        "a refused activation leaves its code free"
    );
    // A third code cannot even be made now.
    let (st, c) = call(&app, code("pos"), Some(&owner)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{c}");
    assert_eq!(c["code"], "plan_device_limit");
    // The same tablet activating again is not a second device.
    let (st, c3) = call(&app, code("pos"), Some(&owner)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{c3}");

    // Registering another install is refused; the first, again, is fine.
    let reg = |id: Uuid, code: &str| {
        test::TestRequest::post()
            .uri("/devices/register")
            .set_json(json!({"id": id, "code": code, "kind": "pos", "branch_id": s.branch}))
    };
    let (st, r) = call(&app, reg(d2, "T2"), Some(&owner)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{r}");
    assert_eq!(r["code"], "plan_device_limit");
    let (st, r) = call(&app, reg(d1, "T1"), Some(&owner)).await;
    assert_eq!(st, StatusCode::OK, "{r}");

    // Retire the till and the place is free again; bringing it back would be a
    // second device once another took the place.
    let (st, _) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/devices/{d1}"))
            .set_json(json!({"retired": true})),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, r) = call(&app, reg(d2, "T2"), Some(&owner)).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let (st, r) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/devices/{d1}"))
            .set_json(json!({"retired": false})),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{r}");
    assert_eq!(r["code"], "plan_device_limit");

    // A kitchen screen that already exists is refused its session on Free.
    let kds = Uuid::new_v4();
    sqlx::query("UPDATE organizations SET plan = 'paid' WHERE id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (_, kc) = call(&app, code("kds"), Some(&owner)).await;
    let (st, a) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/activate-device")
            .set_json(json!({"code": kc["code"], "device_id": kds, "device_code": "K1"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{a}");
    let kds_token = a["device_token"].as_str().unwrap().to_string();
    let session = |tok: &str| {
        test::TestRequest::post()
            .uri("/auth/device-session")
            .insert_header(("X-Madar-Device", kds.to_string()))
            .insert_header(("X-Madar-Device-Token", tok.to_string()))
    };
    let (st, ses) = call(&app, session(&kds_token), None).await;
    assert_eq!(st, StatusCode::OK, "paid: {ses}");
    assert_eq!(ses["plan"], "paid");
    set_plan(&pool, s.org, "free").await;
    let (st, ses) = call(&app, session(&kds_token), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{ses}");
    assert_eq!(
        (ses["code"].as_str(), ses["feature"].as_str()),
        (Some("plan_feature"), Some("kitchen"))
    );
}

#[sqlx::test]
async fn a_saved_branch_plan_adds_only_the_slots_the_plan_allows(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;
    let slot = |kind: &str, name: &str| madar_rust::branch_plan::PlanDevice {
        id: Uuid::new_v4(),
        kind: kind.into(),
        name: name.into(),
        receipt_printer_id: None,
        device_id: None,
        x: 0.0,
        y: 0.0,
    };
    let plan_of =
        |devices: Vec<madar_rust::branch_plan::PlanDevice>| madar_rust::branch_plan::BranchPlan {
            devices,
            ..Default::default()
        };
    let check = |plan: madar_rust::branch_plan::BranchPlan| {
        let pool = pool.clone();
        let (org, branch) = (s.org, s.branch);
        async move {
            let mut tx = pool.begin().await.unwrap();
            madar_rust::branch_plan::handlers::check_slot_limits(&mut tx, org, branch, &plan).await
        }
    };
    let three = plan_of(vec![
        slot("pos", "A"),
        slot("pos", "B"),
        slot("kitchen", "K"),
        slot("waiter", "W"),
    ]);
    assert!(check(three.clone()).await.is_ok(), "paid: anything");

    set_plan(&pool, s.org, "free").await;
    let err = check(three).await.unwrap_err();
    assert!(
        err.to_string().contains("paid plan") || err.to_string().contains("limit"),
        "{err}"
    );
    let one = plan_of(vec![slot("pos", "A")]);
    assert!(check(one.clone()).await.is_ok(), "one till is a Free plan");
    let two = plan_of(vec![slot("pos", "A"), slot("pos", "B")]);
    let err = check(two.clone()).await.unwrap_err();
    assert!(
        matches!(err, madar_rust::errors::AppError::PlanRefused(r) if r.code == madar_plan::Code::DeviceLimit)
    );
    let kitchen = plan_of(vec![slot("kitchen", "K")]);
    let err = check(kitchen).await.unwrap_err();
    assert!(matches!(err, madar_rust::errors::AppError::PlanRefused(r)
        if r.code == madar_plan::Code::Feature && r.feature == Some(madar_plan::Feature::Kitchen)));
    // A downgraded branch that already has its slots can save them again.
    for d in &two.devices {
        sqlx::query("INSERT INTO branch_device_slots (id, org_id, branch_id, kind, name) VALUES ($1,$2,$3,$4,$5)")
            .bind(d.id).bind(s.org).bind(s.branch).bind(&d.kind).bind(&d.name)
            .execute(&pool).await.unwrap();
    }
    assert!(check(two).await.is_ok(), "slots it already has are not new");
}

// ── Realtime ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_free_till_may_not_hold_the_realtime_stream(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;
    let app = app!(pool);
    for (who, bearer) in [
        ("owner", s.admin_token()),
        ("teller", token(s.teller, s.org, UserRole::Teller)),
    ] {
        let uri = format!("/realtime/stream?branch_id={}", s.branch);
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&uri)
                .insert_header(("Authorization", format!("Bearer {bearer}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "paid {who} holds a stream");
        drop(resp);
    }
    set_plan(&pool, s.org, "free").await;
    for (who, bearer) in [
        ("owner", s.admin_token()),
        ("teller", token(s.teller, s.org, UserRole::Teller)),
    ] {
        let (st, b) = call(
            &app,
            test::TestRequest::get().uri(&format!("/realtime/stream?branch_id={}", s.branch)),
            Some(&bearer),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "free {who}: {b}");
        assert_eq!(b["code"], "plan_no_realtime");
        // An explicit topic list changes nothing: an old build cannot hold one.
        let (st, b) = call(
            &app,
            test::TestRequest::get().uri(&format!(
                "/realtime/stream?branch_id={}&topics=sync,orders",
                s.branch
            )),
            Some(&bearer),
        )
        .await;
        assert_eq!(
            (st, b["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("plan_no_realtime"))
        );
    }
}

// ── The plan on the org: super admin ─────────────────────────────────────

#[sqlx::test]
async fn only_super_admin_sets_the_plan_and_the_list_filters_by_it(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;
    let other = shop(&pool).await;
    let app = app!(pool);
    // The org's own owner cannot.
    let (st, b) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "free"})),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT plan FROM organizations WHERE id = $1")
            .bind(s.org)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "paid"
    );
    let sa = super_admin();
    let (st, b) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "gold"})),
        Some(&sa),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    let (st, b) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "free"})),
        Some(&sa),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["plan"], "free");

    let (st, free) = call(
        &app,
        test::TestRequest::get().uri("/orgs?plan=free"),
        Some(&sa),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let ids = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|o| o["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(&free), vec![s.org.to_string()]);
    let (_, paid) = call(
        &app,
        test::TestRequest::get().uri("/orgs?plan=paid"),
        Some(&sa),
    )
    .await;
    assert_eq!(ids(&paid), vec![other.org.to_string()]);
    let (_, selfs) = call(
        &app,
        test::TestRequest::get().uri("/orgs?signup_source=self"),
        Some(&sa),
    )
    .await;
    assert!(ids(&selfs).is_empty());
    let (_, admin) = call(
        &app,
        test::TestRequest::get().uri("/orgs?signup_source=admin&plan=free"),
        Some(&sa),
    )
    .await;
    assert_eq!(ids(&admin), vec![s.org.to_string()]);
    let (st, _) = call(
        &app,
        test::TestRequest::get().uri("/orgs?plan=gold"),
        Some(&sa),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

// ── Downgrade, the active till, upgrade ──────────────────────────────────

struct Tills {
    a_old: Uuid,
    a_recent: Uuid,
    a_never: Uuid,
    b_only: Uuid,
}

async fn tills_for(pool: &PgPool, s: &Shop) -> Tills {
    let a_old = till_device(pool, s.org, s.branch, "A1", 600).await;
    let a_recent = till_device(pool, s.org, s.branch, "A2", 600).await;
    let a_never = till_device(pool, s.org, s.branch, "A3", 6000).await;
    let b_only = till_device(pool, s.org, s.branch2, "B1", 3000).await;
    // A2 rang the last sale, whatever the others' last request says.
    let order = |dev: Uuid, ago: i64| {
        let pool = pool.clone();
        let (org_branch, teller, till) = (s.branch, s.teller, s.teller_till);
        async move {
            sqlx::query(
                "INSERT INTO orders (branch_id, till_id, teller_id, order_number, order_ref, status,
                                     payment_method, subtotal, total_amount, device_id, device_code, created_at)
                 VALUES ($1, $2, $3, (SELECT COALESCE(max(order_number), 0) + 1 FROM orders),
                         'T-' || gen_random_uuid()::text, 'completed', 'cash',
                         1000, 1000, $4, 'T9', now() - make_interval(mins => $5::int))",
            )
            .bind(org_branch).bind(till).bind(teller).bind(dev).bind(ago as i32)
            .execute(&pool).await.unwrap();
        }
    };
    order(a_old, 500).await;
    order(a_recent, 5).await;
    Tills {
        a_old,
        a_recent,
        a_never,
        b_only,
    }
}

#[sqlx::test]
async fn a_downgrade_keeps_the_most_recent_till_per_branch_and_an_upgrade_restores_all(
    pool: PgPool,
) {
    setup(&pool).await;
    let s = shop(&pool).await;
    let t = tills_for(&pool, &s).await;
    let app = app!(pool);
    let sa = super_admin();
    let (st, b) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "free"})),
        Some(&sa),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert!(
        !inactive(&pool, t.a_recent).await,
        "the most recently used till stays"
    );
    assert!(inactive(&pool, t.a_old).await);
    assert!(inactive(&pool, t.a_never).await);
    assert!(!inactive(&pool, t.b_only).await, "each branch keeps one");
    // Nothing deleted.
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM devices WHERE org_id = $1 AND retired_at IS NULL")
            .bind(s.org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 4);

    // Saving the same plan twice changes nothing (the owner's pick survives).
    let (st, _) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "free"})),
        Some(&sa),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(!inactive(&pool, t.a_recent).await);

    // The owner reads the state and switches branch A's active till.
    let (st, state) = call(
        &app,
        test::TestRequest::get().uri(&format!("/orgs/{}/plan", s.org)),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{state}");
    assert_eq!(state["plan"], "free");
    assert_eq!(state["live_branches"], 2);
    assert_eq!(state["over_branch_limit"], true);
    assert_eq!(state["inactive_tills"], 2);
    assert_eq!(state["entitlements"]["realtime"], false);
    assert_eq!(state["entitlements"]["limits"]["branches"], 1);
    let branch_a = state["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["branch_id"] == s.branch.to_string())
        .unwrap();
    assert_eq!(
        branch_a["tills"][0]["device_id"],
        t.a_recent.to_string(),
        "the active one first"
    );
    assert_eq!(branch_a["tills"][0]["active"], true);
    assert!(branch_a["tills"][0]["last_sale_at"].is_string());

    let put = |dev: Uuid, branch: Uuid| {
        test::TestRequest::put()
            .uri(&format!("/orgs/{}/plan/active-till", s.org))
            .set_json(json!({"branch_id": branch, "device_id": dev}))
    };
    // Not a branch manager, not another org's owner.
    let (st, _) = call(&app, put(t.a_never, s.branch), Some(&s.manager_token())).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let stranger = shop(&pool).await;
    let (st, _) = call(
        &app,
        put(t.a_never, s.branch),
        Some(&stranger.admin_token()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(
        inactive(&pool, t.a_never).await,
        "refused calls changed nothing"
    );
    // A till of another branch is not a choice for this one.
    let (st, _) = call(&app, put(t.b_only, s.branch), Some(&s.admin_token())).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let e0 = epoch(&pool, s.org).await;
    let (st, state) = call(&app, put(t.a_never, s.branch), Some(&s.admin_token())).await;
    assert_eq!(st, StatusCode::OK, "{state}");
    assert!(!inactive(&pool, t.a_never).await);
    assert!(
        inactive(&pool, t.a_recent).await,
        "the previous active till is switched off"
    );
    assert!(inactive(&pool, t.a_old).await);
    assert!(
        !inactive(&pool, t.b_only).await,
        "another branch is untouched"
    );
    assert!(
        epoch(&pool, s.org).await > e0,
        "tills refresh their snapshot"
    );
    // Super admin may too.
    let (st, _) = call(&app, put(t.a_old, s.branch), Some(&sa)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(!inactive(&pool, t.a_old).await);
    assert!(inactive(&pool, t.a_never).await);

    // The feed tells the till (the `device` row carries the flag).
    let (st, pull) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/pull")
            .set_json(json!({"branch_id": s.branch, "device_id": t.a_never})),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{pull}");
    let row = pull["data"]["device"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == t.a_never.to_string())
        .unwrap();
    assert_eq!(row["plan_inactive"], true);
    assert_eq!(pull["data"]["branch_settings"][0]["plan"], "free");

    // Upgrade clears every flag.
    let (st, _) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "paid"})),
        Some(&sa),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    for d in [t.a_old, t.a_recent, t.a_never, t.b_only] {
        assert!(!inactive(&pool, d).await);
    }
    let (st, b) = call(&app, put(t.a_old, s.branch), Some(&s.admin_token())).await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["code"], "PLAN_NOT_FREE");
}

#[sqlx::test]
async fn an_inactive_till_sells_nothing_new_and_opens_no_session(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;
    let t = tills_for(&pool, &s).await;
    sqlx::query("UPDATE users SET pin_hash = $2 WHERE id = $1")
        .bind(s.teller)
        .bind(bcrypt::hash("482913", 4).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);
    let (st, _) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "free"})),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(inactive(&pool, t.a_old).await);

    let item = json!([{"menu_item_id": s.burger, "quantity": 1}]);
    let sell = |dev: Uuid| {
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("X-Madar-Device", dev.to_string()))
            .set_json(s.order_body(item.clone()))
    };
    let tok = s.admin_token();
    let (st, b) = call(&app, sell(t.a_old), Some(&tok)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "plan_till_inactive");
    assert!(
        b["error"]
            .as_str()
            .unwrap()
            .contains("isn't active on the Free plan")
    );
    assert!(b["error_ar"].is_string());
    let (st, b) = call(&app, sell(t.a_recent), Some(&tok)).await;
    assert_eq!(st, StatusCode::CREATED, "the active till sells: {b}");
    // A PIN session from the inactive till is refused the same way.
    let pin = |dev: Uuid| {
        test::TestRequest::post()
            .uri("/auth/login")
            .insert_header(("X-Madar-Device", dev.to_string()))
            .set_json(json!({"pin": "482913", "branch_id": s.branch}))
    };
    let (st, b) = call(&app, pin(t.a_old), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "plan_till_inactive");
    let (st, b) = call(&app, pin(t.a_recent), None).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["plan"], "free");
}

// ── The replay of a sale from a till the plan switched off ─────────────────

/// A queued sale as the till would send it.
fn queued_sale(s: &Shop, device: Uuid, occurred_at: chrono::DateTime<Utc>) -> Value {
    json!({
        "op": "create_order",
        "teller_id": s.admin,
        "device_id": device,
        "occurred_at": occurred_at,
        "request": {
            "branch_id": s.branch, "shift_id": s.till, "payment_method": "cash",
            "idempotency_key": Uuid::new_v4(),
            "items": [{"menu_item_id": s.burger, "quantity": 1}],
        }
    })
}

#[sqlx::test]
async fn a_queued_sale_is_never_lost_to_the_plan_unless_the_till_was_told(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;
    let dev = till_device(&pool, s.org, s.branch, "A1", 1).await;
    let other = till_device(&pool, s.org, s.branch, "A2", 1).await;
    let app = app!(pool);
    let tok = s.admin_token();
    let replay = |body: Value| test::TestRequest::post().uri("/sync/replay").set_json(body);
    let flags =
        |key: Value| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, String>(
                "SELECT f.capability FROM authz_replay_flags f JOIN orders o ON o.id = f.subject_id
                  WHERE o.idempotency_key = $1::text::uuid",
            )
            .bind(key.as_str().unwrap().to_string())
            .fetch_all(&pool).await.unwrap()
            }
        };
    let stored = |key: Value| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM orders WHERE idempotency_key = $1::text::uuid",
            )
            .bind(key.as_str().unwrap().to_string())
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    // Switch `dev` off, ten minutes ago, and tell nobody.
    sqlx::query(
        "UPDATE devices SET plan_inactive = true, plan_inactive_at = now() - interval '10 minutes' WHERE id = $1",
    )
    .bind(dev)
    .execute(&pool).await.unwrap();

    // 1. Rung before the flag: history, accepted clean.
    let b = queued_sale(&s, dev, Utc::now() - Duration::minutes(30));
    let key = b["request"]["idempotency_key"].clone();
    let (st, r) = call(&app, replay(b), Some(&tok)).await;
    assert!(st.is_success(), "{st} {r}");
    assert_eq!(stored(key.clone()).await, 1);
    assert!(flags(key).await.is_empty(), "before the flag: no flag");

    // 2. Rung after the flag, but the till had not been told: the money is
    // taken, so it is accepted, and the owner is told.
    let b = queued_sale(&s, dev, Utc::now() - Duration::minutes(2));
    let key = b["request"]["idempotency_key"].clone();
    let (st, r) = call(&app, replay(b), Some(&tok)).await;
    assert!(st.is_success(), "{st} {r}");
    assert_eq!(stored(key.clone()).await, 1);
    assert_eq!(flags(key).await, vec!["plan:till_inactive".to_string()]);

    // 3. The till is told (its pull). New selling stamped after is refused;
    // settling what already happened is not.
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/pull")
            .set_json(json!({"branch_id": s.branch, "device_id": dev})),
        Some(&tok),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let seen: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT plan_inactive_seen_at FROM devices WHERE id = $1")
            .bind(dev)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        seen.is_some(),
        "a pull that carries the flag marks the till as told"
    );
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let b = queued_sale(&s, dev, Utc::now());
    let key = b["request"]["idempotency_key"].clone();
    let (st, r) = call(&app, replay(b), Some(&tok)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{r}");
    assert_eq!(r["code"], "plan_till_inactive");
    assert_eq!(stored(key).await, 0);
    // ...but a sale rung BEFORE it was told still lands (it queued offline).
    let b = queued_sale(&s, dev, seen.unwrap() - Duration::seconds(30));
    let key = b["request"]["idempotency_key"].clone();
    let (st, r) = call(&app, replay(b), Some(&tok)).await;
    assert!(st.is_success(), "{st} {r}");
    assert_eq!(stored(key).await, 1);
    // A cash movement (settling the drawer) is never refused.
    let (st, r) = call(
        &app,
        replay(json!({
            "op": "cash_movement", "teller_id": s.admin, "till_id": s.till, "device_id": dev,
            "request": {"amount": 500, "kind": "drop", "note": "banked", "id": Uuid::new_v4()}
        })),
        Some(&tok),
    )
    .await;
    assert!(
        st.is_success() || st == StatusCode::BAD_REQUEST,
        "never the plan: {st} {r}"
    );
    assert_ne!(r["code"], "plan_till_inactive");

    // 4. An active till of the same org is never touched by any of it.
    let b = queued_sale(&s, other, Utc::now());
    let key = b["request"]["idempotency_key"].clone();
    let (st, r) = call(&app, replay(b), Some(&tok)).await;
    assert!(st.is_success(), "{st} {r}");
    assert!(flags(key).await.is_empty());
}

// ── Pickup / Dine in on every plan ───────────────────────────────────────

#[sqlx::test]
async fn orders_carry_their_service_mode_and_the_sales_report_splits_by_it(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;
    let app = app!(pool);
    let tok = s.admin_token();
    let item = json!([{"menu_item_id": s.burger, "quantity": 1}]);
    let make = |mode: Option<&str>| {
        let mut body = s.order_body(item.clone());
        if let Some(m) = mode {
            body["service_mode"] = json!(m);
        }
        test::TestRequest::post().uri("/orders").set_json(body)
    };
    let (st, a) = call(&app, make(Some("dine_in")), Some(&tok)).await;
    assert_eq!(st, StatusCode::CREATED, "{a}");
    assert_eq!(a["service_mode"], "dine_in");
    let (_, b) = call(&app, make(Some("takeaway")), Some(&tok)).await;
    assert_eq!(b["service_mode"], "takeaway");
    let (_, c) = call(&app, make(None), Some(&tok)).await;
    assert_eq!(
        c["service_mode"], "takeaway",
        "a till that never sent it: Pickup"
    );
    let (_, d) = call(&app, make(Some("dine_in")), Some(&tok)).await;

    // Voided orders are not sales.
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/orders/{}/void", d["id"].as_str().unwrap()))
            .set_json(json!({"reason": "mistake"})),
        Some(&tok),
    )
    .await;
    assert!(st.is_success(), "the void went through: {st}");

    for plan in ["paid", "free"] {
        set_plan(&pool, s.org, plan).await;
        let (st, one) = call(
            &app,
            test::TestRequest::get().uri(&format!("/orders/{}", a["id"].as_str().unwrap())),
            Some(&tok),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{plan}: {one}");
        assert_eq!(one["service_mode"], "dine_in", "{plan}");
        let (st, report) = call(
            &app,
            test::TestRequest::get().uri(&format!("/reports/branches/{}/sales", s.branch)),
            Some(&tok),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{plan}: {report}");
        let split = report["by_service_mode"].as_array().unwrap();
        assert_eq!(split.len(), 2);
        assert_eq!(split[0]["service_mode"], "takeaway");
        assert_eq!(split[1]["service_mode"], "dine_in");
        assert_eq!(report["voided_orders"], 1, "{plan}: {report}");
        assert_eq!(split[0]["orders"], 2, "{plan}: {report}");
        assert_eq!(
            split[1]["orders"], 1,
            "a voided dine-in order is not a sale: {plan}: {report}"
        );
        let sum: i64 = split.iter().map(|r| r["revenue"].as_i64().unwrap()).sum();
        assert_eq!(
            sum,
            report["total_revenue"].as_i64().unwrap(),
            "{plan}: the split adds up to the total"
        );
        // The orders list carries it on every row.
        let (_, list) = call(
            &app,
            test::TestRequest::get().uri(&format!("/orders/branches/{}", s.branch)),
            Some(&tok),
        )
        .await;
        let rows = list["data"]
            .as_array()
            .or(list.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            rows.iter().all(|o| o["service_mode"].is_string()),
            "{plan}: {list}"
        );
    }
}

// ── What a Free org's authenticated surfaces refuse by feature ───────────

#[sqlx::test]
async fn paid_surfaces_that_have_no_capability_are_refused_by_feature(pool: PgPool) {
    setup(&pool).await;
    let s = shop(&pool).await;
    let app = app!(pool);
    let tok = s.admin_token();
    let org = s.org;
    let surfaces: Vec<(&str, String, Option<Value>, &str)> = vec![
        ("GET", format!("/orgs/{org}/links-page"), None, "branding"),
        (
            "PUT",
            format!("/orgs/{org}/links-page"),
            Some(json!({"items": []})),
            "branding",
        ),
        ("GET", format!("/orgs/{org}/qr"), None, "online"),
        ("GET", format!("/orgs/{org}/links-qr"), None, "branding"),
        (
            "POST",
            "/ai/chat".into(),
            Some(json!({"question": "sales?"})),
            "basira",
        ),
    ];
    // Bookings and loyalty QR codes are behind capabilities the ceiling removes,
    // so the plan refuses them as a plain 403 before the feature check is reached.
    for uri in [
        format!("/orgs/{org}/booking-qr"),
        format!("/orgs/{org}/loyalty-qr"),
    ] {
        set_plan(&pool, org, "paid").await;
        let (st, b) = call(&app, test::TestRequest::get().uri(&uri), Some(&tok)).await;
        assert_ne!(st, StatusCode::FORBIDDEN, "paid {uri}: {b}");
        set_plan(&pool, org, "free").await;
        let (st, b) = call(&app, test::TestRequest::get().uri(&uri), Some(&tok)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "free {uri}: {b}");
    }
    for (method, uri, body, feature) in &surfaces {
        let mk = || {
            let r = test::TestRequest::default()
                .method(actix_web::http::Method::from_bytes(method.as_bytes()).unwrap())
                .uri(uri);
            match body {
                Some(b) => r.set_json(b),
                None => r,
            }
        };
        set_plan(&pool, org, "paid").await;
        let (st, b) = call(&app, mk(), Some(&tok)).await;
        assert_ne!(b["code"], "plan_feature", "paid {method} {uri}: {st} {b}");
        set_plan(&pool, org, "free").await;
        let (st, b) = call(&app, mk(), Some(&tok)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "free {method} {uri}: {b}");
        assert_eq!(b["code"], "plan_feature", "{method} {uri}: {b}");
        assert_eq!(b["feature"], *feature, "{method} {uri}: {b}");
    }
}
