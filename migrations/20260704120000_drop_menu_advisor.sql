-- Menu Advisor: ripped out (2026-07-04). The advisor's run/suggestion/decision
-- persistence goes with it — these tables were only ever written and read by
-- the retired /menu-advisor endpoints (children reference menu_advisor_runs,
-- nothing outside the family references in). Menu engineering had no tables
-- (computed per-request from order history) — its endpoint is retired in the
-- same release. Their replacements will be designed fresh.
DROP TABLE IF EXISTS menu_advisor_decisions;
DROP TABLE IF EXISTS menu_advisor_price_suggestions;
DROP TABLE IF EXISTS menu_advisor_bundle_suggestions;
DROP TABLE IF EXISTS menu_advisor_removal_scenarios;
DROP TABLE IF EXISTS menu_advisor_runs;
