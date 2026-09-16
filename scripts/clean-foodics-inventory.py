#!/usr/bin/env python3
"""Clean the Foodics "Inventory Levels Report" into a Madar ingredient catalog.

Reads  drops_clean/Inventory Levels Report.csv
Writes drops_clean/ingredients.csv  (sku,name,name_ar,unit,category,swap_key)

Catalog ONLY — no stock levels. The Foodics quantities are not trustworthy
(38 of 116 are negative, honey reads 2,401 kg, four syrups read ~900 kg), and
the owner wants the first stocktake to set reality. Every ingredient therefore
lands with no movements, so book stock is 0 and the first count is the truth.

`category` is the ingredient_categories slug. `milk` and `coffee_bean` are the
two the swap logic keys on (orders/component_resolve.rs): a `milk_type` option
swaps the drink recipe's `milk` ingredient, a `coffee_type` option swaps its
`coffee_bean` one. `swap_key` names which cleaned modifier option this
ingredient backs, so the option→ingredient recipe lines can be written.
"""
import csv
import os

HERE = os.path.dirname(os.path.abspath(__file__))
DROPS = os.path.join(os.path.dirname(HERE), "drops_clean")
SRC = os.path.join(DROPS, "Inventory Levels Report.csv")
OUT = os.path.join(DROPS, "ingredients.csv")

# Foodics storage unit → Madar inventory_unit (g | kg | ml | l | pcs).
# Owner decision 2026-09-16: everything weighed is stored in GRAMS, never kg —
# a recipe pours grams, so the catalog unit and the recipe unit match and no
# conversion sits between a count and a deduction.
UNIT = {
    "لتر": "l", "كيلو": "g", "جرام": "g", "باكت كيلو": "g",
    "قطعة": "pcs", "كوب": "pcs", "كانز": "pcs", "زجاجة": "pcs",
    "باكت": "pcs", "بورشن": "pcs", "شريحة": "pcs", "فلتر": "pcs",
    "كيس": "pcs", "علبة": "pcs", "شنطة": "pcs",
}

# sku → (English name, Arabic name, category slug, swap_key or "")
# Arabic is the original, tidied; English is the POS/dashboard display name.
MAP = {
    # ── milk (the swap family) ──────────────────────────────────────────────
    "sk-0021": ("Lactose Free Milk", "لبن خالي اللاكتوز", "milk", "Lactose Free"),
    "sk-0022": ("Full Cream Milk", "لبن كامل الدسم", "milk", "Full Cream"),
    "sk-0023": ("Skimmed Milk", "لبن خالي الدسم", "milk", "Skimmed"),
    "sk-0024": ("Almond Milk", "لبن اللوز", "milk", "Almond"),
    "sk-0163": ("Coconut Milk", "لبن جوز الهند", "milk", "Coconut"),
    "sk-0197": ("Oat Milk", "لبن الشوفان", "milk", "Oat"),
    "sk-0198": ("Rifi Full Cream Milk", "ريفي كامل الدسم", "milk", ""),
    "sk-0109": ("Condensed Milk", "لبن مكثف", "dairy", ""),
    "sk-0162": ("Non-Dairy Cream", "كريمة نباتي", "dairy", ""),
    # ── coffee beans (the swap family) ──────────────────────────────────────
    "sk-0025": ("House Blend Beans", "بن صن رايز دي هاوس", "coffee_bean", "House Blend"),
    "sk-0026": ("Colombian Espresso", "بن اسبريسو كولومبي", "coffee_bean", "Colombian Espresso"),
    "sk-0027": ("Colombian V60", "بن فلتر كولومبي", "coffee_bean", "Colombian V60"),
    "sk-0254": ("Decaf Beans", "بن دي كاف", "coffee_bean", "Decaf"),
    # No Foodics stock row: the +20 Ethiopian V60 option was sold but never counted.
    "new-ethiopian-v60": ("Ethiopian V60", "بن فلتر اثيوبي", "coffee_bean", "Ethiopian V60"),
    "sk-0264": ("Turkish Coffee", "بن تركي", "coffee_bean", ""),
    # ── syrups ─────────────────────────────────────────────────────────────
    "sk-0113": ("Vanilla Syrup", "سيرب فانيليا", "syrup", ""),
    "sk-0116": ("Salted Almond Syrup", "سيرب لوز مملح", "syrup", ""),
    "sk-0117": ("Mango Syrup", "سيرب مانجو", "syrup", ""),
    "sk-0123": ("Coconut Syrup", "سيرب جوز هند", "syrup", ""),
    "sk-0127": ("Mint Cubana Syrup", "سيرب منتا كوبانا", "syrup", ""),
    "sk-0129": ("Passion Fruit Syrup", "سيرب باشون فروت", "syrup", ""),
    "sk-0130": ("Spiced Chai Syrup", "سيرب سبايسي شاي", "syrup", ""),
    "sk-0132": ("Blueberry Syrup", "سيرب بلو بيري", "syrup", ""),
    "sk-0134": ("European Strawberry Syrup", "سيرب فراولة أوروبية", "syrup", ""),
    "sk-0135": ("Lemon Syrup", "سيرب ليمون", "syrup", ""),
    "sk-0136": ("Peach Syrup", "سيرب خوخ", "syrup", ""),
    "sk-0146": ("Ocean Blue Syrup", "سيرب أوشن بلو", "syrup", ""),
    "sk-0165": ("Mint Syrup", "سيرب نعناع", "syrup", ""),
    "sk-0192": ("Caramel Syrup", "سيرب كراميل", "syrup", ""),
    "sk-0193": ("Hazelnut Syrup", "سيرب بندق", "syrup", ""),
    "sk-0257": ("Vanilla Syrup (Sugar Free)", "سيرب فانيليا بدون سكر", "syrup", ""),
    "sk-0258": ("Caramel Syrup (Sugar Free)", "سيرب كراميل بدون سكر", "syrup", ""),
    "sk-0259": ("Hazelnut Syrup (Sugar Free)", "سيرب بندق بدون سكر", "syrup", ""),
    "sk-0260": ("Salted Caramel Syrup (Sugar Free)", "سيرب كراميل مملح بدون سكر", "syrup", ""),
    # ── sauces ─────────────────────────────────────────────────────────────
    "sk-0110": ("Pistachio Sauce", "صوص فستق", "sauce", ""),
    "sk-0112": ("Salted Caramel Sauce", "صوص كراميل مملح", "sauce", ""),
    "sk-0115": ("White Chocolate Sauce", "صوص وايت شيكولاتة", "sauce", ""),
    "sk-0118": ("Mango Sauce", "صوص مانجو", "sauce", ""),
    "sk-0119": ("Brown Butter Sauce", "صوص براون باتر", "sauce", ""),
    "sk-0124": ("Caramel Sauce", "صوص كراميل", "sauce", ""),
    "sk-0125": ("Strawberry Sauce", "صوص فراولة", "sauce", ""),
    "sk-0133": ("Mixed Berries Sauce", "صوص ميكس بيري", "sauce", ""),
    "sk-0164": ("Pineapple Sauce", "صوص أناناس", "sauce", ""),
    "sk-0191": ("Passion Fruit Sauce", "صوص باشون فروت", "sauce", ""),
    "sk-0194": ("Chocolate Sauce", "صوص شيكولاتة", "sauce", ""),
    "sk-0274": ("Cinnamon Sauce", "صوص سينامون", "sauce", ""),
    "sk-0243": ("Honey Mustard Sauce", "صوص هوني مسترد", "sauce", ""),
    "sk-0293": ("Sweet Chilli Sauce", "صوص سويت تشيلي", "sauce", ""),
    "sk-0236": ("Mayonnaise", "مايونيز", "sauce", ""),
    # ── dry goods / powders ────────────────────────────────────────────────
    "sk-0120": ("Vanilla Frappe Powder", "فانيليا فرابيه", "dry_goods", ""),
    "sk-0121": ("Chocolate Powder", "شيكولاتة بودر", "dry_goods", ""),
    "sk-0140": ("White Sugar", "سكر أبيض", "dry_goods", ""),
    "sk-0160": ("Matcha", "ماتشا", "dry_goods", ""),
    "sk-0166": ("Dark Chocolate Chips", "شيكولاتة دارك شيبس", "dry_goods", ""),
    "sk-0167": ("Milk Chocolate Chips", "شيكولاتة ميلك شيبس", "dry_goods", ""),
    "sk-0199": ("Honey", "عسل", "dry_goods", ""),
    "sk-0294": ("Smoothie Powder", "بودر سموزي", "dry_goods", ""),
    # ── tea ────────────────────────────────────────────────────────────────
    "sk-0137": ("Black Tea", "شاي أسود", "tea", ""),
    "sk-0188": ("Tea Bags", "باكت شاي", "tea", ""),
    "sk-0190": ("Assam Tea", "شاي عاصم", "tea", ""),
    "sk-0250": ("Green Tea Bags", "باكت شاي أخضر", "tea", ""),
    # ── bottled drinks ─────────────────────────────────────────────────────
    "sk-0142": ("7up", "سفن أب", "beverage", ""),
    "sk-0143": ("Red Bull", "ريد بول", "beverage", ""),
    "sk-0144": ("Red Bull Coconut", "ريد بول جوز هند", "beverage", ""),
    "sk-0187": ("Bovana Mineral Water", "مياه بوفانا معدنية", "beverage", ""),
    "sk-0196": ("Sparkling Water", "مياه فوارة", "beverage", ""),
    "sk-0287": ("Thomas Henry Ginger", "توماس هنري جينجر", "beverage", ""),
    "sk-0288": ("Thomas Henry Grapefruit", "توماس هنري جريب فروت", "beverage", ""),
    "sk-0290": ("Strawberry Juice", "عصير فراولة", "beverage", ""),
    "sk-0291": ("Orange Juice", "عصير برتقال", "beverage", ""),
    "sk-0292": ("Mango Juice", "عصير مانجو", "beverage", ""),
    # ── bakery / desserts sold as stock ────────────────────────────────────
    "sk-0181": ("Classic Cookies", "كوكيز كلاسيك", "bakery", ""),
    "sk-0182": ("English Cake", "إنجليش كيك", "bakery", ""),
    "sk-0185": ("Coffee Biscuits", "بسكويت قهوة", "bakery", ""),
    "sk-0186": ("Almond Biscuits", "بسكويت اللوز", "bakery", ""),
    "sk-0200": ("Marble Cake", "ماربل كيك", "bakery", ""),
    "sk-0220": ("Nutella Cookies", "كوكيز نوتيلا", "bakery", ""),
    "sk-0221": ("Brookies", "بروكيز", "bakery", ""),
    "sk-0269": ("Berries Cheesecake", "تشيز كيك بالتوت", "bakery", ""),
    "sk-0270": ("Lotus Cheesecake", "تشيز كيك لوتس", "bakery", ""),
    "sk-0272": ("Banoffee Jar", "بانوفي جار", "bakery", ""),
    "sk-0295": ("Brownies", "براونيز", "bakery", ""),
    "sk-0296": ("Tiramisu", "تيراميسو", "bakery", ""),
    "sk-0298": ("Cookie Bag", "كيس كوكيز", "bakery", ""),
    "sk-0300": ("Kinder Cookies", "كوكيز كيندر", "bakery", ""),
    "sk-0228": ("Dark Chocolate Bar (Sugar Free)", "شيكولاتة بار دارك خالي من السكر", "bakery", ""),
    "sk-0229": ("Milk Chocolate Bar (Sugar Free)", "شيكولاتة بار ميلك خالي من السكر", "bakery", ""),
    "sk-0275": ("Coffee Dark Chocolate Bar (Sugar Free)", "كوفي دارك شيكولاتة بار بدون سكر", "bakery", ""),
    # ── sandwich fillings ──────────────────────────────────────────────────
    "sk-0233": ("Onion", "بصل", "food", ""),
    "sk-0234": ("Rocket", "جرجير", "food", ""),
    "sk-0235": ("Roast Beef", "روست بيف", "food", ""),
    "sk-0237": ("Cheddar (Roast Beef)", "جبنة شيدر روست بيف", "food", ""),
    "sk-0238": ("Lettuce", "خس", "food", ""),
    "sk-0239": ("Cucumber", "خيار", "food", ""),
    "sk-0240": ("Turkey", "تركي", "food", ""),
    "sk-0241": ("Olives", "زيتون", "food", ""),
    "sk-0242": ("Chicken", "دجاج", "food", ""),
    "sk-0244": ("Sun-Dried Tomato", "طماطم مجففة", "food", ""),
    "sk-0245": ("Mozzarella", "جبنة موتزاريلا", "food", ""),
    "sk-0246": ("White Ciabatta", "خبز شباتة أبيض", "food", ""),
    "sk-0248": ("Brown Ciabatta", "خبز شباتة بني", "food", ""),
    "sk-0251": ("Cheddar (Chicken)", "جبنة شيدر للدجاج", "food", ""),
    "sk-0252": ("Cheddar (Turkey)", "جبنة شيدر تركي", "food", ""),
    # ── packaging & merchandise ────────────────────────────────────────────
    "sk-0156": ("Cup 4oz", "كوب ٤", "packaging", ""),
    "sk-0157": ("Cup 8oz", "كوب ٨", "packaging", ""),
    "sk-0158": ("Cup 12oz", "كوب ١٢", "packaging", ""),
    "sk-0159": ("Can", "كانز", "packaging", ""),
    "sk-0224": ("Coffee Cup Small", "كوب قهوة صغير", "packaging", ""),
    "sk-0225": ("Coffee Cup Medium", "كوب قهوة وسط", "packaging", ""),
    "sk-0226": ("Coffee Cup Large", "كوب قهوة كبير", "packaging", ""),
    "sk-0249": ("V60 Paper Filter", "فلتر ورق V60", "packaging", ""),
    "sk-0276": ("Coffee Bean Bag", "كيس حبوب القهوة", "packaging", ""),
    "sk-0277": ("Plastic Cup 16oz", "كوب بلاستيك ١٦", "packaging", ""),
    "sk-0297": ("Straw", "شاليمو", "packaging", ""),
    "sk-0299": ("Craft Bag", "شنطة كرافت", "packaging", ""),
    "sk-0263": ("Hoodie", "هودي", "merch", ""),
}

# Dropped: duplicates of a bean already in the catalog, both reading 0.
DROP = {
    "sk-0168": "duplicate of sk-0026 Espresso Beans (packet vs kg, qty 0)",
    "sk-0169": "duplicate of sk-0025 House Blend Beans (packet vs kg, qty 0)",
}

CATEGORIES = [
    ("milk", "Milk", 0), ("coffee_bean", "Coffee Bean", 1), ("dairy", "Dairy", 2),
    ("syrup", "Syrup", 3), ("sauce", "Sauce", 4), ("dry_goods", "Dry Goods", 5),
    ("tea", "Tea", 6), ("beverage", "Beverage", 7), ("bakery", "Bakery", 8),
    ("food", "Food", 9), ("packaging", "Packaging", 10), ("merch", "Merchandise", 11),
]


# Foodics stored two milks by weight; every other milk is by volume, and a swap
# family has to be comparable, so they are corrected to litres.
UNIT_OVERRIDE = {"sk-0197": "l", "sk-0198": "l"}


def main():
    with open(SRC, encoding="utf-8-sig", newline="") as f:
        rows = list(csv.DictReader(f))

    missing = [r for r in rows if r["SKU"] not in MAP and r["SKU"] not in DROP]
    if missing:
        raise SystemExit("unmapped SKUs: " + ", ".join(f"{r['SKU']} {r['Name']}" for r in missing))

    out = []
    for r in rows:
        sku = r["SKU"]
        if sku in DROP:
            continue
        en, ar, cat, swap = MAP[sku]
        unit = UNIT_OVERRIDE.get(sku) or UNIT.get(r["Storage Unit"].strip())
        if unit is None:
            raise SystemExit(f"unmapped unit {r['Storage Unit']!r} on {sku}")
        out.append({"sku": sku, "name": en, "name_ar": ar, "unit": unit,
                    "category": cat, "swap_key": swap})

    with open(OUT, "w", encoding="utf-8", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["sku", "name", "name_ar", "unit", "category", "swap_key"])
        w.writeheader()
        w.writerows(out)

    by_cat = {}
    for r in out:
        by_cat[r["category"]] = by_cat.get(r["category"], 0) + 1
    print(f"  ingredients.csv {len(out)} rows (dropped {len(DROP)})")
    print("  per category:", by_cat)
    print("  swap-backed:", [(r["name"], r["swap_key"]) for r in out if r["swap_key"]])


if __name__ == "__main__":
    main()
