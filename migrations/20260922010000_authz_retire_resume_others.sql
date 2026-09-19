-- Retire capability 222, `orders.held.resume_others`.
--
-- Owner decision 2026-09-19, overruling the gate added days earlier by
-- migration 20260921050100: resuming a held order on a till NEVER requires
-- approval, from anyone, for anyone's order — including a manager's. A held
-- order is shared state on the till, exactly like a floor table. Nothing in the
-- POS or the server checks 222 any more (`madar-core/src/queue.rs` rule 4;
-- `src/sync/handlers.rs` no longer flags a replayed sale whose `started_by`
-- differs from the ringer), so the row is removed rather than left as a toggle
-- the owner can switch with no effect.
--
-- THE ID 222 IS RESERVED FOREVER and must never be reused. Ids are stable; gaps
-- are fine, and no other capability shifts. `orders.started_by` is untouched:
-- the sale still records who started the order, which is how a handover is
-- traced in the dashboard.
--
-- Everything referencing capabilities(id) must go first (the FKs have no
-- ON DELETE CASCADE): role grants, per-person overrides and the ask-a-manager
-- policy. All three are idempotent, and a database that never ran
-- 20260921050100's grants is unaffected.
DELETE FROM org_role_grants WHERE capability_id = 222;
DELETE FROM user_overrides WHERE capability_id = 222;
DELETE FROM org_capability_policy WHERE capability_id = 222;
DELETE FROM capabilities WHERE id = 222;

-- Every tablet's cached grant set must forget it on the next sync.
SELECT authz_bump_epoch(o.id) FROM organizations o;
