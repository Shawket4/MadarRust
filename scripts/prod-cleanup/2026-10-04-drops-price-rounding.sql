-- ============================================================================
-- DROPS PROD DATA: the 14% price rise is re-rounded to the NEAREST 5 EGP
-- (owner, 2026-10-04)
--
-- STATUS: staged; runs on prod only when the owner says go. Deliberately NOT
-- in migrations/. Drops only.
--
-- The owner, verbatim: "For drops the prices where changed by 14% increase and
-- rounding up wards only to the nearest 5EGP I want the rounding to be
-- >= 2.5 -> upper 5 else lower 5".
--
-- HOW THE ROWS WERE CHOSEN: every Drops price was compared with its Foodics
--   price (drops/products.csv, drops/cleaned_*.csv; a Can is the Cup + 25, an
--   add-on sauce is Foodics' "Extra Sauce" 50, a syrup 30, Mehaweg 10). A row is
--   listed only when its price IS ceil(foodics × 1.14 / 5) × 5 AND the nearest
--   rule gives something else: new = foodics × 1.14 rounded to the nearest 5,
--   a remainder of 2.5 or more going up. A row the 14% rise left on a
--   multiple the nearest rule also gives (e.g. 62.7 -> 65) is not listed.
--
-- NOT TOUCHED (hand-set after the rise, or never from Foodics), as of
--   2026-10-04: Iced americano 150 / 185, Turkish coffee 100 / 140, Hot mocha
--   170, Iced berries / mango / strawberry matcha 190 (edited 2026-09-29),
--   Coffee Beans 250g, French coffee, Ice Cup, Sandwich & Drink, Cake 50,
--   Talabat 10.
--
-- SAFETY: each update is pinned by id AND by the price it had on 2026-10-04,
--   so a price someone changed since is skipped (and reported), never
--   overwritten. Only Drops rows can match (checked below).
--
-- DEVICES: menu_items / menu_item_sizes / modifier_options fire
--   catalog_revision_bump and sync_emit; tills pick it up on Settings -> Sync now.
--
-- HOW TO RUN (as the postgres superuser, on the host):
--   dry run (default, ends in ROLLBACK):
--     sudo -u postgres psql -d madar -X -f 2026-10-04-drops-price-rounding.sql
--   apply:   ... -v apply=true
--   On prod, take a pg_dump -Fc FIRST.
-- ============================================================================

\set ON_ERROR_STOP on
\if :{?apply}
\else
  \set apply false
\endif
\set org '27b8f8db-fec2-4909-b9f6-9fffbd860a1a'

BEGIN;
SELECT set_config('app.org_id', :'org', true) \gset
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM organizations WHERE id = current_setting('app.org_id')::uuid AND name = 'Drops') THEN
    RAISE EXCEPTION 'not Drops';
  END IF;
END $$;

-- (table, row id, what, foodics price, price on 2026-10-04, new price), piastres.
CREATE TEMP TABLE reround (tbl text, id uuid, what text, foodics int, old int, new int) ON COMMIT DROP;
INSERT INTO reround VALUES
  ('menu_items', '127b3503-1a8f-4e63-a014-c9d2503d0241', 'Banoffee jar / base_price', 19000, 22000, 21500),
  ('menu_items', '78cbd4ed-aa1c-4b9f-974a-9d34396ef8e5', 'Belgian Hot chocolate / base_price', 19000, 22000, 21500),
  ('menu_items', 'ccbe9388-908b-42a9-ac4b-7cf2f363588b', 'Belgian Iced chocolate / base_price', 19000, 22000, 21500),
  ('menu_items', '5453f035-9cd8-4439-b770-1a7e44360fcb', 'Berries cheese cake / base_price', 19000, 22000, 21500),
  ('menu_items', 'b1cae67a-e732-4679-afc3-88b131f1081a', 'Blended berries matcha / base_price', 18000, 21000, 20500),
  ('menu_items', '47527ac2-6f69-4492-8965-b28650528047', 'Blended mango matcha / base_price', 18000, 21000, 20500),
  ('menu_items', 'd41813f2-92ba-42b4-8213-4e626994de88', 'Blended matcha / base_price', 16000, 18500, 18000),
  ('menu_items', '484c173e-cacc-4f71-a5c1-c04e8ea96141', 'Blended Pistachio latte / base_price', 19000, 22000, 21500),
  ('menu_items', 'd28268ba-edc3-4dab-8e8a-9dea4973d521', 'Blended pistachio matcha / base_price', 19000, 22000, 21500),
  ('menu_items', '0dddf6ac-49ad-44bb-a19d-dee47e508f3b', 'Blended salted caramel latte / base_price', 16000, 18500, 18000),
  ('menu_items', '5699fc50-4300-49e1-ae46-47ef87d8e538', 'Blended Spanish latte / base_price', 16000, 18500, 18000),
  ('menu_items', '34847e05-02f0-42f0-8e61-96f4828c2c0b', 'Blended Spanish matcha / base_price', 18000, 21000, 20500),
  ('menu_items', '267025d7-8f2a-42aa-91ab-d039d4fca6fa', 'Blended strawberry matcha / base_price', 18000, 21000, 20500),
  ('menu_items', '9ca89534-f385-4ec3-9d12-a9c0c6091218', 'Blue passion Redbull / base_price', 18000, 21000, 20500),
  ('menu_items', 'ffd90a96-1782-438f-9432-36f3a4dc5f77', 'Boom boom / base_price', 19000, 22000, 21500),
  ('menu_items', '626afa3c-b698-48d7-82a2-de97641d142e', 'Brownis / base_price', 12000, 14000, 13500),
  ('menu_items', '0f3595ef-836e-4746-a606-1abfa40d9896', 'ceaser salad / base_price', 18000, 21000, 20500),
  ('menu_items', 'cab67e84-eb50-444a-b5d8-f9f35139a269', 'Chicken honey masterd / base_price', 23000, 26500, 26000),
  ('menu_items', 'dc21b61c-7f8b-4645-b780-d034a0c300f9', 'chicken sweet chili / base_price', 23000, 26500, 26000),
  ('menu_items', '033f2b4d-f35d-4ca2-a185-237b1c8a86e1', 'Classic Mojitos / base_price', 12000, 14000, 13500),
  ('menu_items', 'c3389cd8-c9ca-4bcc-80d4-276cb520cb3c', 'Coffee cup medium / base_price', 30000, 34500, 34000),
  ('menu_items', 'd53f06a0-c27a-48a2-a204-c1d586b3c79e', 'Cookies Classic / base_price', 11000, 13000, 12500),
  ('menu_items', '1103243e-6727-4a6c-88a8-d647d5378d3a', 'Cookies Nutella / base_price', 12000, 14000, 13500),
  ('menu_items', '7c911129-8ead-43a5-b2be-e83a55dd53c1', 'Fruit Mojitos / base_price', 15000, 17500, 17000),
  ('menu_items', '30b53a95-9261-4dee-802c-5d9e07772544', 'Green Tea / base_price', 8000, 9500, 9000),
  ('menu_items', '55fba144-e2aa-442e-afd0-401b53e25513', 'Hot butter matcha / base_price', 15000, 17500, 17000),
  ('menu_items', '47c45598-0876-4379-96b4-32a8a5cb6b25', 'Hot pistachio matcha / base_price', 16000, 18500, 18000),
  ('menu_items', '8c6c8e00-e2f2-4ee6-83a2-5bf4a8da92aa', 'Hot salted caramel matcha / base_price', 15000, 17500, 17000),
  ('menu_items', '8b61df7c-eb19-4d02-90e4-22c4d4639beb', 'Hot Spanish matcha / base_price', 16000, 18500, 18000),
  ('menu_items', '73b16c22-6f43-48b3-ab8e-aeab5fac297c', 'Hot white chocolate matcha / base_price', 15000, 17500, 17000),
  ('menu_items', 'e97e82fc-2c90-4b85-9ab8-5905254d42a4', 'Ice tea passion fruit / base_price', 12000, 14000, 13500),
  ('menu_items', 'bd484407-5a0c-449e-9ce0-6414ab751352', 'Ice tea peach / base_price', 12000, 14000, 13500),
  ('menu_items', '73b58374-3ae3-4a34-88d8-89653615ad4d', 'Iced mocha / base_price', 16000, 18500, 18000),
  ('menu_items', 'c230aec4-ef8b-4a79-9d25-b5fdc08361cc', 'Iced Pistachio latte / base_price', 18000, 21000, 20500),
  ('menu_items', 'a6de7f62-23f9-4707-8216-a2b07bf6d823', 'Iced pistachio matcha / base_price', 18000, 21000, 20500),
  ('menu_items', '65c62ca4-4d25-4b75-944e-68cdf4048f58', 'Iced salted caramel latte / base_price', 15000, 17500, 17000),
  ('menu_items', '13909cf0-7b45-428c-ab17-66e67284b3e8', 'Iced Spanish latte / base_price', 15000, 17500, 17000),
  ('menu_items', 'b2325e2c-9021-484d-a96b-b875cebfbe81', 'Iced V60 Colombian / base_price', 19000, 22000, 21500),
  ('menu_items', 'aaaabdfe-4521-4835-babf-a7eb931519fe', 'Iced V60 Ethiopian / base_price', 19000, 22000, 21500),
  ('menu_items', '4713fc6f-9dd3-465d-aa4b-0339531782a6', 'Iced white mocha / base_price', 16000, 18500, 18000),
  ('menu_items', '16650ce7-bb0b-4469-b137-8914382e608d', 'Louts cheese cake / base_price', 18000, 21000, 20500),
  ('menu_items', '222b738e-6a78-4ea6-bcf7-239ad4a90f18', 'Mango passion Redbull / base_price', 18500, 21500, 21000),
  ('menu_items', '4161fb77-df4d-49f2-849a-3e49cf45d519', 'Minta Pop / base_price', 19000, 22000, 21500),
  ('menu_items', '305f9127-ad18-4eb7-91c8-f2cee51647a2', 'Pina colada / base_price', 18500, 21500, 21000),
  ('menu_items', '518f130a-34a8-46d3-b337-a2f6c8e6a4d6', 'Pink lemonade / base_price', 14500, 17000, 16500),
  ('menu_items', 'c39157c5-3a3f-46bf-b102-944b418c04b0', 'Sparkling water / base_price', 7500, 9000, 8500),
  ('menu_items', '5e6964ac-a97d-45f8-a3f9-cec3b508954a', 'Strawberry smoothie / base_price', 15000, 17500, 17000),
  ('menu_items', '530bdd29-d563-4873-ab61-3f1e6c89e2a1', 'summer smoothie / base_price', 15000, 17500, 17000),
  ('menu_items', '107c7bc5-7ceb-43e9-aab5-d4a2ddb441f6', 'Super berry Redbull / base_price', 19000, 22000, 21500),
  ('menu_items', '6ae07421-1d25-4328-bdf3-47d4f8438cf8', 'Tiramisu / base_price', 18000, 21000, 20500),
  ('menu_items', 'cab506de-e3d7-4e16-a446-96592e1fee09', 'Tomas ginger / base_price', 8500, 10000, 9500),
  ('menu_items', '0f8004e3-2efa-4f9f-b959-c53d8ef5c892', 'Tomas grapefruit / base_price', 8500, 10000, 9500),
  ('menu_item_sizes', '303070a7-46c8-4e32-bc16-50afc7b713f9', 'Banoffee jar / one_size', 19000, 22000, 21500),
  ('menu_item_sizes', '1a077b94-d428-40b2-a946-ae63365cb1c8', 'Belgian Hot chocolate / one_size', 19000, 22000, 21500),
  ('menu_item_sizes', 'a4d12143-4d9b-4cb1-8804-af858deecd0c', 'Belgian Iced chocolate / Can', 21500, 25000, 24500),
  ('menu_item_sizes', '10a1ec6f-4956-4145-ac26-24291cd02b71', 'Belgian Iced chocolate / Cup', 19000, 22000, 21500),
  ('menu_item_sizes', 'b22c957b-38d4-432f-a40c-bb6e543f6ecf', 'Berries cheese cake / one_size', 19000, 22000, 21500),
  ('menu_item_sizes', '4c0c53e3-6b7f-4341-b56e-afec30a8fbd0', 'Blended berries matcha / Cup', 18000, 21000, 20500),
  ('menu_item_sizes', '5dbc64e9-f51e-43de-94c6-731a552e67be', 'Blended latte / Can', 16000, 18500, 18000),
  ('menu_item_sizes', 'def6b1dc-8be8-43dc-af61-de013c200434', 'Blended mango matcha / Cup', 18000, 21000, 20500),
  ('menu_item_sizes', 'f2b2b38e-e685-442f-9764-31c369403469', 'Blended matcha / Can', 18500, 21500, 21000),
  ('menu_item_sizes', 'b8a5a97f-943f-4b38-ae72-28469d00d586', 'Blended matcha / Cup', 16000, 18500, 18000),
  ('menu_item_sizes', '84470804-e25c-4ed4-be76-a70488612621', 'Blended mocha / Can', 19500, 22500, 22000),
  ('menu_item_sizes', 'f61a4716-7342-4372-a901-16d6aec81afc', 'Blended Pistachio latte / Can', 21500, 25000, 24500),
  ('menu_item_sizes', '2fa541c0-f505-45c1-b858-755471e3188d', 'Blended Pistachio latte / Cup', 19000, 22000, 21500),
  ('menu_item_sizes', 'bb5dfd68-cd2e-4c3c-8838-80afcfee7efa', 'Blended pistachio matcha / Can', 21500, 25000, 24500),
  ('menu_item_sizes', '1552ea0f-1aa2-452d-9f7b-8bf9382d9003', 'Blended pistachio matcha / Cup', 19000, 22000, 21500),
  ('menu_item_sizes', '2ca84c6d-2c8b-47af-a282-0bf1cd009a7a', 'Blended salted caramel latte / Can', 18500, 21500, 21000),
  ('menu_item_sizes', '37c5326c-d0f4-42ac-81c0-8210a77d4bf9', 'Blended salted caramel latte / Cup', 16000, 18500, 18000),
  ('menu_item_sizes', '7af0c4df-c367-48bd-8eee-3e693a56b877', 'Blended salted caramel matcha / Can', 19500, 22500, 22000),
  ('menu_item_sizes', '5081f839-b5e2-41f5-93f2-1910033327f4', 'Blended Spanish latte / Can', 18500, 21500, 21000),
  ('menu_item_sizes', '47662600-a97b-4be2-a8b2-2c437f63bb0b', 'Blended Spanish latte / Cup', 16000, 18500, 18000),
  ('menu_item_sizes', 'd7403e47-d8fd-47ba-a92a-0e9e28228d5e', 'Blended Spanish matcha / Cup', 18000, 21000, 20500),
  ('menu_item_sizes', '31bae492-cc68-4cce-8714-424f559f2347', 'Blended strawberry matcha / Cup', 18000, 21000, 20500),
  ('menu_item_sizes', '5e2b0aa3-7d97-4ce6-9deb-5cc61508f38b', 'Blended white chocolate matcha / Can', 19500, 22500, 22000),
  ('menu_item_sizes', '9fa8a7a8-859d-4df1-ad4b-c8242647bddb', 'Blended white mocha / Can', 19500, 22500, 22000),
  ('menu_item_sizes', '580ffa67-272b-4a16-b670-e88c41334015', 'Blue passion Redbull / Cup', 18000, 21000, 20500),
  ('menu_item_sizes', '622de415-1f75-4f8b-99fb-a500177f8774', 'Boom boom / Can', 21500, 25000, 24500),
  ('menu_item_sizes', 'b64a8f6f-d125-41d8-a6a0-72d4d298729b', 'Boom boom / Cup', 19000, 22000, 21500),
  ('menu_item_sizes', '0ef7afe3-7ff4-44a4-8ce1-27a6811ea015', 'Brownis / one_size', 12000, 14000, 13500),
  ('menu_item_sizes', 'f30630ba-fa5e-4bd0-823f-31f43e8d1134', 'ceaser salad / one_size', 18000, 21000, 20500),
  ('menu_item_sizes', '163e5307-d788-4260-a71b-d73e5ff3892f', 'Chicken honey masterd / one_size', 23000, 26500, 26000),
  ('menu_item_sizes', '4fb21889-7249-40c5-9a38-b8ca43c2ad65', 'chicken sweet chili / one_size', 23000, 26500, 26000),
  ('menu_item_sizes', '0bc7f100-7d41-4d8f-b970-97afeff84f6e', 'Classic Mojitos / Can', 14500, 17000, 16500),
  ('menu_item_sizes', '1767b29a-fa1e-4967-98c6-fa95c876d6a0', 'Classic Mojitos / Cup', 12000, 14000, 13500),
  ('menu_item_sizes', '163033ff-0356-4309-b490-b7c2bdd4c901', 'Coffee cup medium / one_size', 30000, 34500, 34000),
  ('menu_item_sizes', 'a646ae8c-f3d7-4d89-858d-0bece6ef147b', 'Cookies Classic / one_size', 11000, 13000, 12500),
  ('menu_item_sizes', 'b541ffbd-d038-47da-9ee6-e71e9f28a0c4', 'Cookies Nutella / one_size', 12000, 14000, 13500),
  ('menu_item_sizes', 'fcdf70c5-bb9a-4738-9a6f-fa175d517cf2', 'Fruit Mojitos / Cup', 15000, 17500, 17000),
  ('menu_item_sizes', '6b0c7dd6-50bb-4142-8a3d-856fd6874da6', 'Green Tea / one_size', 8000, 9500, 9000),
  ('menu_item_sizes', 'b533f0f1-cd53-4c60-9e71-de746ddde7cd', 'Hot butter matcha / one_size', 15000, 17500, 17000),
  ('menu_item_sizes', '1438be5e-bd41-4fdd-9097-b35d5136cda0', 'Hot pistachio matcha / one_size', 16000, 18500, 18000),
  ('menu_item_sizes', 'ce3f916e-2e87-49db-b669-bf9b34d99cbb', 'Hot salted caramel matcha / one_size', 15000, 17500, 17000),
  ('menu_item_sizes', '847c1151-b279-463d-a216-9f5c37160f67', 'Hot Spanish matcha / one_size', 16000, 18500, 18000),
  ('menu_item_sizes', '9f4d9066-8910-47ef-a063-2a5d27c8904b', 'Hot white chocolate matcha / one_size', 15000, 17500, 17000),
  ('menu_item_sizes', '779b3b81-8bc3-4267-a2c4-0f4715434060', 'Ice tea passion fruit / Can', 14500, 17000, 16500),
  ('menu_item_sizes', '6bb25310-17ab-44dc-804e-f3bc3351d514', 'Ice tea passion fruit / Cup', 12000, 14000, 13500),
  ('menu_item_sizes', '465a114d-412a-414b-8242-69bb42483a12', 'Ice tea peach / Can', 14500, 17000, 16500),
  ('menu_item_sizes', '74a5b458-5d3d-4f70-bb5a-e49842bde768', 'Ice tea peach / Cup', 12000, 14000, 13500),
  ('menu_item_sizes', '26fd010c-f521-4664-bd57-64ae3c9af74f', 'Iced brown butter latte / Can', 19000, 22000, 21500),
  ('menu_item_sizes', 'd3d7ac86-351f-4152-9013-f9dc6fcea8d2', 'Iced latte / Can', 15000, 17500, 17000),
  ('menu_item_sizes', '896145c0-8c09-4c1f-af52-f327c51b783d', 'Iced mocha / Can', 18500, 21500, 21000),
  ('menu_item_sizes', '340d4a1d-a18f-42d4-840f-1f6ce5360766', 'Iced mocha / Cup', 16000, 18500, 18000),
  ('menu_item_sizes', 'd15b42ab-287b-429c-b6bb-b2bcc4a100ad', 'Iced Pistachio latte / Cup', 18000, 21000, 20500),
  ('menu_item_sizes', '02ac09a1-2a1b-4b7b-b526-257b5cb4634e', 'Iced pistachio matcha / Cup', 18000, 21000, 20500),
  ('menu_item_sizes', '241602b0-9bdf-4e68-8758-57e2b9932019', 'Iced salted caramel latte / Cup', 15000, 17500, 17000),
  ('menu_item_sizes', 'd1f4868f-d444-43c5-a92b-cdb9691f2abc', 'Iced Spanish latte / Cup', 15000, 17500, 17000),
  ('menu_item_sizes', '70226119-e307-402c-87e3-49b51f4d216e', 'Iced V60 Colombian / one_size', 19000, 22000, 21500),
  ('menu_item_sizes', 'e426f529-7a1a-4ba9-adca-b2828fbae14d', 'Iced V60 Ethiopian / one_size', 19000, 22000, 21500),
  ('menu_item_sizes', 'e394d9e9-a41f-4cf2-8a42-38cbabf25049', 'Iced white mocha / Can', 18500, 21500, 21000),
  ('menu_item_sizes', '5d58de12-11ef-4bc1-a522-383d067097c0', 'Iced white mocha / Cup', 16000, 18500, 18000),
  ('menu_item_sizes', '47d10470-e69e-49e3-8f7a-7894f2beaf45', 'Louts cheese cake / one_size', 18000, 21000, 20500),
  ('menu_item_sizes', 'c717c602-9861-4bce-a681-1b87226f1ce7', 'Mango passion Redbull / Cup', 18500, 21500, 21000),
  ('menu_item_sizes', '57c1d3c4-3d19-47ef-b9f1-c01b51b8cb20', 'Minta Pop / Can', 21500, 25000, 24500),
  ('menu_item_sizes', 'fd216084-e557-48d2-a4c4-8af37ef0055f', 'Minta Pop / Cup', 19000, 22000, 21500),
  ('menu_item_sizes', '6b4a023c-9b1c-4101-8e75-5aec44b5726e', 'Pina colada / Cup', 18500, 21500, 21000),
  ('menu_item_sizes', 'd5baae43-8329-423c-9263-71711c5b2012', 'Pink lemonade / Cup', 14500, 17000, 16500),
  ('menu_item_sizes', '5c5ca630-6a85-4f18-8794-7d76b48f7eb8', 'Sparkling water / one_size', 7500, 9000, 8500),
  ('menu_item_sizes', '98376d56-8d50-4798-aa4e-2d416a75ccae', 'Strawberry smoothie / Cup', 15000, 17500, 17000),
  ('menu_item_sizes', '3a1911d2-d5a0-4117-b946-ab074cb0d59c', 'summer smoothie / Cup', 15000, 17500, 17000),
  ('menu_item_sizes', 'd98a5b51-2435-4ba1-b6bb-8c365ff8b15e', 'Super berry Redbull / Can', 21500, 25000, 24500),
  ('menu_item_sizes', '678c701e-8e6c-4b59-9d34-5b917f606faa', 'Super berry Redbull / Cup', 19000, 22000, 21500),
  ('menu_item_sizes', '6f6092d8-564a-4fcc-8c45-8392034fe486', 'Tiramisu / one_size', 18000, 21000, 20500),
  ('menu_item_sizes', 'dbf67701-354a-4a3f-9356-afbedecfb095', 'Tomas ginger / one_size', 8500, 10000, 9500),
  ('menu_item_sizes', '90167291-d079-4f00-9ac1-041fcd6796f0', 'Tomas grapefruit / one_size', 8500, 10000, 9500),
  ('modifier_options', 'a69700ba-ca71-4be2-845e-07ed2f4b9842', 'Extras / Brown Butter Sauce', 5000, 6000, 5500),
  ('modifier_options', '6edadb8f-aac3-41cf-8489-21b730a53603', 'Extras / Caramel Sauce', 5000, 6000, 5500),
  ('modifier_options', '556085dc-d0a3-422f-926c-a53f778446c7', 'Extras / Chocolate Sauce', 5000, 6000, 5500),
  ('modifier_options', '8aa3ccb2-d69b-44f2-89b5-421475c2499f', 'Extras / Cinnamon Sauce', 5000, 6000, 5500),
  ('modifier_options', '101ba8a7-228b-4a7d-bcf5-f708940d8576', 'Extras / Condensed Milk', 5000, 6000, 5500),
  ('modifier_options', '55c90021-910b-4870-b9c0-869e39f11a62', 'Extras / Extra Shot', 4000, 5000, 4500),
  ('modifier_options', '207dc834-33ac-4b34-8b53-c485831e143e', 'Extras / Mango Sauce', 5000, 6000, 5500),
  ('modifier_options', '573b147c-e6f9-45e4-9bf0-cbca490d4573', 'Extras / Mixed Berries Sauce', 5000, 6000, 5500),
  ('modifier_options', '0a6b899c-b033-48a1-987c-701729e133a1', 'Extras / Passion Fruit Sauce', 5000, 6000, 5500),
  ('modifier_options', '2d3f1e94-24cb-47c9-9542-4106c789bc9a', 'Extras / Pineapple Sauce', 5000, 6000, 5500),
  ('modifier_options', '62e3c35f-9e5b-46e5-b198-dfc7ed19501d', 'Extras / Pistachio Sauce', 5000, 6000, 5500),
  ('modifier_options', 'ee009c2a-b6cf-472f-96d1-5b4c4ae0884a', 'Extras / Salted Caramel Sauce', 5000, 6000, 5500),
  ('modifier_options', '0201932c-b898-41bc-b880-f00a4de4b41d', 'Extras / Strawberry Sauce', 5000, 6000, 5500),
  ('modifier_options', '60388ea7-0b9d-4f30-8639-5f336e4f6e8f', 'Extras / White Chocolate Sauce', 5000, 6000, 5500),
  ('modifier_options', 'd3fbd7db-62c8-4e55-a3d8-944c7209026c', 'Options / Mehaweg', 1000, 1500, 1000),
  ('modifier_options', 'ff40a8fb-42b5-4480-a6a0-c2247d10d066', 'Turkish Coffee Type / Mehaweg', 1000, 1500, 1000);

-- Every listed row belongs to Drops, and every new price is the rule's.
DO $$
DECLARE n int;
BEGIN
  IF (SELECT count(*) FROM reround) <> 141 THEN RAISE EXCEPTION 'expected 141 rows'; END IF;
  IF EXISTS (SELECT 1 FROM reround WHERE new % 500 <> 0 OR old - new <> 500
              OR new <> (CASE WHEN (foodics * 114) % 50000 >= 25000
                              THEN (foodics * 114 / 50000 + 1) * 500
                              ELSE (foodics * 114 / 50000) * 500 END)) THEN
    RAISE EXCEPTION 'a listed price is not the nearest-5 rule';
  END IF;
  SELECT count(*) INTO n FROM reround r WHERE NOT (
       (r.tbl = 'menu_items' AND EXISTS (SELECT 1 FROM menu_items m WHERE m.id = r.id AND m.org_id = current_setting('app.org_id')::uuid))
    OR (r.tbl = 'menu_item_sizes' AND EXISTS (SELECT 1 FROM menu_item_sizes s JOIN menu_items m ON m.id = s.menu_item_id
                                               WHERE s.id = r.id AND m.org_id = current_setting('app.org_id')::uuid))
    OR (r.tbl = 'modifier_options' AND EXISTS (SELECT 1 FROM modifier_options o JOIN modifier_groups g ON g.id = o.group_id
                                                WHERE o.id = r.id AND g.org_id = current_setting('app.org_id')::uuid)));
  IF n > 0 THEN RAISE EXCEPTION '% listed rows are not Drops rows', n; END IF;
END $$;

\echo '== SKIPPED (price changed since 2026-10-04) =='
SELECT r.what, r.old, coalesce(m.base_price, s.price, o.price) AS now
  FROM reround r
  LEFT JOIN menu_items m ON r.tbl = 'menu_items' AND m.id = r.id
  LEFT JOIN menu_item_sizes s ON r.tbl = 'menu_item_sizes' AND s.id = r.id
  LEFT JOIN modifier_options o ON r.tbl = 'modifier_options' AND o.id = r.id
 WHERE coalesce(m.base_price, s.price, o.price) IS DISTINCT FROM r.old
   AND coalesce(m.base_price, s.price, o.price) IS DISTINCT FROM r.new;

UPDATE menu_items m SET base_price = r.new, updated_at = now()
  FROM reround r WHERE r.tbl = 'menu_items' AND m.id = r.id AND m.base_price = r.old;
UPDATE menu_item_sizes s SET price = r.new
  FROM reround r WHERE r.tbl = 'menu_item_sizes' AND s.id = r.id AND s.price = r.old;
UPDATE modifier_options o SET price = r.new
  FROM reround r WHERE r.tbl = 'modifier_options' AND o.id = r.id AND o.price = r.old;

\echo '== AFTER: rows at the new price, of 141 =='
SELECT r.tbl, count(*) FILTER (WHERE coalesce(m.base_price, s.price, o.price) = r.new) AS at_new, count(*) AS listed
  FROM reround r
  LEFT JOIN menu_items m ON r.tbl = 'menu_items' AND m.id = r.id
  LEFT JOIN menu_item_sizes s ON r.tbl = 'menu_item_sizes' AND s.id = r.id
  LEFT JOIN modifier_options o ON r.tbl = 'modifier_options' AND o.id = r.id
 GROUP BY 1 ORDER BY 1;

\if :apply
COMMIT;
\echo 'APPLIED.'
\else
ROLLBACK;
\echo 'DRY RUN: rolled back. Re-run with -v apply=true to apply.'
\endif
