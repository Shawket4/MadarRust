-- Permissions Phase 0 (PERMISSIONS_ARCHITECTURE.md §6).
--
-- 1. Revocable web sessions: a bearer token issued before
--    users.sessions_valid_after is refused by the permission checker. Bumped on a
--    password, role or active-flag change and on delete.
-- 2. Grant history starts now: every write to the legacy grant tables
--    (`permissions`, `role_permissions`) is recorded with before/after, so Phase 4
--    replay can ask "was this granted at the time".
--
-- Additive only. No data is changed.

ALTER TABLE users ADD COLUMN IF NOT EXISTS sessions_valid_after timestamptz NULL;

CREATE TABLE IF NOT EXISTS authz_grant_events (
    id          bigserial PRIMARY KEY,
    org_id      uuid NULL,
    occurred_at timestamptz NOT NULL DEFAULT now(),
    table_name  text NOT NULL,
    op          text NOT NULL CHECK (op IN ('INSERT', 'UPDATE', 'DELETE')),
    actor_id    uuid NULL,
    before      jsonb NULL,
    after       jsonb NULL
);
CREATE INDEX IF NOT EXISTS authz_grant_events_org_time
    ON authz_grant_events (org_id, occurred_at);

CREATE OR REPLACE FUNCTION authz_record_grant_event() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    v_user uuid;
    v_org  uuid;
    v_actor uuid;
BEGIN
    IF TG_TABLE_NAME = 'permissions' THEN
        v_user := COALESCE((CASE WHEN TG_OP = 'DELETE' THEN OLD.user_id ELSE NEW.user_id END), NULL);
        SELECT org_id INTO v_org FROM users WHERE id = v_user;
    END IF;
    BEGIN
        v_actor := NULLIF(current_setting('app.actor_id', true), '')::uuid;
    EXCEPTION WHEN others THEN
        v_actor := NULL;
    END;
    INSERT INTO authz_grant_events (org_id, table_name, op, actor_id, before, after)
    VALUES (
        v_org, TG_TABLE_NAME, TG_OP, v_actor,
        CASE WHEN TG_OP IN ('UPDATE', 'DELETE') THEN to_jsonb(OLD) END,
        CASE WHEN TG_OP IN ('INSERT', 'UPDATE') THEN to_jsonb(NEW) END
    );
    RETURN NULL;
END;
$$;

DROP TRIGGER IF EXISTS authz_grant_event ON permissions;
CREATE TRIGGER authz_grant_event AFTER INSERT OR UPDATE OR DELETE ON permissions
    FOR EACH ROW EXECUTE FUNCTION authz_record_grant_event();

DROP TRIGGER IF EXISTS authz_grant_event ON role_permissions;
CREATE TRIGGER authz_grant_event AFTER INSERT OR UPDATE OR DELETE ON role_permissions
    FOR EACH ROW EXECUTE FUNCTION authz_record_grant_event();

-- The app role reads nothing here; writes go through the SECURITY DEFINER trigger.
REVOKE ALL ON authz_grant_events FROM PUBLIC;
