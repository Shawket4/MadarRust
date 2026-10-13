-- A kitchen screen works with no person signed in (owner, 2026-10-11): the
-- dashboard's branch plan makes an activation code, the tablet enters it once,
-- and from then on its own device credential is its sign-in
-- (`POST /auth/device-session`). Nobody has an account for the kitchen.
--
-- The session still needs a subject: every kitchen read and bump is checked
-- through the kitchen role's capabilities at a branch, a bump records who did
-- it (`kitchen_ticket_items.bumped_by`) and a push token belongs to someone
-- (`push_devices.user_id`). So each kitchen screen gets ONE users row of its
-- own, made the first time it signs in: role `kitchen`, assigned to the
-- screen's branch, named after the screen, no email, password or PIN. It can
-- never sign in any other way, and it is no one's account.
--
-- `device_id` marks that row. A staff list leaves it out (it is not a
-- person); a bump shows its name ("Kitchen screen · Grill").
ALTER TABLE users ADD COLUMN device_id uuid REFERENCES devices(id) ON DELETE SET NULL;

-- One screen, one principal.
CREATE UNIQUE INDEX users_device_principal ON users (device_id) WHERE device_id IS NOT NULL;

-- Everyone who can sign in has a password or a PIN; a kitchen screen's
-- principal signs in only through its device's credential (kept on
-- `devices`), so it is excused like the guest principal.
ALTER TABLE users
    DROP CONSTRAINT chk_login_method,
    ADD CONSTRAINT chk_login_method
        CHECK (is_guest_principal OR device_id IS NOT NULL
               OR password_hash IS NOT NULL OR pin_hash IS NOT NULL);

COMMENT ON COLUMN users.device_id IS
    'Set on the principal of a kitchen screen (POST /auth/device-session): not a person, never in a staff list.';
