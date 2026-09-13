-- Content-addressed assets (decisions 17/18/19; TILLS_CONTRACT.md §11.2 as
-- amended by §11.10: variants thumb/tile/full/original/animation, two hashes,
-- reference columns point at an asset GROUP).
-- No data backfill here: the backfill-assets binary does it.

CREATE TABLE asset_groups (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      uuid NULL REFERENCES organizations(id) ON DELETE CASCADE,
    kind        text NOT NULL CHECK (kind IN ('image','animation')),
    source_hash text NOT NULL CHECK (source_hash ~ '^[0-9a-f]{64}$'),
    encoder     text NOT NULL,
    label       text NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT asset_groups_org_required_for_images CHECK (org_id IS NOT NULL OR kind = 'animation')
);
CREATE UNIQUE INDEX uq_asset_groups_org_source ON asset_groups (org_id, source_hash, encoder) WHERE org_id IS NOT NULL;
CREATE UNIQUE INDEX uq_asset_groups_global_source ON asset_groups (source_hash, encoder) WHERE org_id IS NULL;

CREATE TABLE assets (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id           uuid NULL REFERENCES organizations(id) ON DELETE CASCADE,
    hash             text NOT NULL CHECK (hash ~ '^[0-9a-f]{64}$'),
    group_id         uuid NOT NULL REFERENCES asset_groups(id) ON DELETE CASCADE,
    encoder          text NOT NULL,
    encoder_settings jsonb NOT NULL DEFAULT '{}'::jsonb,
    kind             text NOT NULL CHECK (kind IN ('image','animation')),
    variant          text NOT NULL CHECK (variant IN ('thumb','tile','full','original','animation')),
    ext              text NOT NULL CHECK (ext IN ('webp','lottie.zst')),
    content_type     text NOT NULL CHECK (content_type IN ('image/webp','application/zstd')),
    bytes            bigint NOT NULL CHECK (bytes > 0),
    width            integer NULL,
    height           integer NULL,
    has_alpha        boolean NOT NULL DEFAULT false,
    source_hash      text NOT NULL CHECK (source_hash ~ '^[0-9a-f]{64}$'),
    source_kind      text NOT NULL CHECK (source_kind IN ('upload','url','backfill','preset','import','ai')),
    label            text NULL,
    created_by       uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT assets_org_required_for_images CHECK (org_id IS NOT NULL OR kind = 'animation')
);
CREATE UNIQUE INDEX uq_assets_org_hash    ON assets (org_id, hash) WHERE org_id IS NOT NULL;
CREATE UNIQUE INDEX uq_assets_global_hash ON assets (hash) WHERE org_id IS NULL;
CREATE UNIQUE INDEX uq_assets_org_source_variant ON assets (org_id, source_hash, variant, encoder) WHERE org_id IS NOT NULL;
CREATE INDEX idx_assets_group ON assets (group_id);

CREATE TABLE asset_jobs (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id          uuid NULL REFERENCES organizations(id) ON DELETE CASCADE,
    purpose         text NOT NULL CHECK (purpose IN ('menu_item_photo','category_photo','bundle_photo','org_logo','loyalty_card_image','step_animation','delivery_menu_photo','import_photo','ai_photo')),
    source_kind     text NOT NULL CHECK (source_kind IN ('upload','url','backfill','preset','import','ai')),
    staged_path     text NULL,
    source_url      text NULL,
    label           text NULL,
    target_table    text NOT NULL,
    target_id       uuid NOT NULL,
    target_field    text NOT NULL,
    status          text NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','running','done','failed')),
    attempts        integer NOT NULL DEFAULT 0,
    last_error      text NULL,
    result_group_id uuid NULL REFERENCES asset_groups(id) ON DELETE SET NULL,
    created_by      uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT asset_jobs_has_source CHECK (staged_path IS NOT NULL OR source_url IS NOT NULL)
);
CREATE INDEX idx_asset_jobs_pending ON asset_jobs (created_at) WHERE status IN ('queued','running');
CREATE INDEX idx_asset_jobs_target ON asset_jobs (target_table, target_id, target_field);
CREATE TRIGGER trg_asset_jobs_updated_at BEFORE UPDATE ON asset_jobs FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE asset_bundles (
    branch_id  uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    seq        bigint NOT NULL,
    file_key   text NOT NULL,
    bytes      bigint NOT NULL,
    sha256     text NOT NULL,
    file_count integer NOT NULL,
    built_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, seq)
);
CREATE TABLE asset_bundle_dirty (
    branch_id   uuid PRIMARY KEY REFERENCES branches(id) ON DELETE CASCADE,
    dirty_since timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE asset_legacy_paths (
    legacy_path text PRIMARY KEY,
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    asset_id    uuid NOT NULL REFERENCES assets(id) ON DELETE CASCADE,   -- the `full` variant
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE asset_backfill_items (
    source_table text NOT NULL,
    source_id    text NOT NULL,          -- text: recipe_step_presets is keyed by slug
    source_field text NOT NULL,
    org_id       uuid NULL,
    legacy_url   text NOT NULL,
    legacy_path  text NULL,
    status       text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','ingested','verified','missing','broken','skipped','failed')),
    group_id     uuid NULL REFERENCES asset_groups(id),
    source_bytes bigint NULL,
    stored_bytes bigint NULL,
    deduped      boolean NOT NULL DEFAULT false,
    attempts     integer NOT NULL DEFAULT 0,
    last_error   text NULL,
    run_id       uuid NULL,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (source_table, source_id, source_field)
);

ALTER TABLE asset_groups ENABLE ROW LEVEL SECURITY;
ALTER TABLE assets       ENABLE ROW LEVEL SECURITY;
ALTER TABLE asset_jobs   ENABLE ROW LEVEL SECURITY;
ALTER TABLE asset_bundles ENABLE ROW LEVEL SECURITY;
ALTER TABLE asset_legacy_paths ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON asset_groups FOR ALL
    USING (org_id IS NULL OR org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON assets FOR ALL
    USING (org_id IS NULL OR org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON asset_jobs FOR ALL
    USING (org_id IS NULL OR org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON asset_bundles FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON asset_legacy_paths FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE asset_groups, assets, asset_jobs, asset_bundles,
    asset_bundle_dirty, asset_legacy_paths, asset_backfill_items TO madar_app;

-- ── Reference columns (legacy URL columns are kept) ──────────────────────────
ALTER TABLE menu_items          ADD COLUMN image_group_id uuid NULL CONSTRAINT menu_items_image_group_id_fkey REFERENCES asset_groups(id) ON DELETE SET NULL;
ALTER TABLE categories          ADD COLUMN image_group_id uuid NULL CONSTRAINT categories_image_group_id_fkey REFERENCES asset_groups(id) ON DELETE SET NULL;
ALTER TABLE bundles             ADD COLUMN image_group_id uuid NULL CONSTRAINT bundles_image_group_id_fkey REFERENCES asset_groups(id) ON DELETE SET NULL;
ALTER TABLE organizations       ADD COLUMN logo_group_id uuid NULL CONSTRAINT organizations_logo_group_id_fkey REFERENCES asset_groups(id) ON DELETE SET NULL,
                                ADD COLUMN brand_card_image_group_id uuid NULL CONSTRAINT organizations_brand_card_image_group_id_fkey REFERENCES asset_groups(id) ON DELETE SET NULL;
ALTER TABLE recipe_step_presets ADD COLUMN animation_group_id uuid NULL CONSTRAINT recipe_step_presets_animation_group_id_fkey REFERENCES asset_groups(id) ON DELETE SET NULL;

-- ── Asset ref change → bundle dirty (+ feed re-emit where no emitter exists) ─
CREATE FUNCTION asset_mark_org_dirty(p_org uuid) RETURNS void
    LANGUAGE sql SECURITY DEFINER SET search_path = public
    AS $$
    INSERT INTO asset_bundle_dirty (branch_id)
    SELECT id FROM branches WHERE org_id = p_org AND deleted_at IS NULL
    ON CONFLICT (branch_id) DO NOTHING;
$$;

-- menu_items / categories / bundles: their sync_emit triggers already re-emit
-- the row on any UPDATE; this only marks the bundle dirty.
CREATE FUNCTION asset_ref_changed_org_row() RETURNS trigger
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
BEGIN
    PERFORM asset_mark_org_dirty(NEW.org_id);
    RETURN NULL;
END $$;
CREATE TRIGGER asset_ref_changed AFTER UPDATE OF image_group_id ON menu_items
    FOR EACH ROW WHEN (OLD.image_group_id IS DISTINCT FROM NEW.image_group_id) EXECUTE FUNCTION asset_ref_changed_org_row();
CREATE TRIGGER asset_ref_changed AFTER UPDATE OF image_group_id ON categories
    FOR EACH ROW WHEN (OLD.image_group_id IS DISTINCT FROM NEW.image_group_id) EXECUTE FUNCTION asset_ref_changed_org_row();
CREATE TRIGGER asset_ref_changed AFTER UPDATE OF image_group_id ON bundles
    FOR EACH ROW WHEN (OLD.image_group_id IS DISTINCT FROM NEW.image_group_id) EXECUTE FUNCTION asset_ref_changed_org_row();

-- organizations: logo feeds branch_settings.logo_hash for every branch.
CREATE FUNCTION asset_ref_changed_organizations() RETURNS trigger
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r branches;
BEGIN
    FOR r IN SELECT * FROM branches WHERE org_id = NEW.id AND deleted_at IS NULL ORDER BY id LOOP
        PERFORM sync_emit(r.id, 'branch_settings', r.id, sync_op(sync_live_branch_settings(r)));
    END LOOP;
    PERFORM asset_mark_org_dirty(NEW.id);
    RETURN NULL;
END $$;
CREATE TRIGGER asset_ref_changed AFTER UPDATE OF logo_group_id, brand_card_image_group_id ON organizations
    FOR EACH ROW WHEN (OLD.logo_group_id IS DISTINCT FROM NEW.logo_group_id
                    OR OLD.brand_card_image_group_id IS DISTINCT FROM NEW.brand_card_image_group_id)
    EXECUTE FUNCTION asset_ref_changed_organizations();

-- recipe_step_presets: its sync_emit trigger re-emits the items; mark their orgs dirty.
CREATE FUNCTION asset_ref_changed_recipe_step_presets() RETURNS trigger
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    o uuid;
BEGIN
    FOR o IN SELECT DISTINCT org_id FROM menu_item_recipe_steps WHERE preset_slug = NEW.slug ORDER BY 1 LOOP
        PERFORM asset_mark_org_dirty(o);
    END LOOP;
    RETURN NULL;
END $$;
CREATE TRIGGER asset_ref_changed AFTER UPDATE OF animation_group_id ON recipe_step_presets
    FOR EACH ROW WHEN (OLD.animation_group_id IS DISTINCT FROM NEW.animation_group_id)
    EXECUTE FUNCTION asset_ref_changed_recipe_step_presets();
