-- OPT-A read fast paths (src/fastpath): the notifications that keep the
-- server's in-memory watermarks honest, and the one helper the idle-pull cache
-- needs. Additive only: four small AFTER triggers whose whole body is a
-- pg_notify, on tables that change rarely (never on a hot write table), and
-- one STABLE function. Nothing is read or written by them.
--
--   authz_epoch (INSERT/UPDATE)        → 'authz_epoch'       '<org>:<epoch>'
--   users.sessions_valid_after changed → 'authz_user'        '<user>'
--   catalog_revision (INSERT/UPDATE)   → 'catalog_revision'  '<org>:<revision>'
--   branches (DELETE)                  → 'sync_changes'      '<branch>'
--       A hard delete cascades the branch's feed away without an emit, so
--       nothing else would tell a cached "this branch exists, at org X".
--
-- A NOTIFY is delivered at COMMIT, in commit order, to every instance that
-- LISTENs: each instance drops what the change touched. The listener treats a
-- lost connection as "everything may have changed" (src/fastpath/listener.rs).

CREATE FUNCTION fastpath_notify_authz_epoch() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('authz_epoch', NEW.org_id::text || ':' || NEW.epoch::text);
    RETURN NULL;
END $$;

CREATE TRIGGER fastpath_notify AFTER INSERT OR UPDATE ON authz_epoch
    FOR EACH ROW EXECUTE FUNCTION fastpath_notify_authz_epoch();

-- `sessions_valid_after` is what revokes a web session (password change, sign
-- out everywhere). A role / active / delete change also bumps the org's
-- authz_epoch (trigger authz_users_sync), which notifies above.
CREATE FUNCTION fastpath_notify_authz_user() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('authz_user', NEW.id::text);
    RETURN NULL;
END $$;

CREATE TRIGGER fastpath_notify_sessions AFTER UPDATE OF sessions_valid_after ON users
    FOR EACH ROW WHEN (OLD.sessions_valid_after IS DISTINCT FROM NEW.sessions_valid_after)
    EXECUTE FUNCTION fastpath_notify_authz_user();

CREATE FUNCTION fastpath_notify_catalog_revision() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('catalog_revision', NEW.org_id::text || ':' || NEW.revision::text);
    RETURN NULL;
END $$;

CREATE TRIGGER fastpath_notify AFTER INSERT OR UPDATE ON catalog_revision
    FOR EACH ROW EXECUTE FUNCTION fastpath_notify_catalog_revision();

CREATE FUNCTION fastpath_notify_branch_deleted() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('sync_changes', OLD.id::text);
    RETURN NULL;
END $$;

CREATE TRIGGER fastpath_notify_deleted AFTER DELETE ON branches
    FOR EACH ROW EXECUTE FUNCTION fastpath_notify_branch_deleted();

-- The earliest moment a branch's state checksums can change WITHOUT a write:
-- the three state types whose live gate reads the clock (`sync_live_booking`,
-- `sync_live_delivery`, `sync_live_kitchen_ticket`; the sweeper's TIME_BASED
-- list) each leave the live set at a fixed time after their last write. A
-- cached "nothing changed" answer is good until then at the latest. NULL =
-- nothing on the clock. Each predicate is its gate's "live now and leaves
-- later" half; `fastpath_tests` fails if another gate starts reading the clock.
CREATE FUNCTION sync_idle_valid_until(p_branch uuid) RETURNS timestamptz
LANGUAGE sql STABLE AS $$
    SELECT LEAST(
        (SELECT min(k.closed_at) + interval '12 hours' FROM kitchen_tickets k
          WHERE k.branch_id = p_branch AND k.close_reason IS DISTINCT FROM 'retired'
            AND k.closed_at IS NOT NULL AND k.closed_at >= now() - interval '12 hours'),
        (SELECT min(d.updated_at) + interval '48 hours' FROM delivery_orders d
          WHERE d.branch_id = p_branch AND d.status IN ('delivered', 'cancelled', 'rejected')
            AND d.updated_at >= now() - interval '48 hours'),
        (SELECT min(b.ends_at) + interval '1 day' FROM bookings b
          WHERE b.branch_id = p_branch AND b.status IN ('confirmed', 'seated')
            AND b.ends_at >= now() - interval '1 day'))
$$;

GRANT EXECUTE ON FUNCTION sync_idle_valid_until(uuid) TO madar_app;
