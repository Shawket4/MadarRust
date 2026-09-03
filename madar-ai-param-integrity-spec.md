# Task: Parameter Integrity for the Madar AI Analytics Module

## Context

Madar is a multi-tenant, shared-schema POS system. The Rust backend serves a React
dashboard, Flutter apps, and a Slint KDS. The dashboard has an AI analytics chat
using a **fixed tool-calling architecture** — the model selects from a closed set of
typed tools and never generates SQL.

The model and provider are set by environment flags and may change. Nothing in this
spec should depend on a particular model's behaviour: the guarantees below are
enforced server-side in the dispatcher, not by prompt tuning against one model's
quirks. If a mechanism only works because a specific model happens to behave a
certain way, it isn't done.

This removes fabricated *data*, but not fabricated *arguments*. The model can still
pick the wrong tool, pass a branch that doesn't exist, invent a date range, or
mischaracterize a result in its summary sentence.

**Goal of this task:** make it structurally impossible for the model to emit an
argument that isn't either (a) resolved server-side, or (b) verified against a value
the model was previously shown.

## Ground rules

- Orders carry a real date and time. "Yesterday" means the calendar day before today
  in the tenant's timezone, `00:00:00` to `23:59:59.999`. There is no shift-close
  offset, no business-day concept, and no period that spans midnight. Do not
  introduce one.
- Do not change the fixed tool-calling architecture. No dynamic SQL generation.
- All DB access stays parameterized (sqlx). RLS remains the security boundary;
  nothing here replaces it.
- Bilingual: every user-facing string and every resolver path must handle Arabic and
  English input.

## How to read the numbers in this spec

You have the codebase; I don't. Any specific figure below — item counts, score
thresholds, tool counts, case counts, range limits — is a starting point written
without sight of the actual schema, tenant sizes, or current tool list. Treat them as
defaults to be replaced by what the code and data actually justify, and say in your
report where you diverged and why. Prefer making them runtime-configurable over
hardcoding whatever you pick.

What is **not** negotiable, because these are the guarantees the whole task exists
to establish:

- the ID-provenance check (2c) — a hard dispatcher check, never prompt guidance
- no silent defaults for a missing required period, and no fallthrough to a guessed
  query after retries are exhausted
- the retry budget is finite and small; the exact number is yours
- calendar-day period semantics in the tenant timezone, half-open `[start, end)`
- provenance display renders resolved values, never the model's raw arguments
- deterministic sampling for tool selection

Everything else is yours to size.

---

## Work item 1 — Server-side resolution of derivable arguments

The model must not emit any value the server can determine on its own.

### 1a. Date periods

Replace free-form date arguments with an enum. Schema for every analytics tool that
takes a time range:

```json
{
  "period": {
    "type": "string",
    "enum": ["today", "yesterday", "this_week", "last_week",
             "this_month", "last_month", "last_7_days", "last_30_days",
             "this_year", "custom"],
    "description": "Use 'custom' ONLY when the user stated explicit calendar dates."
  },
  "start_date": {
    "type": "string",
    "format": "date",
    "description": "Required only when period='custom'. YYYY-MM-DD."
  },
  "end_date": {
    "type": "string",
    "format": "date",
    "description": "Required only when period='custom'. YYYY-MM-DD."
  }
}
```

Resolve the enum in Rust against the tenant's IANA timezone. Produce a half-open
`[start, end)` interval in UTC for the query. Week start is a tenant setting
(default Saturday for Egypt — confirm against the existing tenant config; if no
such setting exists, add one rather than hardcoding).

Validation for `custom`:

- both dates present, parseable, `start <= end`
- `end` not in the future (clamp to now, don't error)
- cap the range length at something the query planner can serve without degrading —
  size this against actual row counts and the indexes present, and make it
  configurable per tenant tier if the data justifies it

Emit the resolved concrete range on the tool result so it can be displayed.

### 1b. Tenant and identity

`tenant_id`, `user_id`, and role scope come from the JWT and are injected by the
dispatcher. Remove them from every tool schema if present. If a model-supplied
argument collides with an injected one, drop the model's value and log a warning
with the conversation id.

### 1c. Ambient context

Inject into the system prompt on every turn:

- current date and time in the tenant's timezone, plus the timezone name
- today's date as `YYYY-MM-DD` and its weekday name
- earliest date for which the tenant has order data
- tenant currency and decimal precision

---

## Work item 2 — Two-phase entity resolution

The model never emits an entity ID it wasn't shown, and never emits free text that
reaches a filter.

### 2a. Closed-set injection (small sets)

Where a set is small enough to inject wholesale, put the full list in the system
prompt each turn as `id: name` pairs (include the Arabic name where one exists). The
model then selects rather than generates, which is the cheapest possible fix.

Decide the cutoff from the real distribution: look at actual per-tenant counts for
branches, categories, payment methods, order types, and employees, and set the
threshold against your prompt token budget rather than a round number. It's fine for
the cutoff to differ per entity kind. Where a tenant exceeds it, fall back to 2b for
that kind only — the mechanism should switch per entity kind per tenant, not
globally.

### 2b. Resolver tool (large sets)

For products, SKUs, customers, and employees on large tenants, add:

```
resolve_entity(kind: EntityKind, text: String, limit: u8)
  -> [{ id, name, name_ar, score, disambiguator }]
```

`disambiguator` is a short contextual string (e.g. category for a product, branch for
an employee) so the model and the user can tell near-duplicates apart.

Matching must normalize for Arabic before scoring:

- strip and normalize the definite article `ال`
- fold `أ إ آ` → `ا`, `ة` → `ه`, `ى` → `ي`, `ؤ ئ` → `ء` handling
- strip tashkeel and tatweel
- normalize Arabic-Indic digits `٠١٢٣٤٥٦٧٨٩` to ASCII
- handle Franco-Arabic (Latin-script Arabic, e.g. "Zamalek" / "زمالك", "7abiba")
- case-fold and collapse whitespace for Latin

Use trigram similarity (`pg_trgm`) or an equivalent; expose the score.

### 2c. ID provenance enforcement

The dispatcher maintains, per conversation, the set of entity IDs that have appeared
in a tool **result** or in the injected closed sets. Any ID in an incoming tool call
that is not in that set is rejected before the query runs. This is the core
structural guarantee — implement it as a hard check in the dispatcher, not as
prompt guidance.

### 2d. Ambiguity handling

When the top candidates are too close to separate, or the best match is too weak to
trust, do not resolve. Return a `needs_disambiguation` result listing the candidates.
The model must ask the user; it must not pick.

Tune the margin and floor empirically against the real catalog — generate match
scores for a sample of realistic queries against actual product and branch names and
pick thresholds from that distribution. Arabic normalization compresses the score
range, so values tuned on Latin text will be wrong. Report the values you chose and
the data you chose them from, and make both runtime-configurable.

---

## Work item 3 — Strict validation with a bounded retry loop

### 3a. Deserialization

Deserialize every tool call into a typed struct:

- `#[serde(deny_unknown_fields)]` on all argument structs
- real Rust enums for all enum-typed params (no `String`)
- `Option<T>` only where the param is genuinely optional; no silent defaults
- **no default to "all time"** — a missing required period is an error

### 3b. Structured errors back to the model

On validation failure, return a tool *result* (not a transport error) that the model
can act on:

```json
{
  "error": "unknown_branch",
  "message": "No branch matching 'Zamalek' for this tenant.",
  "valid_options": ["Zamalek Club", "Zamalek Corniche"],
  "hint": "Call resolve_entity or ask the user which branch they mean."
}
```

Error codes to implement at minimum: `unknown_entity`, `unresolved_id`,
`invalid_period`, `invalid_date_range`, `range_too_large`, `missing_required_param`,
`unknown_field`, `needs_disambiguation`, `no_data_in_range`.

### 3c. Retry budget

The budget must be finite and small — pick the number from what the structured errors
actually recover. If a retry class never succeeds on the second attempt, it shouldn't
get one; if a class reliably self-corrects, it may warrant a different allowance than
the rest. Per-error-code budgets are acceptable if the eval data supports them.

On exhaustion, return a plain-language message to the user stating what was ambiguous
and what they could specify instead. Never fall through to a guessed query. Log every
retry with the conversation id, the rejected arguments, and the error code — you'll
need this log to set the number in the first place.

---

## Work item 4 — Narrow the tool surface

Wrong-tool selection produces more wrong answers than wrong arguments, and the rate
grows with the number of overlapping tools.

Audit the current tool list and merge overlapping tools behind a `group_by` enum:

```
get_sales(period, group_by: [branch|product|category|day|hour|payment_method|order_type],
          branch_ids?, limit?, sort?)
```

The `get_sales` shape above is illustrative, not prescriptive — you can see the actual
tools and the actual query surface, so derive the right decomposition from those.
Merging is not automatically correct either: a single tool with a dozen conditional
parameters can be harder for the model to call correctly than two clean tools. Split
where the argument sets genuinely diverge.

The criterion is not a tool count. It's that no two descriptions could plausibly
answer the same user question, and that every tool's arguments are meaningful for
every valid combination of its other arguments. If you can write a question where two
tools both look right, they're not separated yet. Report the final list with the
rationale for each merge or split.

Write each description to state explicitly what the tool does *not* cover and which
tool to use instead.

Set the tool-selection call to deterministic sampling (temperature 0, or the
configured provider's equivalent). Where a provider doesn't expose it, note that in
the report — it affects how much the eval variance means.

---

## Work item 5 — Provenance display in the dashboard

Every answer in the chat shows what was actually executed, in a collapsible line
beneath the response:

```
queried: sales by branch · 2026-08-01 → 2026-08-31 · Casa di Qasa · EGP
```

Requirements:

- render the **resolved** values (concrete dates, resolved entity names), never the
  model's raw arguments
- show every tool call when a turn made more than one
- bilingual, following the dashboard's current locale
- style per the Madar design system (Ink `#14181E`, Paper `#EFF3F4`, Slate `#76828B`,
  Teal `#0D6273`/`#2E94A6`; Cairo + IBM Plex Mono — use the mono face for the
  provenance line)

---

## Work item 6 — Golden eval set

A script, run on every prompt or schema change, that measures argument accuracy.

- size the set by coverage, not by a target count: every tool, every `group_by`
  variant, every period enum value, every error code in 3b, and every resolver path
  should appear in at least one case, with the common ones represented more heavily.
  Report the resulting count and the coverage matrix rather than aiming at a number.
- roughly half Arabic
- Egyptian colloquial phrasing, not MSA translations of English questions. Examples:
  «مبيعات امبارح», «كام عملنا الشهر اللي فات», «ايه أكتر صنف اتباع في الفرع الجديد»,
  «قارنلي الفرعين الشهر ده», «مبيعات الويك اند»
- each case fixes: input text, frozen `now`, tenant fixture, expected tool name,
  expected **resolved** arguments (post-resolution, not the model's raw output)
- include negative cases: nonexistent branch, ambiguous product, future date range,
  a period the tenant has no data for, a question no tool can answer
- output a report with per-category accuracy: tool selection, period resolution,
  entity resolution, overall
- exit non-zero below a configurable threshold so it can gate CI

Make the eval runner deterministic and re-runnable from a single command; store cases
as data files (YAML or JSON), not as code.

### 6a. Generate the cases yourself

Write the full case set. Derive the expected tool names, parameter names, enum
values, and entity IDs directly from the Rust tool definitions, the OpenAPI JSON, and
the tenant fixture schema — not from inference about what they probably are. If a
case can't be grounded in an actual schema value, drop it rather than guessing.

Build the tenant fixture first: a seeded, deterministic tenant with branches
(including two with confusably similar names, one Arabic-only), a product catalog
with near-duplicate names, a fixed `now`, and enough order history to exercise
year-over-year comparison, empty ranges, and a branch that opened partway through
the span. Mirror the real data's shape where you can see it — the fixture is only
useful if its cardinalities and name collisions resemble a live tenant.

### 6b. Mark your confidence per case

The schema-derived half of each case is verifiable. The intent half — what a given
Arabic phrase *should* resolve to — is not, and it's the half the eval exists to
test. Tag every case:

- `confidence: high` — expected values follow mechanically from the schema and a
  literal reading of the input
- `confidence: review` — you made a judgement call about user intent

Put anything in this list under `review`:

- «الويك اند» / "the weekend" — Fri–Sat, Sat–Sun, or Fri–Sun for an Egyptian F&B
  tenant is a business decision, not a code fact
- «الفرع الجديد» / "the new branch" — most recently opened, or should this be
  `needs_disambiguation`?
- «أحسن» / «أكتر» — best by revenue, by quantity, or by margin
- «الشهر» used mid-month — month-to-date or last complete month
- anything where the correct behaviour is to ask the user rather than answer

Write the review-tagged cases into a separate file and list them in your report with
your reasoning, so they can be confirmed or corrected in one pass instead of a review
of the whole set.

### 6c. Adversarial cases

Generate a subset specifically designed to defeat the mechanisms in work items 1–3
rather than to exercise them: a branch name that is also a product name, a date
written in Arabic-Indic digits, a question mixing Arabic and English mid-sentence, a
question referencing an entity from an earlier turn by pronoun only, and a request
for a metric no tool exposes. These are the cases most likely to reveal a gap.

---

## Deliverables

1. Schema and dispatcher changes for work items 1–4.
2. Frontend provenance component for work item 5.
3. Eval harness, case files, and a baseline report for work item 6.
4. A short migration note covering any tool-schema breaking changes and their effect
   on existing conversation history.

## Report back with

- every threshold you set — closed-set cutoffs, similarity margin and floor, retry
  budget, max range, eval case count — with the data you sized it from
- where you diverged from this spec's defaults or structure, and why
- current accuracy per category from the baseline eval run
- the final tool list, before and after consolidation, with rationale for each merge
  or split
- any place where the ID-provenance rule (2c) couldn't be enforced, and why
- anything in the existing implementation that contradicts this spec, flagged rather
  than silently changed
