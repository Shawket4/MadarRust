You have read access to the Sufrix backend codebase and database schema. Your job is to
produce an implementation plan for integrating Talabat as a fully operational channel, where
Sufrix becomes the control surface and the Talabat merchant dashboard is bypassed entirely.
Investigate the actual code — do not give generic advice. Cite real files, models, and paths.

=== TREAT THIS AS GIVEN (Talabat = Delivery Hero POS Middleware) ===

- Object model: Integration → Chain(s) → Vendor(s). A Vendor = one physical branch on Talabat,
  identified by a platform vendor code mapped to our own "remote id". A vendor belongs to exactly
  one chain and one integration.
- We build the "Plugin" (partner side): a webhook service Talabat calls, plus client calls we make.
- Endpoints we CALL: Login (token), Submit Catalog (PUT, full menu+price CRUD), Update Item
  Availability (86ing), Get items unavailability, Catalog Import Log, Update Vendor Availability
  (branch open/close), Get Vendor Availability, Update Order Status (accept/reject/picked up),
  Mark Order as Prepared, Order IDs + Order Detail (reconciliation).
- Webhooks we IMPLEMENT: Order Dispatch (incoming order), Order Status Update (cancellations),
  Status of Catalog Import, Menu Import Request.
- To bypass the dashboard: use DIRECT integration with auto-accept implemented in our plugin.
  Critical risk: in direct mode, if our plugin can't receive a dispatched order, Talabat
  auto-cancels it after retries — there is no Talabat-device fallback. Reliability is on us.
- Ops constraints: valid SSL webhook endpoint, whitelist Talabat's Middle East egress IPs, PGP
  credential exchange, mandatory 24h notice before maintenance on direct flow.
- Prices in the catalog payload are whatever we send, so channel/branch price overrides are ours
  to model. Reference pattern (Foodics): one master catalog + override layers — price-per-branch,
  price-tag-per-channel (branch-scopable), disable-product-per-branch — NOT duplicated menus.

=== TASKS ===

1. CURRENT STATE. Map how menus, categories, products, modifiers, prices, branches, and orders
   are modeled today. Specifically confirm or refute that menus are scoped one-per-org rather than
   per-branch. Quantify the blast radius of that assumption: which tables, foreign keys, services,
   API endpoints, and UI flows assume a single org-level menu? List them with file paths.

2. GAP ANALYSIS. Against the endpoint list above, enumerate what's missing to support: per-branch
   menu scoping, per-channel + per-branch price overrides, per-branch Talabat enablement (not all
   branches participate), item 86ing, branch open/close, order ingestion with auto-accept, and
   nightly reconciliation. For each gap, note whether it's a schema change, a service, or just config.

3. TARGET DATA MODEL. Propose entities and relationships that add Talabat as a channel WITHOUT
   duplicating menus — favor a master-catalog + scoped-override design (e.g. Channel,
   ChannelVendorMapping(branch↔vendor code, enabled flag), MenuPublication, PriceOverride keyed by
   (product, channel, branch), AvailabilityState). Show how final published price/availability
   resolves. Provide a concrete schema diff and a migration path that preserves existing
   single-menu orgs (backward compatible, reversible).

4. INTEGRATION ARCHITECTURE. Specify where the plugin/webhook service lives, token acquisition and
   refresh, request signing/IP allowlisting, idempotency keys for Order Dispatch, a retry +
   dead-letter strategy sized to the auto-cancel risk, the mapping between our internal order shape
   and Talabat's order payload, and the order status state machine (dispatched → accepted →
   prepared → picked up; plus cancellation handling).

5. SEQUENCING. Give a phased plan with the thinnest viable slice to pilot ONE branch end-to-end
   (menu publish → live order → auto-accept → status updates → reconciliation), then the path to
   general rollout. Rough effort estimate per phase.

6. SHORTCOMINGS & RISKS. Explicitly call out: the one-menu-per-org refactor cost and any data
   currently impossible to express; direct-mode auto-cancel exposure and required uptime/alerting;
   price drift between channels/branches; partial-branch rollout edge cases; multi-country and
   multi-currency handling; catalog import being async (eventual consistency); rate limits.

=== OUTPUT ===
A written plan with: (a) real file/path references for every current-state claim, (b) a proposed
schema diff, (c) an endpoint-to-feature mapping table, (d) an ordered task list with effort
estimates and dependencies. Investigate first; only ask clarifying questions if genuinely blocked,
otherwise state your assumptions inline.

=== CONSTRAINTS ===
Read-only investigation. Propose changes; do not apply migrations or edit schema. Flag anything
that needs a product decision rather than an engineering one.
