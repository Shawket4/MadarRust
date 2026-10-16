# Customers on the till: a scoped feed (design, not shipped)

DB audit 2026-10-16, Phase 6, last bullet. **Status: design only. Nothing here is
implemented.** It needs the owner's product and privacy call (audit §12, decision 8)
and a POS build.

## The problem

`customers` reaches tills through the changefeed as an ORG fan-out
(`sync_emit_customers` -> `sync_emit_org`): every live customer of the org is
written to every branch's feed, and every till holds the whole list.

- **Size.** One feed row per customer per branch, and a full snapshot ships the
  whole list. A loyalty programme with QR sign-up grows the list without bound;
  a 10-branch org with 40,000 members holds 400,000 feed rows for customers alone.
- **Privacy.** Every till in every branch carries every customer's name and
  phone, offline, for as long as the device lives. A lost tablet is the whole
  customer list.

The till uses the list for: search by name/phone at checkout
(`madar-core customers.rs::search_customers`), resolving a scanned loyalty card
(`customer_for_member`), showing the customer on an order or ticket, and
creating a customer offline (`create_customer`, replayed; the server merges on
`phone_key`).

## Proposal: protocol v3, opt-in per till

Old tills (protocol v1, v2) keep receiving the whole org, exactly as today.
Nothing about their feed, checksums or snapshot changes.

A till that sends `protocol >= 3` gets a **scoped** customer set:

1. **Recent at this branch**: a customer with an order, a ticket, a loyalty
   transaction or a booking at this branch within `customer_window_days`
   (default 90, an org setting).
2. **Created at this branch** within the same window.
3. **Pinned by this branch**: anyone a device of the branch looked up online
   in the window (below).

Everyone else is reachable **online on demand**:

- `GET /customers/lookup?phone=<full number>`: exact match on the canonical
  `phone_key`, one result, with the loyalty summary. Rate limited per device
  (a new row in `rate_limit::ROUTE_LIMITS`). No partial-phone or name search
  over the whole org from a till: that is what makes the list scrapable.
- `GET /customers/lookup?member=<id>`: the scanned card.
- A hit pins the customer to the branch (`customer_branch_pins(branch_id,
  customer_id, pinned_at)`), so it rides that branch's scoped feed from then on
  and the till has it offline next time.

Offline, a till searches what it holds. A customer it does not hold is created
new (as today); the server's `phone_key` merge folds the duplicate, and the
order keeps its soft reference (audit §12 decision 2 stays "soft").

## Server mechanics

- **Relevance table** `customer_branch_relevance(org_id, branch_id,
  customer_id, last_at)`, maintained by AFTER triggers on `orders`,
  `open_tickets`, `loyalty_transactions`, `bookings` (customer set) and by the
  pin. A nightly job drops rows older than the window and emits `delete` for
  the scope exit (v3 only, see below).
- **The feed rows stay org fan-out.** v1/v2 tills on the same branch still need
  every customer, so `sync_changes` cannot be split per protocol. The scope is
  applied when a v3 till PULLS:
  - incremental: a `customer` change outside the branch's relevant set is sent
    as `delete` (or omitted when the device never held it, which the server
    cannot know, so `delete` is the safe form);
  - full: only the relevant set;
  - checksum: the `customer` checksum is computed over the scoped set for v3
    (a per-protocol checksum; the compat layer owns the v1/v2 one).
- **Scope entries** (a customer becoming relevant without a customer write)
  re-emit that customer on the branch feed from the relevance trigger, so the
  till hears of it on its next incremental pull.

## What the POS needs (madar repo, not done)

- Send `protocol: 3` on `/sync/pull`; treat a `customer` `delete` as "no longer
  held here", not "erased".
- Online lookup by full phone / scanned card when local search misses; cache
  the result.
- The customer search box says "search this branch's customers" offline and
  offers the online lookup when connected.
- Wipe customers on sign-out of the device's branch binding (a re-bound device
  must not keep the old branch's list).

## Owner decisions

1. Scope window (90 days?) and whether it is per org.
2. Whether a till may search by partial phone or name online (recommended: no;
   exact phone and card only).
3. Whether existing tills should be pushed to v3 by the version gate later (the
   gate stays inert today; audit §7.2).
4. Whether the dashboard should show "customers held on devices" for audit.

## Rollout

Server v3 support is additive and can ship before any POS build uses it. Measure
per org: customers per branch feed (v1) vs relevant set size (v3). Turn on per
till by POS build; the v1 path is removed only under the §7.3 sunset rule.
