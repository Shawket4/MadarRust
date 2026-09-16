-- A step on an item can now carry its OWN note, in either language.
--
-- Until now a preset step's note came from the library and read identically on
-- every item using it: "Steam milk — 60 °C with foam". That is right for the
-- technique and useless for the drink. A Spanish latte wants "40ml condensed
-- milk, mixed with the shot first"; the next drink using the same preset wants
-- something else. The only way to say anything item-specific was to abandon the
-- preset and type a custom step, which throws away the animation.
--
-- So: `note` / `note_ar` on the step itself, valid on BOTH kinds. The step's
-- note wins where it is set; otherwise the preset's note still shows, so every
-- existing step reads exactly as it did.
--
-- SOURCE TABLES: menu_item_recipe_steps (already emits menu_item; a column
-- change needs no new trigger — the projection carries the new fields).
ALTER TABLE menu_item_recipe_steps
    ADD COLUMN IF NOT EXISTS note    text,
    ADD COLUMN IF NOT EXISTS note_ar text;

-- The shape rule only ever governed the NAME: a preset step is named by the
-- library, a custom step by whoever typed it. Notes are free on either.
ALTER TABLE menu_item_recipe_steps DROP CONSTRAINT IF EXISTS recipe_steps_shape;
ALTER TABLE menu_item_recipe_steps ADD CONSTRAINT recipe_steps_shape CHECK (
    (kind = 'preset' AND preset_slug IS NOT NULL AND title IS NULL AND title_ar IS NULL)
    OR (kind = 'custom' AND preset_slug IS NULL AND COALESCE(title, title_ar) IS NOT NULL)
);
