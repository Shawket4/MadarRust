-- An add-on's feed row now carries how a sale line charges it (`pricing`,
-- madar-catalog's OptionView): its group's effect and swap category among
-- the rest. The till prices its lines from that row, so the row must move
-- when the group's effect or swap category moves — for EVERY group, not only
-- the legacy-typed ones this emitter used to re-emit for (a custom group
-- switched from `adds` to `swaps` left its options' rows, and so the till's
-- prices, as they were until each option was edited).
--
-- Same function as 20260915090000_sync_feed_gaps.sql, one condition wider.
CREATE OR REPLACE FUNCTION sync_emit_modifier_groups() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    o uuid;
BEGIN
    -- On DELETE the link rows cascade (their own trigger re-emits the items).
    IF TG_OP <> 'DELETE' THEN PERFORM sync_touch_menu_items_of_group(NEW.id); END IF;
    IF TG_OP = 'DELETE' THEN
        IF OLD.legacy_addon_type IS NOT NULL THEN PERFORM sync_touch_retired_addon_items(OLD.org_id); END IF;
    ELSIF NEW.legacy_addon_type IS NOT NULL
       OR (TG_OP = 'UPDATE' AND (OLD.legacy_addon_type IS NOT NULL
                                 OR OLD.effect IS DISTINCT FROM NEW.effect
                                 OR OLD.swap_category_id IS DISTINCT FROM NEW.swap_category_id)) THEN
        FOR o IN SELECT id FROM modifier_options WHERE group_id = NEW.id ORDER BY id LOOP
            PERFORM sync_touch_or_retire_addon_item(o, NEW.org_id);
        END LOOP;
        IF TG_OP = 'UPDATE' AND OLD.org_id IS DISTINCT FROM NEW.org_id THEN
            PERFORM sync_touch_retired_addon_items(OLD.org_id);
        END IF;
    END IF;
    RETURN NULL;
END $$;

-- Every add-on's feed row gains `pricing` with this release. A row changes on
-- a device only when its sequence moves, so re-emit each one once: a till then
-- pulls the new rows on its next sync instead of pricing from rows that
-- predate the field until someone edits each add-on. Old tills read the same
-- row with one field more.
DO $$
DECLARE
    a uuid;
BEGIN
    FOR a IN SELECT id FROM addon_items ORDER BY id LOOP
        PERFORM sync_touch_addon_item(a);
    END LOOP;
END $$;
