# legacy_till_api — golden responses of the PRE-rename backend

What POS v0.5.1 / v0.6.0 decode today from every shifts/tills-related endpoint,
captured from MadarRust `097a653` (branch tills-rework, before any rename).
Guards decision 12 (legacy `/shifts` adapters + replay aliases must keep old
tablets working).

## How it was captured
`scripts/legacy_till_golden/capture.sh [git-ref]`:
1. builds `madar-rust` from the ref in a temp git worktree;
2. `createdb -T madar_prodcopy madar_golden` (disposable; madar_prodcopy is never written);
3. boots it on 127.0.0.1:8090 with the run_local env (`DATABASE_URL=…/madar_golden`,
   all sweeps disabled, WhatsApp/Shlink pointed at a dead port, no Sentry, no auto-translation);
4. `capture.py` mints HS256 JWTs with the `.env` JWT_SECRET (same claims as
   `auth::jwt::create_token`: an org_admin + two real tellers holding open shifts),
   calls each endpoint, then MUTATES the copy via `/sync/replay` (cash_movement,
   create_order, refund_order, settle_open_ticket, close_shift, open_shift + dedup
   re-sends), delivery finalize and force-close;
5. stops the server and drops madar_golden.

Endpoints come from the tags' `madar-core` call sites (`shifts_api::{get_current_shift,
list_shifts, get_shift_report, list_cash_movements, force_close_shift}`, `tills_api::list_tills`,
`orders_api::{list_orders(shift_id), get_order}`, `refunds_api::{list_order_refunds,
list_shift_refunds}` (0.6.0), `open_tickets_api::{list,get}`, `delivery_api::{list,finalize}`,
`POST /sync/replay`) plus dashboard-side `/reports/shifts/{id}/{summary,deductions}` and `GET /shifts/{id}`.

## File format
Each file: `{ "request": {method, path, body}, "status": N, "body": <response> }`.
`manifest.json` lists every file with the old generated model it must decode into
(`model`) and which releases call it (`clients`); madar's
`tool/old_client_api_check.sh` parses each into those releases' `madar-api` models.

## Volatile / scrubbed fields (see `manifest.json`)
- Timestamps under keys `created_at, updated_at, opened_at, closed_at, issued_at,
  settled_at, voided_at, finalized_at, force_closed_at, generated_at, server_time,
  last_activity_at, bumped_at, fired_at, seated_at, ended_at, started_at` → `2026-01-01T00:00:00Z`
  (type preserved; nulls stay null).
- PII strings under `customer_name, customer_phone, phone, email, address, address_line,
  customer_address, notes, cash_note, note` → `"REDACTED"`.
- Ids minted during the capture (cash-movement client_ref, order idempotency key, new
  order id, new shift id) → `00000000-0000-0000-0000-00000000000N`. Server-minted
  refund/movement row ids and computed money figures still vary between captures.
Prod-copy ids of existing rows (branches, shifts, tellers, tickets) are stable.
