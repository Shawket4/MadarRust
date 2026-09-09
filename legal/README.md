# Madar legal documents

Source for **https://legal.madar-pos.cloud**. Markdown in git → rendered to static HTML.

## ⚠️ Status: DRAFTS. Not reviewed by a lawyer.

These were drafted from the actual database schema and code, so the *factual* claims about
what data Madar collects and where it flows are accurate as of 2026-09-08. The *legal*
framing has not been reviewed by an Egyptian lawyer and must be before publication.

Specifically needing counsel:
- Whether Egypt's PDPL (Law 151/2020) executive regulations are in force, and therefore
  whether Data Protection Centre registration/licensing and formal DPO appointment are
  live obligations today.
- Cross-border transfer basis for Google (Gemini + Translate) — see `subprocessors.md`.
- Retention minimums under Egyptian tax and labour law (payroll records especially).
- Cross-border transfer basis for **Google Wallet**, which is materially different from the
  other Google entries: adding a loyalty card sends a named customer's identity and balance
  to Google and stores it there, where Gemini and Translate receive no customer data at all.
  Whether that needs its own consent step, rather than riding on the restaurant's, is a
  question for counsel — the policy currently describes it plainly and offers the customer a
  wallet-free alternative.
- Whether the controller/processor split described in `dpa.md` matches how contracts read.
- **The member's card page, whose only credential is the token in their barcode.** It now
  shows a purchase history — date, branch, total and the items on each order — so anyone
  holding the link, or a photograph of the barcode, can read it. That is a deliberate trade:
  the same token has to work from a tapped link in a message, with no password. Whether a
  bearer token is adequate protection for a purchase history under the PDPL, whether the
  page needs a warning at the point the link is shared, and whether a shop should be able
  to require the one-time code to open it, are all questions for counsel. The policy
  currently states the exposure plainly rather than taking a position on it.
- **Whether the birthday and "we've missed you" messages may run on an opt-out.** They are
  marketing, sent to a phone number given for a different purpose; today the switch is on
  the customer's card page and every message links to it, and there is no opt-in step at
  signup. Whether Egyptian law — and separately WhatsApp's own rules for business messaging
  — require prior consent rather than a way out is not something to decide by writing it
  down.
- **Whether message CONTENT reaching Google is covered by the wallet consent above.** A
  card added to Google Wallet already sends Google a name and a balance. Putting a greeting
  or a win-back nudge on that card sends Google the words of it too, which is a different
  kind of disclosure from a balance, and the customer consented to the card rather than to
  the messages. Apple is unaffected — its push is empty and the text never leaves us.

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
