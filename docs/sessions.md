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

## The shared cookie and production `.env`

The dashboard and Rewards by Madar each call the API through their own host's
`/api/` proxy, so the shared cookie lives on one of them: the Rewards host
(`https://rewards.madar-pos.cloud/api/auth/session`). Rewards calls it
same-origin; the dashboard calls it cross-origin with credentials. The cookie has
no `Path` attribute, so the browser scopes it to `/api/auth` there.

`https://madar-pos.cloud` and `https://rewards.madar-pos.cloud` are always
allowed (`session_cookie::FIRST_PARTY`), so production needs no `.env` change.
`MADAR_SESSION_ORIGINS` (else `CORS_ALLOWED_ORIGINS`) adds more origins; with
neither set only localhost is added. CORS sends `Access-Control-Allow-Credentials`,
which only `/auth/session` uses: no other route reads the cookie.

## Operations

- Rows that ended more than 30 days ago are pruned hourly.
- Revocations log at `auth.sessions.revoke` (who, what, reason). The row keeps
  `revoked_by` and `revoke_reason`.
- A revocation notifies `authz_user`, so every instance drops its cached answer
  at commit. Checks are served from memory (`fastpath`) while the listener is up,
  so there is no database read per request.
