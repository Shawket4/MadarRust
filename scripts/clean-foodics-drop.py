#!/usr/bin/env python3
"""Clean a Foodics modifier export into Madar's four modifier families.

Reads  drops/{modifier_groups,modifier_options,product_modifiers,products}.csv
Writes drops/cleaned_{modifier_groups,modifier_options,product_modifiers}.csv
       drops/cleaned_product_optionals.csv   (item-private free/priced extras)

What it does, and why (all of it is recorded in FOODICS_DROP_CLEANUP.md):

* Collapses the 9 per-drink milk groups (67 rows) into ONE `milk_type` group of
  6 options. Prices were already consistent across every group: full cream and
  skimmed 0, oat / almond / coconut / lactose-free 55.
* Keeps the bean groups SEPARATE per brew method (Coffee Beans / Espresso Beans
  / V60 Beans, all `coffee_type`) — merging them offered a V60 bean on an
  americano, which Foodics never did.
* `CAN`/`CUP` becomes the `size` family — Madar models sizes as menu_item_sizes
  (own price, rendered as chips above the option cards), not as a modifier group.
* Extras are deduplicated and the spelling fixed.
* cake (a required upsell on 76 coffees) and Talabat (a delivery surcharge) are
  DROPPED on the owner's instruction.
* BREAD, the Turkish coffee choices and the mojito flavours become item-private
  OPTIONALS, the way the matcha drinks carry Honey / Condensed Milk / Vanilla.

`id` is left blank everywhere: Postgres mints the UUIDs.
"""
import csv
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))
DROPS = os.path.join(os.path.dirname(HERE), "drops")

GROUP_COLS = ["id", "reference", "name", "name_localized"]
OPTION_COLS = ["id", "modifier_reference", "modifier_name", "modifier_name_localized",
               "tax_group_reference", "name", "sku", "price", "calories",
               "name_localized", "is_active"]
LINK_COLS = ["product_name", "product_name_localized", "product_sku", "modifier_name",
             "modifier_name_localized", "modifier_reference", "minimum_options",
             "maximum_options", "free_options", "default_options", "unique_options"]
OPTIONAL_COLS = ["product_sku", "product_name", "name", "name_localized", "price"]


def read(name):
    with open(os.path.join(DROPS, name), encoding="utf-8-sig", newline="") as f:
        return list(csv.DictReader(f))


def write(name, cols, rows):
    path = os.path.join(DROPS, name)
    with open(path, "w", encoding="utf-8", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols, quoting=csv.QUOTE_MINIMAL)
        w.writeheader()
        for r in rows:
            w.writerow({c: r.get(c, "") for c in cols})
    print(f"  {name:34} {len(rows):4} rows")


# ── the four families ───────────────────────────────────────────────────────
GROUPS = [
    # reference, EN name, AR name
    ("size",          "Size",           "الحجم"),
    ("milk_type",     "Milk",           "الحليب"),
    ("coffee_type",   "Coffee Beans",   "حبوب القهوة"),
    ("coffee_esp",    "Espresso Beans", "حبوب الاسبريسو"),
    ("coffee_v60",    "V60 Beans",      "حبوب V60"),
    ("extra",         "Extras",         "الإضافات"),
]

# Which reference maps to which addon family for the importer.
FAMILY_REF = {"coffee_esp": "coffee_type", "coffee_v60": "coffee_type"}

# Every milk option seen across the 9 groups, deduplicated. Price is the one
# every group already agreed on.
MILK = [
    ("opt-milk-full",    "Full Cream",   "حليب كامل الدسم", 0),
    ("opt-milk-skimmed", "Skimmed",      "حليب خالي الدسم", 0),
    ("opt-milk-oat",     "Oat",          "حليب الشوفان",    55),
    ("opt-milk-almond",  "Almond",       "حليب اللوز",      55),
    ("opt-milk-coconut", "Coconut",      "حليب جوز الهند",  55),
    ("opt-milk-lactose", "Lactose Free", "خالي اللاكتوز",   55),
]

# Beans are NOT one list: Foodics had a different set per brew method, and
# merging them offered a V60 bean on an americano. Three groups, all
# legacy_addon_type 'coffee_type', keyed by which drink they belong to.
COFFEE = [
    ("opt-bean-house",     "House Blend", "الخلطة الخاصة", 0),
    ("opt-bean-colombian", "Colombian",   "كولومبي",       20),
    ("opt-bean-decaf",     "Decaf",       "منزوع الكافيين", 30),
]
COFFEE_ESPRESSO = [
    ("opt-bean-col-esp", "Colombian Espresso", "كولومبي اسبريسو", 0),
    ("opt-bean-decaf",   "Decaf",              "منزوع الكافيين",  30),
]
COFFEE_V60 = [
    ("opt-bean-col-v60", "Colombian V60", "كولومبي", 0),
    ("opt-bean-eth-v60", "Ethiopian V60", "اثيوبي",  20),
]
# Foodics group name → which of the three sets that product's drinks use.
ESPRESSO_ITEMS = {"psk-2", "psk-4", "sk-0512"}          # Espresso, Macchiato, loyalty
V60_ITEMS = {"psk-9", "psk-20"}                          # V60, Iced V60

SIZES = [
    ("opt-size-cup", "Cup", "كوب", 0),
    ("opt-size-can", "Can", "كان", 25),
]

# Deduplicated, typo-fixed, one price each. "Talabat" (a delivery surcharge) is
# dropped; sugar-free syrups keep their own rows because they price the same but
# are a different product on the shelf.
EXTRAS = [
    ("opt-shot",            "Extra Shot",              "شوت إضافي",            40),
    ("opt-whipped-cream",   "Whipped Cream",           "كريمة مخفوقة",         40),
    ("opt-condensed-milk",  "Condensed Milk",          "حليب مكثف",            50),
    ("opt-extra-sauce",     "Extra Sauce",             "صوص إضافي",            50),
    ("opt-honey",           "Honey",                   "عسل",                  20),
    ("opt-vanilla",         "Vanilla Syrup",           "شراب الفانيليا",       30),
    ("opt-caramel",         "Caramel Syrup",           "شراب الكراميل",        30),
    ("opt-hazelnut",        "Hazelnut Syrup",          "شراب البندق",          30),
    ("opt-vanilla-sf",      "Vanilla Syrup (Sugar Free)",       "شراب الفانيليا خالي السكر", 30),
    ("opt-caramel-sf",      "Caramel Syrup (Sugar Free)",       "شراب الكراميل خالي السكر",  30),
    ("opt-salted-caramel-sf", "Salted Caramel Syrup (Sugar Free)", "شراب الكراميل المملح خالي السكر", 30),
    ("opt-hazelnut-sf",     "Hazelnut Syrup (Sugar Free)",      "شراب البندق خالي السكر",    30),
]

OPTIONS = {"size": SIZES, "milk_type": MILK, "coffee_type": COFFEE,
           "coffee_esp": COFFEE_ESPRESSO, "coffee_v60": COFFEE_V60, "extra": EXTRAS}

# Foodics group name → the family that replaces it.
FAMILY_OF = {
    "iced latte milk": "milk_type", "hot latte milk": "milk_type",
    "blended matcha": "milk_type", "french milk": None,
    "flat milk": "milk_type", "capuccino milk": "milk_type",
    "cortado milk": "milk_type", "blended cold milk": "milk_type",
    "iced cold milk": "milk_type", "hot milk": "milk_type",
    "espresso beans": "coffee_type", "coffee beans": "coffee_type",
    "حبوب v60": "coffee_type",
    "can": "size",
    "extras": "extra",
    # dropped outright (owner)
    "cake": None, "talabat": None,
    # become item-private optionals
    "bread": None, "mojitos flavours": None,
    "turkish coffee": None, "حبوب القهوة التركي": None,
}

# Foodics group name → the optionals it becomes, per product that carried it.
AS_OPTIONALS = {
    "bread": [("Brown Bread", "خبز أسمر", 25)],
    "mojitos flavours": [("Passion Fruit", "باشن فروت", 0), ("Mixed Berries", "توت مشكل", 0),
                         ("Strawberry", "فراولة", 0), ("Mango", "مانجو", 0)],
    # Turkish coffee is NOT optionals any more (owner, 2026-09-16): Single/Double
    # are SIZES (4oz 10 g / 8oz 20 g), Mehaweg is a coffee_type swap against
    # Turkish Coffee, and "With Milk" became its own item, French coffee.
}


def main():
    groups = read("modifier_groups.csv")
    links = read("product_modifiers.csv")
    products = read("products.csv")
    name_of = {p["sku"]: p["name"].strip() for p in products}

    known = {g["name"].strip().lower() for g in groups}
    unmapped = known - set(FAMILY_OF)
    if unmapped:
        raise SystemExit(f"unmapped Foodics groups (add them to FAMILY_OF): {sorted(unmapped)}")

    print("writing:")

    write("cleaned_modifier_groups.csv", GROUP_COLS,
          [{"id": "", "reference": ref, "name": en, "name_localized": ar}
           for ref, en, ar in GROUPS])

    opt_rows = []
    for ref, en, _ar in GROUPS:
        for sku, oen, oar, price in OPTIONS[ref]:
            opt_rows.append({
                "id": "", "modifier_reference": ref, "modifier_name": en,
                "modifier_name_localized": dict((r, a) for r, _e, a in GROUPS)[ref],
                "tax_group_reference": "tx-1", "name": oen, "sku": sku,
                "price": price, "calories": "", "name_localized": oar, "is_active": "Yes",
            })
    write("cleaned_modifier_options.csv", OPTION_COLS, opt_rows)

    # ── product links, in the owner's hierarchy: size → milk → coffee → extra ──
    order = {ref: i for i, (ref, _e, _a) in enumerate(GROUPS)}
    wanted = {}          # (sku, family) → limits
    optionals = []       # item-private rows
    for l in links:
        gname = l["modifier_name"].strip().lower()
        sku = l["product_sku"].strip()
        if not sku:
            continue
        for oen, oar, price in AS_OPTIONALS.get(gname, []):
            optionals.append({"product_sku": sku, "product_name": name_of.get(sku, ""),
                              "name": oen, "name_localized": oar, "price": price})
        fam = FAMILY_OF.get(gname)
        if fam is None:
            continue
        if fam == "coffee_type":
            fam = ("coffee_esp" if sku in ESPRESSO_ITEMS
                   else "coffee_v60" if sku in V60_ITEMS else "coffee_type")
        # A family a product already has stays at its widest limits.
        mn = int(l["minimum_options"] or 0)
        mx = l["maximum_options"].strip()
        if fam == "extra":
            mn, mx = 0, ""                     # extras are never required
        elif fam in ("size", "milk_type", "coffee_type", "coffee_esp", "coffee_v60"):
            mn, mx = 1, "1"                    # exactly one, the DB trigger agrees
        prev = wanted.get((sku, fam))
        if prev is None or mn < prev[0]:
            wanted[(sku, fam)] = (mn, mx)

    link_rows = []
    for (sku, fam), (mn, mx) in sorted(wanted.items(), key=lambda kv: (kv[0][0], order[kv[0][1]])):
        en = dict((r, e) for r, e, _a in GROUPS)[fam]
        ar = dict((r, a) for r, _e, a in GROUPS)[fam]
        link_rows.append({
            "product_name": name_of.get(sku, ""), "product_name_localized": "",
            "product_sku": sku, "modifier_name": en, "modifier_name_localized": ar,
            "modifier_reference": fam, "minimum_options": mn, "maximum_options": mx,
            "free_options": 0, "default_options": "", "unique_options": "No",
        })
    write("cleaned_product_modifiers.csv", LINK_COLS, link_rows)

    seen = set()
    uniq = []
    for r in optionals:
        key = (r["product_sku"], r["name"])
        if key not in seen:
            seen.add(key)
            uniq.append(r)
    write("cleaned_product_optionals.csv", OPTIONAL_COLS, uniq)

    fams = {}
    for (_sku, fam) in wanted:
        fams[fam] = fams.get(fam, 0) + 1
    print("\nproducts per family:", {k: fams.get(k, 0) for k, _e, _a in GROUPS})
    print("optionals:", len(uniq), "rows on",
          len({r['product_sku'] for r in uniq}), "products")


if __name__ == "__main__":
    main()
