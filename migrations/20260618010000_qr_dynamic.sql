-- Per-branch table entities (dine-in table QR target) and a short-link
-- dedup cache that maps (org, kind, target_ref) → Shlink short URL so
-- repeat QR requests reuse the same code rather than creating duplicates.

CREATE TABLE branch_tables (
    id         uuid    PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id     uuid    NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id  uuid    NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    label      text    NOT NULL,
    is_active  boolean NOT NULL DEFAULT true,
    created_at timestamp with time zone NOT NULL DEFAULT now(),
    updated_at timestamp with time zone NOT NULL DEFAULT now(),
    UNIQUE (branch_id, label)
);
CREATE INDEX idx_branch_tables_branch ON branch_tables (branch_id);

CREATE TABLE qr_short_links (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id     uuid NOT NULL REFERENCES organizations(id)  ON DELETE CASCADE,
    branch_id  uuid          REFERENCES branches(id)       ON DELETE CASCADE,
    kind       text NOT NULL,
    target_ref text NOT NULL,
    long_url   text NOT NULL,
    short_code text NOT NULL,
    short_url  text NOT NULL,
    label      text,
    created_at timestamp with time zone NOT NULL DEFAULT now(),
    UNIQUE (org_id, kind, target_ref)
);
CREATE INDEX idx_qr_short_links_org ON qr_short_links (org_id, kind);

GRANT ALL ON branch_tables   TO sufrix;
GRANT ALL ON qr_short_links  TO sufrix;
