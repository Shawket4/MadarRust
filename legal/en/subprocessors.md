---
title: Sub-processors
version: 1.5
effective: 2026-09-11
---

# Sub-processors

Third parties that may process personal data on behalf of Madar's customers. Customers are
notified at least **30 days** before a new one is added.

| Provider | Purpose | Data it receives | Location |
|---|---|---|---|
| Hostinger | Application and database hosting | All platform data | European Union |
| Cloudflare | Serving the customer-facing pages — the menu, the loyalty card and bookings — and the network that carries them | Everything those pages send and receive, including the visitor's **IP address**, the pages requested, and the contents of those requests. Cloudflare terminates the encrypted connection, so it can see this traffic in the clear | Global network; served from the location nearest the visitor |
| Google Cloud Translation | Translating menu item names | **Menu item names only** — no customer data | Outside Egypt |
| WhatsApp / Meta | Delivering one-time codes, order updates, and the loyalty programme's birthday and "we've missed you" messages where a customer has no wallet card to reach | Customer phone number, message text | Outside Egypt |
| Google (Gemini) | Answering managers' plain-language questions about their own business | The manager's question and aggregated business figures — **no customer data**; staff names are replaced with codes before sending | Outside Egypt |
| Apple | Delivering loyalty-card updates to a customer's iPhone | A device notification token and an **empty** push — no name, number, balance or message text. The card file itself, including any message written onto it, is built and signed by us and never passes through Apple | Outside Egypt |
| Google Wallet | Holding a customer's loyalty card, **only if that customer chooses to add one** | Customer name, membership code, current balance, the restaurant's branch coordinates, and the **text of any message the restaurant puts on the card** | Outside Egypt |

### Current configuration (as of 3 September 2026)

Stated concretely so this page can be checked against the running system rather
than taken on trust.

| | |
|---|---|
| Status | **Enabled** |
| Provider and model | Google, `gemini-3.1-flash-lite` |
| Sent per question | The question text, the merchant's branch names, the current date and timezone, and up to **40 rows** of the aggregated result |
| Model calls per question | At most 4, and at most 3 database queries |
| Sampling | Deterministic (temperature 0) |
| Customer data sent | **None.** No measure in the system returns one |
| Staff names sent | **None.** Replaced with `E-1`, `E-2` … before sending |
| Conversations | Stored so a manager can reopen them; kept with the merchant's account and deleted with it. Only the questions, the answers and the queries are stored — never the result rows |
| Turning it off | Removing the provider credential disables it entirely; nothing is then sent to any AI service |

## Cloudflare, and what it can see

Cloudflare sits in front of the customer-facing pages as a reverse proxy. A customer opening
a menu or a loyalty card connects to Cloudflare, and Cloudflare connects to us.

That arrangement is worth stating plainly rather than burying in a table, because it has a
consequence people often assume away: **Cloudflare ends the encrypted connection, not us.**
It can see the traffic in the clear before re-encrypting it to our servers. That includes the
visitor's IP address, which pages they opened, and anything they submitted on them.

What it does **not** carry:

- **The staff-facing system.** The till, the dashboard and the API they use reach our servers
  directly, without passing through Cloudflare.
- **The database.** Cloudflare never holds a copy of anything. It relays requests; it is not a
  store.
- **Card numbers.** These never reach our systems at all, by any route — see the Privacy
  Policy.

Cloudflare acts on our instructions as a processor and does not use this traffic for its own
purposes.

## Run on our own infrastructure

These functions are commonly outsourced. We do not outsource them, so the data stays under
our control:

- **Error and crash monitoring** — self-hosted. Diagnostic data is not sent to any
  monitoring vendor, is configured to exclude personal data, and is deleted after 30 days.
- **Route and distance calculation** — self-hosted. Delivery addresses are **not** sent to a
  mapping company.
- **Short links** — self-hosted.
- **WhatsApp gateway** — self-hosted, though messages necessarily transit Meta.
- **Backups** — held on infrastructure we control, encrypted at rest.

## The AI provider, in detail

The analytics assistant is the only place any data reaches an AI service, and what it can
send is bounded by construction rather than by policy:

- It can only run a **fixed set of pre-written business measures**. It cannot compose its
  own queries and cannot reach any table outside that set.
- **No measure returns customer data.** There is no dimension anywhere in the system that
  produces a customer's name, phone number, address or location, so none can be sent.
- **Staff names are pseudonymised before sending.** A result naming an employee is
  substituted with a stable code (`E-1`, `E-2`) on the way out and the real name is put
  back into the answer on the way in. The provider receives the code; the manager sees the
  name. This applies to the manager's own question, to the figures, and to earlier messages
  replayed for context.
- Business information — branch, product, category, ingredient and supplier names, and the
  figures themselves — **is** sent, because it is what the question is about.
- The provider is not permitted to train on this data under the terms we use.

An operator can turn the assistant off entirely for a deployment by removing the provider
credential, in which case nothing is sent to any AI service at all.

## Wallet passes, in detail

A loyalty card can be added to Apple Wallet or Google Wallet. The two are not equivalent in
what they disclose, and a customer who cares should be able to choose knowingly.

- **Apple** does not receive the card. We build and sign the file ourselves and the phone
  downloads it from us. What reaches Apple is a push notification with **no payload** — it
  only tells the phone to come back and ask us for a fresh copy. We hold a device identifier
  and a notification token per registered device, and nothing else about the device.
- **Google** does receive the card, because Google Wallet stores it server-side. The
  customer's name, membership code, balance and the branch coordinates are sent when the
  card is added and again on each balance change.
- **A message put on the card is a further disclosure, and only for Google.** The loyalty
  programme delivers a birthday greeting or a "we've missed you" through the card itself.
  Apple has no way to send text to a pass — a notification is a *field* whose value changed
  — so the words are written onto the card on our servers and the push stays empty; Apple
  never sees them. Google's card is held by Google, so the words are sent to Google's Wallet
  API to be put on it. There is no way to put a message on a Google card without sending it
  to Google.
- **Only Apple reports delivery.** A device fetching the updated pass is the one delivery
  signal either wallet gives us. Where a card was reached on Apple and no device has come
  back after about 8 hours, the message is sent on WhatsApp instead; a card saved only in
  Google Wallet gets no such fallback, because Google reports nothing back.
- **Neither receives a customer's location, and neither do we.** A card that surfaces near a
  branch does so because the branch coordinates travel *to* the phone and the phone does the
  comparison locally.
- A restaurant that configures neither still has a working programme: the card is a web page
  with the same barcode.

## Not used

- No other AI or machine-learning services.
- No advertising networks, analytics trackers, or data brokers.

## Customer-directed disclosures

Some restaurants instruct us to send their own order data to a third-party system they use —
for example an in-house shopping-mall reporting system. These are **not** Madar
sub-processors: the restaurant chooses the recipient, instructs us in writing, and is
responsible for the lawfulness of the disclosure. We transmit only what the integration
defines, over authenticated connections.

## Transfers outside Egypt

Providers marked "Outside Egypt" involve transferring personal data out of Egypt. Such
transfers are made with the safeguards required by Egyptian data protection law.

**Contact:** privacy@madar-pos.cloud
