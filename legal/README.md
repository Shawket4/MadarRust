# Madar legal documents

Source for **https://legal.madar-pos.cloud**. Markdown in git → rendered to static HTML.

## ⚠️ Status: DRAFTS. Not reviewed by a lawyer.

These were drafted from the actual database schema and code, so the *factual* claims about
what data Madar collects and where it flows are accurate as of 2026-08-31. The *legal*
framing has not been reviewed by an Egyptian lawyer and must be before publication.

Specifically needing counsel:
- Whether Egypt's PDPL (Law 151/2020) executive regulations are in force, and therefore
  whether Data Protection Centre registration/licensing and formal DPO appointment are
  live obligations today.
- Cross-border transfer basis for Google (Gemini + Translate) — see `subprocessors.md`.
- Retention minimums under Egyptian tax and labour law (payroll records especially).
- Whether the controller/processor split described in `dpa.md` matches how contracts read.

## Why git

Regulators ask "what did your policy say in March?". Git answers that for free. Never
edit a published document in place without bumping its version and effective date, and
keep old versions reachable at a stable URL.

## Documents

| File | Audience | Public |
|---|---|---|
| `privacy-policy.md` | diners / end users | yes |
| `terms-of-service.md` | restaurants (customers) | yes |
| `dpa.md` | restaurants — signed | yes |
| `subprocessors.md` | restaurants | yes |
| `employee-privacy-notice.md` | restaurant staff using Dawam | yes |
| `delete-account.md` | end users — **required by Google Play** | yes |
| `data-retention.md` | internal + shared on request | yes |
| `breach-response.md` | internal runbook | **NO** |

## Arabic

Every public document needs an Arabic version. A notice Egyptian users cannot read is
weak notice. `en/` holds English; add `ar/` alongside. State which language governs.

## Deploy

Static site behind nginx on the VPS, same pattern as the other vhosts; certbot issues the
cert and `/etc/letsencrypt/renewal-hooks/deploy/reload-nginx.sh` reloads nginx on renewal.
No backend dependency — these URLs are cited in app store listings and must stay up even
when the API is down.
