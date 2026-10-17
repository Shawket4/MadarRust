-- DB architecture Phase 5.1 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 5.1): the
-- dine-in bill lines become relational, in the order_items shape.
--
-- `open_ticket_items.line` (jsonb) held everything about a fired line. What the
-- line IS now lives in columns and child rows, so SQL can see open bills:
--   open_ticket_items.quantity / item_name / unit_price / size_label /
--                     modifier_names            (line_total was a column already)
--   open_ticket_item_addons     the priced add-ons (addon_item_id, quantity,
--                               unit_price, position)
--   open_ticket_item_optionals  the optional fields chosen (option_id, position)
-- `line.input` stays: it is the client's order request, frozen at fire with its
-- prices filled in, and settle replays it VERBATIM through create-order (that
-- is the contract: the bill is charged at what it showed). It is an
-- idempotency record, not the line's data model.
--
-- Dual write by triggers, so every writer (live fire, replayed fire, add round)
-- stays in step without code changes:
--   BEFORE INSERT OR UPDATE OF line: the columns from NEW.line;
--   AFTER  INSERT OR UPDATE OF line: the child rows replaced from NEW.line->'input'.
-- Backfill: here (6,071 lines on the prod copy), idempotent; the table's own
-- UPDATE triggers are off for that one statement (Phase 1.2's rule).
-- Reconciliation (scripts/db_phase5_check.sql): per ticket, line count,
-- sum(quantity) and sum(line_total) equal between the jsonb and the columns;
-- add-on and optional rows equal the jsonb arrays.
-- `line` keeps being written: the POS reads it (OpenTicketItemView.line), so its
-- drop waits on every device reading the relational fields:
-- migrations/parked/p5_ticket_line_json_contract.sql.
SET LOCAL lock_timeout = '15s';

ALTER TABLE open_ticket_items
    ADD COLUMN IF NOT EXISTS quantity       integer,
    ADD COLUMN IF NOT EXISTS item_name      text,
    ADD COLUMN IF NOT EXISTS unit_price     integer,
    ADD COLUMN IF NOT EXISTS size_label     text,
    ADD COLUMN IF NOT EXISTS modifier_names text[];

CREATE UNIQUE INDEX IF NOT EXISTS uq_open_ticket_items_org_id_id ON open_ticket_items (org_id, id);

CREATE TABLE IF NOT EXISTS open_ticket_item_addons (
    org_id        uuid    NOT NULL,
    item_id       uuid    NOT NULL,
    position      integer NOT NULL,
    addon_item_id uuid    NOT NULL,
    quantity      integer NOT NULL,
    unit_price    integer NOT NULL,
    PRIMARY KEY (item_id, position),
    FOREIGN KEY (org_id, item_id) REFERENCES open_ticket_items (org_id, id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS open_ticket_item_optionals (
    org_id    uuid    NOT NULL,
    item_id   uuid    NOT NULL,
    position  integer NOT NULL,
    option_id uuid    NOT NULL,
    PRIMARY KEY (item_id, position),
    FOREIGN KEY (org_id, item_id) REFERENCES open_ticket_items (org_id, id) ON DELETE CASCADE
);

-- The tenant trio and flat RLS on both children.
CREATE OR REPLACE FUNCTION fill_org_id_open_ticket_item_children() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.org_id IS NULL THEN
        SELECT i.org_id INTO NEW.org_id FROM open_ticket_items i WHERE i.id = NEW.item_id;
        IF NEW.org_id IS NULL THEN
            RAISE EXCEPTION '%: no organization for line %', TG_TABLE_NAME, NEW.item_id
                USING ERRCODE = 'foreign_key_violation';
        END IF;
    END IF;
    RETURN NEW;
END $$;
DO $rls$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['open_ticket_item_addons', 'open_ticket_item_optionals'] LOOP
        EXECUTE format('DROP TRIGGER IF EXISTS fill_org_id ON %I', t);
        EXECUTE format('CREATE TRIGGER fill_org_id BEFORE INSERT ON %I FOR EACH ROW '
                       'EXECUTE FUNCTION fill_org_id_open_ticket_item_children()', t);
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %I', t);
        EXECUTE format($p$CREATE POLICY tenant_isolation ON %I FOR ALL
            USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
            WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)$p$, t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO madar_app', t);
    END LOOP;
END $rls$;

-- ── dual write ──────────────────────────────────────────────────────────────
CREATE OR REPLACE FUNCTION open_ticket_item_columns() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.quantity       := (NEW.line ->> 'qty')::integer;
    NEW.item_name      := NEW.line ->> 'name';
    NEW.unit_price     := (NEW.line ->> 'unit_price')::integer;
    NEW.size_label     := NEW.line ->> 'size_label';
    NEW.modifier_names := ARRAY(SELECT jsonb_array_elements_text(
                                   COALESCE(NEW.line -> 'modifiers', '[]'::jsonb)));
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS open_ticket_item_columns ON open_ticket_items;
CREATE TRIGGER open_ticket_item_columns BEFORE INSERT OR UPDATE OF line ON open_ticket_items
    FOR EACH ROW EXECUTE FUNCTION open_ticket_item_columns();

CREATE OR REPLACE FUNCTION open_ticket_item_children() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.line IS NOT DISTINCT FROM OLD.line THEN
        RETURN NULL;
    END IF;
    DELETE FROM open_ticket_item_addons WHERE item_id = NEW.id;
    DELETE FROM open_ticket_item_optionals WHERE item_id = NEW.id;
    INSERT INTO open_ticket_item_addons (org_id, item_id, position, addon_item_id, quantity, unit_price)
    SELECT NEW.org_id, NEW.id, (a.ord - 1)::int, (a.v ->> 'addon_item_id')::uuid,
           COALESCE((a.v ->> 'quantity')::int, 1), COALESCE((a.v ->> 'unit_price')::int, 0)
      FROM jsonb_array_elements(COALESCE(NEW.line -> 'input' -> 'addons', '[]'::jsonb))
           WITH ORDINALITY AS a(v, ord)
     WHERE a.v ? 'addon_item_id';
    INSERT INTO open_ticket_item_optionals (org_id, item_id, position, option_id)
    SELECT NEW.org_id, NEW.id, (o.ord - 1)::int, (o.v #>> '{}')::uuid
      FROM jsonb_array_elements(COALESCE(NEW.line -> 'input' -> 'optional_field_ids', '[]'::jsonb))
           WITH ORDINALITY AS o(v, ord);
    RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS open_ticket_item_children ON open_ticket_items;
CREATE TRIGGER open_ticket_item_children AFTER INSERT OR UPDATE OF line ON open_ticket_items
    FOR EACH ROW EXECUTE FUNCTION open_ticket_item_children();

-- ── backfill (idempotent) ───────────────────────────────────────────────────
DO $bf$
DECLARE
    trg  record;
    off  text[] := '{}';
    mode text[] := '{}';
    i    int;
BEGIN
    FOR trg IN
        SELECT tgname, tgenabled FROM pg_trigger
         WHERE tgrelid = 'open_ticket_items'::regclass AND NOT tgisinternal
           AND tgenabled <> 'D' AND (tgtype & 16) <> 0
           AND tgname NOT IN ('open_ticket_item_columns', 'open_ticket_item_children')
         ORDER BY tgname
    LOOP
        EXECUTE format('ALTER TABLE open_ticket_items DISABLE TRIGGER %I', trg.tgname);
        off  := off  || trg.tgname::text;
        mode := mode || trg.tgenabled::text;
    END LOOP;
    -- `line = line` fires the BEFORE column trigger (UPDATE OF line) for rows
    -- not filled yet; a rerun touches nothing.
    UPDATE open_ticket_items SET line = line WHERE quantity IS NULL;
    FOR i IN 1 .. coalesce(array_length(off, 1), 0) LOOP
        EXECUTE format('ALTER TABLE open_ticket_items ENABLE %s TRIGGER %I',
            CASE mode[i] WHEN 'A' THEN 'ALWAYS' WHEN 'R' THEN 'REPLICA' ELSE '' END, off[i]);
    END LOOP;
END
$bf$;

-- The child rows, set-based (the AFTER trigger skips an UPDATE that leaves
-- `line` as it was). Idempotent: ON CONFLICT DO NOTHING on (item, position).
INSERT INTO open_ticket_item_addons (org_id, item_id, position, addon_item_id, quantity, unit_price)
SELECT i.org_id, i.id, (a.ord - 1)::int, (a.v ->> 'addon_item_id')::uuid,
       COALESCE((a.v ->> 'quantity')::int, 1), COALESCE((a.v ->> 'unit_price')::int, 0)
  FROM open_ticket_items i
 CROSS JOIN LATERAL jsonb_array_elements(COALESCE(i.line -> 'input' -> 'addons', '[]'::jsonb))
       WITH ORDINALITY AS a(v, ord)
 WHERE a.v ? 'addon_item_id'
ON CONFLICT (item_id, position) DO NOTHING;
INSERT INTO open_ticket_item_optionals (org_id, item_id, position, option_id)
SELECT i.org_id, i.id, (o.ord - 1)::int, (o.v #>> '{}')::uuid
  FROM open_ticket_items i
 CROSS JOIN LATERAL jsonb_array_elements(COALESCE(i.line -> 'input' -> 'optional_field_ids', '[]'::jsonb))
       WITH ORDINALITY AS o(v, ord)
ON CONFLICT (item_id, position) DO NOTHING;
