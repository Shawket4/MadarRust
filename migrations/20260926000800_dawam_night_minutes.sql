-- Dawam RU-8, RU-9: minutes of [a, b) that fall inside the night window
-- (e.g. 22:00–06:00) in the branch's time zone. Night overtime is the
-- overtime inside it, priced at the night rate; the rest is day overtime.
CREATE FUNCTION dawam_night_minutes(a timestamptz, b timestamptz, tz text, night_start time, night_end time)
RETURNS integer LANGUAGE sql STABLE AS $$
    SELECT COALESCE(SUM(GREATEST(0, EXTRACT(EPOCH FROM
               LEAST(b AT TIME ZONE tz, w.w_end) - GREATEST(a AT TIME ZONE tz, w.w_start)) / 60)), 0)::integer
      FROM generate_series((a AT TIME ZONE tz)::date - 1, (b AT TIME ZONE tz)::date, interval '1 day') AS d(day),
           LATERAL (SELECT d.day::date + night_start AS w_start,
                           d.day::date + night_end
                             + CASE WHEN night_end <= night_start THEN interval '1 day' ELSE interval '0' END AS w_end) AS w
     WHERE a IS NOT NULL AND b IS NOT NULL AND b > a
$$;
