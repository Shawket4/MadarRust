-- OTP codes are stored as they are sent, not as bcrypt hashes.
--
-- A one-time code is not a password, and treating it like one cost more than it
-- bought. bcrypt at the default cost measures 2.4 SECONDS on the production box
-- (1 vCPU), and the signup flow ran it twice — once to store the code, once to
-- check it — so joining a loyalty programme took about five seconds of pure key
-- derivation. Worse, neither call was moved off the async executor, and with
-- one Actix worker per CPU that meant the entire API stopped serving anyone for
-- the duration: tills syncing, the dashboard, other customers.
--
-- bcrypt exists to make a STOLEN hash expensive to crack, on the assumption the
-- secret behind it is long-lived and reused. Neither holds here. A code is six
-- digits, lives 300 seconds, is accepted at most five times, is rate-limited to
-- three requests per 30 s per IP, and is deleted on use. Its whole security
-- model is the clock, not the cost factor.
--
-- The trade being accepted, explicitly: anyone who can read this table sees the
-- live codes for the few minutes they are valid, where before they would have
-- had to crack them first. That is a real difference and a thin one — an
-- attacker with SELECT on the production database has better routes than
-- borrowing a five-minute code — and the owner decided it is not worth five
-- seconds on every signup.

ALTER TABLE delivery_otp RENAME COLUMN code_hash TO code;

-- Every row still live holds a bcrypt digest, which no longer matches anything
-- the verifier will compute. Left alone they would reject the code the customer
-- was legitimately sent, for up to five minutes after deploy, with "Incorrect
-- code." Clearing them instead asks those few people to tap "resend", which is
-- the failure they already understand.
DELETE FROM delivery_otp WHERE consumed_at IS NULL;

COMMENT ON COLUMN delivery_otp.code IS
  'The one-time code in plain text. Short-lived (300s), attempt-capped and '
  'rate-limited; see the migration that renamed this column from code_hash.';
