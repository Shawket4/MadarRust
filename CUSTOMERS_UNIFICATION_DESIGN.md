# One customer — unification design

Status: **design approved in principle (2026-09-20), not started.**
Scope: MadarRust (backend), madar/madar (Cashier, KDS, rust-core), MadarDashboard (dashboard + public loyalty/order/reservations apps).

## 1. Problem

One person exists today in five unconnected forms:

| # | Concept | Storage | Key |
|---|---|---|---|
| A | Manual customer (checkout) | `customers` | uuid, `phone_key` = `01…` |
| B | Loyalty member | `loyalty_customers` + ledger, passes, birthdays, winback | uuid, `member_token`, phone = `201…` |
| C | Online/delivery guest | `delivery_orders.customer_name/phone` | phone string |
| D | Booking guest | `bookings.guest_name/guest_phone` | phone string |
| E | Free-text name | `orders.customer_name`, `open_tickets.customer_name`, KDS tender | none |

Consequences: phones cannot be joined (three formats); delivery and bill-settle orders never count toward a customer; one sale can name three different people; `customer_id` means "member id" in the loyalty API and "CRM id" on orders; analytics counts name strings; PDPL erase leaves the phone in delivery/bookings/loyalty; consent lives only on the loyalty card.

## 2. Target model

### 2.1 Identity: one id, shared primary key

`customers` is the only identity. A loyalty membership is a row in `loyalty_customers` **whose `id` equals the customer's `id`** (`loyalty_customers.id → customers(id)`, 1:0..1).

- One uuid per person everywhere. `customer_id` in `/loyalty/lookup|award|adjust` becomes literally true with no API change.
- "Is a member" = the membership row exists. No nullable-column conventions.
- Balances, `member_token`, Apple/Google pass secrets stay in `loyalty_customers`: hot trigger-maintained writes stay off the row that fans out to every till, and secrets stay out of a table the POS role reads.
- The ledger, pass devices, pass cache, birthdays, winbacks keep their FKs untouched.
- `orders.loyalty_customer_id` becomes redundant (= `customer_id` when the customer is a member); kept during transition, dropped in the last phase.

### 2.2 Fields owned by `customers` (single source of truth)

`name`, `phone` (canonical), `phone_display` (as typed), `locale`, `birth_month`, `birth_day`, `marketing_opt_out`, `notes`, `source` (`pos|online|loyalty|booking|table_qr|aggregator|dashboard`), `first_seen_at`, `merged_into`, `merged_at`, `erased_at`, `created_by`, `created_branch_id`.

Removed from `loyalty_customers` after backfill: `name`, `phone`, `locale`, `birthday`, `birth_month`, `birth_day`, `marketing_opt_out`. A view `loyalty_members_v` (membership ⨝ customer) replaces them for reads, so `MemberView` keeps its shape.

### 2.3 Phone: one canonical form

E.164 digits without `+` (`201001234567`) — the form loyalty, delivery and bookings already use.

- One SQL function `phone_canonical(text)`, one Rust function (move `delivery::normalize_phone` to `crate::phone`), one rust-core function, one TS function (`src/lib/phone.ts`). All four share a test-vector file (`phone_vectors.json`) checked in each repo's tests, so they cannot drift.
- `customers.phone_key` is migrated to the canonical form; core `classify_scan_input` canonicalises instead of passing the raw string.
- **DB-enforced uniqueness**: `UNIQUE (org_id, phone) WHERE merged_into IS NULL AND erased_at IS NULL AND phone IS NOT NULL`. Replaces the racy application check.
- One phone per customer. Previous numbers go to `customer_phone_history` (audit + resolving old references), not a multi-phone model.

### 2.4 Resolve-or-create — the only way a customer comes into being

```
customers_resolve_or_create(org, phone, name, source, branch, actor, client_id?) -> (customer_id, created|matched|merged_into)
```

- `INSERT … ON CONFLICT (org_id, phone) WHERE live DO NOTHING`, then select — race-free by construction.
- Matched customer: never overwrites the stored name from a transactional flow (name changes only via explicit edit / replace identity, §4.4).
- No phone → **no customer** (decision 3). The typed name is only the order's snapshot.
- Client-minted ids (offline tills) keep today's behaviour: if the phone is taken, the new row is inserted already `merged_into` the holder.
- Called from: POS create, order create, delivery order create, loyalty join, loyalty scan-by-phone, booking create (public and host), table-QR order when a phone is given, aggregator ingest (future), dashboard create.
- Auto-create on first contact is **on** (decision 2). Rows carry `source`; the dashboard default filter is "member OR ≥2 orders OR created by staff".

### 2.5 References on transactional rows

Every row that involves a person gets `customer_id` **plus an immutable contact snapshot**:

| Table | Reference | Snapshot (what was typed/printed) |
|---|---|---|
| `orders` | `customer_id` (soft FK kept: a sale is never refused) | `customer_name` |
| `delivery_orders` | `customer_id` | `customer_name`, `customer_phone`, address fields, `address_id` |
| `bookings` | `customer_id` | `guest_name`, `guest_phone` |
| `open_tickets` | `customer_id` (nullable) | `customer_name` |

The snapshot is history; the id is identity. Reports and the customer page go through the id (resolved through the merge chain); receipts and the kitchen print the snapshot. This is what makes "one-time order for someone else" (§4.4) safe.

`customers_resolve()` stays the single merge-chain walker; a nightly job re-points references older than 30 days to the surviving id so the chain stays shallow.

### 2.6 Addresses

New `customer_addresses`: `id, org_id, customer_id, label, place_name, floor, unit_number, landmark, address_line, delivery_notes, lat, lng, delivery_zone_id, use_count, last_used_at, created_at, erased_at`.

- Dedup on write: same customer + normalised text equal, or within 30 m and same unit → bump `use_count/last_used_at` instead of inserting.
- Replaces the `DISTINCT ON` scan in `guest_past_locations`.
- Backfilled from `delivery_orders`.

### 2.7 Merge, with memberships

Merge is already implemented on `customers`; extend it:

- Neither or one side is a member → if the loser holds the membership, the membership moves with its id: the **member side always survives** (its id is baked into passes and 7 FKs). The dashboard dialog pre-selects accordingly and explains why.
- Both are members → survivor chosen by the operator; the loser's balance moves as a pair of `adjust` ledger rows (`source='merge'`, `reverses_id` untouched — append-only safe); loser's membership is soft-deleted, its passes are voided (Apple: push an update marking `voided`; Google: object state `INACTIVE`), its `member_token` keeps resolving to the survivor for 90 days so a card already in someone's hand still scans.
- Addresses, bookings, delivery orders, consent (`opt_out = a OR b` — the stricter wins), phone history all re-point in the same transaction; rows locked in id order (as today).

### 2.8 Erase (PDPL) — one action

`POST /customers/{id}/erase` (cap `customers.erase`) in one transaction: blank customer PII; forget the membership (existing `loyalty::model::forget`) and void passes; blank snapshots on `delivery_orders`, `bookings`, `open_tickets`, `orders.customer_name` for that customer; mark addresses erased; purge phone history and OTP rows. Money and ledger rows stay (legal retention), anonymised. `DELETE /loyalty/members/{id}` becomes "leave the programme" (membership only) and no longer wipes identity.

## 3. Migration plan (backend)

All steps idempotent, each its own migration, each with a verification query that must return 0 rows before the next runs.

0. **Bug fixes, independent**: add `customer` to core `SYNCED_TYPES` (LAN sync drops customers today); emit `customer_id` in the order pull projection; grant parity (`madar_app` read on `loyalty_members_v`).
1. `phone_canonical()` + migrate `customers.phone/phone_key`; add `customer_phone_history`.
2. Dedup `customers` by canonical phone through the existing merge chain (oldest with most orders survives). Then add the unique index.
3. **Shared-key backfill**:
   a. member with no customer on that phone → insert `customers` row with **the member's id**, `source='loyalty'`.
   b. member whose phone matches a live customer (or is linked via `customers.loyalty_customer_id`) → insert a `customers` row with the member's id carrying the merged details, and mark the old customer row `merged_into` it. Offline tills holding the old id resolve through the chain. (`customers` dates from 2026-09-18, so this set is tiny — do it before it grows.)
   c. add FK `loyalty_customers.id → customers(id)`; drop `customers.loyalty_customer_id`.
4. Move owned fields (§2.2) to `customers`; create `loyalty_members_v`; switch loyalty reads to the view; drop the columns.
5. Add `customer_id` to `delivery_orders`, `bookings`, `open_tickets`; backfill by canonical phone (creating customers, `source` by table); backfill `orders.customer_id` from delivery rows and from `loyalty_customer_id`.
6. `customer_addresses` + backfill.
7. Switch every entry point to `customers_resolve_or_create` (§2.4). From here no new unlinked rows can appear; a CI test greps for direct `INSERT INTO customers` outside the one function.
8. Last, after all clients ship: drop `orders.loyalty_customer_id`, make the analytics `unique_customers` metric `COUNT(DISTINCT customers_resolve(customer_id))` and add the customer dimension (new/returning, channel mix, LTV, member y/n).

## 4. "Order now" from the wallet pass

### 4.1 What the customer sees

A button on the pass → the ordering page opens **already knowing who they are**: name, phone, last branch, last channel (delivery/pickup), last address, last payment hint. Everything is editable. One tap to the menu.

- Google Wallet: a `linksModuleData` entry ("Order now") — rendered as a real button.
- Apple Wallet: passes have no front buttons; the link goes on the back of the pass as the first back field (tappable), and optionally rides a `changeMessage` notice ("Order again in one tap"). Same link added to the public card page as a primary button, and to winback/birthday WhatsApp messages.
- Link: `{PUBLIC_ORDER_BASE_URL}/now/{member_token}`. Built next to `wallet::card_link` so both wallets share it; gated on the org having online ordering enabled, otherwise omitted (passes refresh when the setting flips).

### 4.2 Trust model — the token identifies, the device authorises

`member_token` is printed as the QR and seen by every cashier, so **the token alone must never expose an address or place an order under someone's name.**

| Holder has | Sees | Can do |
|---|---|---|
| token only | first name, phone masked (`•••• 4567`), branch name | start the flow; must verify |
| token + valid `device_token` for that customer's phone | full prefill, saved addresses, history | order in one tap |

- First tap on a new device: one WhatsApp OTP to the customer's phone (existing `/otp/request|verify`), which issues the existing HMAC `device_token`; stored in localStorage by `public-shell/guest.ts`. Every later tap is zero-friction.
- `GET /public/order-now/{token}` takes the optional `device_token`; returns the masked or the full `OrderNowContext` accordingly. Rate-limited per token and per IP; a wrong-org or unknown token returns the same 404 as today's card endpoints.
- Device tokens are bound to the phone: after a phone change (§4.4) tokens for the old phone stop authorising this customer automatically.
- Prefill is **re-validated at open time**, never trusted: branch still exists/open/accepting the channel, address still inside a live delivery zone for that branch, menu items still available. Anything stale falls back to the normal chooser step with the rest kept.

### 4.3 What gets saved

On a successful order (not before — abandoned carts save nothing): address upserted into `customer_addresses` (§2.6), `last_used_at` bumped. "Last branch/channel/address" are **derived** from the customer's latest order + most recently used address — no separate preference row to go stale. Order submit carries an idempotency key (double-tap / retry safe).

### 4.4 Editing identity — "one-time" vs "replace identity"

Edits are classified by the server (the client only renders the choice):

| Edit | Behaviour |
|---|---|
| Branch, channel, address, notes, payment hint | No prompt. Saved as in §4.3. |
| Name only | Inline choice: **Just this order** (default) / **Update my name**. |
| Phone (with or without name) | Blocking choice, below. |

**Just this order (one-time)** — "I'm ordering for someone else".
- The order's *snapshot* carries the typed name/phone (driver calls that number); `customer_id` stays the pass owner; points earn to the pass owner.
- Address used is **not** saved to the profile unless the customer ticks "save this address".
- No OTP for the new number unless the org requires OTP for delivery, in which case the existing per-order OTP applies to the snapshot phone.
- Flagged `delivery_orders.contact_override = true` so staff see "ordered by X for Y".

**Replace identity** — "this is my new number / correct my name".
- Requires a verified device for the **current** phone (already true to be here) **and** an OTP on the **new** phone. Both proofs, or nothing changes.
- Same `customers.id`; all orders, points, passes, addresses, bookings stay. Old phone → `customer_phone_history`. Audit row written (`who: customer-self`, old → new).
- If the new phone already belongs to another live customer: the customer has just proven control of both → offer **"these are both me — combine"**, which runs the standard merge (§2.7, pass holder survives). Declining leaves both untouched and falls back to one-time.
- If the new phone belongs to a customer with a membership of their own, combining follows the both-members rule (§2.7).
- After commit: passes refresh (name / member line), changefeed emits the customer to tills, a new `device_token` is issued for the new phone, old-phone tokens no longer resolve to this customer.
- Limits: max 2 identity replacements per 30 days per customer; blocked for 24 h after a merge; staff can do the same from the dashboard with `customers.edit` (no OTP, audited).

The same classification runs on the normal (non-pass) ordering page for a returning verified guest, so there is one code path.

## 5. POS / rust-core

- `CheckoutInput`: one `customer_id` + `customer_name` snapshot. `loyalty_customer_id` accepted for one release, then removed. Scanning a loyalty card **attaches the customer** (same id), so the three-people-on-one-sale state is unrepresentable.
- Attach a customer to bills, online orders and past orders — finally enqueue the existing `AttachCustomer` replay op. `TicketView` gains `customer_id`.
- `CustomerView` gains `is_member`, `balance_label` (from the membership feed row), `source`.
- Online-order details show the linked customer (history, member badge) and the `contact_override` marker.
- KDS tender: same customer field set via the shared core (no free-text-only path).
- Local search unchanged (already ranked by canonical key once §2.3 lands). Offline create unchanged (client-minted id, server merges on phone collision).

## 6. Dashboard

- One **Customers** page: member badge, points, source filter, channel mix, last visit. Loyalty → Members becomes this page pre-filtered to members (one search/paging/export implementation).
- Customer detail = the 360: orders across channels, bookings, addresses, loyalty ledger + adjust, passes, consent, phone history, merge, erase.
- Orders table/export, floor tickets, bookings, delivery rows link to the customer. Booking dialog moves to RHF + Zod with the shared phone rule.
- Public apps (`loyalty`, `order`, `reservations`) use `src/lib/phone.ts` and one OTP component.
- Regenerate Orval after each backend phase; all strings through `useTranslation()`.

## 7. Permissions

Existing: `customers.attach|view|create|edit|erase`, `loyalty.read|use|members.list|points.adjust|members.delete`.
Add: `customers.merge` (split from edit, default owner/manager), `customers.addresses.view` (default owner/manager/teller — drivers and tellers need it, waiters don't). Every new surface in §4–6 ships gated (see memory: permissions-cover-new-work).

## 8. Robustness checklist (must hold at every phase)

- DB constraints, not application checks: unique phone, shared-key FK, not-merged-into-self, merge chain depth.
- Every write path idempotent (client ids, idempotency keys, `ON CONFLICT`).
- A sale is never refused because of a customer problem — bad/unknown reference degrades to snapshot-only and is logged.
- Offline tills: old ids always resolve (merge chain), new fields optional for one release in both directions (old core ↔ new server, new core ↔ old server).
- Property tests: phone vectors shared across 4 implementations; merge is associative on balances; erase leaves no PII (test greps every text column for the seeded phone/name).
- Backfill dry-run mode that reports: customers created, merged, conflicts (same phone, different names) for manual review before commit.
- Metrics: count of orders with phone snapshot but null `customer_id` (should trend to 0), resolve-or-create outcomes, identity replacements, OTP failures per token.

## 9. Order of work

1. Phase 0 bug fixes (small, shippable alone).
2. Backend §3 steps 1–4 (identity + shared key) → dashboard Customers/Members merge.
3. Backend steps 5–7 (references, addresses, resolve-or-create) → POS §5.
4. Order-now (§4): backend context endpoint + identity classification → public order app → pass links.
5. Analytics + cleanup (step 8).

## 10. Decisions taken (2026-09-20)

1. Shared-primary-key model (A's tables, C's identity).
2. Auto-create customers on first verified contact, tagged by source.
3. No phone → no customer; name is a snapshot.
4. One phone per customer + history table.
5. Erase cascades to membership, passes, snapshots, addresses.
