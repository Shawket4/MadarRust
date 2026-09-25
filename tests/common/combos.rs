//! The combos module's shop, shared by the `combos_*` suites: the contract's
//! worked example (COMBOS_CONTRACT.md §4 `combo/lunch_large_latte`) seeded in
//! SQL, plus the items the deal vectors use (§5 `two_bites`, `b2g1`).
//!
//! Tax is 0 so every hand-computed line figure is also the order's figure.
//!
//! - Burger 12000, Fries 4000 (one size each, category Mains / Sides).
//! - Latte Regular 5000 / Large 6000 (Drinks). Recipes are Beans at
//!   `cost_per_unit` 100 per g: Burger 10 g, Fries 5, Latte 10 / 14, Cola 1,
//!   Croissant 2, Cookie 1.
//! - Cola 3000 (Drinks): reachable only through the Drink slot's category choice.
//! - Oat milk: a plain add-on at 1500.
//! - Croissant 5500, Cookie 4000 (Bakery) for the deals.
//! - "Lunch deal" combo, P 15000: Main (Burger), Side (Fries), Drink (Latte
//!   included at Regular, or any Drinks item at its cheapest size).

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;

pub fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

pub fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
}

pub struct Shop {
    pub org: Uuid,
    pub branch: Uuid,
    /// A second branch of the same org (branch overrides, other-branch windows).
    pub branch2: Uuid,
    pub admin: Uuid,
    pub manager: Uuid,
    pub teller: Uuid,
    /// An open till of `admin` at `branch`.
    pub till: Uuid,
    /// An open till of `teller` at `branch`.
    pub teller_till: Uuid,
    pub mains: Uuid,
    pub sides: Uuid,
    pub drinks: Uuid,
    pub bakery: Uuid,
    pub beans: Uuid,
    pub burger: Uuid,
    pub fries: Uuid,
    pub latte: Uuid,
    pub cola: Uuid,
    pub croissant: Uuid,
    pub cookie: Uuid,
    pub oat: Uuid,
    /// The "Lunch deal" combo (a menu item of kind combo).
    pub combo: Uuid,
    pub slot_main: Uuid,
    pub slot_side: Uuid,
    pub slot_drink: Uuid,
}

impl Shop {
    pub fn admin_token(&self) -> String {
        token(self.admin, self.org, UserRole::OrgAdmin)
    }
    pub fn manager_token(&self) -> String {
        token(self.manager, self.org, UserRole::BranchManager)
    }
    pub fn teller_token(&self) -> String {
        token(self.teller, self.org, UserRole::Teller)
    }

    /// The worked example's picks: Burger, Fries, a Large Latte with oat milk.
    pub fn lunch_picks(&self) -> Value {
        json!([
            {"slot_id": self.slot_main, "menu_item_id": self.burger},
            {"slot_id": self.slot_side, "menu_item_id": self.fries},
            {"slot_id": self.slot_drink, "menu_item_id": self.latte, "size_label": "Large",
             "addons": [{"addon_item_id": self.oat, "quantity": 1}]}
        ])
    }

    /// One order line of `n` lunch combos (live: no prices).
    pub fn lunch_line(&self, n: i32) -> Value {
        json!({"menu_item_id": self.combo, "quantity": n, "combo": {"picks": self.lunch_picks()}})
    }

    /// A live `POST /orders` body (cash, admin's till).
    pub fn order_body(&self, items: Value) -> Value {
        json!({
            "branch_id": self.branch,
            "till_id": self.till,
            "payment_method": "cash",
            "customer_name": null,
            "notes": null,
            "discount_type": null,
            "discount_value": null,
            "discount_id": null,
            "items": items,
        })
    }
}

pub async fn user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
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

pub async fn till(pool: &PgPool, branch: Uuid, who: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) \
         VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(who)
    .fetch_one(pool)
    .await
    .unwrap()
}

pub async fn category(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO categories (org_id, name) VALUES ($1, $2) RETURNING id")
        .bind(org)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A single-price item (its `one_size` row carries `price`).
pub async fn item(pool: &PgPool, org: Uuid, cat: Uuid, name: &str, price: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, category_id, name, base_price, is_active) \
         VALUES ($1, $2, $3, $4, true) RETURNING id",
    )
    .bind(org)
    .bind(cat)
    .bind(name)
    .bind(price)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// 10 g of `ing` per unit of `item` at `size` (the legacy recipe rows the
/// cost path and the stock deduction read).
pub async fn recipe(pool: &PgPool, item: Uuid, ing: Uuid, size: Option<&str>, grams: i32) {
    sqlx::query(
        "INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) \
         VALUES ($1, $2, $3, $4, 'Beans', 'g')",
    )
    .bind(item)
    .bind(ing)
    .bind(grams)
    .bind(size.unwrap_or("one_size"))
    .execute(pool)
    .await
    .unwrap();
}

pub async fn grant_legacy(pool: &PgPool) {
    for role in ["org_admin", "branch_manager", "teller"] {
        for (r, a) in [
            ("orders", "create"),
            ("orders", "read"),
            ("order_items", "create"),
            ("payments", "create"),
            ("open_tickets", "create"),
            ("open_tickets", "read"),
            ("menu_items", "read"),
        ] {
            sqlx::query(
                "INSERT INTO role_permissions (role, resource, action, granted) \
                 VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING",
            )
            .bind(role)
            .bind(r)
            .bind(a)
            .execute(pool)
            .await
            .unwrap();
        }
    }
}

/// A combo item (kind combo, P = `price`) with its anchor row.
pub async fn combo_item(pool: &PgPool, org: Uuid, cat: Uuid, name: &str, price: i32) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, category_id, name, base_price, is_active, kind) \
         VALUES ($1, $2, $3, $4, true, 'combo') RETURNING id",
    )
    .bind(org)
    .bind(cat)
    .bind(name)
    .bind(price)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO menu_item_combos (menu_item_id, org_id) VALUES ($1, $2)")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}

pub async fn slot(
    pool: &PgPool,
    org: Uuid,
    combo: Uuid,
    name: &str,
    sort: i32,
    min: i16,
    max: i16,
    default_item: Option<Uuid>,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO combo_slots (org_id, combo_item_id, name, sort, min_picks, max_picks, default_item_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(org)
    .bind(combo)
    .bind(name)
    .bind(sort)
    .bind(min)
    .bind(max)
    .bind(default_item)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// An item choice (`menu_item_id`) or a category choice (`category_id`).
pub async fn choice(
    pool: &PgPool,
    org: Uuid,
    slot: Uuid,
    menu_item_id: Option<Uuid>,
    category_id: Option<Uuid>,
    surcharge: i32,
    included: Option<&str>,
    sort: i32,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO combo_slot_choices (org_id, slot_id, menu_item_id, category_id, surcharge, included_size_label, sort) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(org)
    .bind(slot)
    .bind(menu_item_id)
    .bind(category_id)
    .bind(surcharge)
    .bind(included)
    .bind(sort)
    .fetch_one(pool)
    .await
    .unwrap()
}

pub async fn shop(pool: &PgPool) -> Shop {
    let org = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, tax_rate, tax_inclusive) VALUES ($1, 'Combo Org', $2, 0, false)",
    )
    .bind(org)
    .bind(format!("combo-{org}"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    let mut branches = Vec::new();
    for (name, code) in [("Main", "MAIN"), ("Mall", "MALL")] {
        let b = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO branches (id, org_id, name, code, timezone) VALUES ($1, $2, $3, $4, 'Africa/Cairo')",
        )
        .bind(b)
        .bind(org)
        .bind(name)
        .bind(code)
        .execute(pool)
        .await
        .unwrap();
        branches.push(b);
    }
    let (branch, branch2) = (branches[0], branches[1]);
    let admin = user(pool, org, "org_admin").await;
    let manager = user(pool, org, "branch_manager").await;
    let teller = user(pool, org, "teller").await;
    for u in [manager, teller] {
        sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
            .bind(u)
            .bind(branch)
            .execute(pool)
            .await
            .unwrap();
    }
    grant_legacy(pool).await;
    let till_id = till(pool, branch, admin).await;
    let teller_till = till(pool, branch, teller).await;

    let mains = category(pool, org, "Mains").await;
    let sides = category(pool, org, "Sides").await;
    let drinks = category(pool, org, "Drinks").await;
    let bakery = category(pool, org, "Bakery").await;
    let beans: Uuid = sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit, cost_per_unit, category_id) \
         VALUES ($1, 'Beans', 'g', 100, ingredient_category_id($1, 'general')) RETURNING id",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();

    let burger = item(pool, org, mains, "Burger", 12000).await;
    let fries = item(pool, org, sides, "Fries", 4000).await;
    let latte = item(pool, org, drinks, "Latte", 5000).await;
    super::sizes::seed_real_size(pool, latte, "Regular", 5000, 0).await;
    super::sizes::seed_real_size(pool, latte, "Large", 6000, 1).await;
    let cola = item(pool, org, drinks, "Cola", 3000).await;
    let croissant = item(pool, org, bakery, "Croissant", 5500).await;
    let cookie = item(pool, org, bakery, "Cookie", 4000).await;
    for (i, g) in [(burger, 10), (fries, 5), (cola, 1), (croissant, 2), (cookie, 1)] {
        recipe(pool, i, beans, None, g).await;
    }
    recipe(pool, latte, beans, Some("Regular"), 10).await;
    recipe(pool, latte, beans, Some("Large"), 14).await;

    let oat = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1, $2, 'Oat milk', 'extra', 1500)",
    )
    .bind(oat)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();

    let combo = combo_item(pool, org, mains, "Lunch deal", 15000).await;
    let slot_main = slot(pool, org, combo, "Main", 0, 1, 1, Some(burger)).await;
    let slot_side = slot(pool, org, combo, "Side", 1, 1, 1, Some(fries)).await;
    let slot_drink = slot(pool, org, combo, "Drink", 2, 1, 1, Some(latte)).await;
    choice(pool, org, slot_main, Some(burger), None, 0, None, 0).await;
    choice(pool, org, slot_side, Some(fries), None, 0, None, 0).await;
    choice(pool, org, slot_drink, Some(latte), None, 0, Some("Regular"), 0).await;
    choice(pool, org, slot_drink, None, Some(drinks), 0, None, 1).await;

    Shop {
        org,
        branch,
        branch2,
        admin,
        manager,
        teller,
        till: till_id,
        teller_till,
        mains,
        sides,
        drinks,
        bakery,
        beans,
        burger,
        fries,
        latte,
        cola,
        croissant,
        cookie,
        oat,
        combo,
        slot_main,
        slot_side,
        slot_drink,
    }
}
