---
title: Sub-processors
version: 1.1
effective: 2026-09-03
---

# Sub-processors

Third parties that may process personal data on behalf of Madar's customers. Customers are
notified at least **30 days** before a new one is added.

| Provider | Purpose | Data it receives | Location |
|---|---|---|---|
| Hostinger | Application and database hosting | All platform data | European Union |
| Google Cloud Translation | Translating menu item names | **Menu item names only** — no customer data | Outside Egypt |
| WhatsApp / Meta | Delivering one-time codes and order updates | Customer phone number, message text | Outside Egypt |
| Google (Gemini) | Answering managers' plain-language questions about their own business | The manager's question and aggregated business figures — **no customer data**; staff names are replaced with codes before sending | Outside Egypt |

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
