# Sessions: every sign-in can be ended

Since migration `20261028090000_auth_sessions`, every dashboard, till and kitchen
sign-in is a row in `auth_sessions`, and its bearer token names the row (`sid`).
Revoking the row refuses the token on its next request, whatever its own expiry says.
Only a super admin sees or revokes sessions, from **madar-admin → Sessions**
(`/admin/sessions`). The dashboard has no session UI.

## What a revocation does to each client

The answer is **401 `SESSION_REVOKED`** with the usual JSON error body.

| Client | What happens | Data lost |
|---|---|---|
| Dashboard | Bounces to `/login` (its 401 handler) | none |
| POS till | Outbox pauses (no retry spent), re-login banner; the next online PIN sign-in resumes it | none: queued sales wait |
| Kitchen screen | Re-mints a session from its device credential within a minute (only retiring the device stops it) | none |
| Realtime stream | Closed within 20 s (the keepalive re-checks) | none |
| Dawam staff app | Not affected: staff tokens are their phone's session (`staff_devices`) | n/a |

A bare 401 would read as a captive portal on the POS (offline), and a 403 would
dead-letter a queued sale. That is why the code is a coded 401.

## Lifetimes

| Sign-in | Lifetime |
|---|---|
| PIN / branch-bound till, kitchen device session | 24 h (was 12 h) |
| Dashboard email + password, owner signup | 48 h (was 24 h) |
| Demo | `DEMO_TTL_HOURS` (unchanged) |

## The deploy signs nobody out

A token issued before this release has no `sid`. It keeps working until its own
expiry. On first use it is **adopted** as a `legacy` row (keyed by the token's
SHA-256), so it is listed and can be revoked like any other. It cannot be
lengthened (the expiry is signed into it), so it runs out on its old 12/24 h
schedule, and the next ordinary sign-in gets a 24/48 h token.

"Sign out everywhere" for a person also sets `users.sessions_valid_after`. That
ends their old tokens that were never seen, so adoption can never revive one.

## Production `.env`

```
MADAR_SESSION_ORIGINS=https://madar-pos.cloud,https://rewards.madar-pos.cloud
```

Add `https://rewards.madar-pos.cloud` to `CORS_ALLOWED_ORIGINS` when the Rewards
dashboard ships there. CORS now sends `Access-Control-Allow-Credentials` for those
origins, which only `/auth/session` uses: its cookie is host-only on `api.` and
path-scoped, so no other route ever receives one.

## Operations

- Rows that ended more than 30 days ago are pruned hourly.
- Revocations log at `auth.sessions.revoke` (who, what, reason). The row keeps
  `revoked_by` and `revoke_reason`.
- A revocation notifies `authz_user`, so every instance drops its cached answer
  at commit. Checks are served from memory (`fastpath`) while the listener is up,
  so there is no database read per request.
