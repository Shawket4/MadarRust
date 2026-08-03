-- Per-payment-method visibility in the partner analytics API.
--
-- Merchants share order analytics with third parties (see src/integrations),
-- but not every tender belongs in that feed — an aggregator's own orders, or
-- any channel the partner has no business seeing. This flag decides, per
-- payment method, whether the orders it paid for reach the API at all.
--
-- DEFAULT true is deliberate: the analytics endpoint is already live and its
-- schema has been handed to a partner, so an additive column must not silently
-- empty the feed. Merchants opt individual methods OUT.
--
-- Filtering happens on `order_payments` (what was actually tendered), not on
-- `orders.payment_method`, which holds the literal 'mixed' for split orders.
-- An order is hidden if ANY of its legs used a hidden method: a partly-hidden
-- order still carries that method's money inside its total, so including it
-- would leak exactly what this flag exists to conceal.

ALTER TABLE public.org_payment_methods
    ADD COLUMN visible_in_integrations boolean NOT NULL DEFAULT true;

COMMENT ON COLUMN public.org_payment_methods.visible_in_integrations IS
    'When false, orders tendered with this method are excluded entirely from '
    'GET /integrations/analytics/orders — both the rows and the aggregates.';
