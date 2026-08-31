---
title: Security
version: 1.0
effective: 2026-09-01
---

# Security

How the Madar platform protects the data it holds.

## Tenant isolation

Every organisation's data is separated at the **database level**, not merely in application
code. PostgreSQL row-level security policies are attached to the tables themselves, and the
role the application connects as carries no exemption from them. A query issued for one
restaurant cannot return another restaurant's rows, and a bug in application code cannot
widen that boundary — the database refuses it.

Within an organisation, access narrows again by role and by branch: a user sees the branches
they are assigned to and the operations their permissions allow.

## Administrative access

Administrative access to the database and servers is held by **one person — the owner of
Madar**. No other developer, employee or contractor has administrative access to production
systems or to customer data. That access is used only to operate the service, respond to a
support request, or comply with a legal obligation.

Keeping this to a single individual is deliberate: the surface for both mistakes and misuse
is the number of people holding the keys.

We state this plainly rather than claiming otherwise: **operating a hosted service requires
that someone can administer the systems it runs on.** Any provider telling you no
administrator can reach your data — on infrastructure they control, without client-side
encryption — is describing something other than how databases work.

## Encryption

- **In transit:** TLS on all public endpoints, with certificates renewed automatically.
- **At rest:** the backup repository is encrypted.
- **Passwords:** stored only as salted hashes; never recoverable, by us or anyone else.
- **Card numbers:** never touch Madar systems — they go directly to the payment provider.

## Access to servers

Administrative access is by SSH key only; password authentication is disabled. Automated
components authenticate with credentials restricted to a single protocol — a backup
credential can run the backup program and nothing else, so it cannot be repurposed into a
shell if it is ever exposed. Hosts run firewalls and automated intrusion blocking.

## Backups and recovery

Backups run on an automated schedule to infrastructure separate from the production server,
with continuous transaction-log archiving so recovery is possible to a point in time rather
than only to the last nightly copy.

Backups are **verified, not assumed**. Every week an automated job restores the most recent
backup into a disposable environment that matches production, checks that every database is
present and queryable, and compares record counts against the previous run so that silent
loss is detected. A failure raises an alert.

## Monitoring

Error and crash reporting runs on **our own infrastructure**; diagnostic data is not sent to
a third-party monitoring vendor. Reports are configured to exclude personal data and are
deleted after 30 days.

## Reporting a vulnerability

Email **privacy@madar-pos.cloud**. Tell us what you found and how to reproduce it. We will
acknowledge, investigate, and keep you informed. Please give us a reasonable opportunity to
fix an issue before disclosing it publicly.
