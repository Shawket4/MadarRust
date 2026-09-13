-- Deterministic base data for the legacy till goldens. Valid on BOTH the
-- pre-rework schema (captured by the old backend) and the current one (the
-- strict test in src/tills/legacy_tests.rs). Only tables the rework does not
-- touch; every till/order/ticket/delivery row is created through the API by
-- scenario.json.
INSERT INTO organizations (id, name, slug, tax_rate) VALUES
  ('10000000-0000-4000-8000-000000000001', 'Golden Org', 'golden-org', 0);
INSERT INTO branches (id, org_id, name, code, latitude, longitude) VALUES
  ('10000000-0000-4000-8000-0000000000a1', '10000000-0000-4000-8000-000000000001', 'Golden A', 'GLDA', 30.0, 31.0),
  ('10000000-0000-4000-8000-0000000000b1', '10000000-0000-4000-8000-000000000001', 'Golden B', 'GLDB', 30.0, 31.0),
  ('10000000-0000-4000-8000-0000000000c1', '10000000-0000-4000-8000-000000000001', 'Golden C', 'GLDC', 30.0, 31.0);
INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES
  ('10000000-0000-4000-8000-00000000ad01', '10000000-0000-4000-8000-000000000001', 'Golden Admin',  'admin@golden.test',  'x', 'org_admin'),
  ('10000000-0000-4000-8000-00000000ee0a', '10000000-0000-4000-8000-000000000001', 'Teller Alpha',  'alpha@golden.test',  'x', 'teller'),
  ('10000000-0000-4000-8000-00000000ee0b', '10000000-0000-4000-8000-000000000001', 'Teller Bravo',  'bravo@golden.test',  'x', 'teller'),
  ('10000000-0000-4000-8000-00000000aa01', '10000000-0000-4000-8000-000000000001', 'Waiter Whisky', 'whisky@golden.test', 'x', 'waiter');
INSERT INTO user_branch_assignments (user_id, branch_id) VALUES
  ('10000000-0000-4000-8000-00000000ee0a', '10000000-0000-4000-8000-0000000000a1'),
  ('10000000-0000-4000-8000-00000000ee0b', '10000000-0000-4000-8000-0000000000b1'),
  ('10000000-0000-4000-8000-00000000aa01', '10000000-0000-4000-8000-0000000000a1'),
  ('10000000-0000-4000-8000-00000000aa01', '10000000-0000-4000-8000-0000000000b1');
INSERT INTO categories (id, org_id, name) VALUES
  ('10000000-0000-4000-8000-0000000c0001', '10000000-0000-4000-8000-000000000001', 'Drinks');
INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES
  ('10000000-0000-4000-8000-0000000e0001', '10000000-0000-4000-8000-000000000001', '10000000-0000-4000-8000-0000000c0001', 'Latte', 5000, true);
INSERT INTO org_payment_methods (id, org_id, name, color, icon, is_cash, is_active) VALUES
  ('10000000-0000-4000-8000-0000000f0001', '10000000-0000-4000-8000-000000000001', 'cash', '#000', 'cash', true, true),
  ('10000000-0000-4000-8000-0000000f0002', '10000000-0000-4000-8000-000000000001', 'card', '#111', 'card', false, true);
INSERT INTO branch_delivery_settings (branch_id, in_mall_enabled, outside_enabled, in_mall_fee) VALUES
  ('10000000-0000-4000-8000-0000000000b1', true, false, 300);
