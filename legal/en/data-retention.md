---
title: Data Retention Schedule
version: 1.0
effective: 2026-09-01
---

# Data Retention Schedule

Periods run from the trigger in the third column.

## Customer and order data

| Data | Kept for | From |
|---|---|---|
| Orders, order items, payments | 5 years — accounting and tax records | order date |
| Delivery details — name, phone, address | 5 years, as part of the order record | delivery |
| Reservations and waitlist entries | 12 months | booking date |
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
