# Madar Analytics API — Integration Handoff

Read-only order analytics for a single branch. Answers items 1–7 from your last message.

> **Live from 4 August 2026.** Credentials issued before then will not authenticate.

---

## 1. Endpoint

```
GET https://api.madar-pos.cloud/integrations/analytics/orders
```

## 2. Date range format

Two query parameters, both **plain calendar dates** (`YYYY-MM-DD`), both **inclusive**:

| Param | Required | Notes |
|---|---|---|
| `from` | yes | First business day to include. |
| `to` | yes | Last business day to include. |
| `limit` | no | Page size, max 5000. Omit to get the whole window in one response. |
| `offset` | no | Row offset, used with `limit`. Defaults to 0. |

**There is no branch parameter.** The branch is determined by the credential you
authenticate with, so there is nothing for you to pass and no id to keep track of. The
branch it resolved to is returned in the response as `branch_id` / `branch_name`.

Dates are resolved in **the branch's own timezone** (Africa/Cairo), not UTC. You send
`from=2026-06-01&to=2026-06-30` and get all of June as the branch experienced it — we
handle the UTC offset and Egypt's daylight-saving switch on our side, so there is nothing
for you to compute. The response echoes the exact instant window (`from_utc` / `to_utc`)
that was applied, so the coverage is never ambiguous.

Example:

```
GET /integrations/analytics/orders?from=2026-06-01&to=2026-06-30
```

## 3. Authentication — HTTP Basic

```
Authorization: Basic base64(username:password)
```

Over HTTPS only. The credential is read-only, scoped to exactly one branch, and can be
rotated or revoked by the merchant at any time without affecting anything else.

## 4. Username and password

Sent to you separately, never over the same channel as this document.

## 5. Response format — JSON

All monetary values are **integers in piastres** (1 EGP = 100 piastres).

```json
{
  "branch_id": "00000000-0000-0000-0000-000000000000",
  "branch_name": "One Ninety",
  "timezone": "Africa/Cairo",
  "from": "2026-06-01",
  "to": "2026-06-30",
  "from_utc": "2026-05-31T21:00:00Z",
  "to_utc": "2026-06-30T21:00:00Z",
  "total_orders": 0,
  "subtotal": 0,
  "total_discount": 0,
  "total_tax": 0,
  "total_service_charge": 0,
  "total_revenue": 0,
  "avg_order_total": 0,
  "limit": null,
  "offset": 0,
  "returned": 0,
  "orders": [
    {
      "order_id": "00000000-0000-0000-0000-000000000000",
      "order_number": 0,
      "order_ref": "ONE190-260601-0001",
      "status": "completed",
      "business_date": "2026-06-01",
      "created_at": "2026-06-01T12:00:00Z",
      "subtotal": 0,
      "discount_amount": 0,
      "tax_amount": 0,
      "service_charge": 0,
      "total_amount": 0
    }
  ]
}
```

### Field semantics — please read before you build against this

**`total_amount` is `subtotal - discount_amount + tax_amount`.** That identity holds on
every row, so your side can reconcile arithmetically.

**`service_charge` is always `0`.**

**`business_date`** is the order's calendar day in the branch's timezone. It is derived
the same way as the `YYMMDD` segment inside `order_ref`, so the two always agree — useful
when reconciling a specific receipt.

**`order_ref`** is the human-readable reference printed on the customer's receipt
(`<BRANCHCODE>-<YYMMDD>-<NNNN>`), unique across the whole organization. It is the best key
for looking up a disputed order. It may be `null` on a small number of legacy orders that
predate the scheme.

**`avg_order_total`** is `total_revenue / total_orders`, truncated to whole piastres, and
`0` when the window is empty.

### Pagination

Pagination is **optional today**: omit `limit` and you get the entire window in one
response. `total_orders` always reflects the whole window regardless of paging, so you can
page with `limit` + `offset` and still trust the aggregates on every page. Row order is
stable (`created_at`, then `order_id`).

> **Please build your client to handle paging from day one.** As order volume grows we may
> need to make `limit` mandatory with a default page size. If your client already reads
> `returned` / `total_orders` and follows `offset`, that change will be transparent to
> you. If it assumes one response contains everything, it will silently truncate.

## 6. Errors

Standard HTTP status codes with a JSON body of the form `{"error": "..."}`.

| Status | Meaning |
|---|---|
| 400 | Bad parameters (e.g. `from` after `to`, `limit` < 1) |
| 401 | Missing, malformed, wrong, or revoked credentials |
| 429 | Rate limited — 30 requests/minute per IP |

## 7. Branch scope

**LMD / One-Ninety is a single branch**, so this is one credential covering it. The branch
comes from the credential itself, which is why there is no branch parameter on the request.

---

## One thing to confirm on your side

**Your item 6 was missing** from the requirements list you sent — it runs 1, 2, 3, 4, 5, 7
and skips 6. Let us know what it was so we can cover it.
