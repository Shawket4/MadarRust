#!/bin/zsh
# Every suite, sweeping sqlx's per-test databases between them.
# A single `cargo nextest run` over all 1663 tests fills the 12 GiB RAM disk
# (24 GiB machine, so it cannot grow) and whichever suite runs last fails on a
# full volume. Chunking keeps it at ~2 GiB.
export DATABASE_URL=postgres://shawket@localhost:5433/madar
# Tenant pools reap idle connections after 30s in production. An integration
# test binary links the library WITHOUT cfg(test), so without this it gets the
# production reaper and every test that makes a request waits ~5s for its
# throwaway database to become droppable. See src/db.rs.
export MADAR_FAST_TEST_POOLS=1
P=0; F=0; FAILED=()
for f in --lib tests/*.rs; do
  if [ "$f" = "--lib" ]; then m="--lib"; else m="--test $(basename $f .rs)"; fi
  [ "$m" = "--test client_seen" ] && continue
  out=$(cargo nextest run ${=m} 2>&1)
  line=$(echo $out | grep -E "^     Summary" | tail -1)
  p=$(echo $line | sed -nE 's/.* ([0-9]+) tests run: ([0-9]+) passed.*/\2/p')
  fl=$(echo $line | sed -nE 's/.*passed.*, ([0-9]+) failed.*/\1/p')
  [ -z "$p" ] && { FAILED+=("$m: NO SUMMARY"); }
  P=$((P + ${p:-0})); F=$((F + ${fl:-0}))
  [ -n "$fl" ] && FAILED+=("$m: $fl failed")
  psql -p 5433 -d postgres -Atc "select 'drop database \"'||datname||'\";' from pg_database where datname like '_sqlx_test%'" 2>/dev/null | psql -p 5433 -d postgres -q 2>/dev/null
done
echo "TOTAL passed=$P failed=$F"
for x in $FAILED; do echo "  !! $x"; done
df -h /Volumes/MadarTestRAM | tail -1
