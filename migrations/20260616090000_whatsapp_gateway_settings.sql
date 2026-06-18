-- System-wide controls for the WhatsApp send gateway (super-admin only).
--
-- Single-row table (id is a boolean fixed to true). `paused` suppresses ALL
-- outgoing WhatsApp sends (delivery OTP + order-status messages) WITHOUT
-- unlinking the number — so an operator can mute the gateway for maintenance
-- and resume later without re-pairing the QR. The linked session lives in the
-- Go gateway's own SQLite store; only the mute switch is persisted here.
CREATE TABLE IF NOT EXISTS whatsapp_gateway_settings (
    id          boolean     PRIMARY KEY DEFAULT true,
    paused      boolean     NOT NULL DEFAULT false,
    paused_at   timestamptz,
    paused_by   uuid        REFERENCES users(id) ON DELETE SET NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT whatsapp_gateway_settings_singleton CHECK (id)
);

-- Seed the singleton row so reads never have to special-case its absence.
INSERT INTO whatsapp_gateway_settings (id) VALUES (true)
    ON CONFLICT (id) DO NOTHING;
