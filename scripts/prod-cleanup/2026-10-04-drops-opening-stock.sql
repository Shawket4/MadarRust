-- ============================================================================
-- DROPS PROD DATA: opening stock at Centrada from the Foodics "Inventory
-- Levels Report" (owner, 2026-10-04)
--
-- STATUS: staged; runs on prod only when the owner says go. Deliberately NOT
-- in migrations/. Drops only, branch Centrada (the selling branch; Makhzan
-- holds no stock rows).
--
-- SOURCE: ~/Downloads/Inventory Levels Report (1).csv (100 rows). Each Foodics
--   SKU is matched to its ingredient through scripts/clean-foodics-inventory.py
--   (the catalog import's own map), plus five renamed on prod: سيرب لوز مملح
--   -> Roasted Almond Syrup, ريد بول عادي -> Red Bull Original, ريد بول جوز هند
--   -> Red Bull White, خبز شباطة ابيض / بني -> White / Brown Bread.
--
-- UNITS: the catalog weighs in grams (owner, 2026-09-16), so kg x 1000 and,
--   for milk, 1 l = 1000 g. SIX readings cannot be kilograms (a café does not
--   hold 955 kg of syrup) and are read as grams: Honey 2400.96, the four
--   sugar-free syrups (~790-955) and Turkish Coffee 760.686.
--
-- NOT IMPORTED (left at 0):
--     sk-0112 Salted Caramel Sauce: -3.929 (negative)
--     sk-0137 Black Tea: 0 (zero)
--     sk-0156 Cup 4oz: -82 (negative)
--     sk-0159 Can: -188 (negative)
--     sk-0168 حبوب قهوة اسبريسو: 0 (no ingredient)
--     sk-0169 حبوب قهوة دي هوس: 0 (no ingredient)
--     sk-0188 Tea Bags: -9 (negative)
--     sk-0236 Mayonnaise: -10440 (negative)
--     sk-0238 Lettuce: -2474 (negative)
--     sk-0241 Olives: -3366 (negative)
--     sk-0243 Honey Mustard Sauce: -7800 (negative)
--     sk-0244 Sun-Dried Tomato: -1082 (negative)
--     sk-0249 V60 Paper Filter: -338 (negative)
--     sk-0277 Plastic Cup 16oz: -615 (negative)
--     sk-0293 Sweet Chilli Sauce: -2560 (negative)
--     sk-0297 Straw: 0 (zero)
--     sk-0298 Cookie Bag: 0 (zero)
--
-- HOW: one 'stock_count' movement per ingredient, the ordinary way stock moves
--   (the movement trigger adds it to branch_stock.on_hand, creating the row if
--   needed). The movement is the DIFFERENCE to the target level, so a re-run, or
--   a level that moved since, still lands exactly on the file's number.
--
-- HOW TO RUN (as the postgres superuser, on the host):
--   dry run (default, ends in ROLLBACK):
--     sudo -u postgres psql -d madar -X -f 2026-10-04-drops-opening-stock.sql
--   apply:   ... -v apply=true
--   On prod, take a pg_dump -Fc FIRST.
-- ============================================================================

\set ON_ERROR_STOP on
\if :{?apply}
\else
  \set apply false
\endif
\set org    '27b8f8db-fec2-4909-b9f6-9fffbd860a1a'
\set branch '09b75c39-7ef4-4dd9-848c-d5eda4bb5306'

BEGIN;
SELECT set_config('app.org_id', :'org', true) \gset
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM organizations WHERE id = current_setting('app.org_id')::uuid AND name = 'Drops') THEN
    RAISE EXCEPTION 'not Drops';
  END IF;
END $$;
SELECT count(*) AS is_centrada FROM branches WHERE id = :'branch'::uuid AND org_id = :'org'::uuid AND name = 'Centrada' \gset
\if :is_centrada
\else
  \echo 'Centrada is not where it was on 2026-10-04: nothing done.'
  ROLLBACK;
  \quit
\endif

-- (foodics sku, ingredient, name, as in the file, level in the catalog unit, unit, note)
CREATE TEMP TABLE opening (sku text, ingredient_id uuid, name text, foodics text, qty numeric(12,3), unit text, note text) ON COMMIT DROP;
INSERT INTO opening VALUES
  ('sk-0021', 'd7d3554a-3d6e-4989-be6f-a10576665163', 'Lactose Free Milk', '2.164 لتر', 2164.0, 'g', '1 l = 1000 g'),
  ('sk-0022', 'ddc71a77-0aa1-47db-a194-a14253162b96', 'Full Cream Milk', '18.79 لتر', 18790.0, 'g', '1 l = 1000 g'),
  ('sk-0023', 'a5835c93-eb41-4bd5-b70e-4c92dfa87c3e', 'Skimmed Milk', '14.7 لتر', 14700.0, 'g', '1 l = 1000 g'),
  ('sk-0024', 'f83a28e1-19c8-484b-9c0b-91e8f07ca364', 'Almond Milk', '10.812 لتر', 10812.0, 'g', '1 l = 1000 g'),
  ('sk-0025', '18e31aa5-4dad-4d64-9c9b-e4bc1440fcf2', 'House Blend Beans', '14.36 كيلو', 14360.0, 'g', ''),
  ('sk-0026', 'f9d07c43-4921-41ca-834f-8c032353e71c', 'Colombian Espresso', '8.894 كيلو', 8894.0, 'g', ''),
  ('sk-0027', '02b75ce5-b6d9-47d6-9507-d9275dc6b79c', 'Colombian V60', '7.855 كيلو', 7855.0, 'g', ''),
  ('sk-0109', '6ad039a8-9f5c-4a2d-b540-943296787aa2', 'Condensed Milk', '45.14 كيلو', 45140.0, 'g', ''),
  ('sk-0110', '824bd67c-0e09-445a-a960-54f3ed66cc84', 'Pistachio Sauce', '5.915 كيلو', 5915.0, 'g', ''),
  ('sk-0113', 'f0f1e82a-1067-48b8-86be-2c2bfce1e762', 'Vanilla Syrup', '3.47426 كيلو', 3474.26, 'g', ''),
  ('sk-0115', '4598edc7-b20c-4016-9098-ce4d19cc754d', 'White Chocolate Sauce', '5.257 كيلو', 5257.0, 'g', ''),
  ('sk-0116', '03d5e1a0-52ed-434a-8754-6797f740e009', 'Roasted Almond Syrup', '1.786 كيلو', 1786.0, 'g', 'Foodics سيرب لوز مملح'),
  ('sk-0117', 'de1e597f-86aa-43b6-ac0b-d43f0794ae24', 'Mango Syrup', '6.325 كيلو', 6325.0, 'g', ''),
  ('sk-0118', '74537366-8cac-4ef5-8f1c-b3197b92d32e', 'Mango Sauce', '3.399 كيلو', 3399.0, 'g', ''),
  ('sk-0119', '6bc8ce5b-6383-4fe0-bafa-0b7b275d8c08', 'Brown Butter Sauce', '6.276 كيلو', 6276.0, 'g', ''),
  ('sk-0120', '6754d96d-2d41-41f9-91b7-a7ac0fb52b49', 'Vanilla Frappe Powder', '14.18 باكت كيلو', 14180.0, 'g', ''),
  ('sk-0121', '5b100076-44b1-43d4-97a5-20984dfd58ee', 'Chocolate Powder', '3.035 باكت كيلو', 3035.0, 'g', ''),
  ('sk-0123', 'e861bfc0-9f54-4a32-800c-175cc222bd8c', 'Coconut Syrup', '1.88764 كيلو', 1887.64, 'g', ''),
  ('sk-0124', '254cf9c4-e6bf-4c17-ad72-6e228d928b00', 'Caramel Sauce', '3.389 كيلو', 3389.0, 'g', ''),
  ('sk-0125', 'f6617608-b90f-4531-b700-5ee5c4c78e0a', 'Strawberry Sauce', '3.729 كيلو', 3729.0, 'g', ''),
  ('sk-0127', 'd283c766-4916-40c5-a3f6-4e32f9b6f5c1', 'Mint Cubana Syrup', '1.95306 كيلو', 1953.06, 'g', ''),
  ('sk-0129', '0f3666b2-b715-4124-9da9-45c7589b2021', 'Passion Fruit Syrup', '2.41606 كيلو', 2416.06, 'g', ''),
  ('sk-0132', 'f9a5029e-a62d-45da-99ea-85748f078f1c', 'Blueberry Syrup', '3.22406 كيلو', 3224.06, 'g', ''),
  ('sk-0133', '0a172e63-91b2-4cf8-ba9f-0a0744babcb4', 'Mixed Berries Sauce', '6.239 كيلو', 6239.0, 'g', ''),
  ('sk-0134', 'cd5dbed6-fd24-4a50-aaa8-f9529117a50a', 'European Strawberry Syrup', '1.923 كيلو', 1923.0, 'g', ''),
  ('sk-0135', '948e9eab-9dc5-48d5-9f14-6e53882df7e8', 'Lemon Syrup', '2.767 كيلو', 2767.0, 'g', ''),
  ('sk-0136', '2e470241-9a46-44f9-b128-468ce60d74d2', 'Peach Syrup', '0.418 كيلو', 418.0, 'g', ''),
  ('sk-0140', 'cbba79be-f7ea-4b74-95bc-0572b1bcbaf9', 'White Sugar', '21.033 كيلو', 21033.0, 'g', ''),
  ('sk-0142', 'ddff5a79-38ee-465c-855b-99f3836a3192', '7up', '26 زجاجة', 26.0, 'pcs', ''),
  ('sk-0143', 'a332a3af-c590-4e1b-82fe-3b7a4dc659fe', 'Red Bull Original', '10 كانز', 10.0, 'pcs', 'Foodics ريد بول عادي'),
  ('sk-0144', '7228a90b-6eca-4959-97dd-3468e1fb49b2', 'Red Bull White', '21 كانز', 21.0, 'pcs', 'Foodics ريد بول جوز هند'),
  ('sk-0146', '2481406b-1bba-4b66-a035-84c39eeb7ae7', 'Ocean Blue Syrup', '8.48 كيلو', 8480.0, 'g', ''),
  ('sk-0157', '6fbde0bc-c34b-460f-84eb-35d82e36ccc7', 'Cup 8oz', '160 كوب', 160.0, 'pcs', ''),
  ('sk-0158', '1887fdad-affe-408e-bc51-2681180a6f5e', 'Cup 12oz', '442 كوب', 442.0, 'pcs', ''),
  ('sk-0160', '2a9ffc4c-934d-4f67-a10e-f1e1d7ec3175', 'Matcha', '0.082 كيلو', 82.0, 'g', ''),
  ('sk-0163', '98537f67-50e4-4ad2-9789-3423619dfd95', 'Coconut Milk', '0.232 لتر', 232.0, 'g', '1 l = 1000 g'),
  ('sk-0164', '475c692e-c551-4629-bc87-316f39d05c8a', 'Pineapple Sauce', '2.589 كيلو', 2589.0, 'g', ''),
  ('sk-0165', '6f905685-2242-4801-946e-3ddb9e9c0793', 'Mint Syrup', '4.838 كيلو', 4838.0, 'g', ''),
  ('sk-0166', '0cafe5f1-8a84-4e4d-bbe8-4854671ccdbc', 'Dark Chocolate Chips', '7.322 كيلو', 7322.0, 'g', ''),
  ('sk-0167', '4c6495c8-bfa8-4166-830a-d1cac320cde7', 'Milk Chocolate Chips', '5.55 كيلو', 5550.0, 'g', ''),
  ('sk-0181', '54b134a5-87c6-4214-9389-4fe3b7dc0eb3', 'Classic Cookies', '5 قطعة', 5.0, 'pcs', ''),
  ('sk-0182', '5dfae365-7c30-43fb-ba0c-8adc32d689fd', 'English Cake', '10 قطعة', 10.0, 'pcs', ''),
  ('sk-0187', '6f494da0-94f4-4c14-9e67-55799d54f1f2', 'Bovana Mineral Water', '154 زجاجة', 154.0, 'pcs', ''),
  ('sk-0191', 'c98540f5-6c88-4411-b9b1-baeb9646709b', 'Passion Fruit Sauce', '7.434 كيلو', 7434.0, 'g', ''),
  ('sk-0192', 'b3f81996-294b-4013-bfb2-c19a6349df9e', 'Caramel Syrup', '2.171 كيلو', 2171.0, 'g', ''),
  ('sk-0193', 'd956ee3f-b1c8-44d4-8c4b-2a330708f806', 'Hazelnut Syrup', '1.422 كيلو', 1422.0, 'g', ''),
  ('sk-0194', '673beee3-f3ab-4586-99a7-86fd5d93322a', 'Chocolate Sauce', '3.036 كيلو', 3036.0, 'g', ''),
  ('sk-0196', '5110e326-1382-478e-8149-217d6f4b7159', 'Sparkling Water', '19 زجاجة', 19.0, 'pcs', ''),
  ('sk-0197', '9bfdd7af-0f0e-472a-8b70-92ef5e4d8574', 'Oat Milk', '7.27 كيلو', 7270.0, 'g', ''),
  ('sk-0198', 'bfe7738d-b304-4936-907f-0940d302a58b', 'Rifi Full Cream Milk', '8.48424 كيلو', 8484.24, 'g', ''),
  ('sk-0199', '7a09414b-2587-418e-8dce-9c6cfec5e05c', 'Honey', '2400.96 كيلو', 2400.96, 'g', 'read as grams'),
  ('sk-0200', 'f66e424c-7342-4059-a440-27e209e887a3', 'Marble Cake', '11 قطعة', 11.0, 'pcs', ''),
  ('sk-0220', '12510768-c0e2-443e-bfb8-2636b639d800', 'Nutella Cookies', '11 قطعة', 11.0, 'pcs', ''),
  ('sk-0224', 'f490f607-ca2b-436e-a25a-b4f11e016e75', 'Coffee Cup Small', '2 كوب', 2.0, 'pcs', ''),
  ('sk-0225', '1a8a6471-a336-403b-b1fc-de868daa7c49', 'Coffee Cup Medium', '7 كوب', 7.0, 'pcs', ''),
  ('sk-0226', '64c080fc-78fb-4bc3-aaee-86b544e81a33', 'Coffee Cup Large', '2 كوب', 2.0, 'pcs', ''),
  ('sk-0240', 'ea0c95a6-321d-4c95-ae35-3217a64fb01b', 'Turkey', '15 بورشن', 15.0, 'pcs', ''),
  ('sk-0242', 'd8d098bb-9adf-4b3d-9a4b-0c4388eb6652', 'Chicken', '9 بورشن', 9.0, 'pcs', ''),
  ('sk-0246', '7bbdd969-4df3-465a-abfa-4e6a3506781b', 'White Bread', '9 قطعة', 9.0, 'pcs', 'Foodics خبز شباطة ابيض'),
  ('sk-0248', '3e5eb338-9cbf-4fbe-adc0-75f92f22b765', 'Brown Bread', '7 قطعة', 7.0, 'pcs', 'Foodics خبز شباطة بني'),
  ('sk-0250', 'ef8802ea-eafd-4bac-914a-8526d3ef99c8', 'Green Tea Bags', '61 باكت', 61.0, 'pcs', ''),
  ('sk-0251', '1b0521aa-6785-4f75-855d-98f655847cd9', 'Cheddar (Chicken)', '11 بورشن', 11.0, 'pcs', ''),
  ('sk-0252', '8828f3d0-2095-4aa7-a3fb-b7b680618656', 'Cheddar (Turkey)', '13 بورشن', 13.0, 'pcs', ''),
  ('sk-0254', '2d10ebc3-fc62-4600-81af-c94568e3c805', 'Decaf Beans', '1.29 كيلو', 1290.0, 'g', ''),
  ('sk-0257', 'ed96c5c3-16e2-4f33-b9a8-4225fa91debd', 'Vanilla Syrup (Sugar Free)', '882.58 كيلو', 882.58, 'g', 'read as grams'),
  ('sk-0258', 'ebdda1a1-d754-4b8a-bba8-ab4f1bf8905e', 'Caramel Syrup (Sugar Free)', '911.395 كيلو', 911.395, 'g', 'read as grams'),
  ('sk-0259', 'b18e263e-f766-4b69-87c4-1c62ff54e3d9', 'Hazelnut Syrup (Sugar Free)', '955.7 كيلو', 955.7, 'g', 'read as grams'),
  ('sk-0260', '4fd3a695-f80f-4aaa-adc9-d0832b74744d', 'Salted Caramel Syrup (Sugar Free)', '792.74 كيلو', 792.74, 'g', 'read as grams'),
  ('sk-0263', 'fd2975c6-d067-4230-ba35-fd0a98b3a271', 'Hoodie', '3 قطعة', 3.0, 'pcs', ''),
  ('sk-0264', '45f5de09-daf8-43ac-ba9d-4551cce7576c', 'Turkish Coffee', '760.686 كيلو', 760.686, 'g', 'read as grams'),
  ('sk-0269', 'd6adc55c-9e67-4216-980a-50359fe96cf1', 'Berries Cheesecake', '5 قطعة', 5.0, 'pcs', ''),
  ('sk-0272', 'a527de72-a1a3-427f-8d28-714cb28d4e32', 'Banoffee Jar', '3 قطعة', 3.0, 'pcs', ''),
  ('sk-0276', '58453981-e6e0-4334-8999-f65f845ab854', 'Coffee Bean Bag', '452 كيس', 452.0, 'pcs', ''),
  ('sk-0287', '33bf1cf5-e3d4-4d78-9b17-1b94a6a3b5dd', 'Thomas Henry Ginger', '13 زجاجة', 13.0, 'pcs', ''),
  ('sk-0288', '6bbfd1d7-8823-4552-8302-70ec99b56770', 'Thomas Henry Grapefruit', '28 زجاجة', 28.0, 'pcs', ''),
  ('sk-0290', '54234a3e-4db0-49e7-9444-3d2260fcb418', 'Strawberry Juice', '4 زجاجة', 4.0, 'pcs', ''),
  ('sk-0291', '91dfb10e-333e-45a7-9438-482a983a6611', 'Orange Juice', '5 زجاجة', 5.0, 'pcs', ''),
  ('sk-0292', '19832392-e56e-4c0c-900c-e6681a71eb64', 'Mango Juice', '6 كوب', 6.0, 'pcs', ''),
  ('sk-0294', '4b821409-63da-48aa-9394-b1015b6b6475', 'Smoothie Powder', '1.72 كيلو', 1720.0, 'g', ''),
  ('sk-0295', '5f071068-f143-4151-8254-6a98f4c510b1', 'Brownies', '2 قطعة', 2.0, 'pcs', ''),
  ('sk-0296', '00367af0-e180-41fd-addf-428841023193', 'Tiramisu', '5 قطعة', 5.0, 'pcs', ''),
  ('sk-0299', 'c94b71f6-db38-4774-b536-f3b70770e9c9', 'Craft Bag', '525 شنطة', 525.0, 'pcs', ''),
  ('sk-0300', '0799781d-36de-4a5b-9072-0dbdebfe8481', 'Kinder Cookies', '4 قطعة', 4.0, 'pcs', '');

DO $$
DECLARE n int;
BEGIN
  IF (SELECT count(*) FROM opening) <> 83 THEN RAISE EXCEPTION 'expected 83 rows'; END IF;
  SELECT count(*) INTO n FROM opening o
   WHERE NOT EXISTS (SELECT 1 FROM org_ingredients i
                      WHERE i.id = o.ingredient_id AND i.org_id = current_setting('app.org_id')::uuid
                        AND i.unit::text = o.unit AND i.deleted_at IS NULL);
  IF n > 0 THEN RAISE EXCEPTION '% rows are not a live Drops ingredient in that unit', n; END IF;
END $$;

\echo '== BEFORE: Centrada levels that are not 0 =='
SELECT count(*) AS nonzero FROM branch_stock WHERE branch_id = :'branch'::uuid AND on_hand <> 0;

INSERT INTO inventory_movements
  (branch_id, org_ingredient_id, type, quantity, balance_after, reason, source_type, note)
SELECT :'branch'::uuid, o.ingredient_id, 'stock_count',
       o.qty - coalesce(s.on_hand, 0), 0, 'Opening stock (Foodics)', 'import',
       'Foodics Inventory Levels Report, ' || o.foodics || CASE WHEN o.note <> '' THEN ' (' || o.note || ')' ELSE '' END
  FROM opening o
  LEFT JOIN branch_stock s ON s.branch_id = :'branch'::uuid AND s.org_ingredient_id = o.ingredient_id
 WHERE o.qty IS DISTINCT FROM coalesce(s.on_hand, 0);

DO $$
DECLARE n int;
BEGIN
  SELECT count(*) INTO n FROM opening o
   WHERE o.qty IS DISTINCT FROM (SELECT on_hand FROM branch_stock
                                  WHERE branch_id = '09b75c39-7ef4-4dd9-848c-d5eda4bb5306' AND org_ingredient_id = o.ingredient_id);
  IF n > 0 THEN RAISE EXCEPTION 'verify: % levels are not the file''s', n; END IF;
END $$;

\echo '== AFTER =='
SELECT o.name, o.foodics, s.on_hand, o.unit, o.note
  FROM opening o JOIN branch_stock s ON s.branch_id = :'branch'::uuid AND s.org_ingredient_id = o.ingredient_id
 ORDER BY o.name;
SELECT count(*) FILTER (WHERE on_hand <> 0) AS centrada_nonzero, count(*) FILTER (WHERE on_hand < 0) AS below_zero
  FROM branch_stock WHERE branch_id = :'branch'::uuid;

\if :apply
COMMIT;
\echo 'APPLIED.'
\else
ROLLBACK;
\echo 'DRY RUN: rolled back. Re-run with -v apply=true to apply.'
\endif
