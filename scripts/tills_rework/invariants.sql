-- Standalone post-migration check (TILLS_CONTRACT.md §1.3 I1–I9 + schema lint +
-- changefeed live sets). Runnable any time after the migration; prints
-- 'PASS|<check>' lines and RAISEs on the first failure. Row/money equality
-- against the PRE schema is done by scripts/till_migration/invariants_{before,after}.sql
-- (byte-identical diff) in rehearse.sh.
\set ON_ERROR_STOP 1
\pset format unaligned
\pset tuples_only on

DO $$
DECLARE bad bigint; txt text;
BEGIN
  -- I1/I3 per till vs the in-migration snapshot (exact on a quiet database)
  SELECT count(*), min(x.shift_id::text) INTO bad, txt
    FROM archive.tills_rework_snapshot x LEFT JOIN tills t ON t.id = x.shift_id
   WHERE t.id IS NULL
      OR (SELECT count(*) FROM orders o WHERE o.till_id = x.shift_id) <> x.orders_n
      OR (SELECT coalesce(sum(o.total_amount),0) FROM orders o WHERE o.till_id = x.shift_id) <> x.orders_total
      OR (SELECT count(*) FROM order_payments p WHERE p.till_id = x.shift_id) <> x.payments_n
      OR (SELECT coalesce(sum(p.amount),0) FROM order_payments p WHERE p.till_id = x.shift_id) <> x.payments_total
      OR (SELECT coalesce(sum(m.amount),0) FROM till_cash_movements m WHERE m.till_id = x.shift_id) <> x.cash_moves_total
      OR (SELECT coalesce(sum(r.amount),0) FROM order_refunds r WHERE r.till_id = x.shift_id) <> x.refunds_total
      OR (SELECT count(*) FROM open_tickets k WHERE k.settled_till_id = x.shift_id) <> x.tickets_settled_n;
  IF bad <> 0 THEN RAISE EXCEPTION 'I3 FAIL: % till(s) differ from snapshot (first %)', bad, txt; END IF;
  RAISE NOTICE 'PASS|I1-I3 per-till counts and sums match archive.tills_rework_snapshot';

  SELECT count(*) INTO bad FROM order_payments p JOIN orders o ON o.id = p.order_id WHERE p.till_id IS DISTINCT FROM o.till_id;
  IF bad <> 0 THEN RAISE EXCEPTION 'I4 FAIL: % payment legs', bad; END IF;
  RAISE NOTICE 'PASS|I4 order_payments.till_id = orders.till_id';

  IF (SELECT count(*) FROM archive.shift_till_bindings) > (SELECT count(*) FROM tills) THEN RAISE EXCEPTION 'I5 FAIL'; END IF;
  RAISE NOTICE 'PASS|I5 archives present';

  SELECT count(*) INTO bad FROM table_occupancies o
   WHERE (o.started_till_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM tills t WHERE t.id = o.started_till_id AND t.branch_id = o.branch_id))
      OR (o.ended_till_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM tills t WHERE t.id = o.ended_till_id AND t.branch_id = o.branch_id));
  IF bad <> 0 THEN RAISE EXCEPTION 'I6 FAIL: % occupancy refs', bad; END IF;
  RAISE NOTICE 'PASS|I6 occupancy refs are same-branch tills';

  SELECT count(*), string_agg(p.proname, ',') INTO bad, txt FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
   WHERE n.nspname = 'public' AND regexp_replace(p.prosrc, 'work_shift', '', 'g') ~ '(\mshifts\M|shift_id)';
  IF bad <> 0 THEN RAISE EXCEPTION 'I7 FAIL: %', txt; END IF;
  RAISE NOTICE 'PASS|I7 no function body references shifts';

  IF NOT EXISTS (SELECT 1 FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid WHERE t.typname = 'permission_resource' AND e.enumlabel = 'tills')
     OR EXISTS (SELECT 1 FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid WHERE t.typname = 'permission_resource' AND e.enumlabel = 'shifts') THEN
    RAISE EXCEPTION 'I8 FAIL'; END IF;
  RAISE NOTICE 'PASS|I8 permission_resource tills';

  -- Schema lint: no 'shift' identifier left except staff scheduling and the dead enum value.
  SELECT count(*), string_agg(k || ':' || name, ', ') INTO bad, txt FROM (
      SELECT 'rel' k, relname::text name FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE nspname = 'public' AND relname ~ 'shift'
      UNION ALL SELECT 'col', c.relname || '.' || a.attname FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE nspname = 'public' AND a.attnum > 0 AND NOT a.attisdropped AND a.attname ~ 'shift'
      UNION ALL SELECT 'con', conname::text FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace WHERE nspname = 'public' AND conname ~ 'shift'
      UNION ALL SELECT 'trg', tgname::text FROM pg_trigger WHERE tgname ~ 'shift'
      UNION ALL SELECT 'fn', proname::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE nspname = 'public' AND proname ~ 'shift'
      UNION ALL SELECT 'type', typname::text FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE nspname = 'public' AND typname ~ 'shift'
      UNION ALL SELECT 'enum', enumlabel::text FROM pg_enum WHERE enumlabel ~ 'shift') x
   WHERE name !~ 'work_shift' AND name !~ '^staff_schedules' AND name <> 'shift_counts';
  IF bad <> 0 THEN RAISE EXCEPTION 'LINT FAIL: %', txt; END IF;
  RAISE NOTICE 'PASS|schema lint: no stray shift identifiers';

  IF to_regproc('sync_live_rows') IS NOT NULL THEN
    SELECT count(*) INTO bad FROM (
      (SELECT branch_id, type, entity_id FROM sync_changes WHERE op = 'upsert' AND type NOT IN ('kitchen_ticket','delivery','booking')
       EXCEPT SELECT branch_id, type, entity_id FROM sync_live_rows())
      UNION ALL
      (SELECT branch_id, type, entity_id FROM sync_live_rows()
       EXCEPT SELECT branch_id, type, entity_id FROM sync_changes WHERE op = 'upsert')) d;
    IF bad <> 0 THEN RAISE EXCEPTION 'CHANGEFEED FAIL: % rows differ from live sets', bad; END IF;
    RAISE NOTICE 'PASS|changefeed upsert rows = live sets (time-based types: no missing rows)';
  END IF;
END $$;
SELECT 'I9', 'open_tills', count(*) FROM tills WHERE status = 'open';
SELECT 'feed', type, count(*) FROM sync_changes GROUP BY type ORDER BY type;
