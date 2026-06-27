-- Tills = physical cash drawers / registers. A till is the unit of cash
-- continuity and of shift concurrency (NOT the teller): the float physically
-- lives in the drawer, so a new shift opens carrying over THAT drawer's last
-- declared closing, regardless of which teller is on it. Multiple tills may be
-- open at one branch at once (multi-teller); a single till holds one open shift.
--
-- A till is a managed, branch-scoped entity (dashboard CRUD). A POS device binds
-- to a till (like it binds to a branch/printer) and is reconfigurable. `is_default`
-- marks the catch-all till used when a shift opens without an explicit till_id
-- (legacy/offline clients) — exactly one default per branch.

CREATE TABLE tills (
    id         uuid    PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id     uuid    NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id  uuid    NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    name       text    NOT NULL,
    is_default boolean NOT NULL DEFAULT false,
    is_active  boolean NOT NULL DEFAULT true,
    created_at timestamp with time zone NOT NULL DEFAULT now(),
    updated_at timestamp with time zone NOT NULL DEFAULT now(),
    deleted_at timestamp with time zone
);

CREATE INDEX idx_tills_branch ON tills (branch_id) WHERE deleted_at IS NULL;
-- Unique active till name per branch.
CREATE UNIQUE INDEX uq_tills_name    ON tills (branch_id, lower(name)) WHERE deleted_at IS NULL;
-- At most one default till per branch.
CREATE UNIQUE INDEX uq_tills_default ON tills (branch_id) WHERE is_default AND deleted_at IS NULL;

GRANT ALL ON tills TO sufrix;
