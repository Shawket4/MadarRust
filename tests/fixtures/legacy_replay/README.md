# legacy_replay — outbox envelopes of POS v0.5.1 and v0.6.0

The exact `POST /sync/replay` bodies those releases' `madar-core` drain
(`send_outbox_item`) produces, built with each release's OWN generated
`madar-api` models and the same field assignments its core makes
(null-vs-absent matters: e.g. `close_shift.request.cash_note: null`,
`void_order.request.note: null`). Fixed ids/clocks.

- `v0.5.1/` — tag `v0.5.1`: open_shift (±till/edit reason), close_shift (±note),
  cash_movement in/out (no `kind` — sign only), create_order cash/card(+discount)/split/tip/loyalty,
  settle_open_ticket cash / tip+loyalty (no splits in that model), void_order.
- `v0.6.0/` — commit `97c4a29` ("0.6.0"): the above plus cash_movement
  pay_in/pay_out/safe_drop/correction/no-kind, settle split, refund_order.
- `delivery_finalize.body.json` — not an outbox op: the live
  `POST /delivery-orders/{id}/finalize` body (`FinalizeInput{payment_method, shift_id}`).

Regenerate: `cd madar && tool/old_client_api_check.sh --regen-envelopes`
(source: `madar/tool/old_client_api_check/src/bin/gen_envelopes.rs`).
Guarded by `tests/legacy_replay_fixtures_test.rs` — every envelope must parse into the
CURRENT `ReplayOp` (right variant) forever; the aliases are permanent.
