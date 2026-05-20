--
-- Add Bundles feature schema migrations
--

CREATE TYPE public.bundle_status AS ENUM (
    'draft',
    'active',
    'archived'
);

ALTER TYPE public.bundle_status OWNER TO rue;

-- ── Bundles table ──
CREATE TABLE public.bundles (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    org_id uuid NOT NULL REFERENCES public.organizations(id) ON DELETE CASCADE,
    name text NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    description text,
    description_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    price integer NOT NULL CHECK (price >= 0),
    status public.bundle_status DEFAULT 'draft'::public.bundle_status NOT NULL,
    image_url text,
    display_order integer DEFAULT 0 NOT NULL,
    available_from_time time without time zone,
    available_until_time time without time zone,
    available_from_date date,
    available_until_date date,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by uuid REFERENCES public.users(id) ON DELETE SET NULL
);

ALTER TABLE public.bundles OWNER TO rue;

CREATE TRIGGER trg_bundles_updated_at BEFORE UPDATE ON public.bundles FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();

CREATE INDEX idx_bundles_org ON public.bundles USING btree (org_id);
CREATE INDEX idx_bundles_org_status ON public.bundles USING btree (org_id, status);

-- ── Bundle Components table ──
CREATE TABLE public.bundle_components (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    bundle_id uuid NOT NULL REFERENCES public.bundles(id) ON DELETE CASCADE,
    item_id uuid NOT NULL REFERENCES public.menu_items(id) ON DELETE RESTRICT,
    quantity integer DEFAULT 1 NOT NULL CHECK (quantity > 0),
    position integer DEFAULT 0 NOT NULL,
    CONSTRAINT bundle_components_bundle_id_item_id_key UNIQUE (bundle_id, item_id)
);

ALTER TABLE public.bundle_components OWNER TO rue;

CREATE INDEX idx_bundle_components_bundle ON public.bundle_components USING btree (bundle_id);

-- ── Bundle Branch Availability table ──
CREATE TABLE public.bundle_branch_availability (
    bundle_id uuid NOT NULL REFERENCES public.bundles(id) ON DELETE CASCADE,
    branch_id uuid NOT NULL REFERENCES public.branches(id) ON DELETE CASCADE,
    PRIMARY KEY (bundle_id, branch_id)
);

ALTER TABLE public.bundle_branch_availability OWNER TO rue;

-- ── Order Items additions & modifications ──
ALTER TABLE public.order_items ADD COLUMN bundle_id uuid REFERENCES public.bundles(id) ON DELETE SET NULL;
ALTER TABLE public.order_items ADD COLUMN bundle_unit_price integer;
ALTER TABLE public.order_items ALTER COLUMN menu_item_id DROP NOT NULL;

-- ── Order Line Bundle Components table ──
CREATE TABLE public.order_line_bundle_components (
    order_line_id uuid NOT NULL REFERENCES public.order_items(id) ON DELETE CASCADE,
    item_id uuid NOT NULL REFERENCES public.menu_items(id) ON DELETE RESTRICT,
    quantity integer DEFAULT 1 NOT NULL CHECK (quantity > 0),
    size_label text,
    PRIMARY KEY (order_line_id, item_id)
);

ALTER TABLE public.order_line_bundle_components OWNER TO rue;

CREATE TABLE public.order_line_bundle_component_addons (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    order_line_id uuid NOT NULL REFERENCES public.order_items(id) ON DELETE CASCADE,
    component_item_id uuid NOT NULL REFERENCES public.menu_items(id) ON DELETE RESTRICT,
    addon_item_id uuid NOT NULL,
    addon_name text NOT NULL,
    unit_price integer NOT NULL,
    quantity integer DEFAULT 1 NOT NULL,
    line_total integer NOT NULL
);

ALTER TABLE public.order_line_bundle_component_addons OWNER TO rue;

CREATE INDEX idx_ol_bundle_comp_addons_line
    ON public.order_line_bundle_component_addons (order_line_id);

CREATE TABLE public.order_line_bundle_component_optionals (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    order_line_id uuid NOT NULL REFERENCES public.order_items(id) ON DELETE CASCADE,
    component_item_id uuid NOT NULL REFERENCES public.menu_items(id) ON DELETE RESTRICT,
    optional_field_id uuid,
    field_name text NOT NULL,
    price integer DEFAULT 0 NOT NULL,
    org_ingredient_id uuid,
    ingredient_name text,
    ingredient_unit text,
    quantity_deducted numeric(12,3)
);

ALTER TABLE public.order_line_bundle_component_optionals OWNER TO rue;

CREATE INDEX idx_ol_bundle_comp_optionals_line
    ON public.order_line_bundle_component_optionals (order_line_id);
