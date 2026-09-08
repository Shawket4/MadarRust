---
title: Privacy Policy
version: 1.3
effective: 2026-09-09
---

# Privacy Policy

This policy explains how **Madar** handles personal data in the Madar point-of-sale and
ordering platform.

## 1. Who is responsible for your data

Madar is used **by restaurants** to run their business, and that shapes who is responsible
for what.

- When you order food, make a reservation, or take a delivery, **the restaurant you ordered
  from is responsible** for that data — it is the controller. Madar processes it on the
  restaurant's behalf, on its instructions, as its processor.
- Madar is directly responsible only for the data of its own account holders — the people
  who log in to manage a restaurant — and for our own business records.

To have your order data corrected or deleted, **contact the restaurant you ordered from**.
When they instruct us, we act on it, and we will help you reach them.

## 2. What we collect

### Delivery orders and reservations

| Data | Why |
|---|---|
| Name | to identify the order |
| Phone number | to confirm the order and contact you about delivery |
| Delivery address — street, floor, landmark | to deliver the order |
| Order contents and amount | to fulfil and bill the order |

Your phone number is verified with a one-time code sent over WhatsApp.

**There is no marketing database.** This data exists to fulfil your order. We do not build
profiles from it and we do not sell it. A loyalty membership is deliberately different, and
is described below.

### Dining in

When you eat in, the waiter opens a bill against your table. If you gave a name on arrival —
for a booking, or so the staff could find you — it is held on that bill, with the number of
guests and any note about the order, and becomes part of the order record when the bill is
settled. The floor plan itself records **tables, not people**: where a table is, whether it
is occupied, and whether it still needs clearing.

### Loyalty programme

Joining is a deliberate act. You scan the code at the counter and enter your name and phone
number; ordering from a restaurant does not enrol you. The restaurant may require the number
to be verified with a one-time WhatsApp code, the same way ordering does.

| Data | Why |
|---|---|
| Name | to greet you at the counter and to put on your card |
| Phone number | to identify your membership, so one person is one membership |
| A random membership code | what your card's barcode contains. It is random, not derived from your name or number, and cannot be guessed from anyone else's |
| Points or stamps, and lifetime totals | to work out what you have earned |
| Each earning and each reward, with the order it belonged to | so a balance can be explained, checked and corrected |
| The branch whose code you scanned | reporting for the restaurant |
| Your birthday — the **day and month only** | to wish you a happy birthday, where the restaurant does that |

**We do not ask for the year you were born.** A birthday greeting needs to know *when*,
not how old you are — and a full date of birth is an identity credential, the thing a bank
asks for to prove who you are. The field is optional, it appears only where the restaurant
runs birthday rewards, and where they do not it is not shown and nothing is stored. On the
day, the restaurant's programme sends you a WhatsApp greeting and, if they have set one,
adds a reward to your card.

**A loyalty membership is, by design, a record of how often you visit and what you spend,
linked to your name and phone number.** That is what makes rewards possible, and it is the
one place in Madar where a customer's purchases are deliberately connected over time. It is
the restaurant's record. It is not shared with other restaurants on Madar, it is not sold,
and it is not used for advertising. You can leave at any time — see section 9.

### Madar accounts (restaurant staff and managers)

Name, email address, a securely hashed password, and the branches and permissions assigned
to you. Your password itself is never stored.

### Payments

Card payments are handled by the payment provider. **Madar never receives or stores card
numbers** — only which method was used and the amount.

## 3. What we do not do

- No advertising identifiers, no cross-app tracking, no ad networks.
- No tracking of where you are. The one feature that involves location — a wallet card
  surfacing near a branch — is decided by your own phone and sends us nothing.
- No selling or renting of personal data.
- No storage of card numbers.
- **No customer data is sent to any AI service.** See section 6.

## 4. Who we share data with

Only what running the service requires. The current providers, what each receives and where
it operates are listed at **[Sub-processors](/subprocessors.html)**.

One disclosure worth stating plainly: **menu item names** are sent to Google Cloud
Translation to produce translated menus. No customer data is involved — only the names of
dishes.

Where a restaurant asks us to send its order data to another system it uses — for example
an in-house shopping-mall reporting system — we do so **only on that restaurant's written
instruction**. The restaurant is responsible for that disclosure.

## 5. The analytics assistant

Restaurant managers can ask questions about their own business in plain language — "what
sold best last week", "which branch is busiest on Fridays" — and get an answer with the
figures behind it. Producing that answer involves sending the manager's question and the
resulting figures to an AI provider (see
**[Sub-processors](/subprocessors.html)**).

What this means for **you as a diner** is simple, and it is a deliberate design choice
rather than a policy promise we ask you to take on trust: **no customer data can reach the
assistant at all.** It can only ask for a fixed set of pre-written business measures —
revenue, units sold, waiting times, stock levels and the like. There is no measure in the
system that returns a customer's name, phone number, address or location, so there is
nothing for it to send. The assistant cannot write its own queries and cannot reach data
outside that fixed set.

Staff names are handled separately and are described in the
**[Employee Privacy Notice](/employee-privacy-notice.html)**.

Questions and answers are stored so a manager can return to a conversation. They are kept
with the restaurant's own account data and are deleted with it. The result rows themselves
are never stored — reopening a conversation re-runs the query, so the figures shown are
current rather than a stale copy.

The provider currently in use, the model, and exactly what is sent on each question are
listed under **[Sub-processors](/subprocessors.html)**, so the statements above can be
checked against the running system rather than taken on trust.

## 6. Loyalty cards in Apple and Google Wallet

Where the restaurant has set it up, your loyalty card can be added to Apple Wallet or Google
Wallet. This is optional: the card also works as a page on your phone with the same barcode,
and the counter scans either.

**What the card holds.** Your name, your balance, the restaurant's name and colours, and
your membership code as a barcode. The Apple version also shows your phone number on the
back of the card. All of that is your own data, on your own phone.

**Apple Wallet.** The card file is built and signed by us and downloaded to your phone.
**Apple is not sent your name, your number or your balance.** When you add the card your
phone registers with us so the card can keep itself up to date, and we store an identifier
for that device and a notification token for it. When your balance changes we send an
**empty** notification through Apple — it carries nothing about you — and your phone then
asks us for the new card.

**Google Wallet.** Google's system works differently, and the difference matters: the card
is **stored on Google's servers**. Adding it sends **your name, your membership code, your
balance and the restaurant's branch coordinates to Google**, and sends them again whenever
your balance changes. If you would rather Google did not hold that, use the web card or
Apple Wallet — the programme works identically either way. Google is listed under
**[Sub-processors](/subprocessors.html)**.

**On the card appearing when you are near the shop.** The card carries the coordinates of
the restaurant's branches, and your phone compares them against where it is. **That
comparison happens on your phone. Your location is never sent to us. We do not receive it,
store it, or have any way to see where you are.** Your phone's own settings control whether
this happens at all.

## 7. Where data is stored

Primary systems are hosted in Europe. Backups are held on separate infrastructure under our
control. Some providers listed under Sub-processors operate outside Egypt; that is
identified there.

## 8. How long we keep it

Set out in full in the **[Data Retention Schedule](/data-retention.html)**. In summary:
order records are kept for **5 years** to meet accounting and tax requirements; account data
is kept while the account is active; diagnostic error reports are deleted after **30 days**.

## 9. Your rights

Under Egyptian data protection law you may request access to your data, its correction or
deletion, and may object to how it is used. Because the restaurant is the controller of
order data, requests about orders go to them and we assist.

**To leave a loyalty programme**, ask the restaurant, or write to us and we will help you
reach them. Deleting the membership removes your name, phone number and membership code, and
the card stops working. The orders themselves stay in the restaurant's accounting records for
the period in the retention schedule. Note that **removing the pass from your phone stops
the card being shown but does not, by itself, delete the membership** — ask the restaurant
for that.

To delete a Madar account, see **[Delete your account](/delete-account.html)**.

## 10. Security

How the platform is secured is described in **[Security](/security.html)**.

## 11. Children

Madar is a tool for businesses and their customers. It is not directed at children.

## 12. Changes

We will notify material changes through the product. Every previous version of this policy
remains available with its effective date.

## 13. Contact

**privacy@madar-pos.cloud**
