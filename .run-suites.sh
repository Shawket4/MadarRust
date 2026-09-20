#!/bin/zsh
# Run each integration suite, sweeping sqlx's per-test databases in between so
# the 12 GiB RAM cluster cannot fill mid-run (a full volume kills the server and
# every later suite fails with PoolTimedOut, which looks like a code failure).
export DATABASE_URL=postgres://shawket@localhost:5433/madar
for m in "$@"; do
  printf "%-12s " $m
  out=$(cargo nextest run --test $m 2>&1)
  echo ${out} | grep -E "^     Summary" | tail -1 || { echo "NO SUMMARY"; echo ${out} | tail -5; }
  psql -p 5433 -d postgres -Atc "select 'drop database \"'||datname||'\";' from pg_database where datname like '_sqlx_test%'" 2>/dev/null | psql -p 5433 -d postgres -q 2>/dev/null
done
df -h /Volumes/MadarTestRAM | tail -1
