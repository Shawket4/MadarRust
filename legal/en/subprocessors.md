---
title: Sub-processors
version: 1.0
effective: 2026-09-01
---

# Sub-processors

Third parties that may process personal data on behalf of Madar's customers. Customers are
notified at least **30 days** before a new one is added.

| Provider | Purpose | Data it receives | Location |
|---|---|---|---|
| Hostinger | Application and database hosting | All platform data | European Union |
| Google Cloud Translation | Translating menu item names | **Menu item names only** — no customer data | Outside Egypt |
| WhatsApp / Meta | Delivering one-time codes and order updates | Customer phone number, message text | Outside Egypt |

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

## Not used

- **No AI providers.** No data is sent to any AI or machine-learning service.
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
