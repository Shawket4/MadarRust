---
title: Data Retention Schedule
version: 1.2
effective: 2026-09-09
---

# Data Retention Schedule

Periods run from the trigger in the third column.

## Customer and order data

| Data | Kept for | From |
|---|---|---|
| Orders, order items, payments | 5 years — accounting and tax records | order date |
| Delivery details — name, phone, address | 5 years, as part of the order record | delivery |
| Reservations and waitlist entries | 12 months | booking date |
| Dine-in bill — table, guest name if given, guest count, notes | 5 years, as part of the order record | settlement |
| Loyalty membership — name, phone, membership code, balances | life of the membership | deletion request |
| Loyalty birthday — **day and month only, never the year** | life of the membership | deletion request |
| Birthday greetings sent — which member, which year | 2 years | greeting |
| Loyalty points ledger — each earning and reward | 5 years, with the order it belongs to | transaction |
| Wallet pass device registrations — device identifier, notification token | until the pass is removed from the device or the membership is deleted | registration |
| WhatsApp one-time codes | minutes — expire on use | issue |

## Employee data

| Data | Kept for | From |
|---|---|---|
| Payroll — payslips, deductions, bonuses, advances | 5 years | payroll period |
| Employment record — profile, contract dates, identification | 5 years | end of employment |
| **Attendance GPS coordinates** | **90 days**, then permanently erased | punch |
| Attendance times, lateness, overtime | 5 years, as payroll evidence | punch |
| Leave requests and balances | 5 years | request |
| Documents uploaded by the employer | 5 years | end of employment |

**On attendance coordinates.** The *time* of a clock-in has to be kept as long as payroll,
because it is the evidence for what someone was paid. The *coordinates* do not: once a punch
is settled, the latitude and longitude have served their only purpose. After 90 days they
are erased automatically. What remains is the punch time, the method, and the geofence
result — the distance in metres between the employee and the branch — which records that the
punch was valid **without recording where the employee was**.

**On birthdays.** Only the day and month are stored. The year is not collected
at all — a greeting needs to know when, not how old someone is, and a full date
of birth is a different category of data from a calendar day. A record that a
greeting was sent is kept so nobody is messaged twice in one year; it holds the
member, the year and what was given, and nothing about the message.

**On loyalty memberships.** Deleting a membership removes the name, phone number and
membership code, and every device registration with it, so the card stops working and stops
updating. The *orders* remain — they are the restaurant's accounting records and are kept for
the same five years as any other order. What is left no longer identifies the member.

Removing a pass from a phone is not a deletion request. It stops the card being displayed;
the membership continues until the restaurant deletes it.

## Account and technical data

| Data | Kept for | From |
|---|---|---|
| User accounts | life of the account | deletion request |
| Error and diagnostic reports | 30 days, deleted automatically | event |
| Database backups | rolling cycle — 4 full and 7 differential backups | backup |

**Backups are an honest exception.** Data deleted from the live system persists in backups
until those backups age out on the normal cycle. We do not surgically edit backups: doing so
would destroy their integrity as recovery points, which is the entire reason they exist.
Deleted data therefore disappears from backups within the backup cycle and is never restored
to live systems except as part of a whole-system recovery.

## Requests

See **[Delete your account](/delete-account.html)**. Where a record must be kept for a
statutory period, we keep it for that period and no longer.

**Contact:** privacy@madar-pos.cloud
