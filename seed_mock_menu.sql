-- ─────────────────────────────────────────────────────────────────────────────
-- seed_mock_menu.sql — fill a NEW organization's menu with a realistic mock café
-- ─────────────────────────────────────────────────────────────────────────────
-- What it creates (all scoped to one org):
--   suppliers · ingredient categories · ~95 ingredients (cost, supplier, pack,
--   yield %, density, cost history) · 9 menu categories (one inactive) ·
--   49 menu items (en/ar names + descriptions, sizes, per-size recipes, step-by-step
--   prep with animated presets, price epochs) · 13 add-on groups / ~60 options
--   (swaps with replaces-ingredient, option recipes, defaults, one inactive) ·
--   per-item group attachments (required/min/max overrides, option allow-lists) ·
--   per-item optional fields (priced, with ingredients, one inactive) · kitchen
--   stations + category / item routing for every branch · org margin target ·
--   catalog revision bump.
--
-- Writes the unified menu model only — menu_item_sizes, modifier_groups,
-- modifier_options, menu_item_modifier_groups, recipe_lines — with the ids the
-- unification backfill uses (group = md5(org:addon:type), options group =
-- md5(item:options), one_size = md5(item:one_size)). The legacy tables
-- (addon_items, addon_item_ingredients, item_sizes, menu_item_recipes,
-- menu_item_addon_slots, menu_item_allowed_addons, menu_item_optional_fields)
-- are read-only views over it, so old clients see the same menu.
-- Not representable in the unified model, so not seeded: size-scoped optionals,
-- per-item/per-size/combo add-on ingredient overrides, per-attachment labels.
--
-- Safe by design: one transaction, refuses to run if the org already has menu
-- items / modifier groups, reuses same-named suppliers & ingredients.
--
-- Run:
--   psql "$DATABASE_URL" -v org=test1 -v dry_run=1 -f seed_mock_menu.sql   # roll back
--   psql "$DATABASE_URL" -v org=test1 -f seed_mock_menu.sql
-- Production (Postgres on the host, not in docker):
--   sudo -u postgres psql -d madar -v org=<org-uuid> -v dry_run=1 -f seed_mock_menu.sql
-- `org` matches the organization name, slug or id (case-insensitive).
-- Images are not seeded: they go through the asset pipeline, upload them in the dashboard.
-- ─────────────────────────────────────────────────────────────────────────────

\set ON_ERROR_STOP on
\if :{?org}
\else
  \set org test1
\endif
\if :{?dry_run}
\else
  \set dry_run 0
\endif

BEGIN;

SELECT set_config('seed.org', :'org', true);

DO $seed$
DECLARE
  cfg jsonb := $json$
{
"suppliers": [
  {"name":"Nile Roasters","contact":"Omar Hassan","phone":"+201001112233","email":"orders@nileroasters.example"},
  {"name":"Delta Dairy","contact":"Mona Adel","phone":"+201002223344","email":"sales@deltadairy.example"},
  {"name":"Cairo Fresh Produce","contact":"Karim Said","phone":"+201003334455","email":"karim@cairofresh.example"},
  {"name":"Metro Wholesale","contact":"Laila Farouk","phone":"+201004445566","email":"b2b@metro.example"},
  {"name":"Golden Bakery Supply","contact":"Youssef Nabil","phone":"+201005556677","email":"hello@goldenbakery.example"},
  {"name":"PackRight Egypt","contact":"Nour Tarek","phone":"+201006667788","email":"sales@packright.example"}
],
"ingredients": [
  ["House Blend Beans","g",90,"coffee_bean","Nile Roasters","bag",1000,null,null,"Medium roast espresso blend",76],
  ["Ethiopia Single Origin Beans","g",140,"coffee_bean","Nile Roasters","bag",1000,null,null,"Yirgacheffe, washed, light roast",null],
  ["Decaf Beans","g",110,"coffee_bean","Nile Roasters","bag",1000,null,null,"Swiss-water decaf",null],
  ["Turkish Coffee Ground","g",60,"coffee_bean","Nile Roasters","bag",500,null,null,"Extra-fine grind",null],
  ["Full Cream Milk","ml",4.5,"milk","Delta Dairy","carton",1000,null,1.03,"3% fat",3.8],
  ["Skimmed Milk","ml",4.2,"milk","Delta Dairy","carton",1000,null,1.03,null,null],
  ["Oat Milk","ml",12,"milk","Metro Wholesale","carton",1000,null,1.02,"Barista edition",null],
  ["Almond Milk","ml",14,"milk","Metro Wholesale","carton",1000,null,1.02,"Unsweetened",null],
  ["Lactose-Free Milk","ml",6,"milk","Delta Dairy","carton",1000,null,1.03,null,null],
  ["Coconut Milk","ml",13,"milk","Metro Wholesale","carton",1000,null,1.02,null,null],
  ["Soy Milk","ml",11,"milk","Metro Wholesale","carton",1000,null,1.02,"Discontinued — kept for history",null],
  ["Heavy Cream","ml",16,"dairy","Delta Dairy","carton",1000,null,1.01,"35% fat",null],
  ["Whipped Cream","g",20,"dairy","Delta Dairy","can",500,null,null,null,null],
  ["Sweetened Condensed Milk","g",12,"dairy","Metro Wholesale","can",397,null,null,null,null],
  ["Cream Cheese","g",30,"dairy","Delta Dairy","tub",1000,null,null,null,null],
  ["Greek Yogurt","g",18,"dairy","Delta Dairy","tub",1000,null,null,null,null],
  ["Mozzarella","g",40,"dairy","Delta Dairy","block",2000,null,null,"Low-moisture, shredded in house",null],
  ["Cheddar Slice","pcs",450,"dairy","Delta Dairy","pack",84,null,null,null,null],
  ["Halloumi","g",45,"dairy","Delta Dairy","block",250,null,null,null,null],
  ["Parmesan","g",90,"dairy","Metro Wholesale","wedge",1000,null,null,null,null],
  ["Mascarpone","g",55,"dairy","Metro Wholesale","tub",500,null,null,null,null],
  ["Butter","g",35,"dairy","Delta Dairy","block",1000,null,null,"Unsalted",null],
  ["Vanilla Ice Cream","g",18,"dairy","Delta Dairy","tub",5000,null,null,null,null],
  ["Eggs","pcs",450,"dairy","Cairo Fresh Produce","tray",30,null,null,"Free-range, large",null],
  ["Vanilla Syrup","ml",30,"syrup","Metro Wholesale","bottle",750,null,1.3,null,26],
  ["Caramel Syrup","ml",30,"syrup","Metro Wholesale","bottle",750,null,1.3,null,null],
  ["Hazelnut Syrup","ml",32,"syrup","Metro Wholesale","bottle",750,null,1.3,null,null],
  ["Simple Syrup","ml",5,"syrup",null,null,null,null,1.25,"Made in house 1:1",null],
  ["Caramel Sauce","g",25,"syrup","Metro Wholesale","bottle",1000,null,null,null,null],
  ["Chocolate Sauce","g",22,"syrup","Metro Wholesale","bottle",1000,null,null,null,null],
  ["Sugar","g",3.5,"dry_goods","Metro Wholesale","sack",10000,null,null,"White granulated",3],
  ["Cocoa Powder","g",25,"dry_goods","Metro Wholesale","bag",1000,null,null,null,null],
  ["Matcha Powder","g",250,"dry_goods","Metro Wholesale","tin",100,null,null,"Ceremonial grade",null],
  ["Chai Spice Mix","g",60,"dry_goods","Metro Wholesale","bag",500,null,null,null,null],
  ["Cinnamon Powder","g",15,"dry_goods","Metro Wholesale","jar",500,null,null,null,null],
  ["Cardamom","g",60,"dry_goods","Metro Wholesale","jar",250,null,null,"Ground green cardamom",null],
  ["Mint Tea Bags","pcs",150,"dry_goods","Metro Wholesale","box",100,null,null,null,null],
  ["Dried Hibiscus","g",null,"dry_goods","Cairo Fresh Produce","bag",1000,null,null,"Cost pending from supplier",null],
  ["Granola","g",20,"dry_goods","Metro Wholesale","bag",1000,null,null,null,null],
  ["Honey","g",25,"dry_goods","Metro Wholesale","jar",1000,null,null,"Clover honey",null],
  ["Nutella","g",40,"dry_goods","Metro Wholesale","jar",3000,null,null,null,null],
  ["Lotus Biscuit Crumbs","g",35,"dry_goods","Metro Wholesale","bag",1000,null,null,null,null],
  ["Strawberry Jam","g",15,"dry_goods","Metro Wholesale","jar",1000,null,null,null,null],
  ["Penne Pasta","g",6,"dry_goods","Metro Wholesale","bag",5000,null,null,null,null],
  ["Flour","g",2.5,"dry_goods","Metro Wholesale","sack",25000,null,null,null,null],
  ["Ladyfingers","pcs",300,"bakery","Golden Bakery Supply","box",200,null,null,null,null],
  ["Fresh Mint","g",20,"produce","Cairo Fresh Produce","bunch",100,70,null,"Leaves only after picking",null],
  ["Lemon","pcs",400,"produce","Cairo Fresh Produce","crate",100,null,null,null,null],
  ["Oranges","pcs",500,"produce","Cairo Fresh Produce","crate",60,null,null,"Juicing oranges",null],
  ["Mango Pulp","g",15,"produce","Cairo Fresh Produce","can",850,null,null,null,null],
  ["Strawberries","g",12,"produce","Cairo Fresh Produce","punnet",500,90,null,null,null],
  ["Mixed Berries","g",30,"produce","Metro Wholesale","frozen bag",1000,null,null,"Frozen",null],
  ["Avocado","pcs",2500,"produce","Cairo Fresh Produce","box",20,80,null,null,2200],
  ["Tomato","g",3,"produce","Cairo Fresh Produce","crate",10000,95,null,null,null],
  ["Lettuce","g",5,"produce","Cairo Fresh Produce","head",500,75,null,"Romaine",null],
  ["Onion","g",2,"produce","Cairo Fresh Produce","sack",10000,90,null,null,null],
  ["Cucumber","g",3,"produce","Cairo Fresh Produce","crate",5000,95,null,null,null],
  ["Garlic","g",6,"produce","Cairo Fresh Produce","bag",1000,85,null,null,null],
  ["Frozen Fries","g",3.5,"produce","Metro Wholesale","bag",2500,null,null,"9mm straight cut",null],
  ["Chicken Breast","g",22,"protein","Metro Wholesale","pack",2000,85,null,"Boneless, skinless",19],
  ["Beef Patty","pcs",3500,"protein","Metro Wholesale","box",40,null,null,"150g, 80/20",null],
  ["Turkey Slices","g",45,"protein","Metro Wholesale","pack",500,null,null,"Smoked",null],
  ["Tuna","g",30,"protein","Metro Wholesale","can",1800,null,null,"In sunflower oil, drained",null],
  ["Beef Bacon","g",60,"protein","Metro Wholesale","pack",500,null,null,null,null],
  ["Croissant Dough","pcs",900,"bakery","Golden Bakery Supply","box",48,null,null,"Frozen, bake-off",null],
  ["Brioche Bun","pcs",600,"bakery","Golden Bakery Supply","bag",12,null,null,null,null],
  ["Ciabatta","pcs",700,"bakery","Golden Bakery Supply","bag",10,null,null,null,null],
  ["Baguette","pcs",800,"bakery","Golden Bakery Supply","bag",10,null,null,"Half baguette",null],
  ["Sourdough Slice","pcs",350,"bakery","Golden Bakery Supply","loaf",20,null,null,null,null],
  ["Cheesecake Slice","pcs",3500,"bakery","Golden Bakery Supply","cake",12,null,null,"New York style",null],
  ["Chocolate Cake Slice","pcs",3000,"bakery","Golden Bakery Supply","cake",12,null,null,null,null],
  ["Lava Cake","pcs",2800,"bakery","Golden Bakery Supply","box",12,null,null,"Frozen",null],
  ["Brownie","pcs",1500,"bakery","Golden Bakery Supply","tray",24,null,null,null,null],
  ["Blueberry Muffin","pcs",1200,"bakery","Golden Bakery Supply","box",12,null,null,null,null],
  ["Cinnamon Roll","pcs",1400,"bakery","Golden Bakery Supply","box",12,null,null,null,null],
  ["Waffle Batter","g",8,"bakery",null,null,null,null,null,"Made in house",null],
  ["Basil Pesto","g",45,"sauces","Metro Wholesale","jar",1000,null,null,null,null],
  ["Garlic Mayo","g",15,"sauces",null,null,null,null,null,"Made in house",null],
  ["Sriracha","g",25,"sauces","Metro Wholesale","bottle",800,null,null,null,null],
  ["BBQ Sauce","g",18,"sauces","Metro Wholesale","bottle",2000,null,null,null,null],
  ["Cheese Sauce","g",30,"sauces","Metro Wholesale","can",3000,null,null,null,null],
  ["Tomato Sauce","g",10,"sauces",null,null,null,null,null,"Slow-cooked, made in house",null],
  ["Caesar Dressing","g",30,"sauces","Metro Wholesale","bottle",1000,null,null,null,null],
  ["Alfredo Sauce","g",28,"sauces",null,null,null,null,null,"Made in house",null],
  ["Ice Cubes","g",0.1,"general",null,null,null,null,null,null,null],
  ["Soda Water","ml",3,"general","Metro Wholesale","can",330,null,1,null,null],
  ["Paper Cup 8oz","pcs",150,"packaging","PackRight Egypt","sleeve",50,null,null,null,null],
  ["Paper Cup 12oz","pcs",180,"packaging","PackRight Egypt","sleeve",50,null,null,null,null],
  ["Paper Cup 16oz","pcs",210,"packaging","PackRight Egypt","sleeve",50,null,null,null,null],
  ["Plastic Cup 16oz","pcs",250,"packaging","PackRight Egypt","sleeve",50,null,null,null,null],
  ["Cup Lid","pcs",60,"packaging","PackRight Egypt","sleeve",100,null,null,null,null],
  ["Sleeve","pcs",40,"packaging","PackRight Egypt","box",500,null,null,"Kraft cup sleeve",null],
  ["Straw","pcs",30,"packaging","PackRight Egypt","box",500,null,null,"Paper straw",null],
  ["Takeaway Box","pcs",400,"packaging","PackRight Egypt","case",100,null,null,null,null]
],
"inactive_ingredients": ["Soy Milk"],
"ingredient_categories": [
  ["coffee_bean","Coffee Beans",1],["milk","Milk",2],["dairy","Dairy & Eggs",3],["syrup","Syrups & Sauces (sweet)",4],
  ["dry_goods","Dry Goods",5],["produce","Produce",6],["protein","Protein",7],["bakery","Bakery",8],
  ["sauces","Savory Sauces",9],["packaging","Packaging",10]
],
"categories": [
  {"name":"Hot Coffee","ar":"القهوة الساخنة","station":"Bar"},
  {"name":"Iced Coffee","ar":"القهوة المثلجة","station":"Bar"},
  {"name":"Tea & Refreshers","ar":"الشاي والمشروبات المنعشة","station":"Bar"},
  {"name":"Breakfast","ar":"الفطار","station":"Kitchen"},
  {"name":"Sandwiches","ar":"السندوتشات","station":"Kitchen"},
  {"name":"Mains & Salads","ar":"الأطباق الرئيسية والسلطات","station":"Kitchen"},
  {"name":"Desserts","ar":"الحلويات","station":"Kitchen"},
  {"name":"Bakery","ar":"المخبوزات","station":"Bar"},
  {"name":"Seasonal Specials","ar":"عروض الموسم","station":"Bar","inactive":true}
],
"groups": [
  {"t":"milk_type","name":"Milk","ar":"نوع الحليب","sel":"single","min":0,"max":1,"req":false,"options":[
    {"n":"Full Cream Milk","ar":"حليب كامل الدسم","p":0,"def":true,"ings":[["Full Cream Milk",0]]},
    {"n":"Skimmed Milk","ar":"حليب خالي الدسم","p":0,"ings":[["Skimmed Milk",0]],"rep":"Full Cream Milk"},
    {"n":"Oat Milk","ar":"حليب الشوفان","p":2000,"ings":[["Oat Milk",0]],"rep":"Full Cream Milk"},
    {"n":"Almond Milk","ar":"حليب اللوز","p":2200,"ings":[["Almond Milk",0]],"rep":"Full Cream Milk"},
    {"n":"Lactose-Free Milk","ar":"حليب خالي من اللاكتوز","p":1000,"ings":[["Lactose-Free Milk",0]],"rep":"Full Cream Milk"},
    {"n":"Coconut Milk","ar":"حليب جوز الهند","p":2200,"ings":[["Coconut Milk",0]],"rep":"Full Cream Milk","off":true}]},
  {"t":"coffee_type","name":"Coffee Beans","ar":"نوع البن","sel":"single","min":0,"max":1,"req":false,"options":[
    {"n":"House Blend","ar":"خلطة البيت","p":0,"def":true,"ings":[["House Blend Beans",0]]},
    {"n":"Ethiopia Single Origin","ar":"إثيوبي أحادي المصدر","p":1500,"ings":[["Ethiopia Single Origin Beans",0]],"rep":"House Blend Beans"},
    {"n":"Decaf","ar":"منزوع الكافيين","p":1000,"ings":[["Decaf Beans",0]],"rep":"House Blend Beans"}]},
  {"t":"extras","name":"Extras","ar":"إضافات","sel":"multi","min":0,"max":5,"req":false,"options":[
    {"n":"Extra Shot","ar":"شوت إضافي","p":1500,"ings":[["House Blend Beans",18]]},
    {"n":"Vanilla Syrup","ar":"شراب الفانيليا","p":1000,"ings":[["Vanilla Syrup",15]]},
    {"n":"Caramel Syrup","ar":"شراب الكراميل","p":1000,"ings":[["Caramel Syrup",15]]},
    {"n":"Hazelnut Syrup","ar":"شراب البندق","p":1000,"ings":[["Hazelnut Syrup",15]]},
    {"n":"Whipped Cream","ar":"كريمة مخفوقة","p":1000,"ings":[["Whipped Cream",25]]},
    {"n":"Caramel Drizzle","ar":"صوص كراميل","p":800,"ings":[["Caramel Sauce",10]]},
    {"n":"Chocolate Drizzle","ar":"صوص شوكولاتة","p":800,"ings":[["Chocolate Sauce",10]]},
    {"n":"Cinnamon Dust","ar":"رشة قرفة","p":300,"ings":[["Cinnamon Powder",1]]}]},
  {"t":"sweetness","name":"Sweetness","ar":"درجة السكر","sel":"single","min":1,"max":1,"req":true,"options":[
    {"n":"No Sugar","ar":"بدون سكر","p":0},
    {"n":"Light","ar":"سكر خفيف","p":0,"ings":[["Sugar",5]]},
    {"n":"Medium","ar":"مظبوط","p":0,"def":true,"ings":[["Sugar",10]]},
    {"n":"Sweet","ar":"زيادة","p":0,"ings":[["Sugar",15]]},
    {"n":"Extra Sweet","ar":"سكر زيادة جداً","p":0,"ings":[["Sugar",20]]}]},
  {"t":"ice_level","name":"Ice","ar":"الثلج","sel":"single","min":0,"max":1,"req":false,"options":[
    {"n":"No Ice","ar":"بدون ثلج","p":0},
    {"n":"Light Ice","ar":"ثلج خفيف","p":0,"ings":[["Ice Cubes",80]]},
    {"n":"Regular Ice","ar":"ثلج عادي","p":0,"def":true,"ings":[["Ice Cubes",150]]},
    {"n":"Extra Ice","ar":"ثلج زيادة","p":0,"ings":[["Ice Cubes",220]]}]},
  {"t":"tea_addins","name":"Tea Add-ins","ar":"إضافات الشاي","sel":"multi","min":0,"max":3,"req":false,"options":[
    {"n":"Fresh Mint","ar":"نعناع فريش","p":300,"ings":[["Fresh Mint",5]]},
    {"n":"Lemon Slice","ar":"شريحة ليمون","p":300,"ings":[["Lemon",0.25]]},
    {"n":"Honey","ar":"عسل","p":800,"ings":[["Honey",15]]}]},
  {"t":"toppings","name":"Toppings","ar":"الإضافات على الحلو","sel":"multi","min":0,"max":3,"req":false,"options":[
    {"n":"Nutella","ar":"نوتيلا","p":1500,"ings":[["Nutella",30]]},
    {"n":"Lotus Crumbs","ar":"لوتس","p":1200,"ings":[["Lotus Biscuit Crumbs",20]]},
    {"n":"Mixed Berries","ar":"توت مشكل","p":1800,"ings":[["Mixed Berries",40]]},
    {"n":"Vanilla Ice Cream Scoop","ar":"بولة آيس كريم فانيليا","p":2000,"ings":[["Vanilla Ice Cream",60]]},
    {"n":"Strawberries","ar":"فراولة","p":1500,"ings":[["Strawberries",50]]}]},
  {"t":"bread_type","name":"Bread","ar":"نوع الخبز","sel":"single","min":1,"max":1,"req":true,"options":[
    {"n":"Ciabatta","ar":"شاباتا","p":0,"def":true,"ings":[["Ciabatta",1]]},
    {"n":"Brioche","ar":"بريوش","p":0,"ings":[["Brioche Bun",1]]},
    {"n":"Baguette","ar":"باجيت","p":0,"ings":[["Baguette",1]]},
    {"n":"Sourdough","ar":"خبز الساوردو","p":500,"ings":[["Sourdough Slice",2]]}]},
  {"t":"sauces","name":"Sauces","ar":"الصوصات","sel":"multi","min":0,"max":2,"req":false,"options":[
    {"n":"Garlic Mayo","ar":"مايونيز بالثوم","p":500,"ings":[["Garlic Mayo",30]]},
    {"n":"Sriracha","ar":"سريراتشا","p":500,"ings":[["Sriracha",15]]},
    {"n":"BBQ","ar":"باربكيو","p":500,"ings":[["BBQ Sauce",30]]},
    {"n":"Cheese Sauce","ar":"صوص الجبنة","p":1000,"ings":[["Cheese Sauce",40]]}]},
  {"t":"sides","name":"Sides","ar":"الأطباق الجانبية","sel":"single","min":0,"max":1,"req":false,"options":[
    {"n":"French Fries","ar":"بطاطس محمرة","p":2500,"ings":[["Frozen Fries",150]]},
    {"n":"Side Salad","ar":"سلطة جانبية","p":2500,"ings":[["Lettuce",50],["Tomato",40],["Cucumber",40]]},
    {"n":"Cheesy Fries","ar":"بطاطس بالجبنة","p":3500,"ings":[["Frozen Fries",150],["Cheese Sauce",40]]}]},
  {"t":"doneness","name":"Doneness","ar":"درجة الاستواء","sel":"single","min":1,"max":1,"req":true,"options":[
    {"n":"Medium","ar":"ميديم","p":0},
    {"n":"Medium Well","ar":"ميديم ويل","p":0,"def":true},
    {"n":"Well Done","ar":"ويل دن","p":0}]},
  {"t":"serving","name":"Serving","ar":"طريقة التقديم","sel":"single","min":0,"max":1,"req":false,"options":[
    {"n":"Warmed","ar":"مسخن","p":0,"def":true},
    {"n":"Room Temperature","ar":"بدون تسخين","p":0}]},
  {"t":"pasta_extras","name":"Pasta Extras","ar":"إضافات المكرونة","sel":"multi","min":0,"max":null,"req":false,"options":[
    {"n":"Grilled Chicken","ar":"فراخ مشوية","p":4000,"ings":[["Chicken Breast",120]]},
    {"n":"Extra Parmesan","ar":"بارميزان إضافي","p":1500,"ings":[["Parmesan",15]]},
    {"n":"Chili Flakes","ar":"شطة مجروشة","p":0}]}
],
"items": [
  {"cat":"Hot Coffee","n":"Espresso","ar":"إسبريسو","d":"A short, intense shot of our house blend.","dar":"شوت مركز من خلطة البيت.",
   "sizes":[["Single",4500,1],["Double",6000,2],["Triple",7500,3,true]],
   "r":[["House Blend Beans",18],["Paper Cup 8oz",1,"f"]],
   "st":["grind","weigh","tamp","lock_in","pull_shot","knock_out"],
   "g":[{"t":"coffee_type"},{"t":"extras","max":2,"only":["Extra Shot","Cinnamon Dust"]}],
   "o":[{"n":"Served in ceramic cup","ar":"في فنجان سيراميك","p":0}]},
  {"cat":"Hot Coffee","n":"Americano","ar":"أمريكانو","d":"Espresso lengthened with hot water.","dar":"إسبريسو مع مياه ساخنة.",
   "sizes":[["Small",5500,1],["Medium",6500,1.5],["Large",7500,2]],
   "r":[["House Blend Beans",18],["Paper Cup 8oz",1,"Small"],["Paper Cup 12oz",1,"Medium"],["Paper Cup 16oz",1,"Large"],["Cup Lid",1,"f"]],
   "st":["grind","pull_shot",{"t":"Top up with hot water","ar":"أضف المياه الساخنة"},"lid"],
   "g":[{"t":"coffee_type"},{"t":"extras"},{"t":"sweetness","req":false,"min":0,"label":"Sugar?","lar":"سكر؟"}]},
  {"cat":"Hot Coffee","n":"Cappuccino","ar":"كابتشينو","d":"Equal parts espresso, steamed milk and velvety foam.","dar":"إسبريسو وحليب مبخر ورغوة ناعمة.",
   "sizes":[["Small",6500,1],["Medium",7500,1.25],["Large",8500,1.5]],
   "r":[["House Blend Beans",18],["Full Cream Milk",150],["Paper Cup 8oz",1,"Small"],["Paper Cup 12oz",1,"Medium"],["Paper Cup 16oz",1,"Large"],["Cup Lid",1,"f"]],
   "st":["grind","pull_shot","decant_milk","steam_milk","foam_spoon","lid"],
   "g":[{"t":"milk_type"},{"t":"coffee_type"},{"t":"extras"},{"t":"sweetness","req":false,"min":0}],
   "o":[{"n":"Extra Foam","ar":"رغوة زيادة","p":0},{"n":"Dry (less milk)","ar":"حليب أقل","p":0}]},
  {"cat":"Hot Coffee","n":"Latte","ar":"لاتيه","d":"Espresso with silky steamed milk and a thin layer of foam.","dar":"إسبريسو مع حليب مبخر ناعم وطبقة رغوة خفيفة.",
   "sizes":[["Small",7000,1],["Medium",8000,1.35],["Large",9000,1.7]],
   "r":[["House Blend Beans",18,"f"],["Full Cream Milk",200],["Paper Cup 8oz",1,"Small"],["Paper Cup 12oz",1,"Medium"],["Paper Cup 16oz",1,"Large"],["Cup Lid",1,"f"],["Sleeve",1,"f"]],
   "st":["grind","pull_shot","steam_milk","tap_pitcher","latte_art","lid","sleeve"],
   "g":[{"t":"milk_type"},{"t":"coffee_type"},{"t":"extras","max":3},{"t":"sweetness","req":false,"min":0}],
   "o":[{"n":"Extra Hot","ar":"ساخن جداً","p":0},{"n":"Half Sweet Vanilla","ar":"فانيليا نص سكر","p":500,"ing":"Vanilla Syrup","q":8},{"n":"Oat Foam Cap","ar":"رغوة شوفان","p":1200,"ing":"Oat Milk","q":60,"size":"Large"}]},
  {"cat":"Hot Coffee","n":"Flat White","ar":"فلات وايت","d":"Double ristretto with a thin, glossy microfoam.","dar":"دبل ريستريتو مع رغوة ناعمة لامعة.",
   "base":8000,
   "r":[["House Blend Beans",36],["Full Cream Milk",120],["Paper Cup 8oz",1],["Cup Lid",1]],
   "st":["grind","tamp","pull_shot","steam_milk","thermometer","latte_art"],
   "g":[{"t":"milk_type"},{"t":"coffee_type"},{"t":"sweetness","req":false,"min":0}]},
  {"cat":"Hot Coffee","n":"Mocha","ar":"موكا","d":"Espresso, chocolate sauce and steamed milk topped with cream.","dar":"إسبريسو وصوص شوكولاتة وحليب مبخر مع كريمة.",
   "sizes":[["Small",8000,1],["Medium",9000,1.3],["Large",10000,1.6]],
   "r":[["House Blend Beans",18,"f"],["Full Cream Milk",180],["Chocolate Sauce",25],["Whipped Cream",20,"f"],["Paper Cup 8oz",1,"Small"],["Paper Cup 12oz",1,"Medium"],["Paper Cup 16oz",1,"Large"],["Cup Lid",1,"f"]],
   "st":["pour_thick","pull_shot","steam_milk","stir","whipped_cream","drizzle"],
   "g":[{"t":"milk_type"},{"t":"extras","only":["Extra Shot","Whipped Cream","Chocolate Drizzle","Hazelnut Syrup"]},{"t":"sweetness","req":false,"min":0}]},
  {"cat":"Hot Coffee","n":"Spanish Latte","ar":"سبانيش لاتيه","d":"Latte sweetened with condensed milk.","dar":"لاتيه محلى بالحليب المكثف.",
   "sizes":[["Small",8500,1],["Medium",9500,1.3],["Large",10500,1.6]],
   "r":[["House Blend Beans",18,"f"],["Full Cream Milk",170],["Sweetened Condensed Milk",30],["Paper Cup 8oz",1,"Small"],["Paper Cup 12oz",1,"Medium"],["Paper Cup 16oz",1,"Large"],["Cup Lid",1,"f"]],
   "st":["pour_thick","grind","pull_shot","steam_milk","stir","lid"],
   "g":[{"t":"milk_type"},{"t":"coffee_type"},{"t":"extras","max":2}]},
  {"cat":"Hot Coffee","n":"Caramel Macchiato","ar":"كراميل ماكياتو","d":"Vanilla milk marked with espresso and caramel drizzle.","dar":"حليب بالفانيليا مع إسبريسو وصوص كراميل.",
   "sizes":[["Small",8500,1],["Medium",9500,1.3],["Large",10500,1.6]],
   "r":[["Vanilla Syrup",15],["Full Cream Milk",180],["House Blend Beans",18,"f"],["Caramel Sauce",15,"f"],["Paper Cup 8oz",1,"Small"],["Paper Cup 12oz",1,"Medium"],["Paper Cup 16oz",1,"Large"],["Cup Lid",1,"f"]],
   "st":["pour_thick","steam_milk","decant_milk","pull_shot","layer_pour","drizzle","lid"],
   "g":[{"t":"milk_type"},{"t":"coffee_type"},{"t":"extras","max":3}]},
  {"cat":"Hot Coffee","n":"Turkish Coffee","ar":"قهوة تركي","d":"Finely ground coffee brewed slowly in a cezve.","dar":"بن ناعم يُطهى على مهل في الكنكة.",
   "sizes":[["Single",4000,1],["Double",6000,2]],
   "r":[["Turkish Coffee Ground",8],["Paper Cup 8oz",1,"f"]],
   "st":[{"t":"Add coffee and cold water to the cezve","ar":"ضع البن والمياه الباردة في الكنكة"},{"t":"Heat slowly until the foam rises","ar":"سخّن على مهل حتى تعلو الوش"},"pour_liquid"],
   "g":[{"t":"sweetness","label":"Sugar level","lar":"السكر"}],
   "o":[{"n":"With Cardamom","ar":"بالحبهان","p":500,"ing":"Cardamom","q":1},{"n":"Extra Foam (Wesh)","ar":"وش زيادة","p":0}]},
  {"cat":"Hot Coffee","n":"Hot Chocolate","ar":"هوت شوكليت","d":"Rich cocoa whisked into steamed milk.","dar":"كاكاو غني مع حليب مبخر.",
   "sizes":[["Small",6500,1],["Large",8500,1.5]],
   "r":[["Full Cream Milk",220],["Cocoa Powder",20],["Sugar",10],["Paper Cup 12oz",1,"Small"],["Paper Cup 16oz",1,"Large"],["Cup Lid",1,"f"]],
   "st":["dose_powder","steam_milk","whisk_matcha","pour_liquid","whipped_cream"],
   "g":[{"t":"milk_type"},{"t":"extras","only":["Whipped Cream","Cinnamon Dust","Hazelnut Syrup"]}]},
  {"cat":"Seasonal Specials","n":"Pumpkin Spice Latte","ar":"لاتيه اليقطين بالتوابل","d":"Autumn favourite — back in October.","dar":"مشروب الخريف المفضل — يعود في أكتوبر.","inactive":true,
   "sizes":[["Small",9500,1],["Large",11500,1.5]],
   "r":[["House Blend Beans",18,"f"],["Full Cream Milk",200],["Chai Spice Mix",3],["Vanilla Syrup",15],["Paper Cup 12oz",1,"Small"],["Paper Cup 16oz",1,"Large"],["Cup Lid",1,"f"]],
   "st":["pull_shot","dose_powder","steam_milk","stir","whipped_cream","stencil_dust"],
   "g":[{"t":"milk_type"},{"t":"extras","max":2}]},

  {"cat":"Iced Coffee","n":"Iced Latte","ar":"آيس لاتيه","d":"Espresso over ice and cold milk.","dar":"إسبريسو على ثلج وحليب بارد.",
   "sizes":[["Medium",8000,1],["Large",9500,1.4]],
   "r":[["House Blend Beans",18],["Full Cream Milk",180],["Plastic Cup 16oz",1,"f"],["Cup Lid",1,"f"],["Straw",1,"f"]],
   "st":["scoop_ice","pour_liquid","pull_shot","layer_pour","lid"],
   "g":[{"t":"milk_type"},{"t":"coffee_type"},{"t":"extras","max":3},{"t":"sweetness","req":false,"min":0},{"t":"ice_level"}]},
  {"cat":"Iced Coffee","n":"Iced Americano","ar":"آيس أمريكانو","d":"Double espresso over ice and cold water.","dar":"دبل إسبريسو على ثلج ومياه باردة.",
   "sizes":[["Medium",6500,1],["Large",7500,1.5]],
   "r":[["House Blend Beans",18],["Plastic Cup 16oz",1,"f"],["Cup Lid",1,"f"],["Straw",1,"f"]],
   "st":["scoop_ice","pour_liquid","pull_shot","lid"],
   "g":[{"t":"coffee_type"},{"t":"extras","only":["Extra Shot","Vanilla Syrup","Caramel Syrup","Hazelnut Syrup"]},{"t":"ice_level"},{"t":"sweetness","req":false,"min":0}]},
  {"cat":"Iced Coffee","n":"Iced Spanish Latte","ar":"آيس سبانيش لاتيه","d":"Our best seller, iced.","dar":"الأكثر مبيعاً — مثلج.",
   "sizes":[["Medium",9500,1],["Large",11000,1.4]],
   "r":[["House Blend Beans",18,"f"],["Full Cream Milk",170],["Sweetened Condensed Milk",35],["Plastic Cup 16oz",1,"f"],["Cup Lid",1,"f"],["Straw",1,"f"]],
   "st":["pour_thick","scoop_ice","pour_liquid","pull_shot","layer_pour","lid"],
   "g":[{"t":"milk_type"},{"t":"coffee_type"},{"t":"extras","max":2},{"t":"ice_level"}]},
  {"cat":"Iced Coffee","n":"Cold Brew","ar":"كولد برو","d":"Steeped for 18 hours for a smooth, low-acid cup.","dar":"منقوع ١٨ ساعة لطعم ناعم وحموضة قليلة.",
   "base":8500,
   "r":[["House Blend Beans",30],["Plastic Cup 16oz",1],["Cup Lid",1],["Straw",1]],
   "st":[{"t":"Steep coarse grounds 18h (batch, day before)","ar":"انقع البن الخشن ١٨ ساعة (تحضير اليوم السابق)"},"strain","scoop_ice","pour_liquid","lid"],
   "g":[{"t":"coffee_type"},{"t":"milk_type","label":"Add milk?","lar":"تضيف حليب؟"},{"t":"ice_level"},{"t":"sweetness","req":false,"min":0}]},
  {"cat":"Iced Coffee","n":"Caramel Frappe","ar":"كراميل فرابيه","d":"Blended coffee, caramel and ice, crowned with cream.","dar":"قهوة مخفوقة مع كراميل وثلج وكريمة.",
   "sizes":[["Medium",10000,1],["Large",12000,1.4]],
   "r":[["Full Cream Milk",150],["House Blend Beans",18],["Caramel Syrup",25],["Ice Cubes",180],["Whipped Cream",25,"f"],["Caramel Sauce",10,"f"],["Plastic Cup 16oz",1,"f"],["Cup Lid",1,"f"],["Straw",1,"f"]],
   "st":["scoop_ice","pour_liquid","pull_shot","blend","pour_thick","whipped_cream","drizzle"],
   "g":[{"t":"milk_type"},{"t":"extras","only":["Extra Shot","Whipped Cream","Caramel Drizzle"]}],
   "o":[{"n":"No Whipped Cream","ar":"بدون كريمة","p":0}]},
  {"cat":"Iced Coffee","n":"Iced Mocha","ar":"آيس موكا","d":"Chocolate, espresso and cold milk over ice.","dar":"شوكولاتة وإسبريسو وحليب بارد على ثلج.",
   "sizes":[["Medium",9500,1],["Large",11000,1.4]],
   "r":[["House Blend Beans",18,"f"],["Full Cream Milk",170],["Chocolate Sauce",25],["Plastic Cup 16oz",1,"f"],["Cup Lid",1,"f"],["Straw",1,"f"]],
   "st":["pour_thick","scoop_ice","pour_liquid","pull_shot","stir","lid"],
   "g":[{"t":"milk_type"},{"t":"extras","max":3},{"t":"ice_level"}]},

  {"cat":"Tea & Refreshers","n":"Mint Tea","ar":"شاي بالنعناع","d":"Black tea brewed with fresh mint leaves.","dar":"شاي أسود مع ورق نعناع فريش.",
   "sizes":[["Small",3500,1],["Large",4500,1.5]],
   "r":[["Mint Tea Bags",1,"f"],["Fresh Mint",3],["Paper Cup 8oz",1,"Small"],["Paper Cup 12oz",1,"Large"]],
   "st":["brew_pot","steep","drop_in","pour_tea"],
   "g":[{"t":"sweetness"},{"t":"tea_addins"}]},
  {"cat":"Tea & Refreshers","n":"Matcha Latte","ar":"ماتشا لاتيه","d":"Ceremonial matcha whisked with steamed milk.","dar":"ماتشا فاخرة مخفوقة مع حليب مبخر.",
   "sizes":[["Medium",9500,1],["Large",11000,1.4]],
   "r":[["Matcha Powder",3],["Full Cream Milk",200],["Paper Cup 12oz",1,"Medium"],["Paper Cup 16oz",1,"Large"],["Cup Lid",1,"f"]],
   "st":["dose_powder","whisk_matcha","steam_milk","pour_liquid","lid"],
   "g":[{"t":"milk_type"},{"t":"sweetness","req":false,"min":0},{"t":"extras","only":["Vanilla Syrup"]}],
   "o":[{"n":"Iced","ar":"مثلج","p":0,"ing":"Ice Cubes","q":150}]},
  {"cat":"Tea & Refreshers","n":"Chai Latte","ar":"تشاي لاتيه","d":"Spiced black tea with honey and steamed milk.","dar":"شاي بالتوابل مع عسل وحليب مبخر.",
   "sizes":[["Medium",8500,1],["Large",10000,1.4]],
   "r":[["Chai Spice Mix",6],["Full Cream Milk",200],["Honey",10],["Paper Cup 12oz",1,"Medium"],["Paper Cup 16oz",1,"Large"],["Cup Lid",1,"f"]],
   "st":["dose_powder","steam_milk","stir","stencil_dust","lid"],
   "g":[{"t":"milk_type"},{"t":"extras","only":["Extra Shot","Cinnamon Dust","Vanilla Syrup"],"label":"Make it dirty?","lar":"تضيف شوت؟"}]},
  {"cat":"Tea & Refreshers","n":"Hibiscus Iced Tea","ar":"كركديه مثلج","d":"Chilled hibiscus infusion — tart and refreshing.","dar":"كركديه بارد منعش.",
   "base":5000,
   "r":[["Dried Hibiscus",10],["Plastic Cup 16oz",1],["Straw",1]],
   "st":["brew_pot","steep","strain","scoop_ice","pour_liquid"],
   "g":[{"t":"ice_level"},{"t":"sweetness"}]},
  {"cat":"Tea & Refreshers","n":"Lemon Mint","ar":"ليمون بالنعناع","d":"Fresh lemon and mint blended with ice.","dar":"ليمون فريش ونعناع مخفوق بالثلج.",
   "sizes":[["Medium",6000,1],["Large",7500,1.4]],
   "r":[["Lemon",1],["Fresh Mint",8],["Simple Syrup",30],["Ice Cubes",150],["Plastic Cup 16oz",1,"f"],["Straw",1,"f"]],
   "st":["muddle","squeeze_citrus","scoop_ice","blend","pour_liquid"],
   "g":[{"t":"sweetness","req":false,"min":0}],
   "o":[{"n":"Sparkling","ar":"بالصودا","p":1000,"ing":"Soda Water","q":200},{"n":"No Blend (on the rocks)","ar":"بدون خلاط","p":0}]},
  {"cat":"Tea & Refreshers","n":"Mango Smoothie","ar":"سموذي مانجو","d":"Mango pulp blended with vanilla ice cream.","dar":"مانجو مخفوقة مع آيس كريم فانيليا.",
   "sizes":[["Medium",9000,1],["Large",11000,1.4]],
   "r":[["Mango Pulp",180],["Vanilla Ice Cream",60],["Ice Cubes",100],["Plastic Cup 16oz",1,"f"],["Straw",1,"f"]],
   "st":["pour_thick","scoop_ice_cream","scoop_ice","blend","pour_liquid"],
   "g":[{"t":"sweetness","req":false,"min":0},{"t":"toppings","max":1,"only":["Mixed Berries","Strawberries"],"label":"Fruit on top","lar":"فاكهة على الوش"}]},
  {"cat":"Tea & Refreshers","n":"Strawberry Smoothie","ar":"سموذي فراولة","d":"Fresh strawberries, yogurt and honey.","dar":"فراولة فريش مع زبادي وعسل.",
   "sizes":[["Medium",9000,1],["Large",11000,1.4]],
   "r":[["Strawberries",150],["Greek Yogurt",100],["Honey",15],["Ice Cubes",100],["Plastic Cup 16oz",1,"f"],["Straw",1,"f"]],
   "st":["drop_in","scoop_ice","blend","pour_liquid"],
   "g":[{"t":"sweetness","req":false,"min":0}]},
  {"cat":"Tea & Refreshers","n":"Fresh Orange Juice","ar":"عصير برتقال فريش","d":"Squeezed to order.","dar":"يُعصر عند الطلب.",
   "sizes":[["Medium",7000,1],["Large",9000,1.5]],
   "r":[["Oranges",3],["Plastic Cup 16oz",1,"f"],["Straw",1,"f"]],
   "st":["squeeze_citrus","strain","pour_liquid"],
   "g":[{"t":"ice_level"},{"t":"sweetness","req":false,"min":0}]},

  {"cat":"Breakfast","n":"Butter Croissant","ar":"كرواسون بالزبدة","d":"Flaky, all-butter croissant baked every morning.","dar":"كرواسون هش بالزبدة يُخبز كل صباح.",
   "base":5500,
   "r":[["Croissant Dough",1],["Butter",5]],
   "st":["bake","plate"],
   "g":[{"t":"serving"}],
   "o":[{"n":"Side of Jam","ar":"مربى فراولة","p":800,"ing":"Strawberry Jam","q":30},{"n":"Side of Butter","ar":"زبدة جانبية","p":500,"ing":"Butter","q":15}]},
  {"cat":"Breakfast","n":"Cheese Croissant","ar":"كرواسون بالجبنة","d":"Croissant filled with mozzarella and cream cheese.","dar":"كرواسون محشي موتزاريلا وكريم تشيز.",
   "base":7500,
   "r":[["Croissant Dough",1],["Mozzarella",30],["Cream Cheese",20]],
   "st":["cut","spread_sauce","stack_slice","bake","plate"],
   "g":[{"t":"serving"}]},
  {"cat":"Breakfast","n":"Egg & Cheese Brioche","ar":"بريوش بالبيض والجبنة","d":"Soft scrambled eggs and cheddar in a buttered brioche.","dar":"بيض سكرامبل وشيدر في بريوش بالزبدة.",
   "base":11000,
   "r":[["Brioche Bun",1],["Eggs",2],["Cheddar Slice",1],["Butter",10],["Takeaway Box",1]],
   "st":[{"t":"Soft-scramble the eggs","ar":"اعمل البيض سكرامبل طري"},"stack_bread","stack_slice","press","plate"],
   "g":[{"t":"sauces","max":1,"only":["Sriracha","Garlic Mayo"]}],
   "o":[{"n":"Add Beef Bacon","ar":"إضافة بيكون بقري","p":3000,"ing":"Beef Bacon","q":40},{"n":"Add Avocado","ar":"إضافة أفوكادو","p":2500,"ing":"Avocado","q":0.5}]},
  {"cat":"Breakfast","n":"Shakshuka","ar":"شكشوكة","d":"Eggs poached in a spiced tomato and pepper sauce, with sourdough.","dar":"بيض مطهو في صلصة طماطم متبلة مع خبز ساوردو.",
   "base":12000,
   "r":[["Eggs",3],["Tomato",150],["Onion",40],["Tomato Sauce",60],["Sourdough Slice",2]],
   "st":["slice_ingredient",{"t":"Sauté onion, add tomato and sauce","ar":"شوّح البصل وأضف الطماطم والصلصة"},{"t":"Crack eggs in and cover until set","ar":"اكسر البيض وغطِّ حتى ينضج"},"season","plate"],
   "g":[{"t":"sauces","max":1,"only":["Sriracha"],"label":"Make it spicy","lar":"حار؟"}],
   "o":[{"n":"Add Mozzarella","ar":"إضافة موتزاريلا","p":1500,"ing":"Mozzarella","q":30},{"n":"Extra Egg","ar":"بيضة إضافية","p":1200,"ing":"Eggs","q":1},{"n":"Gluten-free (no bread)","ar":"بدون خبز","p":0,"off":true}]},
  {"cat":"Breakfast","n":"Avocado Toast","ar":"أفوكادو توست","d":"Smashed avocado on sourdough with a fried egg and tomato.","dar":"أفوكادو مهروس على ساوردو مع بيض وطماطم.",
   "base":14000,
   "r":[["Sourdough Slice",2],["Avocado",1],["Eggs",1],["Tomato",30]],
   "st":["slice_ingredient",{"t":"Smash avocado with lemon and salt","ar":"اهرس الأفوكادو بالليمون والملح"},"stack_bread","flip","garnish","plate"],
   "g":[{"t":"sauces","max":1,"only":["Sriracha"]}],
   "o":[{"n":"Poached instead of fried","ar":"بيض مسلوق بدل مقلي","p":0},{"n":"Add Halloumi","ar":"إضافة حلومي","p":2500,"ing":"Halloumi","q":50}]},
  {"cat":"Breakfast","n":"Granola Bowl","ar":"بول جرانولا","d":"Greek yogurt, crunchy granola, honey and berries.","dar":"زبادي يوناني مع جرانولا وعسل وتوت.",
   "sizes":[["Regular",11000,1],["Large",14000,1.5]],
   "r":[["Greek Yogurt",200],["Granola",60],["Honey",15],["Mixed Berries",50],["Takeaway Box",1,"f"]],
   "st":["pour_thick","drop_in","drizzle","garnish"],
   "g":[{"t":"toppings","max":2,"only":["Nutella","Lotus Crumbs","Strawberries"]}]},

  {"cat":"Sandwiches","n":"Chicken Pesto Panini","ar":"بانيني دجاج بالبيستو","d":"Grilled chicken, basil pesto, mozzarella and tomato, pressed.","dar":"دجاج مشوي وبيستو وموتزاريلا وطماطم في بانيني.",
   "base":16500,
   "r":[["Chicken Breast",120],["Basil Pesto",25],["Mozzarella",40],["Tomato",30],["Takeaway Box",1]],
   "st":["slice_ingredient","spread_sauce","stack_slice","press","cut","plate"],
   "g":[{"t":"bread_type"},{"t":"sauces"},{"t":"sides"}]},
  {"cat":"Sandwiches","n":"Club Sandwich","ar":"كلوب ساندوتش","d":"Chicken, smoked turkey, beef bacon, egg, cheddar and greens.","dar":"دجاج وتركي مدخن وبيكون وبيض وشيدر وخضار.",
   "base":17500,
   "r":[["Chicken Breast",100],["Turkey Slices",40],["Beef Bacon",30],["Cheddar Slice",1],["Eggs",1],["Lettuce",20],["Tomato",30],["Takeaway Box",1]],
   "st":["stack_bread","spread_sauce","stack_slice","stack_slice","cut","plate"],
   "g":[{"t":"bread_type","only":["Sourdough","Brioche"]},{"t":"sauces"},{"t":"sides","req":true,"min":1,"label":"Choose a side","lar":"اختر طبق جانبي"}],
   "o":[{"n":"Not toasted","ar":"بدون تحميص","p":0}]},
  {"cat":"Sandwiches","n":"Halloumi Ciabatta","ar":"شاباتا حلومي","d":"Grilled halloumi, tomato, cucumber and pesto.","dar":"حلومي مشوي مع طماطم وخيار وبيستو.",
   "base":14500,
   "r":[["Halloumi",80],["Tomato",30],["Cucumber",30],["Basil Pesto",15],["Takeaway Box",1]],
   "st":["slice_ingredient","flip","spread_sauce","stack_slice","press","plate"],
   "g":[{"t":"bread_type"},{"t":"sauces"}]},
  {"cat":"Sandwiches","n":"Turkey & Cheese Baguette","ar":"باجيت تركي وجبنة","d":"Smoked turkey, cheddar, butter and lettuce.","dar":"تركي مدخن وشيدر وزبدة وخس.",
   "base":15000,
   "r":[["Turkey Slices",80],["Cheddar Slice",1],["Lettuce",20],["Butter",10],["Takeaway Box",1]],
   "st":["stack_bread","spread_sauce","stack_slice","wrap"],
   "g":[{"t":"bread_type","only":["Baguette","Ciabatta"]},{"t":"sauces"},{"t":"sides"}]},
  {"cat":"Sandwiches","n":"Tuna Melt","ar":"تونة ميلت","d":"Tuna, red onion and garlic mayo under melted mozzarella.","dar":"تونة وبصل ومايونيز بالثوم مع موتزاريلا سايحة.",
   "base":14000,
   "r":[["Tuna",100],["Mozzarella",40],["Onion",20],["Garlic Mayo",20],["Takeaway Box",1]],
   "st":["slice_ingredient",{"t":"Mix tuna, onion and mayo","ar":"اخلط التونة والبصل والمايونيز"},"stack_slice","grate","press","plate"],
   "g":[{"t":"bread_type"},{"t":"sides"}]},

  {"cat":"Mains & Salads","n":"Classic Beef Burger","ar":"برجر لحم كلاسيك","d":"150g beef patty, cheddar, lettuce, tomato and onion in brioche.","dar":"قطعة لحم ١٥٠ جم مع شيدر وخس وطماطم وبصل في بريوش.",
   "sizes":[["Single",18000,1],["Double",25000,2]],
   "r":[["Beef Patty",1],["Cheddar Slice",1],["Brioche Bun",1,"f"],["Lettuce",20,"f"],["Tomato",30,"f"],["Onion",15,"f"],["Takeaway Box",1,"f"]],
   "st":["season",{"t":"Grill patty to chosen doneness","ar":"اشوِ اللحم حسب درجة الاستواء"},"stack_slice","stack_bread","spread_sauce","plate"],
   "g":[{"t":"doneness"},{"t":"sauces"},{"t":"sides"}],
   "o":[{"n":"Extra Cheddar","ar":"شيدر إضافي","p":1500,"ing":"Cheddar Slice","q":1},{"n":"No Onions","ar":"بدون بصل","p":0},{"n":"Double Bacon Stack","ar":"بيكون دبل","p":4000,"ing":"Beef Bacon","q":60,"size":"Double"}]},
  {"cat":"Mains & Salads","n":"Crispy Chicken Burger","ar":"برجر فراخ كريسبي","d":"Buttermilk-fried chicken breast, lettuce and garlic mayo.","dar":"صدر فراخ مقلي مقرمش مع خس ومايونيز بالثوم.",
   "base":17000,
   "r":[["Chicken Breast",150],["Flour",30],["Brioche Bun",1],["Lettuce",20],["Garlic Mayo",20],["Takeaway Box",1]],
   "st":[{"t":"Dredge chicken in seasoned flour","ar":"غلّف الفراخ بالدقيق المتبل"},{"t":"Deep-fry 6 min at 175°C","ar":"اقلِ ٦ دقائق على ١٧٥°"},"stack_bread","stack_slice","plate"],
   "g":[{"t":"sauces"},{"t":"sides"}],
   "o":[{"n":"Spicy Coating","ar":"تتبيلة حارة","p":0},{"n":"Extra Cheddar","ar":"شيدر إضافي","p":1500,"ing":"Cheddar Slice","q":1}]},
  {"cat":"Mains & Salads","n":"Chicken Caesar Salad","ar":"سلطة سيزر بالفراخ","d":"Romaine, grilled chicken, parmesan, croutons and Caesar dressing.","dar":"خس روماني وفراخ مشوية وبارميزان وخبز محمص وصوص سيزر.",
   "sizes":[["Regular",15000,1],["Large",19000,1.5]],
   "r":[["Lettuce",150],["Chicken Breast",120],["Parmesan",15],["Caesar Dressing",40],["Sourdough Slice",1],["Takeaway Box",1,"f"]],
   "st":["cut","slice_ingredient",{"t":"Toss with dressing","ar":"قلّب مع الصوص"},"grate","garnish","plate"],
   "g":[{"t":"pasta_extras","only":["Extra Parmesan"],"label":"Extras","lar":"إضافات"}],
   "o":[{"n":"No Croutons","ar":"بدون خبز محمص","p":0},{"n":"Add Avocado","ar":"إضافة أفوكادو","p":2500,"ing":"Avocado","q":0.5},{"n":"Dressing on the side","ar":"الصوص جانبي","p":0}]},
  {"cat":"Mains & Salads","n":"Penne Arrabbiata","ar":"بيني أرابياتا","d":"Penne in a garlicky, chili-spiked tomato sauce.","dar":"مكرونة بيني بصلصة طماطم حارة بالثوم.",
   "sizes":[["Regular",13000,1],["Large",17000,1.5]],
   "r":[["Penne Pasta",120],["Tomato Sauce",120],["Garlic",5],["Parmesan",10],["Takeaway Box",1,"f"]],
   "st":[{"t":"Boil penne 10 min","ar":"اسلق المكرونة ١٠ دقائق"},"ladle_boba",{"t":"Toss with sauce and garlic","ar":"قلّب مع الصلصة والثوم"},"grate","plate"],
   "g":[{"t":"pasta_extras"}]},
  {"cat":"Mains & Salads","n":"Chicken Alfredo","ar":"فيتوتشيني ألفريدو بالفراخ","d":"Penne in creamy parmesan Alfredo with grilled chicken.","dar":"مكرونة بصوص ألفريدو كريمي بالبارميزان مع فراخ مشوية.",
   "sizes":[["Regular",17500,1],["Large",22000,1.5]],
   "r":[["Penne Pasta",120],["Alfredo Sauce",100],["Chicken Breast",100],["Parmesan",15],["Takeaway Box",1,"f"]],
   "st":[{"t":"Boil penne 10 min","ar":"اسلق المكرونة ١٠ دقائق"},"slice_ingredient",{"t":"Toss with Alfredo and chicken","ar":"قلّب مع الألفريدو والفراخ"},"grate","plate"],
   "g":[{"t":"pasta_extras","only":["Extra Parmesan","Chili Flakes"]}]},

  {"cat":"Desserts","n":"New York Cheesecake","ar":"تشيز كيك نيويورك","d":"Dense, creamy baked cheesecake.","dar":"تشيز كيك مخبوز كريمي.",
   "base":12000,
   "r":[["Cheesecake Slice",1]],
   "st":["cut","plate","drizzle"],
   "g":[{"t":"toppings","max":1,"only":["Mixed Berries","Lotus Crumbs","Strawberries"],"label":"Pick a topping","lar":"اختر الصوص"}]},
  {"cat":"Desserts","n":"Chocolate Fudge Cake","ar":"كيك شوكولاتة فادج","d":"Three layers of chocolate sponge and fudge frosting.","dar":"ثلاث طبقات كيك شوكولاتة مع كريمة فادج.",
   "base":11000,
   "r":[["Chocolate Cake Slice",1],["Chocolate Sauce",20]],
   "st":["cut","plate","drizzle"],
   "g":[{"t":"toppings","only":["Vanilla Ice Cream Scoop","Nutella"]},{"t":"serving"}]},
  {"cat":"Desserts","n":"Molten Lava Cake","ar":"لافا كيك","d":"Warm chocolate cake with a molten centre and vanilla ice cream.","dar":"كيك شوكولاتة دافئ بقلب سائل مع آيس كريم فانيليا.",
   "base":13000,
   "r":[["Lava Cake",1],["Vanilla Ice Cream",60]],
   "st":["bake","plate","scoop_ice_cream","garnish"],
   "g":[{"t":"toppings","max":2}],
   "o":[{"n":"No Ice Cream","ar":"بدون آيس كريم","p":0}]},
  {"cat":"Desserts","n":"Tiramisu","ar":"تيراميسو","d":"Espresso-soaked ladyfingers layered with mascarpone cream.","dar":"بسكويت مغموس في الإسبريسو مع كريمة الماسكاربوني.",
   "base":12500,"station":"Bar",
   "r":[["Mascarpone",80],["Ladyfingers",4],["House Blend Beans",10],["Cocoa Powder",3],["Takeaway Box",1]],
   "st":["pull_shot",{"t":"Dip ladyfingers in espresso","ar":"اغمس البسكويت في الإسبريسو"},"layer_pour","stencil_dust","plate"],
   "g":[{"t":"serving","only":["Room Temperature"],"label":"Served chilled","lar":"يُقدم بارداً"}]},
  {"cat":"Desserts","n":"Belgian Waffle","ar":"وافل بلجيكي","d":"Crisp golden waffle with your choice of toppings.","dar":"وافل ذهبي مقرمش مع إضافات من اختيارك.",
   "base":11500,
   "r":[["Waffle Batter",150],["Butter",10]],
   "st":["pour_thick",{"t":"Cook in waffle iron 4 min","ar":"اطهُ في ماكينة الوافل ٤ دقائق"},"plate","drizzle","garnish"],
   "g":[{"t":"toppings","req":true,"min":1,"max":3,"label":"Choose 1–3 toppings","lar":"اختر من ١ إلى ٣ إضافات"}]},
  {"cat":"Desserts","n":"Fudge Brownie","ar":"براوني فادج","d":"Chewy chocolate brownie, served warm.","dar":"براوني شوكولاتة طري يُقدم دافئاً.",
   "base":7500,
   "r":[["Brownie",1]],
   "st":["bake","plate"],
   "g":[{"t":"serving"},{"t":"toppings","max":1,"only":["Vanilla Ice Cream Scoop","Nutella"]}]},

  {"cat":"Bakery","n":"Cinnamon Roll","ar":"سينابون","d":"Soft roll swirled with cinnamon sugar and cream-cheese icing.","dar":"لفائف طرية بالقرفة مع كريمة الجبن.",
   "base":7000,
   "r":[["Cinnamon Roll",1],["Cream Cheese",15]],
   "st":["bake","spread_sauce","plate"],
   "g":[{"t":"serving"}]},
  {"cat":"Bakery","n":"Blueberry Muffin","ar":"مافن توت أزرق","d":"Buttery muffin packed with blueberries.","dar":"مافن بالزبدة مليء بالتوت الأزرق.",
   "base":6000,
   "r":[["Blueberry Muffin",1]],
   "st":["bake","plate"],
   "g":[{"t":"serving"}],
   "o":[{"n":"Side of Butter","ar":"زبدة جانبية","p":500,"ing":"Butter","q":15}]}
],
"margin_target_pct": 70
}
$json$::jsonb;

  v_org        uuid;
  v_org_name   text;
  v_cnt        int;
  r            jsonb;
  s            jsonb;
  l            jsonb;
  a            jsonb;
  v_id         uuid;
  v_item       uuid;
  v_group      uuid;
  v_size       uuid;
  v_ing        uuid;
  v_unit       text;
  v_yield      numeric;
  v_qty        numeric;
  v_factor     numeric;
  v_mode       text;
  v_label      text;
  v_idx        int;
  v_pos        int;
  v_only       text[];
  v_opt_ids    uuid[];
  v_branch     record;
  v_bar        uuid;
  v_kitchen    uuid;
  v_station    uuid;
BEGIN
  -- ── Resolve the organization ────────────────────────────────────────────────
  SELECT count(*) INTO v_cnt FROM organizations o
   WHERE o.deleted_at IS NULL
     AND (lower(o.name) = lower(current_setting('seed.org'))
          OR lower(o.slug) = lower(current_setting('seed.org'))
          OR o.id::text = lower(current_setting('seed.org')));
  IF v_cnt = 0 THEN
    RAISE EXCEPTION 'No organization matches "%" (name, slug or id)', current_setting('seed.org');
  ELSIF v_cnt > 1 THEN
    RAISE EXCEPTION '% organizations match "%" — pass the org id instead: -v org=<uuid>', v_cnt, current_setting('seed.org');
  END IF;
  SELECT o.id, o.name INTO v_org, v_org_name FROM organizations o
   WHERE o.deleted_at IS NULL
     AND (lower(o.name) = lower(current_setting('seed.org'))
          OR lower(o.slug) = lower(current_setting('seed.org'))
          OR o.id::text = lower(current_setting('seed.org')));

  IF EXISTS (SELECT 1 FROM menu_items WHERE org_id = v_org AND deleted_at IS NULL)
     OR EXISTS (SELECT 1 FROM modifier_groups WHERE org_id = v_org) THEN
    RAISE EXCEPTION 'Organization "%" (%) already has menu items / add-ons — this seed is for an empty menu. Nothing was changed.', v_org_name, v_org;
  END IF;
  RAISE NOTICE 'Seeding mock menu into "%" (%)', v_org_name, v_org;

  CREATE TEMP TABLE _ing (name text PRIMARY KEY, id uuid, unit text, yield numeric) ON COMMIT DROP;
  CREATE TEMP TABLE _cat (name text PRIMARY KEY, id uuid, station text) ON COMMIT DROP;
  CREATE TEMP TABLE _grp (t text PRIMARY KEY, id uuid, min int, max int, req boolean) ON COMMIT DROP;
  CREATE TEMP TABLE _opt (t text, name text, id uuid, PRIMARY KEY (t, name)) ON COMMIT DROP;
  CREATE TEMP TABLE _itm (name text PRIMARY KEY, id uuid, station text) ON COMMIT DROP;

  -- ── Suppliers (reuse same-named) ────────────────────────────────────────────
  FOR r IN SELECT * FROM jsonb_array_elements(cfg->'suppliers') LOOP
    IF NOT EXISTS (SELECT 1 FROM suppliers WHERE org_id = v_org AND name = r->>'name' AND deleted_at IS NULL) THEN
      INSERT INTO suppliers (org_id, name, contact_name, phone, email)
      VALUES (v_org, r->>'name', r->>'contact', r->>'phone', r->>'email');
    END IF;
  END LOOP;

  -- ── Ingredient categories ───────────────────────────────────────────────────
  FOR r IN SELECT * FROM jsonb_array_elements(cfg->'ingredient_categories') LOOP
    INSERT INTO ingredient_categories (org_id, slug, name, sort_order)
    VALUES (v_org, r->>0, r->>1, (r->>2)::int)
    ON CONFLICT (org_id, slug) DO NOTHING;
  END LOOP;

  -- ── Ingredients (reuse same-named; new ones get cost history) ───────────────
  -- [name, unit, cost_piastres_per_unit, category_slug, supplier, pack_unit, pack_size,
  --  yield_pct, density_g_per_ml, description, cost_90_days_ago]
  FOR r IN SELECT * FROM jsonb_array_elements(cfg->'ingredients') LOOP
    SELECT id, unit::text, yield_pct INTO v_ing, v_unit, v_yield
      FROM org_ingredients WHERE org_id = v_org AND name = r->>0 AND deleted_at IS NULL;
    IF v_ing IS NULL THEN
      INSERT INTO org_ingredients (org_id, name, unit, description, cost_per_unit, is_active,
                                   supplier_id, pack_unit, pack_size, yield_pct, density_g_per_ml, category_id)
      VALUES (v_org, r->>0, (r->>1)::inventory_unit, r->>9, (r->>2)::numeric,
              NOT (cfg->'inactive_ingredients' ? (r->>0)),
              (SELECT id FROM suppliers WHERE org_id = v_org AND name = r->>4 AND deleted_at IS NULL LIMIT 1),
              r->>5, (r->>6)::numeric, (r->>7)::numeric, (r->>8)::numeric,
              ingredient_category_id(v_org, r->>3))
      RETURNING id, unit::text, yield_pct INTO v_ing, v_unit, v_yield;

      IF jsonb_typeof(r->2) = 'number' THEN
        IF jsonb_typeof(r->10) = 'number' THEN
          INSERT INTO ingredient_cost_history (org_ingredient_id, cost_per_unit, effective_from, effective_until, note)
          VALUES (v_ing, (r->>10)::numeric, now() - interval '90 days', now() - interval '30 days', 'Initial cost');
          INSERT INTO ingredient_cost_history (org_ingredient_id, cost_per_unit, effective_from, note)
          VALUES (v_ing, (r->>2)::numeric, now() - interval '30 days', 'Supplier price change');
        ELSE
          INSERT INTO ingredient_cost_history (org_ingredient_id, cost_per_unit, effective_from, note)
          VALUES (v_ing, (r->>2)::numeric, now(), 'Initial cost');
        END IF;
      END IF;
    END IF;
    INSERT INTO _ing VALUES (r->>0, v_ing, v_unit, v_yield);
    v_ing := NULL;
  END LOOP;

  -- ── Menu categories ─────────────────────────────────────────────────────────
  FOR r IN SELECT * FROM jsonb_array_elements(cfg->'categories') LOOP
    SELECT id INTO v_id FROM categories WHERE org_id = v_org AND name = r->>'name' AND deleted_at IS NULL;
    IF v_id IS NULL THEN
      INSERT INTO categories (org_id, name, name_translations, is_active)
      VALUES (v_org, r->>'name', jsonb_build_object('en', r->>'name', 'ar', r->>'ar'),
              NOT coalesce((r->>'inactive')::boolean, false))
      RETURNING id INTO v_id;
    END IF;
    INSERT INTO _cat VALUES (r->>'name', v_id, r->>'station');
    v_id := NULL;
  END LOOP;

  -- ── Add-on groups: modifier_groups / modifier_options / recipe_lines ────────
  -- The legacy addon_items / addon_item_ingredients are read-only views over
  -- these (legacy_source = 'addon', type = legacy_addon_type), so they are
  -- derived, never written.
  v_idx := 0;
  FOR r IN SELECT * FROM jsonb_array_elements(cfg->'groups') LOOP
    v_group := md5(v_org::text || ':addon:' || (r->>'t'))::uuid;   -- same id the backfill mints
    INSERT INTO modifier_groups (id, org_id, name, name_translations, selection_type, min_selections,
                                 max_selections, is_required, sort, is_active, legacy_addon_type)
    VALUES (v_group, v_org, r->>'name', jsonb_build_object('en', r->>'name', 'ar', r->>'ar'),
            r->>'sel', (r->>'min')::int, (r->>'max')::int, (r->>'req')::boolean, v_idx, true, r->>'t');
    INSERT INTO _grp VALUES (r->>'t', v_group, (r->>'min')::int, (r->>'max')::int, (r->>'req')::boolean);

    v_pos := 0;
    FOR s IN SELECT * FROM jsonb_array_elements(r->'options') LOOP
      v_id := gen_random_uuid();
      -- A swap option ("rep") carries replaces_ingredient_id plus a quantity-0
      -- recipe line: the swap marker (same quantity as the replaced ingredient).
      INSERT INTO modifier_options (id, group_id, name, name_translations, price, sort, is_default, is_active,
                                    replaces_ingredient_id, legacy_source)
      VALUES (v_id, v_group, s->>'n', jsonb_build_object('en', s->>'n', 'ar', s->>'ar'), (s->>'p')::int, v_pos,
              coalesce((s->>'def')::boolean, false), NOT coalesce((s->>'off')::boolean, false),
              (SELECT id FROM _ing WHERE name = s->>'rep'), 'addon');
      INSERT INTO _opt VALUES (r->>'t', s->>'n', v_id);

      FOR l IN SELECT * FROM jsonb_array_elements(coalesce(s->'ings', '[]'::jsonb)) LOOP
        SELECT id, unit, yield INTO v_ing, v_unit, v_yield FROM _ing WHERE name = l->>0;
        IF v_ing IS NULL THEN RAISE EXCEPTION 'Unknown ingredient "%" on option "%"', l->>0, s->>'n'; END IF;
        v_qty := round((l->>1)::numeric / coalesce(nullif(v_yield, 0) / 100, 1), 3);
        INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit)
        VALUES ('modifier_option', v_id, v_ing, v_qty, v_unit);
      END LOOP;
      v_pos := v_pos + 1;
    END LOOP;
    v_idx := v_idx + 1;
  END LOOP;

  -- ── Menu items ──────────────────────────────────────────────────────────────
  FOR r IN SELECT * FROM jsonb_array_elements(cfg->'items') LOOP
    IF NOT EXISTS (SELECT 1 FROM _cat WHERE name = r->>'cat') THEN
      RAISE EXCEPTION 'Unknown category "%" on item "%"', r->>'cat', r->>'n';
    END IF;
    v_item := gen_random_uuid();
    INSERT INTO menu_items (id, org_id, category_id, name, name_translations, description, description_translations,
                            base_price, is_active)
    VALUES (v_item, v_org, (SELECT id FROM _cat WHERE name = r->>'cat'),
            r->>'n', jsonb_build_object('en', r->>'n', 'ar', r->>'ar'),
            r->>'d', jsonb_build_object('en', r->>'d', 'ar', r->>'dar'),
            coalesce((r->>'base')::int, (r->'sizes'->0->>1)::int),
            NOT coalesce((r->>'inactive')::boolean, false));
    INSERT INTO _itm VALUES (r->>'n', v_item, r->>'station');

    -- Price epoch for the base price (what the create-item handler writes).
    INSERT INTO menu_item_price_epochs (menu_item_id, size_label, price, effective_from)
    VALUES (v_item, NULL, coalesce((r->>'base')::int, (r->'sizes'->0->>1)::int), now());

    -- Sizes → menu_item_sizes, epochs, per-size recipes (recipe_lines owner
    -- item_size). A size-less item gets the synthesized 'one_size' row, as the
    -- backfill does. item_sizes / menu_item_recipes are views over these.
    v_idx := 0;
    FOR s IN
      SELECT * FROM jsonb_array_elements(
        CASE WHEN r ? 'sizes' THEN r->'sizes'
             ELSE jsonb_build_array(jsonb_build_array('one_size', (r->>'base')::int, 1)) END)
    LOOP
      v_label  := s->>0;
      v_factor := (s->>2)::numeric;
      IF v_label = 'one_size' THEN
        v_size := md5(v_item::text || ':one_size')::uuid;
        INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort, is_active)
        VALUES (v_size, v_item, 'one_size', (s->>1)::int, 0, true);
      ELSE
        v_size := gen_random_uuid();
        INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort, is_active)
        VALUES (v_size, v_item, v_label, (s->>1)::int, v_idx, NOT coalesce((s->>3)::boolean, false));
        INSERT INTO menu_item_price_epochs (menu_item_id, size_label, price, effective_from)
        VALUES (v_item, v_label, (s->>1)::int, now());
      END IF;

      -- recipe line: [ingredient, qty, mode] — mode "s" (default) scales with the
      -- size factor, "f" is fixed, any other value is the only size it applies to.
      FOR l IN SELECT * FROM jsonb_array_elements(r->'r') LOOP
        v_mode := coalesce(l->>2, 's');
        CONTINUE WHEN v_mode NOT IN ('s', 'f') AND v_mode <> v_label;
        SELECT id, unit, yield INTO v_ing, v_unit, v_yield FROM _ing WHERE name = l->>0;
        IF v_ing IS NULL THEN RAISE EXCEPTION 'Unknown ingredient "%" on item "%"', l->>0, r->>'n'; END IF;
        v_qty := (l->>1)::numeric * CASE WHEN v_mode = 's' THEN v_factor ELSE 1 END;
        v_qty := round(v_qty / coalesce(nullif(v_yield, 0) / 100, 1), 3);
        INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit)
        VALUES ('item_size', v_size, v_ing, v_qty, v_unit);
      END LOOP;
      v_idx := v_idx + 1;
    END LOOP;

    -- Preparation steps: preset animation when the server has it, custom otherwise.
    v_pos := 1;
    FOR s IN SELECT * FROM jsonb_array_elements(coalesce(r->'st', '[]'::jsonb)) LOOP
      IF jsonb_typeof(s) = 'string' AND EXISTS (SELECT 1 FROM recipe_step_presets WHERE slug = s#>>'{}' AND is_active) THEN
        INSERT INTO menu_item_recipe_steps (org_id, menu_item_id, position, kind, preset_slug)
        VALUES (v_org, v_item, v_pos, 'preset', s#>>'{}');
      ELSIF jsonb_typeof(s) = 'string' THEN
        INSERT INTO menu_item_recipe_steps (org_id, menu_item_id, position, kind, title)
        VALUES (v_org, v_item, v_pos, 'custom', initcap(replace(s#>>'{}', '_', ' ')));
      ELSE
        INSERT INTO menu_item_recipe_steps (org_id, menu_item_id, position, kind, title, title_ar)
        VALUES (v_org, v_item, v_pos, 'custom', s->>'t', s->>'ar');
      END IF;
      v_pos := v_pos + 1;
    END LOOP;

    -- Add-on groups on the item: menu_item_modifier_groups (legacy_origin 'slot').
    -- Overrides (min/max/required) sit on the attachment; an "only" list becomes
    -- included_option_ids (NULL = whole group). The legacy slot / allow-list
    -- views derive from these rows. Per-attachment labels ("label"/"lar") have no
    -- column in the unified model and are ignored.
    v_idx := 0;
    FOR a IN SELECT * FROM jsonb_array_elements(coalesce(r->'g', '[]'::jsonb)) LOOP
      IF NOT EXISTS (SELECT 1 FROM _grp WHERE t = a->>'t') THEN
        RAISE EXCEPTION 'Unknown add-on group "%" on item "%"', a->>'t', r->>'n';
      END IF;

      v_opt_ids := NULL;
      IF a ? 'only' THEN
        SELECT array_agg(x) INTO v_only FROM jsonb_array_elements_text(a->'only') x;
        SELECT array_agg(o.id ORDER BY array_position(v_only, o.name)) INTO v_opt_ids
          FROM _opt o WHERE o.t = a->>'t' AND o.name = ANY (v_only);
        IF coalesce(array_length(v_opt_ids, 1), 0) <> array_length(v_only, 1) THEN
          RAISE EXCEPTION 'Item "%": an "only" option is not in group "%"', r->>'n', a->>'t';
        END IF;
      END IF;

      INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort, min_override, max_override,
                                             is_required_override, included_option_ids, legacy_origin)
      SELECT v_item, g.id, v_idx, (a->>'min')::int, (a->>'max')::int, (a->>'req')::boolean, v_opt_ids, 'slot'
        FROM _grp g WHERE g.t = a->>'t';
      v_idx := v_idx + 1;
    END LOOP;

    -- Optional fields: the item's own 'Options' group (legacy_origin 'options',
    -- options legacy_source 'optional'); menu_item_optional_fields is a view over
    -- it. A "size" on an optional is NOT representable on a modifier option (the
    -- unification backfill reports it as optional.size_scoped): it is ignored and
    -- the optional — and its deduction — applies to every size.
    IF jsonb_array_length(coalesce(r->'o', '[]'::jsonb)) > 0 THEN
      v_group := md5(v_item::text || ':options')::uuid;
      INSERT INTO modifier_groups (id, org_id, name, name_translations, selection_type, min_selections,
                                   max_selections, is_required, sort, is_active, legacy_addon_type)
      VALUES (v_group, v_org, 'Options', '{"en":"Options","ar":"خيارات"}'::jsonb, 'multi', 0, NULL, false, 100, true, NULL);
      v_pos := 0;
      FOR s IN SELECT * FROM jsonb_array_elements(r->'o') LOOP
        v_id := gen_random_uuid();
        v_ing := NULL; v_unit := NULL; v_qty := NULL;
        IF s ? 'ing' THEN
          SELECT id, unit, yield INTO v_ing, v_unit, v_yield FROM _ing WHERE name = s->>'ing';
          IF v_ing IS NULL THEN RAISE EXCEPTION 'Unknown ingredient "%" on optional "%"', s->>'ing', s->>'n'; END IF;
          v_qty := round((s->>'q')::numeric / coalesce(nullif(v_yield, 0) / 100, 1), 3);
        END IF;
        INSERT INTO modifier_options (id, group_id, name, name_translations, price, sort, is_default, is_active, legacy_source)
        VALUES (v_id, v_group, s->>'n', jsonb_build_object('en', s->>'n', 'ar', s->>'ar'), (s->>'p')::int, v_pos,
                false, NOT coalesce((s->>'off')::boolean, false), 'optional');
        IF v_ing IS NOT NULL THEN
          INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit)
          VALUES ('modifier_option', v_id, v_ing, v_qty, v_unit);
        END IF;
        v_pos := v_pos + 1;
      END LOOP;
      INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort, legacy_origin)
      VALUES (v_item, v_group, 100, 'options');
    END IF;
  END LOOP;

  -- Per-item add-on ingredient overrides (menu_item_addon_overrides) no longer
  -- exist: a swap is modelled once on the option (replaces_ingredient_id + a
  -- quantity-0 recipe line, see the groups above), not per item/size/combo.

  -- ── Kitchen stations + routing, per branch ──────────────────────────────────
  FOR v_branch IN SELECT id, name FROM branches WHERE org_id = v_org AND deleted_at IS NULL LOOP
    SELECT id INTO v_bar FROM kitchen_stations
     WHERE branch_id = v_branch.id AND lower(name) = 'bar' AND deleted_at IS NULL;
    IF v_bar IS NULL THEN
      INSERT INTO kitchen_stations (org_id, branch_id, name, name_translations, sort_order, is_default)
      VALUES (v_org, v_branch.id, 'Bar', '{"en":"Bar","ar":"البار"}'::jsonb, 0,
              NOT EXISTS (SELECT 1 FROM kitchen_stations WHERE branch_id = v_branch.id AND is_default AND deleted_at IS NULL))
      RETURNING id INTO v_bar;
    END IF;
    SELECT id INTO v_kitchen FROM kitchen_stations
     WHERE branch_id = v_branch.id AND lower(name) = 'kitchen' AND deleted_at IS NULL;
    IF v_kitchen IS NULL THEN
      INSERT INTO kitchen_stations (org_id, branch_id, name, name_translations, sort_order, is_default)
      VALUES (v_org, v_branch.id, 'Kitchen', '{"en":"Kitchen","ar":"المطبخ"}'::jsonb, 1, false)
      RETURNING id INTO v_kitchen;
    END IF;

    INSERT INTO category_station_routes (branch_id, category_id, station_id)
    SELECT v_branch.id, c.id, CASE c.station WHEN 'Kitchen' THEN v_kitchen ELSE v_bar END FROM _cat c
    ON CONFLICT (branch_id, category_id) DO NOTHING;

    INSERT INTO menu_item_station_routes (branch_id, menu_item_id, station_id)
    SELECT v_branch.id, i.id, CASE i.station WHEN 'Kitchen' THEN v_kitchen ELSE v_bar END FROM _itm i
     WHERE i.station IS NOT NULL
    ON CONFLICT (branch_id, menu_item_id) DO NOTHING;

    RAISE NOTICE 'Branch "%": stations Bar/Kitchen routed', v_branch.name;
    v_bar := NULL; v_kitchen := NULL;
  END LOOP;

  -- ── Org margin target + catalog revision (clients re-sync on the bump) ─────
  INSERT INTO margin_targets (org_id, branch_id, target_pct)
  VALUES (v_org, NULL, (cfg->>'margin_target_pct')::numeric)
  ON CONFLICT ON CONSTRAINT margin_targets_org_id_branch_id_key DO NOTHING;

  INSERT INTO catalog_revision (org_id, revision) VALUES (v_org, 1)
  ON CONFLICT (org_id) DO UPDATE SET revision = catalog_revision.revision + 1, updated_at = now();

  PERFORM set_config('seed.org_id', v_org::text, true);
END
$seed$;

-- ── Summary ───────────────────────────────────────────────────────────────────
SELECT 'categories' AS what, count(*) AS n FROM categories WHERE org_id = current_setting('seed.org_id')::uuid AND deleted_at IS NULL
UNION ALL SELECT 'ingredients', count(*) FROM org_ingredients WHERE org_id = current_setting('seed.org_id')::uuid AND deleted_at IS NULL
UNION ALL SELECT 'menu items', count(*) FROM menu_items WHERE org_id = current_setting('seed.org_id')::uuid AND deleted_at IS NULL
UNION ALL SELECT 'sizes', count(*) FROM menu_item_sizes s JOIN menu_items m ON m.id = s.menu_item_id WHERE m.org_id = current_setting('seed.org_id')::uuid
UNION ALL SELECT 'recipe lines (sizes)', count(*) FROM recipe_lines rl JOIN menu_item_sizes s ON rl.owner_type = 'item_size' AND s.id = rl.owner_id JOIN menu_items m ON m.id = s.menu_item_id WHERE m.org_id = current_setting('seed.org_id')::uuid
UNION ALL SELECT 'prep steps', count(*) FROM menu_item_recipe_steps WHERE org_id = current_setting('seed.org_id')::uuid
UNION ALL SELECT 'add-on groups', count(*) FROM modifier_groups WHERE org_id = current_setting('seed.org_id')::uuid AND legacy_addon_type IS NOT NULL
UNION ALL SELECT 'add-on options', count(*) FROM addon_items WHERE org_id = current_setting('seed.org_id')::uuid
UNION ALL SELECT 'optional fields', count(*) FROM menu_item_optional_fields f JOIN menu_items m ON m.id = f.menu_item_id WHERE m.org_id = current_setting('seed.org_id')::uuid
UNION ALL SELECT 'kitchen routes', count(*) FROM category_station_routes r JOIN branches b ON b.id = r.branch_id WHERE b.org_id = current_setting('seed.org_id')::uuid;

\if :dry_run
  ROLLBACK;
  \echo 'dry run — rolled back, nothing was written'
\else
  COMMIT;
  \echo 'mock menu committed'
\endif
