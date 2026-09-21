//! Fixtures shared by the analytics and AI suites.
//!
//! `ai`'s tests seed their world with `analytics`'s fixtures. That used to be a
//! reach into a sibling's `#[cfg(test)]` module; now that each suite is its own
//! binary, the shared part lives here and neither suite owns the other's setup.

#![allow(dead_code)]

use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;

pub fn secret() -> JwtSecret {
    JwtSecret("test_secret".into())
}

pub fn org_admin_token(org: Uuid) -> String {
    org_admin_token_for(org, admin_id_for(org))
}

/// The org admin [`seed`] creates, derived from the org id so a token can be
/// minted without threading the user id through every test. Architecture E
/// resolves access from the user's own role assignments, so a token for an id
/// with no `users` row now holds nothing — it used to be believed on its role
/// claim alone.
pub fn admin_id_for(org: Uuid) -> Uuid {
    Uuid::from_u128(org.as_u128() ^ 0xad_1a_ad_1a_ad_1a_ad_1a_ad_1a_ad_1a_ad_1a_ad_1a)
}

/// A token for a SPECIFIC user id. Needed wherever a handler writes a row that
/// references `users`: a token minted for an id with no user behind it is
/// rejected by the foreign key, not by the auth layer, which produces a
/// confusing failure a long way from its cause.
pub fn org_admin_token_for(org: Uuid, user: Uuid) -> String {
    create_token(&secret(), user, Some(org), UserRole::OrgAdmin, None, 24).unwrap()
}

/// What [`seed`] created, so tests can assert against known figures.
pub struct Seeded {
    pub org: Uuid,
    #[allow(dead_code)]
    pub branch: Uuid,
    /// A real org-admin row, for tests whose handlers write rows referencing
    /// `users`.
    #[allow(dead_code)]
    pub admin: Uuid,
    /// A second real user in the same org, for isolation tests.
    #[allow(dead_code)]
    pub other_admin: Uuid,
}

/// One organization with a branch, a closed shift, two products in a category,
/// two paid orders, and one waste movement.
///
/// Deliberately small but *broad*: it touches every dataset the registry
/// exposes joins for, so a broken join surfaces here rather than in production.
pub async fn seed(pool: &PgPool, label: &str) -> Seeded {
    let (org, teller, branch, _till, shift) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let (admin, other_admin) = (admin_id_for(org), Uuid::new_v4());
    let (category, latte, mocha) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let (ingredient, inv) = (Uuid::new_v4(), Uuid::new_v4());

    sqlx::query(
        "INSERT INTO organizations (id, name, slug, timezone) VALUES ($1,$2,$3,'Africa/Cairo')",
    )
    .bind(org)
    .bind(format!("Org {label}"))
    .bind(format!("org-{}", org.simple()))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ($1,'Teller One','teller',$2,'x')",
    )
    .bind(teller)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    for (id, name) in [(admin, "Admin One"), (other_admin, "Admin Two")] {
        sqlx::query(
            "INSERT INTO users (id, name, role, org_id, password_hash) \
             VALUES ($1,$2,'org_admin',$3,'x')",
        )
        .bind(id)
        .bind(name)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO branches (id, org_id, name, code) VALUES ($1,$2,$3,$4)")
        .bind(branch)
        .bind(org)
        .bind(format!("Branch {label}"))
        .bind(org.simple().to_string()[..6].to_uppercase())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, \
         closing_cash_declared, closing_cash_system, closed_at) \
         VALUES ($1,$2,$3,'closed',10000,25000,25500, now())",
    )
    .bind(shift)
    .bind(branch)
    .bind(teller)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1,$2,'Drinks')")
        .bind(category)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    for (id, name, price) in [(latte, "Latte", 5000), (mocha, "Mocha", 7000)] {
        sqlx::query(
            "INSERT INTO menu_items (id, org_id, name, category_id, base_price) VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(id)
        .bind(org)
        .bind(name)
        .bind(category)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    }

    // Two orders: one plain, one with a discount, both completed and paid.
    for (n, item, name, unit, qty, discount) in [
        (1i32, latte, "Latte", 5000i32, 2i32, 0i32),
        (2, mocha, "Mocha", 7000, 1, 700),
    ] {
        let order = Uuid::new_v4();
        let subtotal = unit * qty;
        let total = subtotal - discount;
        sqlx::query(
            "INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, \
             order_ref, status, subtotal, discount_amount, total_amount, order_type) \
             VALUES ($1,$2,$3,$4,$5,'cash',$6,'completed',$7,$8,$9,'dine_in')",
        )
        .bind(order)
        .bind(branch)
        .bind(shift)
        .bind(teller)
        .bind(n)
        .bind(format!("REF-{}-{n}", &order.simple().to_string()[..6]))
        .bind(subtotal)
        .bind(discount)
        .bind(total)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO order_items (order_id, menu_item_id, item_name, unit_price, quantity, \
             line_total, unit_cost, line_cost) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .bind(order)
        .bind(item)
        .bind(name)
        .bind(unit)
        .bind(qty)
        .bind(subtotal)
        .bind((unit / 4) as i64)
        .bind((subtotal / 4) as i64)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1,'cash',$2,true)")
            .bind(order)
            .bind(total)
            .execute(pool)
            .await
            .unwrap();
    }

    // A wasted ingredient, so the inventory dataset has something to find.
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category_id)\
         VALUES ($1, $2, 'Milk', 'l', 1200, ingredient_category_id($2, 'dairy'))",
    )
    .bind(ingredient)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO branch_stock (id, branch_id, org_ingredient_id, on_hand, cost_per_unit)\
         VALUES ($1, $2, $3, 50, 1200)",
    )
    .bind(inv)
    .bind(branch)
    .bind(ingredient)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO inventory_movements (branch_id, org_ingredient_id, branch_stock_id, type, \
         quantity, balance_after, unit_cost, reason) \
         VALUES ($1,$2,$3,'waste',-3,47,1200,'Spillage')",
    )
    .bind(branch)
    .bind(ingredient)
    .bind(inv)
    .execute(pool)
    .await
    .unwrap();

    Seeded {
        org,
        branch,
        admin,
        other_admin,
    }
}

pub async fn metrics_app(
    pool: &PgPool,
) -> impl actix_web::dev::Service<
    actix_http::Request,
    Response = actix_web::dev::ServiceResponse,
    Error = actix_web::Error,
> {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(|cfg| {
                madar_rust::analytics::routes::configure(cfg, web::Data::new(pool.clone()))
            }),
    )
    .await
}
