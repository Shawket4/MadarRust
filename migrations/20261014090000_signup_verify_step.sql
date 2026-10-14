-- Signup proves the phone at the CODE step, not at the end (owner, 2026-10-14):
-- `POST /public/signup/verify` checks the code on the server and hands back a
-- signup token; `POST /public/signup` takes that token. The token itself is
-- never stored, only its SHA-256, on the code's row.
ALTER TABLE delivery_otp
    ADD COLUMN verified_at timestamptz NULL,
    ADD COLUMN proof_hash  text        NULL;

CREATE UNIQUE INDEX delivery_otp_proof_hash ON delivery_otp (proof_hash) WHERE proof_hash IS NOT NULL;
