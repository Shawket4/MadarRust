# legacy_till_api — golden responses of the PRE-rename backend

What POS v0.5.1 / v0.6.0 decode from every shifts/tills-related endpoint,
captured from MadarRust `097a653` (the last commit before the tills rename).
Guards decision 12 (legacy `/shifts` adapters + replay aliases must keep old
tablets working), byte-for-byte: `src/tills/legacy_tests.rs` replays the same
scenario against the current backend and compares every value.

## How it is captured (deterministic)
`scripts/legacy_till_golden/capture.sh [git-ref]` (default `097a653`):
1. creates an EMPTY database on the throwaway test cluster (:5433) and builds the
   ref in a temp worktree (pass `TARGET=<dir>` to reuse a cargo target dir);
2. boots it (it migrates the empty DB and seeds role permissions);
3. loads `scripts/legacy_till_golden/seed.sql` (fixed ids: one org, branches
   A/B/C, admin, two tellers, a waiter, one menu item, cash/card methods);
4. `capture.py` runs `scripts/legacy_till_golden/scenario.json` — the setup
   requests (shifts, orders, tickets, a delivery order) and then every saved
   request, including the `/sync/replay` ops and their dedup re-sends;
5. stops the server and drops the database.

## File format
Each file: `{ "request": {method, path, body}, "status": N, "body": <response> }`.
`manifest.json` lists every file with the old generated model it must decode into
(`model`) and which releases call it (`clients`); madar's
`tool/old_client_api_check.sh` parses each into those releases' `madar-api` models.

## What is NOT compared (explicit lists in `manifest.json`)
- `volatile_timestamp_keys`: wall-clock timestamps, scrubbed to
  `2026-01-01T00:00:00Z` (nulls stay null).
- `dated_ref_keys`: the `-YYMMDD-` business-date segment of human refs.
- Server-minted UUIDs (order, ticket, refund, movement, delivery ids) differ per
  run; the test requires a consistent one-to-one mapping between the golden's
  and the actual ids, and every seed/client-supplied id must match exactly.
