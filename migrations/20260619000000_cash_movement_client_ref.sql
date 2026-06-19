-- Idempotency + temp-id reconciliation for OFFLINE cash movements (POS rebuild,
-- backend offline-first P0). The POS mints a stable `client_ref` (UUID) per
-- movement so a replayed offline movement dedupes instead of double-applying —
-- a double-apply would corrupt the shift's expected-cash reconciliation.
--
-- Additive + nullable + partial unique index → the live Flutter client (which
-- omits client_ref, leaving it NULL) is unaffected; many NULLs are allowed.
ALTER TABLE public.shift_cash_movements
    ADD COLUMN IF NOT EXISTS client_ref uuid;

CREATE UNIQUE INDEX IF NOT EXISTS uq_shift_cash_movements_client_ref
    ON public.shift_cash_movements (client_ref)
    WHERE client_ref IS NOT NULL;
