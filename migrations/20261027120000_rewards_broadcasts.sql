-- Rewards dashboard: messages to members' cards (REWARDS_DASHBOARD_DESIGN.md §4).
--
-- A broadcast is many `wallet::notices` announcements: the line rides on the
-- card (Apple: a field with changeMessage; Google: a message), WhatsApp only
-- when the sender asked for it. The audience is madar-card's `Audience`,
-- resolved to recipients when sending starts (a snapshot). Opted-out members
-- are never recipients. `pass_notice_broadcast_id` on the membership remembers
-- which broadcast a card's notice came from, so the web service's existing
-- "the device fetched the pass" signal marks that recipient seen.

CREATE TABLE IF NOT EXISTS loyalty_broadcasts (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    line_en           text NOT NULL DEFAULT '' CHECK (char_length(line_en) <= 100),
    line_ar           text NOT NULL DEFAULT '' CHECK (char_length(line_ar) <= 100),
    audience          jsonb NOT NULL DEFAULT '{"rules": []}',
    whatsapp_fallback boolean NOT NULL DEFAULT false,
    status            text NOT NULL DEFAULT 'scheduled'
                      CHECK (status IN ('scheduled', 'sending', 'sent', 'cancelled')),
    scheduled_for     timestamptz NOT NULL DEFAULT now(),
    created_by        uuid NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    started_at        timestamptz NULL,
    finished_at       timestamptz NULL,
    cancelled_at      timestamptz NULL,
    CONSTRAINT loyalty_broadcasts_says_something CHECK (line_en <> '' OR line_ar <> '')
);
CREATE INDEX IF NOT EXISTS loyalty_broadcasts_org_created ON loyalty_broadcasts (org_id, created_at DESC);
CREATE INDEX IF NOT EXISTS loyalty_broadcasts_due ON loyalty_broadcasts (scheduled_for)
    WHERE status IN ('scheduled', 'sending');
ALTER TABLE loyalty_broadcasts ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON loyalty_broadcasts;
CREATE POLICY tenant_isolation ON loyalty_broadcasts FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE, DELETE ON loyalty_broadcasts TO madar_app;

CREATE TABLE IF NOT EXISTS loyalty_broadcast_recipients (
    broadcast_id uuid NOT NULL REFERENCES loyalty_broadcasts(id) ON DELETE CASCADE,
    org_id       uuid NOT NULL,
    customer_id  uuid NOT NULL REFERENCES loyalty_memberships(id) ON DELETE CASCADE,
    -- pending until processed; then what reached them.
    outcome      text NOT NULL DEFAULT 'pending'
                 CHECK (outcome IN ('pending', 'sent', 'skipped_recent', 'skipped_opted_out', 'failed')),
    -- 'apple', 'google', 'apple,google', 'whatsapp', or 'none' (no card, no WhatsApp asked).
    channel      text NULL,
    seen_at      timestamptz NULL,
    processed_at timestamptz NULL,
    PRIMARY KEY (broadcast_id, customer_id)
);
CREATE INDEX IF NOT EXISTS loyalty_broadcast_recipients_member
    ON loyalty_broadcast_recipients (customer_id, processed_at DESC);
ALTER TABLE loyalty_broadcast_recipients ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON loyalty_broadcast_recipients;
CREATE POLICY tenant_isolation ON loyalty_broadcast_recipients FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE, DELETE ON loyalty_broadcast_recipients TO madar_app;

ALTER TABLE loyalty_memberships
    ADD COLUMN IF NOT EXISTS pass_notice_broadcast_id uuid NULL;
