-- Preparation steps for a menu item.
--
-- A step is one of two things:
--
--   * a PRESET — one of the curated steps that ships with the backend. The
--     preset owns its animation, its name and its note, in both languages, so
--     "Steam milk" means the same thing and looks the same on every item that
--     uses it. An item's step only points at it.
--   * a CUSTOM step — a name someone typed in the dashboard for something the
--     library has no preset for. Naming a step yourself is exactly what makes
--     it custom, and a custom step has no animation.
--
-- Steps belong to the item, not the size: the order of preparation does not
-- change between a 12 oz and a 16 oz, and the amounts already live in the
-- per-size recipe lines.
--
-- The preset table is DERIVED, never hand-written. On start the backend scans
-- its animations folder and reconciles this table against what actually shipped
-- (see `recipes::steps::reconcile`), so a preset's fingerprint can never
-- disagree with the bytes on disk, and adding a preset is a file plus a line of
-- names in the manifest, not another migration.

-- ── The preset library (global: one curated set for every org) ───────────────
CREATE TABLE recipe_step_presets (
    slug        text PRIMARY KEY,
    name        text NOT NULL,
    name_ar     text NOT NULL,
    -- The technique detail under the name ("60 °C with foam"). Optional.
    note        text,
    note_ar     text,
    -- SHA-256 of the animation file. Clients cache by this, so a replaced file
    -- is re-fetched and an unchanged one never is.
    sha256      text NOT NULL,
    bytes       integer NOT NULL,
    -- Order the library is offered in.
    sort_order  smallint NOT NULL DEFAULT 0,
    -- False once the file stops shipping. The row is KEPT so items that use the
    -- preset keep showing its name, and so history stays readable.
    is_active   boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT step_presets_slug_format CHECK (slug ~ '^[a-z0-9_]{1,64}$')
);

-- Global reference data, like `role_permissions`: readable by every tenant,
-- written only by the backend through the owner pool.
ALTER TABLE recipe_step_presets ENABLE ROW LEVEL SECURITY;
CREATE POLICY global_read ON recipe_step_presets FOR SELECT USING (true);
GRANT ALL ON TABLE recipe_step_presets TO sufrix;

-- ── The steps of one menu item ──────────────────────────────────────────────
CREATE TABLE menu_item_recipe_steps (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    menu_item_id uuid NOT NULL REFERENCES menu_items(id)    ON DELETE CASCADE,
    position     smallint NOT NULL CHECK (position >= 1),
    kind         text NOT NULL CHECK (kind IN ('preset', 'custom')),
    -- Set for a preset step. Deliberately NOT a foreign key: a step must
    -- survive its preset being retired, falling back to the name held on the
    -- retired row rather than vanishing from the recipe.
    preset_slug  text,
    -- The typed name of a custom step, in either or both languages.
    title        text,
    title_ar     text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT recipe_steps_position_unique UNIQUE (menu_item_id, position),
    CONSTRAINT recipe_steps_shape CHECK (
        (kind = 'preset' AND preset_slug IS NOT NULL AND title IS NULL AND title_ar IS NULL)
        OR (kind = 'custom' AND preset_slug IS NULL AND COALESCE(title, title_ar) IS NOT NULL)
    )
);
CREATE INDEX idx_recipe_steps_item ON menu_item_recipe_steps (menu_item_id, position);

ALTER TABLE menu_item_recipe_steps ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON menu_item_recipe_steps FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE menu_item_recipe_steps TO sufrix;
