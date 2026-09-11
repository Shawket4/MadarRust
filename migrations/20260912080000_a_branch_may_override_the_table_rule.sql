-- A branch may override the table rule, the same way it overrides the tax.
--
-- `require_table_for_orders` lives on the organisation only. That was the
-- right first cut — the rule costs nothing at a branch with no tables, so a
-- single org-wide switch seemed to cover every shop. It does not cover the
-- shop that runs a dining room in one place and a counter in another WHERE
-- THE COUNTER ALSO HAS A FEW TABLES: two stools by the window, a bench
-- outside. Switch the rule on for the dining room and the counter refuses
-- every walk-up sale until somebody seats it on a stool. Leave it off and
-- the dining room is back to sales the floor never heard about. The org
-- flag can only say one thing to both branches, and one of them is wrong.
--
-- The shape is the one the tax policy already uses on `branches`, and it
-- matters that it is identical: NULLABLE, where NULL means INHERIT. Not a
-- defaulted `false`. A defaulted false is the org flag silently switched off
-- at every branch — the org says "seat everyone", every branch row says
-- "no", and which one wins is a question the schema has stopped being able
-- to answer. With NULL, an org-wide change still reaches every branch that
-- never asked to differ; a branch that genuinely wants the other setting says
-- so explicitly, in either direction; and clearing the override puts the
-- branch back under the org rather than pinning it to whatever was true the
-- day it was cleared.
--
-- Resolution is branch-first then org — `COALESCE(b.x, o.x)` — exactly as
-- `tax::policy::for_branch` does for the four tax fields. The two paths that
-- do not check the rule at all (settling a ticket, replaying a queued offline
-- order) are unchanged; only where the flag is READ changes, not what it
-- guards.
--
-- No backfill. NULL on every existing branch IS the correct value: every
-- branch today inherits, because until now there was nothing else it could do.
ALTER TABLE branches
    ADD COLUMN require_table_for_orders boolean;

COMMENT ON COLUMN branches.require_table_for_orders IS
    'Overrides the org rule. NULL = inherit, which is not the same as false. '
    'An explicit false at one branch lets a counter with a couple of tables '
    'keep ringing walk-ups while the org''s dining rooms seat everyone.';
