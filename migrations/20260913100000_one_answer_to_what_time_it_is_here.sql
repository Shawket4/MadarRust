-- The effective timezone of a branch: its own zone, else its org's, else Cairo.
-- One definition, so every query (and src/tz.rs) reads the same wall-clock.
-- NULL only for an unknown branch. Deleted branches keep their zone: their
-- orders still have to print in it.
CREATE OR REPLACE FUNCTION effective_timezone(p_branch_id uuid) RETURNS text
LANGUAGE sql STABLE AS $$
    SELECT COALESCE(b.timezone::text, o.timezone::text, 'Africa/Cairo')
      FROM branches b
      JOIN organizations o ON o.id = b.org_id
     WHERE b.id = p_branch_id
$$;
