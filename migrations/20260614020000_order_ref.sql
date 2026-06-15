-- V31 — Robust unique order reference (order_ref), stage 1 of 2.
--
-- Adds an additive, human-readable order reference of the form
--   <BRANCHCODE>-<YYMMDD>-<NNNN>   e.g.  DT-260614-0042
-- alongside (NOT replacing) the existing per-shift orders.order_number. It is
-- unique per (branch, business_date) by construction and org-globally unique
-- because the branch code is embedded, so a single global UNIQUE(order_ref)
-- backstops it (added in stage 2).
--
-- This migration leaves orders.order_ref NULLABLE: the historical backfill
-- (`cargo run --bin backfill-order-ref`) and the UNIQUE + NOT NULL finalize live
-- in stage 2 (20260614030000_order_ref_finalize.sql) so the heavy data rewrite of
-- a potentially large orders table never runs inside a migration transaction.
-- branches.code, by contrast, is fully finalized here — branches is tiny.

-- ── orders.order_ref (nullable for now) ──────────────────────────────────────
ALTER TABLE public.orders ADD COLUMN order_ref text;

-- ── per (branch, business_date) sequence source ──────────────────────────────
-- The PK row is the lock target: create_order does
--   INSERT ... ON CONFLICT (branch_id, business_date) DO UPDATE
--     SET last_seq = last_seq + 1 RETURNING last_seq
-- which serialises concurrent inserts for one branch+day and is transactional
-- (rolls back with the order on any error, so the common path is gap-free).
-- This — NOT the existing pg_advisory_xact_lock(hashtext(shift_id)) — is what
-- guards the sequence: a single shift can straddle two business days
-- (client-supplied created_at + AT TIME ZONE midnight), so the shift lock cannot.
CREATE TABLE public.order_ref_counters (
    branch_id     uuid NOT NULL REFERENCES public.branches(id),
    business_date date NOT NULL,
    last_seq      integer NOT NULL DEFAULT 0,
    PRIMARY KEY (branch_id, business_date)
);

-- ── branches.code: short A-Z0-9 org-unique prefix embedded in order_ref ───────
ALTER TABLE public.branches ADD COLUMN code text;

-- Derive a code from a branch's own name/id. Preferred = name upper-cased and
-- stripped to [A-Z0-9], first 6 chars. Arabic / symbol-only names strip to '' →
-- fall back to 'B' + first 5 hex of the id (always matches ^[A-Z0-9]{1,6}$ and is
-- effectively unique within an org). Pure function of the row; per-org collision
-- handling lives in the trigger / the one-shot UPDATE below, not here.
CREATE FUNCTION public.derive_branch_code(p_id uuid, p_name text)
RETURNS text
LANGUAGE sql IMMUTABLE
AS $$
    SELECT CASE
        WHEN nullif(upper(substring(regexp_replace(coalesce(p_name, ''), '[^A-Za-z0-9]', '', 'g') from 1 for 6)), '') IS NOT NULL
            THEN upper(substring(regexp_replace(p_name, '[^A-Za-z0-9]', '', 'g') from 1 for 6))
        ELSE 'B' || upper(substring(replace(p_id::text, '-', '') from 1 for 5))
    END
$$;

-- BEFORE INSERT trigger: fill code when the INSERT omits it (keeps every existing
-- `INSERT INTO branches (...)` — handlers and test fixtures alike — working
-- untouched). Falls back to the id-hex form if the preferred code is already taken
-- in the org, so the value is always org-unique.
CREATE FUNCTION public.set_branch_code() RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    candidate text;
BEGIN
    IF NEW.code IS NOT NULL THEN
        RETURN NEW;
    END IF;
    candidate := public.derive_branch_code(NEW.id, NEW.name);
    IF EXISTS (SELECT 1 FROM public.branches WHERE org_id = NEW.org_id AND code = candidate) THEN
        candidate := 'B' || upper(substring(replace(NEW.id::text, '-', '') from 1 for 5));
    END IF;
    NEW.code := candidate;
    RETURN NEW;
END;
$$;

CREATE TRIGGER branches_set_code BEFORE INSERT ON public.branches
    FOR EACH ROW EXECUTE FUNCTION public.set_branch_code();

-- Populate existing branches. The readable code goes to the first branch per
-- (org, derived-code); collisions (rn > 1) fall back to the unique id-hex form.
-- Set-based and deterministic (UPDATE does not fire the INSERT trigger).
WITH ranked AS (
    SELECT id,
           public.derive_branch_code(id, name) AS base,
           row_number() OVER (
               PARTITION BY org_id, public.derive_branch_code(id, name)
               ORDER BY id
           ) AS rn
    FROM public.branches
    WHERE code IS NULL
)
UPDATE public.branches b
SET code = CASE
    WHEN r.rn = 1 THEN r.base
    ELSE 'B' || upper(substring(replace(b.id::text, '-', '') from 1 for 5))
END
FROM ranked r
WHERE b.id = r.id;

-- Finalize branches.code (branches is small, so this is safe inline).
ALTER TABLE public.branches
    ADD CONSTRAINT branches_code_format CHECK (code ~ '^[A-Z0-9]{1,6}$');
CREATE UNIQUE INDEX branches_org_code_key ON public.branches (org_id, code);
ALTER TABLE public.branches ALTER COLUMN code SET NOT NULL;
