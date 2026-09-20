//! Price lives in SIZES — the model, pinned.
//!
//! An item has no price of its own. Every live item always has at least one
//! size; a single number shown anywhere is the LOWEST active size price.
//! `menu_items.base_price` survives only as a trigger-maintained mirror of that
//! lowest price, so clients at or below v0.7.11 keep charging the right thing.
//!
//! Every test here fails against the pre-change code.
use sqlx::PgPool;
use uuid::Uuid;

use crate::orders::handlers::catalog_unit_price;

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Px', $2)")
        .bind(id)
        .bind(format!("px-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org_id)
        .bind(format!("B{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_category(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'C')")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// Create an item the way a caller does: with a single price and nothing else.
async fn seed_item(pool: &PgPool, org_id: Uuid, cat: Uuid, name: &str, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, category_id, name, base_price)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(org_id)
    .bind(cat)
    .bind(name)
    .bind(price)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn add_size(pool: &PgPool, item: Uuid, label: &str, price: i32) {
    sqlx::query("INSERT INTO menu_item_sizes (menu_item_id, label, price) VALUES ($1, $2, $3)")
        .bind(item)
        .bind(label)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
}

/// Reproduce the production divergence: the SIZE price is what the editor last
/// wrote, while `menu_items.base_price` is the stale field the till kept
/// charging. Written directly, because the mirror trigger (which the migration
/// installs only AFTER reconciling) would otherwise keep the two in step.
async fn diverge(pool: &PgPool, item: Uuid, item_price: i32, size_price: i32) {
    sqlx::query("UPDATE menu_item_sizes SET price = $2 WHERE menu_item_id = $1")
        .bind(item)
        .bind(size_price)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE menu_items SET base_price = $2 WHERE id = $1")
        .bind(item)
        .bind(item_price)
        .execute(pool)
        .await
        .unwrap();
}

/// Replace an item's whole size set in ONE transaction — the ordinary editor
/// save, and the only way to get from the born-with `one_size` row to real
/// sizes without ever committing a size-less item.
async fn set_sizes(pool: &PgPool, item: Uuid, sizes: &[(&str, i32)]) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM menu_item_sizes WHERE menu_item_id = $1")
        .bind(item)
        .execute(&mut *tx)
        .await
        .unwrap();
    for (label, price) in sizes {
        sqlx::query("INSERT INTO menu_item_sizes (menu_item_id, label, price) VALUES ($1, $2, $3)")
            .bind(item)
            .bind(label)
            .bind(price)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
}

/// The migration's reconciliation, verbatim: for every item with exactly one
/// active size take the HIGHER of the item's stale field and the size price,
/// then mirror every item's `base_price` from its cheapest active size.
async fn reconcile_org(pool: &PgPool, org: Uuid) {
    sqlx::query(
        "UPDATE menu_item_sizes z SET price = GREATEST(z.price, m.base_price)
           FROM menu_items m
          WHERE m.id = z.menu_item_id AND m.org_id = $1 AND m.deleted_at IS NULL
            AND z.is_active AND z.price < m.base_price
            AND (SELECT count(*) FROM menu_item_sizes s
                  WHERE s.menu_item_id = m.id AND s.is_active) = 1",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "UPDATE menu_items m SET base_price = sub.lowest
           FROM (SELECT menu_item_id, min(price) AS lowest FROM menu_item_sizes
                  WHERE is_active GROUP BY menu_item_id) sub
          WHERE m.id = sub.menu_item_id AND m.org_id = $1",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
}

async fn base_price(pool: &PgPool, item: Uuid) -> i32 {
    sqlx::query_scalar("SELECT base_price FROM menu_items WHERE id = $1")
        .bind(item)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── 1. An item created with a single price gets a one_size row ────────────────

#[sqlx::test]
async fn an_item_created_with_a_single_price_gets_a_one_size_row(pool: PgPool) {
    let org = seed_org(&pool).await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Espresso", 9500).await;

    let rows: Vec<(String, i32)> =
        sqlx::query_as("SELECT label, price FROM menu_item_sizes WHERE menu_item_id = $1")
            .bind(item)
            .fetch_all(&pool)
            .await
            .unwrap();

    assert_eq!(
        rows,
        vec![("one_size".to_string(), 9500)],
        "an item is never born price-less: it carries its price in a size row"
    );

    // The synthetic row stays invisible to the legacy `item_sizes` projection,
    // so an old till still sees a size-less item exactly as it did before.
    let legacy: i64 = sqlx::query_scalar("SELECT count(*) FROM item_sizes WHERE menu_item_id = $1")
        .bind(item)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        legacy, 0,
        "one_size is a sentinel, not a choice an old client offers"
    );
}

// ── 2. A one-size item is charged from its size, not from the item ────────────

#[sqlx::test]
async fn a_one_size_item_is_charged_from_its_size(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Americano", 10000).await;

    // The owner edits the SIZE upward. This is exactly the production bug: the
    // item's own number went stale while the till kept charging it.
    sqlx::query("UPDATE menu_item_sizes SET price = 11500 WHERE menu_item_id = $1")
        .bind(item)
        .execute(&pool)
        .await
        .unwrap();

    let (_, _, price, _) = catalog_unit_price(&pool, item, None, branch).await.unwrap();
    assert_eq!(price, 11500, "a size edit reaches the charged price");

    // …and the mirror followed, so an old till charges the same thing.
    assert_eq!(base_price(&pool, item).await, 11500);
}

// ── 3. A multi-size item shows the LOWEST size price ──────────────────────────

#[sqlx::test]
async fn a_multi_size_item_shows_the_lowest_size_price(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let cat = seed_category(&pool, org).await;
    // Rue's "Coffee Soft Serve" in miniature: a stale item price BELOW both sizes.
    let item = seed_item(&pool, org, cat, "Coffee Soft Serve", 12500).await;
    set_sizes(&pool, item, &[("small", 15000), ("large", 17000)]).await;

    // No size chosen ⇒ the "from" price, which is the cheapest size.
    let (_, _, from_price, _) = catalog_unit_price(&pool, item, None, branch).await.unwrap();
    assert_eq!(
        from_price, 15000,
        "lowest wins; there is no default-size marker"
    );
    assert_eq!(
        base_price(&pool, item).await,
        15000,
        "the mirror shows the same 'from' price"
    );

    // Each size still charges its own absolute price.
    for (label, expected) in [("small", 15000), ("large", 17000)] {
        let (_, _, p, _) = catalog_unit_price(&pool, item, Some(label), branch)
            .await
            .unwrap();
        assert_eq!(p, expected, "size {label}");
    }

    // Adding a cheaper size moves the displayed number down; adding a dearer one
    // does not. Nothing about the charged prices of the existing sizes changes.
    add_size(&pool, item, "tiny", 9000).await;
    assert_eq!(base_price(&pool, item).await, 9000);
    add_size(&pool, item, "huge", 30000).await;
    assert_eq!(base_price(&pool, item).await, 9000);
}

// ── 4. A size edit reaches the charged price, through the editor endpoint ─────

#[sqlx::test]
async fn a_size_edit_through_the_editor_reaches_the_charged_price(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 13000).await;

    // The editor is the ONLY way a price changes: it upserts the size row.
    sqlx::query(
        "INSERT INTO menu_item_sizes (menu_item_id, label, price) VALUES ($1, 'one_size', $2)
         ON CONFLICT (menu_item_id, label) DO UPDATE SET price = EXCLUDED.price",
    )
    .bind(item)
    .bind(14500)
    .execute(&pool)
    .await
    .unwrap();

    let (_, _, price, _) = catalog_unit_price(&pool, item, None, branch).await.unwrap();
    assert_eq!(price, 14500);
    assert_eq!(base_price(&pool, item).await, 14500);
}

// ── 5. An item with no sizes at all is impossible ─────────────────────────────

#[sqlx::test]
async fn an_item_can_never_end_up_with_no_sizes(pool: PgPool) {
    let org = seed_org(&pool).await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Cookie", 12000).await;

    // (a) Removing the last active size is refused at COMMIT.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM menu_item_sizes WHERE menu_item_id = $1")
        .bind(item)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        tx.commit().await.is_err(),
        "a live item must never commit with no active size"
    );

    // (b) Deactivating the last one is refused too.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE menu_item_sizes SET is_active = false WHERE menu_item_id = $1")
        .bind(item)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        tx.commit().await.is_err(),
        "deactivating the last size is refused"
    );

    // (c) But REPLACING the whole size set in one transaction is fine — that is
    //     the ordinary editor save, and the check is deferred to commit.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM menu_item_sizes WHERE menu_item_id = $1")
        .bind(item)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO menu_item_sizes (menu_item_id, label, price)
         VALUES ($1, 'small', 12000), ($1, 'large', 16000)",
    )
    .bind(item)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit()
        .await
        .expect("replacing the whole size set is allowed");
    assert_eq!(base_price(&pool, item).await, 12000);

    // (d) Deleting the ITEM still works — sizes cascade.
    sqlx::query("DELETE FROM menu_items WHERE id = $1")
        .bind(item)
        .execute(&pool)
        .await
        .unwrap();
}

// ── 6. Old-client goldens (v0.5 … v0.7.11) ───────────────────────────────────

/// What a till at or below v0.7.11 reads. It has no concept of a size-less item
/// carrying a hidden size row: it takes the item's `base_price`, and offers the
/// sizes in `sizes`. Both must look exactly as they always did — except that
/// the number it charges is now the live one instead of a stale field.
#[sqlx::test]
async fn old_clients_still_see_one_price_per_item_and_it_is_the_live_one(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let cat = seed_category(&pool, org).await;

    // The Drops shape: a one-size item whose size was edited up to 11500 while
    // the item's own field stayed at the stale 10000.
    let drops = seed_item(&pool, org, cat, "Americano", 10000).await;
    diverge(&pool, drops, 10000, 11500).await;

    // The Rue shape: a one-size item whose size row is an untouched backfill
    // leftover (12000) below the price actually being charged (22000).
    let rue = seed_item(&pool, org, cat, "Cookie jar", 22000).await;
    diverge(&pool, rue, 22000, 12000).await;

    // A multi-size item: an old till sees its real sizes and a "from" price.
    let multi = seed_item(&pool, org, cat, "Soft Serve", 12500).await;
    set_sizes(&pool, multi, &[("small", 15000), ("large", 17000)]).await;

    // Now the migration runs, org-wide: take the HIGHER of the two for every
    // single-size item, then re-mirror every item from its cheapest size.
    reconcile_org(&pool, org).await;

    // GOLDEN: the single number an old client reads off each item.
    assert_eq!(
        base_price(&pool, drops).await,
        11500,
        "Drops: rises to the size price"
    );
    assert_eq!(
        base_price(&pool, rue).await,
        22000,
        "Rue: keeps the charged price"
    );
    assert_eq!(
        base_price(&pool, multi).await,
        15000,
        "multi-size: the lowest size"
    );

    // GOLDEN: the size list an old client offers is unchanged in shape — the
    // synthetic one_size row never appears, so a single-price item still has
    // no size picker.
    let legacy_sizes: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT menu_item_id, label FROM item_sizes
          WHERE menu_item_id = ANY($1) ORDER BY label",
    )
    .bind(vec![drops, rue, multi])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        legacy_sizes,
        vec![(multi, "large".to_string()), (multi, "small".to_string())],
        "only the genuinely multi-size item offers sizes to an old till"
    );

    // GOLDEN: and what it charges matches what the server would charge.
    for (item, expected) in [(drops, 11500), (rue, 22000), (multi, 15000)] {
        let (_, _, p, _) = catalog_unit_price(&pool, item, None, branch).await.unwrap();
        assert_eq!(
            p, expected,
            "server agrees with the number the old till read"
        );
    }
}

// ── 7. The reconciliation rule itself: no price may fall ─────────────────────

#[sqlx::test]
async fn the_reconciliation_takes_the_higher_and_never_lowers_a_charged_price(pool: PgPool) {
    let org = seed_org(&pool).await;
    let cat = seed_category(&pool, org).await;

    // item price, size price, expected outcome
    let cases = [
        ("drops-espresso", 8500, 9500, 9500), // size edited up  → item rises
        ("drops-v60", 19000, 21500, 21500),   // ditto
        ("rue-cookie-jar", 22000, 12000, 22000), // stale backfill row → item kept
        ("rue-brownies", 12000, 11500, 12000), // ditto
        ("agreed", 15000, 15000, 15000),      // already in step → untouched
    ];

    for (name, item_price, size_price, _) in cases {
        let id = seed_item(&pool, org, cat, name, item_price).await;
        diverge(&pool, id, item_price, size_price).await;
    }

    reconcile_org(&pool, org).await;

    for (name, item_price, size_price, expected) in cases {
        let got: i32 =
            sqlx::query_scalar("SELECT base_price FROM menu_items WHERE org_id = $1 AND name = $2")
                .bind(org)
                .bind(name)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(got, expected, "{name}");
        assert!(
            got >= item_price && got >= size_price,
            "{name}: no price may fall (was item {item_price} / size {size_price}, now {got})"
        );
    }
}
