# Old tills and refusal 409s on `/sync/replay`

MadarRust `fix/old-till-409-safety`, 2026-10-18. Code: `src/sync/old_till.rs`.
Tests: `tests/replay_409_codes.rs`.

## The problem

Every POS build before 0.15.0 classifies a `/sync/replay` 409 as an ACK when
the op is marked `Idem::Yes` (and for a sale void, `Idem::VoidIdem`), then deletes the
outbox row. Since v1.14.0 the server marks the refusal 409s with a code. A
build before 0.15.0 never reads that code, so a refused refund, void, cash
correction, ticket void or booking move disappears from the till and nobody
is told. 0.15.0 (`fix/outbox-409-acks`) acks a 409 only for the codes in its
`conflict_acks` table.

## What the server does now

For an old till only, `POST /sync/replay` answers these refusal 409s with
**422**. The body stays the same (`error` text and `code`):

`ORDER_REFUNDED`, `ORDER_FULLY_REFUNDED`, `CASH_ALREADY_CORRECTED`,
`TICKET_SETTLED`, `BOOKING_NOT_ACTIVE`, `BOOKING_OTHER_BRANCH`,
`BOOKING_SEATED_ELSEWHERE`, `BOOKING_SEATED`.

These stay unchanged:

- The "already applied" 409s stay 409 for every caller, because acking them
  is correct: `REPLAY_ALREADY_APPLIED`, `LINE_ALREADY_VOIDED`, `TICKET_VOIDED`
  (0.15.0 `conflict_acks`).
- Uncoded 409s, such as a plain unique violation, which is a duplicate an old
  till is right to ack.
- The older floor codes (`TABLE_*`, `TRANSFER_*`, `TICKET_NOT_LIVE`).
- The answers to 0.15.0+ builds and to any caller the headers do not prove
  old.
- `/sync/replay/batch` and every live route.

## Telling an old till from a new one

Nothing in the replay body distinguishes the builds. 0.15.0 added a `protocol`
field on `/sync/pull` only. The headers do distinguish them:

| Build | `X-Madar-Client` | `User-Agent` |
|---|---|---|
| 0.1.1 – 0.6.x | not sent | `madar-core/0.1.0` |
| 0.7.0 – 0.14.1 | `pos/<semver> (<platform>)` | `madar-core/0.1.0` |
| 0.15.0+ | `pos/<semver> (<platform>)` | `madar-core/0.1.0` |
| kitchen screen | `pos/kitchen/0.1.0 (…)` (version unreadable) | |

The server treats a caller as old in two cases:

- `X-Madar-Client` is `pos/<semver>` and the version is below 0.15.0.
- `X-Madar-Client` is absent and `User-Agent` starts with `madar-core/`. Since
  0.7.0 the core always sends the header (falling back to `pos/0.7.0`), so this
  is only the pre-0.7 core.

Every other caller is answered as today. This includes the dashboard, the
kitchen screen, browsers, and requests with no headers.

If a 0.15.0+ build is ever taken for old (for example, a host that passed no
version, so it reports `pos/0.7.0`), nothing is lost. 0.15.0 dead-letters a
coded 409 refusal and a 422 the same way, and it words the stuck row by the
code first, whatever the status (`outbox_words::refusal`).

## Versions in the field

These are from `client_seen` on the prod-dump copy (`pg-rc1140`, DB
`madar_fresh`, last seen up to 2026-10-17):

| Version | Rows | Platforms |
|---|---|---|
| pre-0.7 (`madar-core/0.1.0`, no header) | 4 | — (legacy shift ops still replayed 2026-10-16) |
| 0.7.0 | 168 | macos, android, ios |
| 0.7.1, 0.7.2, 0.7.4, 0.7.5, 0.7.8, 0.7.9, 0.7.10, 0.7.11 | 1–4 each | mixed |
| 0.9.0 | 1 | ios |
| 0.12.0 | 14 | ios |
| 0.13.0 | 10 | android, ios |
| 0.13.7 | 1 | ios |
| 0.13.10 | 5 | android |
| kitchen 0.1.0 | 1 | ios |

No 0.15.0 build appears yet, so every till in the dump is an old till.

## How each build maps a replay error (from the madar git history)

There are two steps: `net::status_to_error` (HTTP status to `CoreError`), then
`classify_send` (`CoreError` and the op's `Idem` to `SendOutcome`). Every tag
from v0.1.1 to v0.15.0 was read. Identical functions are grouped by hash.

`status_to_error`, for a body that carries our `{error, code}` envelope:

| Status | v0.1.1 – v0.7.8 | v0.7.9 – v0.14.1 | Notes |
|---|---|---|---|
| 400, 422 | Validation | Validation | Exceptions: `PAYMENT_METHOD_UNAVAILABLE`, and from 0.9.0 the Dawam/staff code lists. The 8 refusal codes are in none of them, checked at v0.14.1. |
| 403 | Forbidden | Forbidden | From 0.13.7, a 403 without the envelope is Offline. From 0.14.x, plan refusals keep their code. |
| 409 | Server{409} | Server{409} | |
| 5xx | Transient | Transient | |

`classify_send`:

| Outcome | v0.1.1 – v0.6.1 | v0.7.0 – v0.7.8 | v0.7.9 – v0.7.10 | v0.7.11 – v0.14.1 | v0.15.0 |
|---|---|---|---|---|---|
| Validation (400/422) | Dead | Dead | Dead | Dead | Dead |
| Forbidden (403) | Dead | Dead | **Refused** | **Refused** | Refused |
| 409 + `Idem::Yes` | **Ack** | **Ack** | **Ack** | **Ack** | Ack only for a `conflict_acks` code, else Dead |
| 409 + `VoidIdem` | **Ack** | **Ack** | **Ack** | **Ack** | Dead |
| 409 + `Idem::No` | Dead | Dead | Dead | Dead | Dead |
| 404 + `Idem::Yes` | Ack | Ack | Ack | Dead | Dead |
| 429 | Dead | Throttled | Throttled | Throttled | Throttled |
| 5xx | Retry, Dead after budget | same | same | same | same |

What each outcome does in the drain loop, the same in every version:

- **Dead**: `mark_dead`. The row is kept in the outbox, shown in the stuck
  list with the server's text, and can be retried by hand. It is not counted
  as pending and is not retried automatically.
- **Refused** (0.7.9+): `mark_dead_refused`. The row is listed under "N actions
  need a manager" for a manager's PIN to re-send with an approval, or falls
  back to a plain Dead row.
- **Ack**: `mark_acked`. The row is deleted.

One exception for Dead: from 0.9.0, a Dead or Refused `set_ticket_customer`
is discarded on purpose, because the customer waits for the settle. None of
the 8 refusal codes is ever answered to that op.

Ops marked `Idem::Yes` in 0.7.x – 0.14.1, where a 409 is acked:
`close_till`, `refund_order`, `cash_movement`, `award_loyalty_points`,
`create_customer`, `attach_customer`, `set_ticket_customer`,
`record_staff_drink`, `spot_report_view`, fire (`open_ticket`), `void_ticket`,
`void_ticket_line`, bump/unbump, the table ops (hold, release, clear, transfer
create/cancel/fulfil), `seat_booking`, `no_show_booking`, `lan_mirror`.
`void_order` is `VoidIdem`. Pre-0.7 builds have the same list under the
legacy op names.

## Why 422

- Every build in the field maps 422 to Validation and then to Dead,
  whatever the op. The row is kept and shown as stuck. It is never acked and
  never retried in a loop. This is the one outcome that is the same from v0.1.1 to
  v0.15.0.
- 400 maps the same way, but on this route it means a malformed envelope.
  422 keeps the two apart.
- 403 is wrong for these refusals. It is Dead only up to 0.7.8. From 0.7.9 it
  is Refused, which files the op under "needs a manager", as if a permission
  were missing. A refusal from the server is not a permission problem.
- 404 is acked by builds before 0.7.11. Any 5xx retries until the budget runs out.

## Tests

`tests/replay_409_codes.rs` sends each refusal through `/sync/replay` as five
old-till callers (0.14.1, 0.13.10, 0.12.0, 0.7.0, and the pre-0.7 User-Agent)
and five of today's callers (0.15.0, 0.16.0, the kitchen screen, the
dashboard, and no header). The old tills get 422 with a body identical to the
409's. Today's callers get the 409 exactly as before. The three
"already applied" codes stay 409 for every caller. The live refund, ticket
void and booking no-show routes answer an old till exactly as before.
`BOOKING_OTHER_BRANCH` has no replay path, because a replayed fire drops a
booking it cannot use. A unit test in `src/sync/old_till.rs` covers it.
