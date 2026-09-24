//! The staff comp's INPUT: which sizes and which required choice groups a staff
//! drink's base is judged on — madar-shared's `madar_catalog::staff::
//! comp_input`, over the order's loaded catalogue (`orders::catalog_view`).
//!
//! It replaced this server's SQL builder (`staff_pool::order_line::
//! comp_input`, which read the same rows per line); the input that builder
//! made for every case below is madar-catalog's `staff_input_vectors.json`,
//! written by this suite while the two ran side by side. This suite seeds a
//! catalogue with every case the rule reads — sizes and a label only the
//! branch prices, a required group with a default, an optional group, a group
//! flagged required with a zero minimum, a swap group, a legacy milk group, an
//! allow-listed attachment with its own minimum, an allow-list that leaves
//! nothing, an inactive group, a switched-off option and add-on, a branch
//! price and a branch that turned an option off — and checks that this
//! server's loader builds the views recorded there and the crate's input over
//! them is the one recorded. `MADAR_WRITE_STAFF_INPUT_VECTORS=1` rewrites the
//! file in the madar-shared checkout beside this one
//! (`crates/madar-catalog/vectors/`) — a deliberate change of the rule.
use madar_catalog::staff::{StaffLine, comp_input, vectors::StaffInputVector};
use madar_rust::orders::catalog_view::Catalog;
use madar_rust::staff_pool::comp::CompPick;
use sqlx::PgPool;
use uuid::Uuid;

fn id(label: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("staff-input:{label}").as_bytes())
}

fn vectors_out() -> std::path::PathBuf {
    let shared = std::env::var("MADAR_SHARED_DIR")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../madar-shared").into());
    std::path::Path::new(&shared).join("crates/madar-catalog/vectors/staff_input_vectors.json")
}

const FIXTURE: &str = r#"
INSERT INTO organizations (id, name, slug) VALUES ('{id:org}', 'Staff Input', 'staff-input-{id:org}');
INSERT INTO branches (id, org_id, name) VALUES ('{id:branch}', '{id:org}', 'Main');
INSERT INTO categories (id, org_id, name) VALUES ('{id:cat}', '{id:org}', 'Hot');
INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES
  ('{id:latte}', '{id:org}', '{id:cat}', 'Latte', 4000, true),
  ('{id:tea}',   '{id:org}', '{id:cat}', 'Tea',   2500, true);
DELETE FROM menu_item_sizes WHERE menu_item_id = '{id:latte}';
INSERT INTO menu_item_sizes (menu_item_id, label, price, sort, is_active) VALUES
  ('{id:latte}', 'S', 4000, 0, true),
  ('{id:latte}', 'M', 5000, 1, true),
  ('{id:latte}', 'L', 6000, 2, false);
INSERT INTO branch_menu_size_overrides (branch_id, menu_item_id, size_label, price_override) VALUES
  ('{id:branch}', '{id:latte}', 'M', 5200),
  ('{id:branch}', '{id:latte}', 'XL', 9000);

INSERT INTO modifier_groups (id, org_id, name, selection_type, min_selections, is_required, legacy_addon_type, effect, is_active) VALUES
  ('{id:syrup}',   '{id:org}', 'Syrup',   'multi',  1, true,  NULL,        'none',  true),
  ('{id:extras}',  '{id:org}', 'Extras',  'multi',  0, false, NULL,        'adds',  true),
  ('{id:sauce}',   '{id:org}', 'Sauce',   'multi',  0, true,  NULL,        'adds',  true),
  ('{id:milk}',    '{id:org}', 'Milk',    'single', 1, true,  NULL,        'swaps', true),
  ('{id:legacy}',  '{id:org}', 'Old milk','multi',  1, true,  'milk_type', 'none',  true),
  ('{id:listed}',  '{id:org}', 'Toppings','multi',  0, false, NULL,        'adds',  true),
  ('{id:emptied}', '{id:org}', 'Crumbs',  'multi',  1, true,  NULL,        'adds',  true),
  ('{id:off}',     '{id:org}', 'Retired', 'multi',  1, true,  NULL,        'adds',  false);

INSERT INTO addon_items (id, org_id, name, type, default_price, is_active) VALUES
  ('{id:vanilla}',   '{id:org}', 'Vanilla',    'extra', 1000, true),
  ('{id:hazelnut}',  '{id:org}', 'Hazelnut',   'extra', 1500, true),
  ('{id:sugarfree}', '{id:org}', 'Sugar free', 'extra',  500, true),
  ('{id:whip}',      '{id:org}', 'Whip',       'extra',  700, true),
  ('{id:saucea}',    '{id:org}', 'Sauce A',    'extra',  300, true),
  ('{id:sauceb}',    '{id:org}', 'Sauce B',    'extra',  400, true),
  ('{id:whole}',     '{id:org}', 'Whole',      'extra',    0, true),
  ('{id:oat}',       '{id:org}', 'Oat',        'extra',  800, true),
  ('{id:skim}',      '{id:org}', 'Skim',       'milk_type', 200, true),
  ('{id:x}',         '{id:org}', 'X',          'extra',  100, true),
  ('{id:y}',         '{id:org}', 'Y',          'extra',  200, false),
  ('{id:z}',         '{id:org}', 'Z',          'extra',  300, true),
  ('{id:crumb}',     '{id:org}', 'Crumb',      'extra',  100, true),
  ('{id:gone}',      '{id:org}', 'Gone',       'extra',  100, true);
INSERT INTO modifier_options (id, group_id, name, price, sort, is_default, is_active) VALUES
  ('{id:vanilla}',   '{id:syrup}',   'Vanilla',    1000, 0, true,  true),
  ('{id:hazelnut}',  '{id:syrup}',   'Hazelnut',   1500, 1, false, true),
  ('{id:sugarfree}', '{id:syrup}',   'Sugar free',  500, 2, false, false),
  ('{id:whip}',      '{id:extras}',  'Whip',        700, 0, false, true),
  ('{id:saucea}',    '{id:sauce}',   'Sauce A',     300, 0, false, true),
  ('{id:sauceb}',    '{id:sauce}',   'Sauce B',     400, 1, true,  true),
  ('{id:whole}',     '{id:milk}',    'Whole',         0, 0, true,  true),
  ('{id:oat}',       '{id:milk}',    'Oat',         800, 1, false, true),
  ('{id:skim}',      '{id:legacy}',  'Skim',        200, 0, false, true),
  ('{id:x}',         '{id:listed}',  'X',           100, 0, false, true),
  ('{id:y}',         '{id:listed}',  'Y',           200, 1, true,  true),
  ('{id:z}',         '{id:listed}',  'Z',           300, 2, false, true),
  ('{id:crumb}',     '{id:emptied}', 'Crumb',       100, 0, false, true),
  ('{id:gone}',      '{id:off}',     'Gone',        100, 0, false, true);
INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort) VALUES
  ('{id:latte}', '{id:syrup}',   0),
  ('{id:latte}', '{id:extras}',  1),
  ('{id:latte}', '{id:sauce}',   2),
  ('{id:latte}', '{id:milk}',    3),
  ('{id:latte}', '{id:legacy}',  4),
  ('{id:latte}', '{id:listed}',  5),
  ('{id:latte}', '{id:emptied}', 6),
  ('{id:latte}', '{id:off}',     7);
UPDATE menu_item_modifier_groups SET min_override = 2, is_required_override = true,
       included_option_ids = ARRAY['{id:y}', '{id:z}']::uuid[]
 WHERE menu_item_id = '{id:latte}' AND group_id = '{id:listed}';
UPDATE menu_item_modifier_groups SET included_option_ids = ARRAY[]::uuid[]
 WHERE menu_item_id = '{id:latte}' AND group_id = '{id:emptied}';
INSERT INTO branch_addon_overrides (branch_id, addon_item_id, price_override, is_available) VALUES
  ('{id:branch}', '{id:hazelnut}', 1600, true),
  ('{id:branch}', '{id:saucea}',   NULL, false);
"#;

fn fixture_sql() -> String {
    let mut sql = FIXTURE.to_string();
    while let Some(start) = sql.find("{id:") {
        let end = start + sql[start..].find('}').unwrap();
        let label = sql[start + 4..end].to_string();
        sql.replace_range(start..=end, &id(&label).to_string());
    }
    sql
}

struct Case {
    name: String,
    item: Uuid,
    line: StaffLine,
}

fn cases() -> Vec<Case> {
    let pick = |option: &str, unit_price: i32, quantity: i32| CompPick {
        option_id: id(option).to_string(),
        unit_price,
        quantity,
    };
    let picks: Vec<(&str, Vec<CompPick>)> = vec![
        ("no_picks", vec![]),
        ("default_syrup", vec![pick("vanilla", 1000, 1)]),
        (
            "dearer_syrup_and_whip",
            vec![pick("hazelnut", 1600, 1), pick("whip", 700, 1)],
        ),
        ("two_toppings", vec![pick("y", 200, 1), pick("z", 300, 2)]),
        ("oat_swap", vec![pick("oat", 800, 1)]),
    ];
    let mut out = Vec::new();
    for (item, sizes) in [
        ("latte", vec![None, Some("S"), Some("M"), Some("L")]),
        ("tea", vec![None]),
    ] {
        for size in &sizes {
            for (pname, p) in &picks {
                for (eligible, qty) in [(true, 1), (true, 2), (false, 1)] {
                    let unit_price = match *size {
                        Some("M") => 5200,
                        Some("L") => 6000,
                        Some(_) | None if item == "latte" => 4000,
                        _ => 2500,
                    };
                    out.push(Case {
                        name: format!(
                            "{item}_{}_{pname}_{}x{qty}",
                            size.unwrap_or("sizeless"),
                            if eligible { "eligible" } else { "refused" }
                        ),
                        item: id(item),
                        line: StaffLine {
                            size_label: size.map(str::to_string),
                            eligible,
                            unit_price,
                            picks: p.clone(),
                            optionals_per_unit: if *pname == "no_picks" { 0 } else { 150 },
                            quantity: qty,
                        },
                    });
                }
            }
        }
    }
    out
}

#[sqlx::test]
async fn the_shared_input_is_the_servers(pool: PgPool) {
    sqlx::raw_sql(&fixture_sql()).execute(&pool).await.unwrap();
    let branch = id("branch");
    let mut catalog = Catalog::new(Some(branch));
    catalog
        .ensure_on(&pool, &[id("latte"), id("tea")], &[])
        .await
        .unwrap();

    let recorded: Vec<StaffInputVector> =
        serde_json::from_str(madar_catalog::vectors::STAFF_INPUT).unwrap();
    let mut vectors = Vec::new();
    for c in cases() {
        let view = &catalog.item(c.item).expect("loaded").view;
        let input = comp_input(view, &c.line);
        if let Some(r) = recorded.iter().find(|r| r.name == c.name) {
            assert_eq!(&r.item, view, "{}: the loader's view", c.name);
            assert_eq!(r.line, c.line, "{}", c.name);
            assert_eq!(input, r.expected, "{}: the input", c.name);
        } else if std::env::var("MADAR_WRITE_STAFF_INPUT_VECTORS").is_err() {
            panic!("{} is not in staff_input_vectors.json", c.name);
        }
        vectors.push(StaffInputVector {
            name: c.name,
            item: view.clone(),
            line: c.line,
            expected: input,
        });
    }
    assert_eq!(vectors.len(), recorded.len().max(vectors.len()));
    // The fixture reaches every branch of the rule.
    let latte = &catalog.item(id("latte")).unwrap().view;
    let groups: Vec<String> = comp_input(
        latte,
        &StaffLine {
            size_label: Some("S".into()),
            eligible: true,
            unit_price: 4000,
            ..Default::default()
        },
    )
    .groups
    .into_iter()
    .map(|g| g.id)
    .collect();
    assert_eq!(
        groups,
        [id("syrup"), id("sauce"), id("listed")].map(|g| g.to_string())
    );
    assert_eq!(latte.groups.len(), 7, "every active attached group is loaded");

    if std::env::var("MADAR_WRITE_STAFF_INPUT_VECTORS").is_ok() {
        std::fs::write(
            vectors_out(),
            serde_json::to_string_pretty(&vectors).unwrap() + "\n",
        )
        .unwrap();
    }
}
