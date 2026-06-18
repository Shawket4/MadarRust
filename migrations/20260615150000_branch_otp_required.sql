-- Per-branch toggle for public-checkout OTP phone verification.
-- Default true = existing branches keep mandatory OTP (no behavior change).
ALTER TABLE branch_delivery_settings
    ADD COLUMN IF NOT EXISTS otp_required boolean NOT NULL DEFAULT true;
