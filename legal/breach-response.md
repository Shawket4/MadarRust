---
title: Data Breach Response Runbook (INTERNAL)
version: 1.0-draft
last_updated: 2026-08-31
---

# Data Breach Response Runbook

**INTERNAL — do not publish to legal.madar-pos.cloud.**

**Draft — the notification clock and recipient must be confirmed with counsel.**

Egypt's PDPL requires notifying the Data Protection Centre within **72 hours** of becoming
aware of a breach. 72 hours is not long. The purpose of this document is that nobody is
designing a process while the clock runs.

## 0. Before anything — preserve evidence

**Do not wipe, rebuild, or "clean up" the affected system first.** Logs and disk state are
the evidence of what happened and what was reached. Snapshot before remediating.

## 1. Contain (immediately)

- Revoke credentials/tokens that may be exposed.
- Isolate the affected system — but capture volatile state (running processes, connections)
  before shutting anything down.
- **Do not** restore over a compromised system until the entry point is understood; you
  will restore into the same hole.

## 2. Assess (hours 0–24)

Answer, in writing:

- What data? Which categories, which tables, which fields?
- Whose? Diners, restaurant staff, employees? Roughly how many?
- **Was it sensitive?** National ID numbers, salary, attendance coordinates and customer
  addresses all sit in this system. Those raise the severity materially.
- Was it exfiltrated, or only exposed?
- Is it ongoing?

Anchor the timeline: when did it start, when did we become aware. **Awareness starts the
72-hour clock** — record that timestamp explicitly.

## 3. Notify

| Who | When | Notes |
|---|---|---|
| Data Protection Centre | **within 72h of awareness** | confirm the current filing route with counsel |
| Affected **restaurants** (controllers) | without undue delay | they are the controllers; they may have their own duty to notify diners |
| Affected individuals | where the law requires, or where risk is high | usually via the controller |
| Counsel | immediately | before external communications |

**We are usually the processor.** For diner and employee data, the restaurant is the
controller — our first duty is to inform them promptly and give them what they need to
meet their own obligations. Do not notify a restaurant's customers directly without them.

Record what was sent, to whom, and when.

## 4. Remediate

Fix the entry point, rotate all plausibly exposed secrets, and verify the fix. Only then
restore service.

## 5. Post-incident

Within two weeks: written record of what happened, why, what was done, and what changed.
Keep it — regulators ask, and so do enterprise customers.

## Contacts

| Role | Who |
|---|---|
| Incident lead | **[TBC]** |
| DPO | **[TBC]** |
| Counsel | **[TBC]** |
| Data Protection Centre filing route | **[TBC — confirm]** |

## Known exposure map (keep current)

| Where | Sensitive data |
|---|---|
| Production database (VPS) | everything below |
| `staff_profiles` | **national ID**, salary, photo, emergency contacts |
| `attendance_records` | **employee GPS coordinates** |
| payroll tables | payslips, deductions, advances |
| delivery orders | customer name, phone, address |
| `users` | email, password hashes |
| **Backup repository (OptiPlex)** | a full copy of all of the above — **currently unencrypted at rest** |
| Self-hosted error monitoring | should contain no personal data (scrubbing configured); verify during assessment |

The backup repository holds the same data as production. **A breach of the backup box is a
breach of everything.** Its unencrypted state is a known, accepted gap — see the DR notes.
