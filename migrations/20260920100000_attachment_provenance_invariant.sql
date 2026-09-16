-- An attachment always says where it comes from and which options it offers.
--
-- Old tills (v0.5–v0.7) never read menu_item_modifier_groups: they read the
-- contract-shim views. `menu_item_addon_slots` only shows `legacy_origin='slot'`,
-- `menu_item_optional_fields` only `legacy_origin='options'`, and
-- `menu_item_allowed_addons` is the per-ITEM union of `included_option_ids`
-- (NULL = no rows). So an attachment written with NULL provenance (Studio attach,
-- duplicate, the item Options endpoint) was invisible or half-visible there:
--   * NULL legacy_origin  → not a slot (required not enforced), a private
--     "Options" group's optionals were neither charged nor deducted;
--   * NULL included_option_ids next to a restricted group on the same item →
--     the legacy allowlist is non-empty, so the NULL group's options vanish.
--
-- Rule (also used by the handlers):
--   legacy_origin       = 'options'   when the group is item-private (legacy_addon_type NULL)
--                       = 'slot'      when required (item override, else group is_required)
--                       = 'allowlist' otherwise
--   included_option_ids = every option of the group (ordered by sort, name, id)
--
-- Inactive options are included on purpose: the shim views and both POS paths filter
-- on is_active themselves, and keeping them listed means re-activating an option
-- brings it back on every item instead of silently leaving it off.
--
-- A BEFORE trigger fills the two columns whenever a writer leaves them NULL (old
-- dashboards, raw SQL, the demo seed), instead of a CHECK that would 500 those writers.
-- Options added to (or moved into) a group are appended to every attachment that
-- offered the whole group; a deleted option is removed from every list.

CREATE OR REPLACE FUNCTION mimg_default_origin(p_group_id uuid, p_required_override boolean)
RETURNS text
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
    SELECT CASE
             WHEN g.legacy_addon_type IS NULL THEN 'options'
             WHEN COALESCE(p_required_override, g.is_required) THEN 'slot'
             ELSE 'allowlist'
           END
      FROM modifier_groups g
     WHERE g.id = p_group_id
$$;

CREATE OR REPLACE FUNCTION mimg_all_option_ids(p_group_id uuid)
RETURNS uuid[]
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
    SELECT COALESCE(array_agg(o.id ORDER BY o.sort, o.name, o.id), '{}'::uuid[])
      FROM modifier_options o
     WHERE o.group_id = p_group_id
$$;

-- 1. Backfill.
UPDATE menu_item_modifier_groups m
   SET legacy_origin       = COALESCE(m.legacy_origin, mimg_default_origin(m.group_id, m.is_required_override)),
       included_option_ids = COALESCE(m.included_option_ids, mimg_all_option_ids(m.group_id))
 WHERE m.legacy_origin IS NULL OR m.included_option_ids IS NULL;

-- 2. Fill on every write.
CREATE OR REPLACE FUNCTION mimg_fill_provenance() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF NEW.legacy_origin IS NULL THEN
        NEW.legacy_origin := mimg_default_origin(NEW.group_id, NEW.is_required_override);
    END IF;
    IF NEW.included_option_ids IS NULL THEN
        NEW.included_option_ids := mimg_all_option_ids(NEW.group_id);
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER mimg_fill_provenance
    BEFORE INSERT OR UPDATE ON menu_item_modifier_groups
    FOR EACH ROW EXECUTE FUNCTION mimg_fill_provenance();

-- 3. Keep "the whole group" lists whole when the group's options change.
CREATE OR REPLACE FUNCTION modifier_options_maintain_included() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('DELETE', 'UPDATE') AND (TG_OP = 'DELETE' OR OLD.group_id IS DISTINCT FROM NEW.group_id) THEN
        UPDATE menu_item_modifier_groups m
           SET included_option_ids = array_remove(m.included_option_ids, OLD.id)
         WHERE m.group_id = OLD.group_id
           AND OLD.id = ANY(m.included_option_ids);
    END IF;

    IF TG_OP = 'INSERT' OR (TG_OP = 'UPDATE' AND OLD.group_id IS DISTINCT FROM NEW.group_id) THEN
        -- An attachment "offered the whole group" when it lists every OTHER option.
        UPDATE menu_item_modifier_groups m
           SET included_option_ids = array_append(m.included_option_ids, NEW.id)
         WHERE m.group_id = NEW.group_id
           AND m.included_option_ids IS NOT NULL
           AND NOT (NEW.id = ANY(m.included_option_ids))
           AND NOT EXISTS (
                 SELECT 1 FROM modifier_options o
                  WHERE o.group_id = NEW.group_id
                    AND o.id <> NEW.id
                    AND NOT (o.id = ANY(m.included_option_ids)));
    END IF;
    RETURN NULL;
END $$;

CREATE TRIGGER modifier_options_maintain_included
    AFTER INSERT OR DELETE OR UPDATE OF group_id ON modifier_options
    FOR EACH ROW EXECUTE FUNCTION modifier_options_maintain_included();
