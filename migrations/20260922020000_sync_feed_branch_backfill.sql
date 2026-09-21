-- A branch born AFTER its org's data was never in the changefeed.
--
-- `sync_emit_org` fans an org-level write out to the branches that exist AT
-- THAT MOMENT (20260914090300_sync_changefeed.sql:130-144). A branch created
-- later therefore has no feed rows for anything already in the org — and
-- because a device's first "full snapshot" is built from feed rows, not from
-- the live tables (src/sync/pull/mod.rs:407-416), a brand-new till on that
-- branch sees no payment methods, no menu, no discounts, no staff. The
-- checksum self-heal cannot notice: it is computed from the same feed, so the
-- device and the server agree perfectly on nothing.
--
-- Ten of the twenty-four feed types are org-fanned-out and affected:
-- category, menu_item, bundle, ingredient, payment_method, payment_availability
-- (its user/device legs), discount, teller, addon_item, customer.
--
-- The durable answer is to make the feed complete by construction: a branch
-- backfills itself from the live tables the moment it is created. The
-- authority for "what should be in the feed" already exists — sync_live_rows()
-- — and is what the changefeed's own invariant test compares against.

-- Emit every live row this branch is missing. Idempotent, so it is safe to run
-- on a healthy branch (it inserts nothing) and safe to run repeatedly.
CREATE OR REPLACE FUNCTION sync_backfill_branch(p_branch uuid) RETURNS integer
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    n integer;
BEGIN
    IF p_branch IS NULL THEN
        RETURN 0;
    END IF;
    PERFORM pg_advisory_xact_lock_shared(sync_branch_lock_key(p_branch));
    INSERT INTO sync_changes (branch_id, type, entity_id, op)
    SELECT l.branch_id, l.type, l.entity_id, 'upsert'
      FROM sync_live_rows() l
     WHERE l.branch_id = p_branch
       AND NOT EXISTS (SELECT 1 FROM sync_changes c
                        WHERE c.branch_id = l.branch_id
                          AND c.type = l.type
                          AND c.entity_id = l.entity_id)
    ON CONFLICT (branch_id, type, entity_id) DO NOTHING;
    GET DIAGNOSTICS n = ROW_COUNT;
    IF n > 0 THEN
        PERFORM pg_notify('sync_changes', p_branch::text);
    END IF;
    RETURN n;
END;
$$;

-- A new branch is complete from birth. AFTER INSERT, so the row is visible to
-- sync_live_rows() (which joins `branches`); the existing sync_emit_branches
-- trigger emits this branch's own branch_settings row, and the NOT EXISTS above
-- makes the two orders of firing equivalent.
CREATE OR REPLACE FUNCTION sync_backfill_new_branch() RETURNS trigger
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
BEGIN
    PERFORM sync_backfill_branch(NEW.id);
    RETURN NULL;
END;
$$;

DROP TRIGGER IF EXISTS sync_backfill ON branches;
CREATE TRIGGER sync_backfill AFTER INSERT ON branches
    FOR EACH ROW EXECUTE FUNCTION sync_backfill_new_branch();

-- Heal every branch that is already short, so nobody has to reinstall a till.
-- The rows land with fresh seqs, so an existing device picks them up on its
-- NEXT ordinary incremental pull — no full re-sync, no action by the teller.
SELECT sync_backfill_branch(id) FROM branches WHERE deleted_at IS NULL;
