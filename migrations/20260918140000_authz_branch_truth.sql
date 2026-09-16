-- Branch allow-lists: one source of truth (POS_SIGNIN_OVERHAUL.md §5, "A + B").
--
-- The bug this fixes: the mirror set every non-manager's assignment to
-- `all_branches = true` unconditionally, so a person's explicit
-- `user_branch_assignments` rows — the rows the dashboard's per-branch toggles
-- write — were copied into `role_assignment_branches` and then ignored, because
-- an `all_branches` assignment covers every branch anyway. Flicking a toggle
-- changed a table nothing read. Architecture E role assignments are now the one
-- answer to "which branches does this person work at", and the legacy table
-- feeds them (and is projected back to, for pre-0.8 readers).
--
-- The migration rule (POS_SIGNIN_OVERHAUL.md §5.3): a person with NO explicit
-- branches keeps working EVERYWHERE in their org. In the prod copy 10 PIN
-- holders (6 tellers, 3 waiters, 1 kitchen) have no allow-list row at all;
-- deriving "allowed nowhere" from "listed nowhere" would lock every one of them
-- out overnight. So an empty allow-list still means all_branches, and only a
-- person who HAS explicit rows is narrowed to them — which is what the owner
-- flicking the toggle asked for in the first place.
--
-- Branch managers are untouched: they were already narrowed to their explicit
-- rows, and widening a manager with no rows to the whole org would be a real
-- escalation, not a rescue.

CREATE OR REPLACE FUNCTION authz_sync_user(p_user uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    u        users%ROWTYPE;
    v_role   uuid;
    v_assign uuid;
    v_all    boolean;
BEGIN
    SELECT * INTO u FROM users WHERE id = p_user;
    IF NOT FOUND OR u.org_id IS NULL OR u.role = 'super_admin' OR u.is_guest_principal
       OR u.deleted_at IS NOT NULL THEN
        UPDATE role_assignments SET revoked_at = now()
         WHERE user_id = p_user AND revoked_at IS NULL;
        RETURN;
    END IF;
    -- Access set by hand in the access editor wins; users.role is then only the
    -- label older tablets read.
    IF EXISTS (SELECT 1 FROM role_assignments
                WHERE user_id = p_user AND revoked_at IS NULL AND NOT managed) THEN
        UPDATE role_assignments SET revoked_at = now()
         WHERE user_id = p_user AND revoked_at IS NULL AND managed;
        RETURN;
    END IF;
    PERFORM authz_ensure_org_roles(u.org_id);
    SELECT id INTO v_role FROM org_roles
     WHERE org_id = u.org_id AND key = u.role::text AND is_system AND deleted_at IS NULL;

    -- Org-wide only while nobody has said otherwise.
    v_all := u.role <> 'branch_manager'
             AND NOT EXISTS (SELECT 1
                               FROM user_branch_assignments a
                               JOIN branches b ON b.id = a.branch_id
                                              AND b.org_id = u.org_id
                                              AND b.deleted_at IS NULL
                              WHERE a.user_id = p_user);

    UPDATE role_assignments SET revoked_at = now()
     WHERE user_id = p_user AND revoked_at IS NULL AND managed AND org_role_id <> v_role;

    SELECT id INTO v_assign FROM role_assignments
     WHERE user_id = p_user AND org_role_id = v_role AND revoked_at IS NULL;
    IF v_assign IS NULL THEN
        INSERT INTO role_assignments (org_id, user_id, org_role_id, all_branches, reason, managed)
        VALUES (u.org_id, p_user, v_role, v_all, 'follows the account role', true)
        RETURNING id INTO v_assign;
    ELSE
        UPDATE role_assignments SET all_branches = v_all
         WHERE id = v_assign AND all_branches IS DISTINCT FROM v_all;
    END IF;

    DELETE FROM role_assignment_branches rab
     WHERE rab.assignment_id = v_assign
       AND NOT EXISTS (SELECT 1 FROM user_branch_assignments a
                        WHERE a.user_id = p_user AND a.branch_id = rab.branch_id);
    INSERT INTO role_assignment_branches (assignment_id, branch_id, org_id)
    SELECT v_assign, a.branch_id, u.org_id
      FROM user_branch_assignments a JOIN branches b ON b.id = a.branch_id AND b.org_id = u.org_id
     WHERE a.user_id = p_user
    ON CONFLICT DO NOTHING;
END $$;

-- Re-run the mirror for every person it still owns, so today's rows match the
-- rule above. Hand-made (`managed = false`) access is left exactly as an owner
-- set it.
DO $$
DECLARE r record;
BEGIN
    FOR r IN
        SELECT DISTINCT u.id
          FROM users u
          JOIN role_assignments ra ON ra.user_id = u.id AND ra.revoked_at IS NULL AND ra.managed
         WHERE u.deleted_at IS NULL
    LOOP
        PERFORM authz_sync_user(r.id);
    END LOOP;
END $$;

-- Every org's cached effective sets must be recomputed.
SELECT authz_bump_epoch(id) FROM organizations;
