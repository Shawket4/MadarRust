-- Payment method availability (decision 10, TILLS_CONTRACT.md §1.2).
--
-- Methods are defined per organisation (org_payment_methods). Availability
-- narrows by branch, by teller and by device. "No rows = no restriction";
-- rows present = an allow-list. Effective set at the till =
--   org active methods ∩ branch list (if any) ∩ user list (if any) ∩ device list (if any).

CREATE TABLE branch_payment_methods (
    branch_id         uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    payment_method_id uuid NOT NULL REFERENCES org_payment_methods(id) ON DELETE CASCADE,
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, payment_method_id)
);
CREATE TABLE user_payment_methods (
    user_id           uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    payment_method_id uuid NOT NULL REFERENCES org_payment_methods(id) ON DELETE CASCADE,
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, payment_method_id)
);
CREATE TABLE device_payment_methods (
    device_id         uuid NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    payment_method_id uuid NOT NULL REFERENCES org_payment_methods(id) ON DELETE CASCADE,
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (device_id, payment_method_id)
);

CREATE INDEX idx_branch_payment_methods_org ON branch_payment_methods (org_id);
CREATE INDEX idx_user_payment_methods_org   ON user_payment_methods (org_id);
CREATE INDEX idx_device_payment_methods_org ON device_payment_methods (org_id);

-- One organisation per row: the method, the owner and the row agree.
CREATE FUNCTION payment_availability_same_org() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    method_org uuid;
    owner_org  uuid;
BEGIN
    SELECT org_id INTO method_org FROM org_payment_methods WHERE id = NEW.payment_method_id;
    CASE TG_TABLE_NAME
        WHEN 'branch_payment_methods' THEN SELECT org_id INTO owner_org FROM branches WHERE id = NEW.branch_id;
        WHEN 'user_payment_methods'   THEN SELECT org_id INTO owner_org FROM users    WHERE id = NEW.user_id;
        WHEN 'device_payment_methods' THEN SELECT org_id INTO owner_org FROM devices  WHERE id = NEW.device_id;
    END CASE;
    IF method_org IS DISTINCT FROM NEW.org_id OR owner_org IS DISTINCT FROM NEW.org_id THEN
        RAISE EXCEPTION '%: payment method (org %) and owner (org %) must both belong to org %',
            TG_TABLE_NAME, method_org, owner_org, NEW.org_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER branch_payment_methods_same_org BEFORE INSERT OR UPDATE ON branch_payment_methods
    FOR EACH ROW EXECUTE FUNCTION payment_availability_same_org();
CREATE TRIGGER user_payment_methods_same_org BEFORE INSERT OR UPDATE ON user_payment_methods
    FOR EACH ROW EXECUTE FUNCTION payment_availability_same_org();
CREATE TRIGGER device_payment_methods_same_org BEFORE INSERT OR UPDATE ON device_payment_methods
    FOR EACH ROW EXECUTE FUNCTION payment_availability_same_org();

ALTER TABLE branch_payment_methods ENABLE ROW LEVEL SECURITY;
ALTER TABLE user_payment_methods   ENABLE ROW LEVEL SECURITY;
ALTER TABLE device_payment_methods ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON branch_payment_methods FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON user_payment_methods FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON device_payment_methods FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE branch_payment_methods, user_payment_methods, device_payment_methods TO madar_app;
