-- Create the new org_payment_methods table
CREATE TABLE IF NOT EXISTS public.org_payment_methods (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id UUID NOT NULL REFERENCES public.organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    label_translations JSONB NOT NULL DEFAULT '{}'::jsonb,
    color TEXT NOT NULL,
    icon TEXT NOT NULL,
    is_cash BOOLEAN NOT NULL DEFAULT false,
    is_active BOOLEAN NOT NULL DEFAULT true,
    display_order INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(org_id, name)
);

-- Trigger for updated_at
DROP TRIGGER IF EXISTS trg_org_payment_methods_updated_at ON public.org_payment_methods;
CREATE TRIGGER trg_org_payment_methods_updated_at 
BEFORE UPDATE ON public.org_payment_methods 
FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();

-- Seed existing organizations with the default 6 payment methods
INSERT INTO public.org_payment_methods (org_id, name, label_translations, color, icon, is_cash, display_order)
SELECT 
    o.id,
    pm.name,
    pm.label_translations::jsonb,
    pm.color,
    pm.icon,
    pm.is_cash,
    pm.display_order
FROM public.organizations o
CROSS JOIN (
    VALUES 
        ('cash', '{"en": "Cash", "ar": "نقدي"}', 'emerald', 'payments_outlined', true, 1),
        ('card', '{"en": "Card", "ar": "بطاقة"}', 'blue', 'credit_card_rounded', false, 2),
        ('digital_wallet', '{"en": "Digital Wallet", "ar": "محفظة رقمية"}', 'purple', 'account_balance_wallet_rounded', false, 3),
        ('mixed', '{"en": "Mixed", "ar": "مختلط"}', 'amber', 'pie_chart_rounded', false, 4),
        ('talabat_online', '{"en": "Talabat Online", "ar": "طلبات أونلاين"}', 'orange', 'delivery_dining_rounded', false, 5),
        ('talabat_cash', '{"en": "Talabat Cash", "ar": "طلبات كاش"}', 'orange', 'delivery_dining_rounded', true, 6)
) AS pm(name, label_translations, color, icon, is_cash, display_order)
ON CONFLICT (org_id, name) DO NOTHING;

-- Alter existing columns that use the ENUM
ALTER TABLE public.orders 
    ALTER COLUMN payment_method TYPE TEXT USING payment_method::text;

ALTER TABLE public.order_payments 
    ALTER COLUMN method TYPE TEXT USING method::text;

-- Drop the ENUM type
DROP TYPE IF EXISTS public.payment_method;
