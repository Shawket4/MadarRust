-- Content-addressed files may be shared by several asset rows.
--
-- 090400 made (org_id, hash) unique on `assets`, so two groups whose variants
-- happen to encode to the same bytes (a logo with the same pixels as a menu
-- photo; two different photos with byte-identical thumbnails) could not both
-- exist: the second insert failed and the backfill retried it until `failed`.
-- A file is identified by (org, hash); a ROW is identified by (group, variant).
--
-- A group also records its conversion profile: a logo/card group keeps a
-- lossless `original`, a photo group does not. Source dedup must never hand a
-- logo the photo group made from the same file (it has no original).

DROP INDEX uq_assets_org_hash;
DROP INDEX uq_assets_global_hash;
DROP INDEX uq_assets_org_source_variant;
CREATE UNIQUE INDEX uq_assets_group_variant ON assets (group_id, variant);
CREATE INDEX idx_assets_org_hash ON assets (org_id, hash);

ALTER TABLE asset_groups ADD COLUMN profile text NOT NULL DEFAULT 'photo'
    CONSTRAINT asset_groups_profile_check CHECK (profile IN ('photo','keeps_original'));
-- Groups made before this migration: a stored original means keeps_original.
-- So does a lossless `full` (only logo/card conversions encode `full`
-- losslessly); there the original was byte-identical to `full` and 090400's
-- (org, hash) rule folded it into the `full` row.
UPDATE asset_groups g SET profile = 'keeps_original'
 WHERE EXISTS (SELECT 1 FROM assets a WHERE a.group_id = g.id
                 AND (a.variant = 'original' OR (a.variant = 'full' AND a.encoder_settings->>'lossless' = 'true')));
-- Give those folded originals their own row, sharing the `full` file.
INSERT INTO assets (org_id, hash, group_id, encoder, encoder_settings, kind, variant, ext, content_type,
                    bytes, width, height, has_alpha, source_hash, source_kind, label, created_by, created_at)
SELECT a.org_id, a.hash, a.group_id, a.encoder, a.encoder_settings, a.kind, 'original', a.ext, a.content_type,
       a.bytes, a.width, a.height, a.has_alpha, a.source_hash, a.source_kind, a.label, a.created_by, a.created_at
  FROM assets a JOIN asset_groups g ON g.id = a.group_id
 WHERE g.profile = 'keeps_original' AND a.variant = 'full'
   AND NOT EXISTS (SELECT 1 FROM assets o WHERE o.group_id = a.group_id AND o.variant = 'original');

DROP INDEX uq_asset_groups_org_source;
DROP INDEX uq_asset_groups_global_source;
CREATE UNIQUE INDEX uq_asset_groups_org_source ON asset_groups (org_id, source_hash, encoder, profile) WHERE org_id IS NOT NULL;
CREATE UNIQUE INDEX uq_asset_groups_global_source ON asset_groups (source_hash, encoder, profile) WHERE org_id IS NULL;
