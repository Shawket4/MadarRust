--
-- PostgreSQL database dump
--

-- Dumped from database version 17.10 (Homebrew)
-- Dumped by pg_dump version 17.5

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: public; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA public;


--
-- Name: SCHEMA public; Type: COMMENT; Schema: -; Owner: -
--

COMMENT ON SCHEMA public IS 'standard public schema';


--
-- Name: bundle_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.bundle_status AS ENUM (
    'draft',
    'active',
    'archived'
);


--
-- Name: discount_type; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.discount_type AS ENUM (
    'percentage',
    'fixed'
);


--
-- Name: inventory_adjustment_type; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.inventory_adjustment_type AS ENUM (
    'add',
    'remove',
    'transfer_out',
    'transfer_in'
);


--
-- Name: inventory_movement_type; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.inventory_movement_type AS ENUM (
    'sale',
    'void_restock',
    'adjustment_add',
    'adjustment_remove',
    'waste',
    'transfer_out',
    'transfer_in',
    'purchase_in',
    'stock_count'
);


--
-- Name: inventory_unit; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.inventory_unit AS ENUM (
    'g',
    'kg',
    'ml',
    'l',
    'pcs'
);


--
-- Name: item_size; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.item_size AS ENUM (
    'small',
    'medium',
    'large',
    'extra_large',
    'one_size'
);


--
-- Name: order_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.order_status AS ENUM (
    'pending',
    'preparing',
    'ready',
    'completed',
    'voided',
    'refunded'
);


--
-- Name: permission_action; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.permission_action AS ENUM (
    'create',
    'read',
    'update',
    'delete'
);


--
-- Name: permission_resource; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.permission_resource AS ENUM (
    'orgs',
    'branches',
    'users',
    'categories',
    'menu_items',
    'addon_groups',
    'addon_items',
    'recipes',
    'inventory',
    'inventory_adjustments',
    'inventory_transfers',
    'orders',
    'order_items',
    'payments',
    'payment_methods',
    'shifts',
    'shift_counts',
    'soft_serve_batches',
    'discounts',
    'reports',
    'permissions',
    'stocktakes',
    'inventory_waste',
    'suppliers',
    'purchase_orders'
);


--
-- Name: printer_brand; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.printer_brand AS ENUM (
    'star',
    'epson'
);


--
-- Name: purchase_order_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.purchase_order_status AS ENUM (
    'draft',
    'ordered',
    'received',
    'cancelled',
    'partially_received'
);


--
-- Name: shift_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.shift_status AS ENUM (
    'open',
    'closed',
    'force_closed'
);


--
-- Name: stocktake_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.stocktake_status AS ENUM (
    'draft',
    'in_progress',
    'finalized',
    'cancelled'
);


--
-- Name: stocktake_variance_reason; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.stocktake_variance_reason AS ENUM (
    'theft',
    'spoilage',
    'breakage',
    'miscount',
    'supplier_short',
    'transfer_error',
    'other'
);


--
-- Name: user_role; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.user_role AS ENUM (
    'super_admin',
    'org_admin',
    'branch_manager',
    'teller'
);


--
-- Name: void_reason; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.void_reason AS ENUM (
    'customer_request',
    'wrong_order',
    'quality_issue',
    'other'
);


--
-- Name: set_updated_at(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.set_updated_at() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: _sqlx_migrations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public._sqlx_migrations (
    version bigint NOT NULL,
    description text NOT NULL,
    installed_on timestamp with time zone DEFAULT now() NOT NULL,
    success boolean NOT NULL,
    checksum bytea NOT NULL,
    execution_time bigint NOT NULL
);


--
-- Name: addon_item_ingredients; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.addon_item_ingredients (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    addon_item_id uuid NOT NULL,
    quantity_used numeric(12,3) NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    ingredient_name text NOT NULL,
    ingredient_unit text NOT NULL,
    org_ingredient_id uuid
);


--
-- Name: addon_items; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.addon_items (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    type text NOT NULL,
    default_price integer DEFAULT 0 NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: branch_inventory; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.branch_inventory (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    org_ingredient_id uuid NOT NULL,
    current_stock numeric(12,3) DEFAULT 0 NOT NULL,
    reorder_threshold numeric(12,3) DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: branch_inventory_adjustments; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.branch_inventory_adjustments (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    branch_inventory_id uuid NOT NULL,
    type public.inventory_adjustment_type NOT NULL,
    quantity numeric(12,3) NOT NULL,
    note text NOT NULL,
    transfer_id uuid,
    adjusted_by uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: branch_inventory_transfers; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.branch_inventory_transfers (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    source_branch_id uuid NOT NULL,
    destination_branch_id uuid NOT NULL,
    org_ingredient_id uuid NOT NULL,
    quantity numeric(12,3) NOT NULL,
    note text,
    initiated_by uuid NOT NULL,
    initiated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT chk_transfer_branches CHECK ((source_branch_id <> destination_branch_id))
);


--
-- Name: branches; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.branches (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    address text,
    phone text,
    timezone text DEFAULT 'Africa/Cairo'::text NOT NULL,
    printer_ip inet,
    printer_port integer DEFAULT 9100,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    printer_brand public.printer_brand,
    latitude double precision,
    longitude double precision,
    geo_radius_meters integer DEFAULT 200
);


--
-- Name: bundle_branch_availability; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.bundle_branch_availability (
    bundle_id uuid NOT NULL,
    branch_id uuid NOT NULL
);


--
-- Name: bundle_components; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.bundle_components (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    bundle_id uuid NOT NULL,
    item_id uuid NOT NULL,
    quantity integer DEFAULT 1 NOT NULL,
    "position" integer DEFAULT 0 NOT NULL,
    CONSTRAINT bundle_components_quantity_check CHECK ((quantity > 0))
);


--
-- Name: bundle_price_epochs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.bundle_price_epochs (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    bundle_id uuid NOT NULL,
    price integer NOT NULL,
    effective_from timestamp with time zone NOT NULL,
    effective_until timestamp with time zone,
    changed_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT chk_bundle_epoch_dates CHECK (((effective_until IS NULL) OR (effective_until > effective_from)))
);


--
-- Name: bundles; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.bundles (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    description text,
    description_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    price integer NOT NULL,
    status public.bundle_status DEFAULT 'draft'::public.bundle_status NOT NULL,
    image_url text,
    available_from_time time without time zone,
    available_until_time time without time zone,
    available_from_date date,
    available_until_date date,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by uuid,
    CONSTRAINT bundles_price_check CHECK ((price >= 0))
);


--
-- Name: categories; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.categories (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    image_url text,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: discounts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.discounts (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    type public.discount_type NOT NULL,
    value integer NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: ingredient_cost_history; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.ingredient_cost_history (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_ingredient_id uuid NOT NULL,
    cost_per_unit numeric(15,2) NOT NULL,
    effective_from timestamp with time zone NOT NULL,
    effective_until timestamp with time zone,
    changed_by uuid,
    note text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: inventory_movements; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.inventory_movements (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    org_ingredient_id uuid NOT NULL,
    branch_inventory_id uuid,
    type public.inventory_movement_type NOT NULL,
    quantity numeric(12,3) NOT NULL,
    balance_after numeric(12,3),
    unit_cost bigint,
    reason text,
    below_zero boolean DEFAULT false NOT NULL,
    source_type text,
    source_id uuid,
    note text,
    created_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: item_sizes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.item_sizes (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    menu_item_id uuid NOT NULL,
    label public.item_size NOT NULL,
    price_override integer NOT NULL,
    is_active boolean DEFAULT true NOT NULL
);


--
-- Name: menu_advisor_bundle_suggestions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_advisor_bundle_suggestions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    run_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    payload jsonb NOT NULL,
    promoted_bundle_id uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    focus_menu_item_id uuid GENERATED ALWAYS AS (((payload #>> '{focus_item,menu_item_id}'::text[]))::uuid) STORED NOT NULL,
    focus_size_label text GENERATED ALWAYS AS ((payload #>> '{focus_item,size_label}'::text[])) STORED NOT NULL,
    missing_costs boolean GENERATED ALWAYS AS (((payload ->> 'missing_costs'::text))::boolean) STORED NOT NULL,
    bundle_cm bigint GENERATED ALWAYS AS (((payload ->> 'bundle_cm'::text))::bigint) STORED,
    bundle_discount_pct double precision GENERATED ALWAYS AS (((payload ->> 'bundle_discount_pct'::text))::double precision) STORED NOT NULL
);


--
-- Name: menu_advisor_decisions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_advisor_decisions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    suggestion_id uuid NOT NULL,
    suggestion_kind text NOT NULL,
    branch_id uuid NOT NULL,
    decision text NOT NULL,
    notes text,
    decided_by uuid NOT NULL,
    decided_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT menu_advisor_decisions_decision_check CHECK ((decision = ANY (ARRAY['accepted'::text, 'rejected'::text, 'ignored'::text]))),
    CONSTRAINT menu_advisor_decisions_suggestion_kind_check CHECK ((suggestion_kind = ANY (ARRAY['price'::text, 'bundle'::text, 'removal'::text])))
);


--
-- Name: menu_advisor_price_suggestions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_advisor_price_suggestions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    run_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    category_id uuid,
    payload jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    menu_item_id uuid GENERATED ALWAYS AS (((payload #>> '{key,menu_item_id}'::text[]))::uuid) STORED NOT NULL,
    size_label text GENERATED ALWAYS AS ((payload #>> '{key,size_label}'::text[])) STORED NOT NULL,
    item_name text GENERATED ALWAYS AS ((payload ->> 'item_name'::text)) STORED NOT NULL,
    classification_mode text GENERATED ALWAYS AS ((payload #>> '{classification,mode}'::text[])) STORED NOT NULL,
    cm_quadrant text GENERATED ALWAYS AS ((payload #>> '{classification,quadrant}'::text[])) STORED,
    revenue_class text GENERATED ALWAYS AS ((payload #>> '{classification,class}'::text[])) STORED,
    action text GENERATED ALWAYS AS ((payload ->> 'action'::text)) STORED NOT NULL,
    confidence text GENERATED ALWAYS AS ((payload ->> 'confidence'::text)) STORED NOT NULL,
    popularity_share double precision GENERATED ALWAYS AS (((payload ->> 'popularity_share'::text))::double precision) STORED NOT NULL,
    current_price bigint GENERATED ALWAYS AS (((payload ->> 'current_price'::text))::bigint) STORED NOT NULL,
    suggested_price bigint GENERATED ALWAYS AS (((payload ->> 'suggested_price'::text))::bigint) STORED,
    suggested_delta_pct double precision GENERATED ALWAYS AS (((payload ->> 'suggested_delta_pct'::text))::double precision) STORED,
    CONSTRAINT menu_advisor_price_suggestions_action_check CHECK ((action = ANY (ARRAY['hold'::text, 'raise_price'::text, 'lower_price'::text, 'bundle'::text, 'remove'::text, 'reformulate'::text, 'monitor'::text]))),
    CONSTRAINT menu_advisor_price_suggestions_classification_mode_check CHECK ((classification_mode = ANY (ARRAY['cm'::text, 'revenue'::text, 'insufficient'::text]))),
    CONSTRAINT menu_advisor_price_suggestions_confidence_check CHECK ((confidence = ANY (ARRAY['low'::text, 'medium'::text, 'high'::text])))
);


--
-- Name: menu_advisor_removal_scenarios; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_advisor_removal_scenarios (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    run_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    menu_item_id uuid GENERATED ALWAYS AS (((payload #>> '{key,menu_item_id}'::text[]))::uuid) STORED NOT NULL,
    size_label text GENERATED ALWAYS AS ((payload #>> '{key,size_label}'::text[])) STORED NOT NULL,
    recommendation text GENERATED ALWAYS AS ((payload ->> 'recommendation'::text)) STORED NOT NULL,
    net_cm_change double precision GENERATED ALWAYS AS (((payload ->> 'net_cm_change'::text))::double precision) STORED NOT NULL,
    CONSTRAINT menu_advisor_removal_scenarios_recommendation_check CHECK ((recommendation = ANY (ARRAY['remove'::text, 'keep_and_bundle'::text, 'keep_and_reformulate'::text, 'no_strong_signal'::text])))
);


--
-- Name: menu_advisor_runs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_advisor_runs (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    org_id uuid NOT NULL,
    status text DEFAULT 'in_progress'::text NOT NULL,
    config jsonb NOT NULL,
    error_message text,
    items_total integer DEFAULT 0 NOT NULL,
    items_cm_tracked integer DEFAULT 0 NOT NULL,
    items_revenue_only integer DEFAULT 0 NOT NULL,
    items_insufficient integer DEFAULT 0 NOT NULL,
    window_days double precision NOT NULL,
    started_at timestamp with time zone DEFAULT now() NOT NULL,
    completed_at timestamp with time zone,
    CONSTRAINT menu_advisor_runs_status_check CHECK ((status = ANY (ARRAY['in_progress'::text, 'completed'::text, 'failed'::text])))
);


--
-- Name: menu_item_addon_slots; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_item_addon_slots (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    menu_item_id uuid NOT NULL,
    addon_type text NOT NULL,
    is_required boolean DEFAULT false NOT NULL,
    min_selections integer DEFAULT 0 NOT NULL,
    max_selections integer,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    label text,
    label_translations jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: menu_item_optional_fields; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_item_optional_fields (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    menu_item_id uuid NOT NULL,
    name text NOT NULL,
    price integer DEFAULT 0 NOT NULL,
    org_ingredient_id uuid,
    ingredient_name text,
    ingredient_unit text,
    quantity_used numeric(12,3),
    size_label public.item_size,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT chk_optional_ingredient CHECK ((((org_ingredient_id IS NULL) AND (ingredient_name IS NULL) AND (ingredient_unit IS NULL) AND (quantity_used IS NULL)) OR ((ingredient_name IS NOT NULL) AND (ingredient_unit IS NOT NULL) AND (quantity_used IS NOT NULL))))
);


--
-- Name: menu_item_price_epochs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_item_price_epochs (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    menu_item_id uuid NOT NULL,
    size_label text,
    price integer NOT NULL,
    effective_from timestamp with time zone NOT NULL,
    effective_until timestamp with time zone,
    changed_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT chk_price_epoch_dates CHECK (((effective_until IS NULL) OR (effective_until > effective_from)))
);


--
-- Name: menu_item_recipes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_item_recipes (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    menu_item_id uuid NOT NULL,
    size_label public.item_size NOT NULL,
    quantity_used numeric(12,3) NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    ingredient_name text NOT NULL,
    ingredient_unit text NOT NULL,
    org_ingredient_id uuid
);


--
-- Name: menu_items; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_items (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    category_id uuid,
    name text NOT NULL,
    description text,
    image_url text,
    base_price integer DEFAULT 0 NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    description_translations jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: order_item_addons; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.order_item_addons (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    order_item_id uuid NOT NULL,
    addon_item_id uuid NOT NULL,
    addon_name text NOT NULL,
    unit_price integer NOT NULL,
    quantity integer DEFAULT 1 NOT NULL,
    line_total integer NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    line_cost bigint
);


--
-- Name: order_item_optionals; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.order_item_optionals (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    order_item_id uuid NOT NULL,
    optional_field_id uuid,
    field_name text NOT NULL,
    price integer DEFAULT 0 NOT NULL,
    org_ingredient_id uuid,
    ingredient_name text,
    ingredient_unit text,
    quantity_deducted numeric(12,3),
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    cost bigint
);


--
-- Name: order_items; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.order_items (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    order_id uuid NOT NULL,
    menu_item_id uuid,
    item_name text NOT NULL,
    size_label text,
    unit_price integer NOT NULL,
    quantity integer DEFAULT 1 NOT NULL,
    line_total integer NOT NULL,
    notes text,
    deductions_snapshot jsonb DEFAULT '[]'::jsonb NOT NULL,
    bundle_id uuid,
    bundle_unit_price integer,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    line_cost bigint,
    unit_cost bigint,
    cost_missing boolean DEFAULT true NOT NULL
);


--
-- Name: order_line_bundle_component_addons; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.order_line_bundle_component_addons (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    order_line_id uuid NOT NULL,
    component_item_id uuid NOT NULL,
    addon_item_id uuid NOT NULL,
    addon_name text NOT NULL,
    unit_price integer NOT NULL,
    quantity integer DEFAULT 1 NOT NULL,
    line_total integer NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: order_line_bundle_component_optionals; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.order_line_bundle_component_optionals (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    order_line_id uuid NOT NULL,
    component_item_id uuid NOT NULL,
    optional_field_id uuid,
    field_name text NOT NULL,
    price integer DEFAULT 0 NOT NULL,
    org_ingredient_id uuid,
    ingredient_name text,
    ingredient_unit text,
    quantity_deducted numeric(12,3),
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: order_line_bundle_components; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.order_line_bundle_components (
    order_line_id uuid NOT NULL,
    item_id uuid NOT NULL,
    quantity integer DEFAULT 1 NOT NULL,
    size_label text,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    line_cost bigint,
    CONSTRAINT order_line_bundle_components_quantity_check CHECK ((quantity > 0))
);


--
-- Name: order_payments; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.order_payments (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    order_id uuid NOT NULL,
    method text NOT NULL,
    amount integer NOT NULL,
    reference text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: orders; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.orders (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    shift_id uuid NOT NULL,
    teller_id uuid NOT NULL,
    order_number integer NOT NULL,
    status public.order_status DEFAULT 'pending'::public.order_status NOT NULL,
    payment_method text NOT NULL,
    subtotal integer DEFAULT 0 NOT NULL,
    discount_type public.discount_type,
    discount_value integer DEFAULT 0 NOT NULL,
    discount_amount integer DEFAULT 0 NOT NULL,
    tax_amount integer DEFAULT 0 NOT NULL,
    total_amount integer DEFAULT 0 NOT NULL,
    customer_name text,
    notes text,
    voided_at timestamp with time zone,
    void_reason public.void_reason,
    voided_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    idempotency_key uuid,
    amount_tendered integer,
    change_given integer,
    tip_amount integer DEFAULT 0,
    discount_id uuid,
    tip_payment_method text,
    void_note text,
    CONSTRAINT chk_orders_voided_consistency CHECK ((((voided_at IS NULL) AND (voided_by IS NULL)) OR ((voided_at IS NOT NULL) AND (voided_by IS NOT NULL) AND (status = 'voided'::public.order_status))))
);


--
-- Name: org_ingredients; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.org_ingredients (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    unit public.inventory_unit NOT NULL,
    description text,
    cost_per_unit numeric(15,2),
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    category text DEFAULT 'general'::text NOT NULL,
    supplier_id uuid
);


--
-- Name: org_payment_methods; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.org_payment_methods (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    label_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    color text NOT NULL,
    icon text NOT NULL,
    is_cash boolean DEFAULT false NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: organizations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.organizations (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name text NOT NULL,
    slug text NOT NULL,
    logo_url text,
    currency_code character(3) DEFAULT 'EGP'::bpchar NOT NULL,
    tax_rate numeric(5,4) DEFAULT 0.14 NOT NULL,
    receipt_footer text,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    onboarding_completed boolean DEFAULT false NOT NULL,
    onboarding_completed_at timestamp with time zone,
    stocktake_variance_threshold_pct numeric(6,3) DEFAULT 10 NOT NULL
);


--
-- Name: permissions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.permissions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    user_id uuid NOT NULL,
    resource public.permission_resource NOT NULL,
    action public.permission_action NOT NULL,
    granted boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: purchase_order_lines; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.purchase_order_lines (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    purchase_order_id uuid NOT NULL,
    org_ingredient_id uuid NOT NULL,
    purchase_unit text NOT NULL,
    units_per_purchase_unit numeric(12,4) DEFAULT 1 NOT NULL,
    quantity_ordered numeric(12,3) NOT NULL,
    quantity_received numeric(12,3) DEFAULT 0 NOT NULL,
    unit_cost bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: purchase_orders; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.purchase_orders (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    supplier_id uuid,
    status public.purchase_order_status DEFAULT 'draft'::public.purchase_order_status NOT NULL,
    reference text,
    note text,
    expected_at timestamp with time zone,
    received_at timestamp with time zone,
    received_by uuid,
    created_by uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: role_permissions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.role_permissions (
    role public.user_role NOT NULL,
    resource public.permission_resource NOT NULL,
    action public.permission_action NOT NULL,
    granted boolean DEFAULT true NOT NULL
);


--
-- Name: shift_cash_movements; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.shift_cash_movements (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    shift_id uuid NOT NULL,
    amount integer NOT NULL,
    note text NOT NULL,
    moved_by uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: shifts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.shifts (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    teller_id uuid NOT NULL,
    status public.shift_status DEFAULT 'open'::public.shift_status NOT NULL,
    opening_cash integer DEFAULT 0 NOT NULL,
    closing_cash_declared integer,
    closing_cash_system integer,
    cash_discrepancy integer GENERATED ALWAYS AS ((closing_cash_declared - closing_cash_system)) STORED,
    opened_at timestamp with time zone DEFAULT now() NOT NULL,
    closed_at timestamp with time zone,
    closed_by uuid,
    notes text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    opening_cash_original integer,
    opening_cash_was_edited boolean DEFAULT false NOT NULL,
    opening_cash_edit_reason text,
    force_closed_by uuid,
    force_closed_at timestamp with time zone,
    force_close_reason text
);


--
-- Name: stocktake_items; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.stocktake_items (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    stocktake_id uuid NOT NULL,
    org_ingredient_id uuid NOT NULL,
    branch_inventory_id uuid,
    expected_qty numeric(12,3) NOT NULL,
    counted_qty numeric(12,3),
    variance numeric(12,3) GENERATED ALWAYS AS ((counted_qty - expected_qty)) STORED,
    unit_cost bigint,
    note text,
    counted_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    variance_reason public.stocktake_variance_reason
);


--
-- Name: stocktakes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.stocktakes (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    status public.stocktake_status DEFAULT 'in_progress'::public.stocktake_status NOT NULL,
    note text,
    started_by uuid NOT NULL,
    started_at timestamp with time zone DEFAULT now() NOT NULL,
    finalized_by uuid,
    finalized_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: suppliers; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.suppliers (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    contact_name text,
    phone text,
    email text,
    is_active boolean DEFAULT true NOT NULL,
    deleted_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: user_branch_assignments; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.user_branch_assignments (
    user_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    assigned_at timestamp with time zone DEFAULT now() NOT NULL,
    assigned_by uuid
);


--
-- Name: users; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.users (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid,
    name text NOT NULL,
    email text,
    phone text,
    password_hash text,
    pin_hash text,
    role public.user_role NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    last_login_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    CONSTRAINT chk_login_method CHECK (((password_hash IS NOT NULL) OR (pin_hash IS NOT NULL))),
    CONSTRAINT chk_super_admin_no_org CHECK ((((role = 'super_admin'::public.user_role) AND (org_id IS NULL)) OR ((role <> 'super_admin'::public.user_role) AND (org_id IS NOT NULL))))
);


--
-- Name: _sqlx_migrations _sqlx_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public._sqlx_migrations
    ADD CONSTRAINT _sqlx_migrations_pkey PRIMARY KEY (version);


--
-- Name: addon_item_ingredients addon_item_ingredients_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.addon_item_ingredients
    ADD CONSTRAINT addon_item_ingredients_pkey PRIMARY KEY (id);


--
-- Name: addon_item_ingredients addon_item_ingredients_unique; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.addon_item_ingredients
    ADD CONSTRAINT addon_item_ingredients_unique UNIQUE (addon_item_id, ingredient_name);


--
-- Name: addon_items addon_items_org_id_name_type_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.addon_items
    ADD CONSTRAINT addon_items_org_id_name_type_key UNIQUE (org_id, name, type);


--
-- Name: addon_items addon_items_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.addon_items
    ADD CONSTRAINT addon_items_pkey PRIMARY KEY (id);


--
-- Name: branch_inventory_adjustments branch_inventory_adjustments_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_adjustments
    ADD CONSTRAINT branch_inventory_adjustments_pkey PRIMARY KEY (id);


--
-- Name: branch_inventory branch_inventory_branch_id_org_ingredient_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory
    ADD CONSTRAINT branch_inventory_branch_id_org_ingredient_id_key UNIQUE (branch_id, org_ingredient_id);


--
-- Name: branch_inventory branch_inventory_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory
    ADD CONSTRAINT branch_inventory_pkey PRIMARY KEY (id);


--
-- Name: branch_inventory_transfers branch_inventory_transfers_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_transfers
    ADD CONSTRAINT branch_inventory_transfers_pkey PRIMARY KEY (id);


--
-- Name: branches branches_org_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branches
    ADD CONSTRAINT branches_org_id_name_key UNIQUE (org_id, name);


--
-- Name: branches branches_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branches
    ADD CONSTRAINT branches_pkey PRIMARY KEY (id);


--
-- Name: bundle_branch_availability bundle_branch_availability_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundle_branch_availability
    ADD CONSTRAINT bundle_branch_availability_pkey PRIMARY KEY (bundle_id, branch_id);


--
-- Name: bundle_components bundle_components_bundle_id_item_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundle_components
    ADD CONSTRAINT bundle_components_bundle_id_item_id_key UNIQUE (bundle_id, item_id);


--
-- Name: bundle_components bundle_components_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundle_components
    ADD CONSTRAINT bundle_components_pkey PRIMARY KEY (id);


--
-- Name: bundle_price_epochs bundle_price_epochs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundle_price_epochs
    ADD CONSTRAINT bundle_price_epochs_pkey PRIMARY KEY (id);


--
-- Name: bundles bundles_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundles
    ADD CONSTRAINT bundles_pkey PRIMARY KEY (id);


--
-- Name: categories categories_org_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.categories
    ADD CONSTRAINT categories_org_id_name_key UNIQUE (org_id, name);


--
-- Name: categories categories_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.categories
    ADD CONSTRAINT categories_pkey PRIMARY KEY (id);


--
-- Name: discounts discounts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.discounts
    ADD CONSTRAINT discounts_pkey PRIMARY KEY (id);


--
-- Name: ingredient_cost_history ingredient_cost_history_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ingredient_cost_history
    ADD CONSTRAINT ingredient_cost_history_pkey PRIMARY KEY (id);


--
-- Name: inventory_movements inventory_movements_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inventory_movements
    ADD CONSTRAINT inventory_movements_pkey PRIMARY KEY (id);


--
-- Name: item_sizes item_sizes_menu_item_id_label_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.item_sizes
    ADD CONSTRAINT item_sizes_menu_item_id_label_key UNIQUE (menu_item_id, label);


--
-- Name: item_sizes item_sizes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.item_sizes
    ADD CONSTRAINT item_sizes_pkey PRIMARY KEY (id);


--
-- Name: menu_advisor_bundle_suggestions menu_advisor_bundle_suggestions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_bundle_suggestions
    ADD CONSTRAINT menu_advisor_bundle_suggestions_pkey PRIMARY KEY (id);


--
-- Name: menu_advisor_decisions menu_advisor_decisions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_decisions
    ADD CONSTRAINT menu_advisor_decisions_pkey PRIMARY KEY (id);


--
-- Name: menu_advisor_price_suggestions menu_advisor_price_suggestions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_price_suggestions
    ADD CONSTRAINT menu_advisor_price_suggestions_pkey PRIMARY KEY (id);


--
-- Name: menu_advisor_removal_scenarios menu_advisor_removal_scenarios_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_removal_scenarios
    ADD CONSTRAINT menu_advisor_removal_scenarios_pkey PRIMARY KEY (id);


--
-- Name: menu_advisor_runs menu_advisor_runs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_runs
    ADD CONSTRAINT menu_advisor_runs_pkey PRIMARY KEY (id);


--
-- Name: menu_item_addon_slots menu_item_addon_slots_menu_item_id_addon_type_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_addon_slots
    ADD CONSTRAINT menu_item_addon_slots_menu_item_id_addon_type_key UNIQUE (menu_item_id, addon_type);


--
-- Name: menu_item_addon_slots menu_item_addon_slots_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_addon_slots
    ADD CONSTRAINT menu_item_addon_slots_pkey PRIMARY KEY (id);


--
-- Name: menu_item_optional_fields menu_item_optional_fields_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_optional_fields
    ADD CONSTRAINT menu_item_optional_fields_pkey PRIMARY KEY (id);


--
-- Name: menu_item_price_epochs menu_item_price_epochs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_price_epochs
    ADD CONSTRAINT menu_item_price_epochs_pkey PRIMARY KEY (id);


--
-- Name: menu_item_recipes menu_item_recipes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_recipes
    ADD CONSTRAINT menu_item_recipes_pkey PRIMARY KEY (id);


--
-- Name: menu_item_recipes menu_item_recipes_unique; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_recipes
    ADD CONSTRAINT menu_item_recipes_unique UNIQUE (menu_item_id, size_label, ingredient_name);


--
-- Name: menu_items menu_items_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_items
    ADD CONSTRAINT menu_items_pkey PRIMARY KEY (id);


--
-- Name: order_item_addons order_item_addons_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_item_addons
    ADD CONSTRAINT order_item_addons_pkey PRIMARY KEY (id);


--
-- Name: order_item_optionals order_item_optionals_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_item_optionals
    ADD CONSTRAINT order_item_optionals_pkey PRIMARY KEY (id);


--
-- Name: order_items order_items_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_items
    ADD CONSTRAINT order_items_pkey PRIMARY KEY (id);


--
-- Name: order_line_bundle_component_addons order_line_bundle_component_addons_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_line_bundle_component_addons
    ADD CONSTRAINT order_line_bundle_component_addons_pkey PRIMARY KEY (id);


--
-- Name: order_line_bundle_component_optionals order_line_bundle_component_optionals_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_line_bundle_component_optionals
    ADD CONSTRAINT order_line_bundle_component_optionals_pkey PRIMARY KEY (id);


--
-- Name: order_line_bundle_components order_line_bundle_components_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_line_bundle_components
    ADD CONSTRAINT order_line_bundle_components_pkey PRIMARY KEY (order_line_id, item_id);


--
-- Name: order_payments order_payments_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_payments
    ADD CONSTRAINT order_payments_pkey PRIMARY KEY (id);


--
-- Name: orders orders_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_pkey PRIMARY KEY (id);


--
-- Name: orders orders_shift_id_order_number_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_shift_id_order_number_key UNIQUE (shift_id, order_number);


--
-- Name: org_ingredients org_ingredients_org_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_ingredients
    ADD CONSTRAINT org_ingredients_org_id_name_key UNIQUE (org_id, name);


--
-- Name: org_ingredients org_ingredients_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_ingredients
    ADD CONSTRAINT org_ingredients_pkey PRIMARY KEY (id);


--
-- Name: org_payment_methods org_payment_methods_org_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_payment_methods
    ADD CONSTRAINT org_payment_methods_org_id_name_key UNIQUE (org_id, name);


--
-- Name: org_payment_methods org_payment_methods_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_payment_methods
    ADD CONSTRAINT org_payment_methods_pkey PRIMARY KEY (id);


--
-- Name: organizations organizations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.organizations
    ADD CONSTRAINT organizations_pkey PRIMARY KEY (id);


--
-- Name: organizations organizations_slug_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.organizations
    ADD CONSTRAINT organizations_slug_key UNIQUE (slug);


--
-- Name: permissions permissions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.permissions
    ADD CONSTRAINT permissions_pkey PRIMARY KEY (id);


--
-- Name: permissions permissions_user_id_resource_action_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.permissions
    ADD CONSTRAINT permissions_user_id_resource_action_key UNIQUE (user_id, resource, action);


--
-- Name: purchase_order_lines purchase_order_lines_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.purchase_order_lines
    ADD CONSTRAINT purchase_order_lines_pkey PRIMARY KEY (id);


--
-- Name: purchase_orders purchase_orders_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.purchase_orders
    ADD CONSTRAINT purchase_orders_pkey PRIMARY KEY (id);


--
-- Name: role_permissions role_permissions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.role_permissions
    ADD CONSTRAINT role_permissions_pkey PRIMARY KEY (role, resource, action);


--
-- Name: shift_cash_movements shift_cash_movements_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shift_cash_movements
    ADD CONSTRAINT shift_cash_movements_pkey PRIMARY KEY (id);


--
-- Name: shifts shifts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shifts
    ADD CONSTRAINT shifts_pkey PRIMARY KEY (id);


--
-- Name: stocktake_items stocktake_items_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktake_items
    ADD CONSTRAINT stocktake_items_pkey PRIMARY KEY (id);


--
-- Name: stocktake_items stocktake_items_stocktake_id_org_ingredient_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktake_items
    ADD CONSTRAINT stocktake_items_stocktake_id_org_ingredient_id_key UNIQUE (stocktake_id, org_ingredient_id);


--
-- Name: stocktakes stocktakes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktakes
    ADD CONSTRAINT stocktakes_pkey PRIMARY KEY (id);


--
-- Name: suppliers suppliers_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.suppliers
    ADD CONSTRAINT suppliers_pkey PRIMARY KEY (id);


--
-- Name: user_branch_assignments user_branch_assignments_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_branch_assignments
    ADD CONSTRAINT user_branch_assignments_pkey PRIMARY KEY (user_id, branch_id);


--
-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);


--
-- Name: bundle_price_epochs_bundle_from_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX bundle_price_epochs_bundle_from_idx ON public.bundle_price_epochs USING btree (bundle_id, effective_from DESC);


--
-- Name: idx_bia_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bia_branch ON public.branch_inventory_adjustments USING btree (branch_id);


--
-- Name: idx_bia_inv; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bia_inv ON public.branch_inventory_adjustments USING btree (branch_inventory_id);


--
-- Name: idx_bit_dest; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bit_dest ON public.branch_inventory_transfers USING btree (destination_branch_id);


--
-- Name: idx_bit_ingredient; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bit_ingredient ON public.branch_inventory_transfers USING btree (org_ingredient_id);


--
-- Name: idx_bit_source; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bit_source ON public.branch_inventory_transfers USING btree (source_branch_id);


--
-- Name: idx_branch_inventory_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_branch_inventory_branch ON public.branch_inventory USING btree (branch_id);


--
-- Name: idx_branch_inventory_ingredient; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_branch_inventory_ingredient ON public.branch_inventory USING btree (org_ingredient_id);


--
-- Name: idx_branches_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_branches_org ON public.branches USING btree (org_id);


--
-- Name: idx_branches_org_geo; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_branches_org_geo ON public.branches USING btree (org_id) WHERE ((latitude IS NOT NULL) AND (longitude IS NOT NULL) AND (deleted_at IS NULL));


--
-- Name: idx_bundle_components_bundle; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bundle_components_bundle ON public.bundle_components USING btree (bundle_id);


--
-- Name: idx_bundles_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bundles_org ON public.bundles USING btree (org_id);


--
-- Name: idx_bundles_org_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bundles_org_status ON public.bundles USING btree (org_id, status);


--
-- Name: idx_discounts_org_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_discounts_org_id ON public.discounts USING btree (org_id);


--
-- Name: idx_inventory_movements_branch_time; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_inventory_movements_branch_time ON public.inventory_movements USING btree (branch_id, created_at DESC);


--
-- Name: idx_inventory_movements_ingredient; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_inventory_movements_ingredient ON public.inventory_movements USING btree (org_ingredient_id);


--
-- Name: idx_inventory_movements_source; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_inventory_movements_source ON public.inventory_movements USING btree (source_type, source_id);


--
-- Name: idx_menu_item_recipes_item_size; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_menu_item_recipes_item_size ON public.menu_item_recipes USING btree (menu_item_id, size_label);


--
-- Name: idx_menu_items_category; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_menu_items_category ON public.menu_items USING btree (category_id);


--
-- Name: idx_menu_items_name_trgm; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_menu_items_name_trgm ON public.menu_items USING gin (name public.gin_trgm_ops);


--
-- Name: idx_menu_items_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_menu_items_org ON public.menu_items USING btree (org_id);


--
-- Name: idx_mias_menu_item; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_mias_menu_item ON public.menu_item_addon_slots USING btree (menu_item_id);


--
-- Name: idx_miof_ingredient; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_miof_ingredient ON public.menu_item_optional_fields USING btree (org_ingredient_id) WHERE (org_ingredient_id IS NOT NULL);


--
-- Name: idx_miof_menu_item; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_miof_menu_item ON public.menu_item_optional_fields USING btree (menu_item_id);


--
-- Name: idx_oio_order_item; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_oio_order_item ON public.order_item_optionals USING btree (order_item_id);


--
-- Name: idx_order_item_addons_oi; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_order_item_addons_oi ON public.order_item_addons USING btree (order_item_id);


--
-- Name: idx_order_items_menu_item; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_order_items_menu_item ON public.order_items USING btree (menu_item_id) WHERE (menu_item_id IS NOT NULL);


--
-- Name: idx_order_items_order; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_order_items_order ON public.order_items USING btree (order_id);


--
-- Name: idx_order_payments_order_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_order_payments_order_id ON public.order_payments USING btree (order_id);


--
-- Name: idx_orders_branch_created; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_branch_created ON public.orders USING btree (branch_id, created_at DESC);


--
-- Name: idx_orders_branch_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_branch_id ON public.orders USING btree (branch_id);


--
-- Name: idx_orders_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_created_at ON public.orders USING btree (created_at);


--
-- Name: idx_orders_shift_created; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_shift_created ON public.orders USING btree (shift_id, created_at DESC) WHERE (shift_id IS NOT NULL);


--
-- Name: idx_orders_shift_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_shift_id ON public.orders USING btree (shift_id);


--
-- Name: idx_orders_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_status ON public.orders USING btree (status);


--
-- Name: idx_orders_status_created; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_status_created ON public.orders USING btree (status, created_at DESC);


--
-- Name: idx_orders_teller; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_teller ON public.orders USING btree (teller_id);


--
-- Name: idx_org_ingredients_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_org_ingredients_org ON public.org_ingredients USING btree (org_id);


--
-- Name: idx_org_ingredients_supplier; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_org_ingredients_supplier ON public.org_ingredients USING btree (supplier_id);


--
-- Name: idx_permissions_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_permissions_user ON public.permissions USING btree (user_id);


--
-- Name: idx_po_lines_order; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_po_lines_order ON public.purchase_order_lines USING btree (purchase_order_id);


--
-- Name: idx_purchase_orders_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_purchase_orders_branch ON public.purchase_orders USING btree (branch_id, created_at DESC);


--
-- Name: idx_purchase_orders_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_purchase_orders_org ON public.purchase_orders USING btree (org_id);


--
-- Name: idx_shift_cash_movements_shift; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_shift_cash_movements_shift ON public.shift_cash_movements USING btree (shift_id);


--
-- Name: idx_shifts_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_shifts_branch ON public.shifts USING btree (branch_id);


--
-- Name: idx_shifts_branch_one_open; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_shifts_branch_one_open ON public.shifts USING btree (branch_id) WHERE (status = 'open'::public.shift_status);


--
-- Name: idx_shifts_one_open_per_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_shifts_one_open_per_branch ON public.shifts USING btree (branch_id) WHERE (status = 'open'::public.shift_status);


--
-- Name: idx_shifts_one_open_per_teller; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_shifts_one_open_per_teller ON public.shifts USING btree (teller_id) WHERE (status = 'open'::public.shift_status);


--
-- Name: idx_shifts_opened_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_shifts_opened_at ON public.shifts USING btree (opened_at);


--
-- Name: idx_shifts_teller; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_shifts_teller ON public.shifts USING btree (teller_id);


--
-- Name: idx_stocktake_items_stocktake; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_stocktake_items_stocktake ON public.stocktake_items USING btree (stocktake_id);


--
-- Name: idx_stocktakes_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_stocktakes_branch ON public.stocktakes USING btree (branch_id, started_at DESC);


--
-- Name: idx_stocktakes_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_stocktakes_org ON public.stocktakes USING btree (org_id);


--
-- Name: idx_suppliers_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_suppliers_org ON public.suppliers USING btree (org_id) WHERE (deleted_at IS NULL);


--
-- Name: idx_uba_branch_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_uba_branch_user ON public.user_branch_assignments USING btree (branch_id, user_id);


--
-- Name: idx_users_email; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_users_email ON public.users USING btree (email) WHERE (email IS NOT NULL);


--
-- Name: idx_users_email_active; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_users_email_active ON public.users USING btree (email) WHERE ((deleted_at IS NULL) AND (email IS NOT NULL));


--
-- Name: idx_users_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_users_org ON public.users USING btree (org_id);


--
-- Name: idx_users_teller_unique_name_per_org; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_users_teller_unique_name_per_org ON public.users USING btree (org_id, lower(name)) WHERE ((role = 'teller'::public.user_role) AND (deleted_at IS NULL));


--
-- Name: ingredient_cost_history_ingredient_from_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ingredient_cost_history_ingredient_from_idx ON public.ingredient_cost_history USING btree (org_ingredient_id, effective_from DESC);


--
-- Name: menu_advisor_bundle_suggestions_run_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_advisor_bundle_suggestions_run_idx ON public.menu_advisor_bundle_suggestions USING btree (run_id);


--
-- Name: menu_advisor_decisions_branch_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_advisor_decisions_branch_idx ON public.menu_advisor_decisions USING btree (branch_id, decided_at DESC);


--
-- Name: menu_advisor_decisions_suggestion_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_advisor_decisions_suggestion_idx ON public.menu_advisor_decisions USING btree (suggestion_id, decided_at DESC);


--
-- Name: menu_advisor_price_suggestions_latest_kpi_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_advisor_price_suggestions_latest_kpi_idx ON public.menu_advisor_price_suggestions USING btree (branch_id, menu_item_id, size_label);


--
-- Name: menu_advisor_price_suggestions_run_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_advisor_price_suggestions_run_idx ON public.menu_advisor_price_suggestions USING btree (run_id);


--
-- Name: menu_advisor_removal_scenarios_run_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_advisor_removal_scenarios_run_idx ON public.menu_advisor_removal_scenarios USING btree (run_id);


--
-- Name: menu_advisor_runs_branch_completed_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_advisor_runs_branch_completed_idx ON public.menu_advisor_runs USING btree (branch_id, completed_at DESC) WHERE (status = 'completed'::text);


--
-- Name: menu_advisor_runs_branch_started_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_advisor_runs_branch_started_idx ON public.menu_advisor_runs USING btree (branch_id, started_at DESC);


--
-- Name: menu_advisor_runs_one_active_per_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX menu_advisor_runs_one_active_per_branch ON public.menu_advisor_runs USING btree (branch_id) WHERE (status = 'in_progress'::text);


--
-- Name: menu_item_price_epochs_item_from_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_item_price_epochs_item_from_idx ON public.menu_item_price_epochs USING btree (menu_item_id, effective_from DESC);


--
-- Name: orders_idempotency_key_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX orders_idempotency_key_idx ON public.orders USING btree (idempotency_key) WHERE (idempotency_key IS NOT NULL);


--
-- Name: orders orders_set_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER orders_set_updated_at BEFORE UPDATE ON public.orders FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: addon_item_ingredients trg_addon_item_ingredients_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_addon_item_ingredients_updated_at BEFORE UPDATE ON public.addon_item_ingredients FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: addon_items trg_addon_items_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_addon_items_updated_at BEFORE UPDATE ON public.addon_items FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: branch_inventory trg_branch_inventory_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_branch_inventory_updated_at BEFORE UPDATE ON public.branch_inventory FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: branches trg_branches_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_branches_updated_at BEFORE UPDATE ON public.branches FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: bundles trg_bundles_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_bundles_updated_at BEFORE UPDATE ON public.bundles FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: categories trg_categories_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_categories_updated_at BEFORE UPDATE ON public.categories FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: menu_item_recipes trg_menu_item_recipes_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_menu_item_recipes_updated_at BEFORE UPDATE ON public.menu_item_recipes FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: menu_items trg_menu_items_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_menu_items_updated_at BEFORE UPDATE ON public.menu_items FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: menu_item_optional_fields trg_miof_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_miof_updated_at BEFORE UPDATE ON public.menu_item_optional_fields FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: orders trg_orders_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_orders_updated_at BEFORE UPDATE ON public.orders FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: org_ingredients trg_org_ingredients_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_org_ingredients_updated_at BEFORE UPDATE ON public.org_ingredients FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: org_payment_methods trg_org_payment_methods_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_org_payment_methods_updated_at BEFORE UPDATE ON public.org_payment_methods FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: organizations trg_organizations_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_organizations_updated_at BEFORE UPDATE ON public.organizations FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: shifts trg_shifts_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_shifts_updated_at BEFORE UPDATE ON public.shifts FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: users trg_users_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_users_updated_at BEFORE UPDATE ON public.users FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: addon_item_ingredients addon_item_ingredients_addon_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.addon_item_ingredients
    ADD CONSTRAINT addon_item_ingredients_addon_item_id_fkey FOREIGN KEY (addon_item_id) REFERENCES public.addon_items(id) ON DELETE CASCADE;


--
-- Name: addon_item_ingredients addon_item_ingredients_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.addon_item_ingredients
    ADD CONSTRAINT addon_item_ingredients_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: addon_items addon_items_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.addon_items
    ADD CONSTRAINT addon_items_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- Name: branch_inventory_adjustments branch_inventory_adjustments_adjusted_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_adjustments
    ADD CONSTRAINT branch_inventory_adjustments_adjusted_by_fkey FOREIGN KEY (adjusted_by) REFERENCES public.users(id);


--
-- Name: branch_inventory_adjustments branch_inventory_adjustments_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_adjustments
    ADD CONSTRAINT branch_inventory_adjustments_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: branch_inventory_adjustments branch_inventory_adjustments_branch_inventory_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_adjustments
    ADD CONSTRAINT branch_inventory_adjustments_branch_inventory_id_fkey FOREIGN KEY (branch_inventory_id) REFERENCES public.branch_inventory(id) ON DELETE RESTRICT;


--
-- Name: branch_inventory branch_inventory_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory
    ADD CONSTRAINT branch_inventory_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: branch_inventory branch_inventory_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory
    ADD CONSTRAINT branch_inventory_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: branch_inventory_transfers branch_inventory_transfers_destination_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_transfers
    ADD CONSTRAINT branch_inventory_transfers_destination_branch_id_fkey FOREIGN KEY (destination_branch_id) REFERENCES public.branches(id) ON DELETE RESTRICT;


--
-- Name: branch_inventory_transfers branch_inventory_transfers_initiated_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_transfers
    ADD CONSTRAINT branch_inventory_transfers_initiated_by_fkey FOREIGN KEY (initiated_by) REFERENCES public.users(id);


--
-- Name: branch_inventory_transfers branch_inventory_transfers_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_transfers
    ADD CONSTRAINT branch_inventory_transfers_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- Name: branch_inventory_transfers branch_inventory_transfers_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_transfers
    ADD CONSTRAINT branch_inventory_transfers_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: branch_inventory_transfers branch_inventory_transfers_source_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_transfers
    ADD CONSTRAINT branch_inventory_transfers_source_branch_id_fkey FOREIGN KEY (source_branch_id) REFERENCES public.branches(id) ON DELETE RESTRICT;


--
-- Name: branches branches_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branches
    ADD CONSTRAINT branches_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- Name: bundle_branch_availability bundle_branch_availability_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundle_branch_availability
    ADD CONSTRAINT bundle_branch_availability_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: bundle_branch_availability bundle_branch_availability_bundle_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundle_branch_availability
    ADD CONSTRAINT bundle_branch_availability_bundle_id_fkey FOREIGN KEY (bundle_id) REFERENCES public.bundles(id) ON DELETE CASCADE;


--
-- Name: bundle_components bundle_components_bundle_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundle_components
    ADD CONSTRAINT bundle_components_bundle_id_fkey FOREIGN KEY (bundle_id) REFERENCES public.bundles(id) ON DELETE CASCADE;


--
-- Name: bundle_components bundle_components_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundle_components
    ADD CONSTRAINT bundle_components_item_id_fkey FOREIGN KEY (item_id) REFERENCES public.menu_items(id) ON DELETE RESTRICT;


--
-- Name: bundle_price_epochs bundle_price_epochs_bundle_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundle_price_epochs
    ADD CONSTRAINT bundle_price_epochs_bundle_id_fkey FOREIGN KEY (bundle_id) REFERENCES public.bundles(id);


--
-- Name: bundle_price_epochs bundle_price_epochs_changed_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundle_price_epochs
    ADD CONSTRAINT bundle_price_epochs_changed_by_fkey FOREIGN KEY (changed_by) REFERENCES public.users(id);


--
-- Name: bundles bundles_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundles
    ADD CONSTRAINT bundles_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: bundles bundles_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bundles
    ADD CONSTRAINT bundles_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: categories categories_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.categories
    ADD CONSTRAINT categories_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- Name: discounts discounts_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.discounts
    ADD CONSTRAINT discounts_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- Name: branch_inventory_adjustments fk_bia_transfer; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_inventory_adjustments
    ADD CONSTRAINT fk_bia_transfer FOREIGN KEY (transfer_id) REFERENCES public.branch_inventory_transfers(id) ON DELETE SET NULL;


--
-- Name: ingredient_cost_history ingredient_cost_history_changed_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ingredient_cost_history
    ADD CONSTRAINT ingredient_cost_history_changed_by_fkey FOREIGN KEY (changed_by) REFERENCES public.users(id);


--
-- Name: ingredient_cost_history ingredient_cost_history_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ingredient_cost_history
    ADD CONSTRAINT ingredient_cost_history_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id);


--
-- Name: inventory_movements inventory_movements_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inventory_movements
    ADD CONSTRAINT inventory_movements_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: inventory_movements inventory_movements_branch_inventory_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inventory_movements
    ADD CONSTRAINT inventory_movements_branch_inventory_id_fkey FOREIGN KEY (branch_inventory_id) REFERENCES public.branch_inventory(id) ON DELETE SET NULL;


--
-- Name: inventory_movements inventory_movements_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inventory_movements
    ADD CONSTRAINT inventory_movements_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: inventory_movements inventory_movements_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inventory_movements
    ADD CONSTRAINT inventory_movements_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: item_sizes item_sizes_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.item_sizes
    ADD CONSTRAINT item_sizes_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id) ON DELETE CASCADE;


--
-- Name: menu_advisor_bundle_suggestions menu_advisor_bundle_suggestions_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_bundle_suggestions
    ADD CONSTRAINT menu_advisor_bundle_suggestions_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: menu_advisor_bundle_suggestions menu_advisor_bundle_suggestions_promoted_bundle_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_bundle_suggestions
    ADD CONSTRAINT menu_advisor_bundle_suggestions_promoted_bundle_id_fkey FOREIGN KEY (promoted_bundle_id) REFERENCES public.bundles(id) ON DELETE SET NULL;


--
-- Name: menu_advisor_bundle_suggestions menu_advisor_bundle_suggestions_run_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_bundle_suggestions
    ADD CONSTRAINT menu_advisor_bundle_suggestions_run_id_fkey FOREIGN KEY (run_id) REFERENCES public.menu_advisor_runs(id) ON DELETE CASCADE;


--
-- Name: menu_advisor_decisions menu_advisor_decisions_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_decisions
    ADD CONSTRAINT menu_advisor_decisions_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: menu_advisor_decisions menu_advisor_decisions_decided_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_decisions
    ADD CONSTRAINT menu_advisor_decisions_decided_by_fkey FOREIGN KEY (decided_by) REFERENCES public.users(id);


--
-- Name: menu_advisor_price_suggestions menu_advisor_price_suggestions_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_price_suggestions
    ADD CONSTRAINT menu_advisor_price_suggestions_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: menu_advisor_price_suggestions menu_advisor_price_suggestions_category_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_price_suggestions
    ADD CONSTRAINT menu_advisor_price_suggestions_category_id_fkey FOREIGN KEY (category_id) REFERENCES public.categories(id) ON DELETE SET NULL;


--
-- Name: menu_advisor_price_suggestions menu_advisor_price_suggestions_run_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_price_suggestions
    ADD CONSTRAINT menu_advisor_price_suggestions_run_id_fkey FOREIGN KEY (run_id) REFERENCES public.menu_advisor_runs(id) ON DELETE CASCADE;


--
-- Name: menu_advisor_removal_scenarios menu_advisor_removal_scenarios_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_removal_scenarios
    ADD CONSTRAINT menu_advisor_removal_scenarios_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: menu_advisor_removal_scenarios menu_advisor_removal_scenarios_run_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_removal_scenarios
    ADD CONSTRAINT menu_advisor_removal_scenarios_run_id_fkey FOREIGN KEY (run_id) REFERENCES public.menu_advisor_runs(id) ON DELETE CASCADE;


--
-- Name: menu_advisor_runs menu_advisor_runs_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_runs
    ADD CONSTRAINT menu_advisor_runs_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: menu_advisor_runs menu_advisor_runs_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_advisor_runs
    ADD CONSTRAINT menu_advisor_runs_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: menu_item_addon_slots menu_item_addon_slots_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_addon_slots
    ADD CONSTRAINT menu_item_addon_slots_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id) ON DELETE CASCADE;


--
-- Name: menu_item_optional_fields menu_item_optional_fields_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_optional_fields
    ADD CONSTRAINT menu_item_optional_fields_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id) ON DELETE CASCADE;


--
-- Name: menu_item_optional_fields menu_item_optional_fields_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_optional_fields
    ADD CONSTRAINT menu_item_optional_fields_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE SET NULL;


--
-- Name: menu_item_price_epochs menu_item_price_epochs_changed_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_price_epochs
    ADD CONSTRAINT menu_item_price_epochs_changed_by_fkey FOREIGN KEY (changed_by) REFERENCES public.users(id);


--
-- Name: menu_item_price_epochs menu_item_price_epochs_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_price_epochs
    ADD CONSTRAINT menu_item_price_epochs_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id);


--
-- Name: menu_item_recipes menu_item_recipes_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_recipes
    ADD CONSTRAINT menu_item_recipes_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id) ON DELETE CASCADE;


--
-- Name: menu_item_recipes menu_item_recipes_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_recipes
    ADD CONSTRAINT menu_item_recipes_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: menu_items menu_items_category_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_items
    ADD CONSTRAINT menu_items_category_id_fkey FOREIGN KEY (category_id) REFERENCES public.categories(id);


--
-- Name: menu_items menu_items_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_items
    ADD CONSTRAINT menu_items_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- Name: order_item_addons order_item_addons_addon_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_item_addons
    ADD CONSTRAINT order_item_addons_addon_item_id_fkey FOREIGN KEY (addon_item_id) REFERENCES public.addon_items(id);


--
-- Name: order_item_addons order_item_addons_order_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_item_addons
    ADD CONSTRAINT order_item_addons_order_item_id_fkey FOREIGN KEY (order_item_id) REFERENCES public.order_items(id) ON DELETE CASCADE;


--
-- Name: order_item_optionals order_item_optionals_optional_field_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_item_optionals
    ADD CONSTRAINT order_item_optionals_optional_field_id_fkey FOREIGN KEY (optional_field_id) REFERENCES public.menu_item_optional_fields(id) ON DELETE SET NULL;


--
-- Name: order_item_optionals order_item_optionals_order_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_item_optionals
    ADD CONSTRAINT order_item_optionals_order_item_id_fkey FOREIGN KEY (order_item_id) REFERENCES public.order_items(id) ON DELETE CASCADE;


--
-- Name: order_items order_items_bundle_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_items
    ADD CONSTRAINT order_items_bundle_id_fkey FOREIGN KEY (bundle_id) REFERENCES public.bundles(id) ON DELETE SET NULL;


--
-- Name: order_items order_items_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_items
    ADD CONSTRAINT order_items_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id);


--
-- Name: order_items order_items_order_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_items
    ADD CONSTRAINT order_items_order_id_fkey FOREIGN KEY (order_id) REFERENCES public.orders(id) ON DELETE CASCADE;


--
-- Name: order_line_bundle_component_addons order_line_bundle_component_addons_addon_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_line_bundle_component_addons
    ADD CONSTRAINT order_line_bundle_component_addons_addon_fkey FOREIGN KEY (addon_item_id) REFERENCES public.addon_items(id) ON DELETE RESTRICT;


--
-- Name: order_line_bundle_component_addons order_line_bundle_component_addons_item_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_line_bundle_component_addons
    ADD CONSTRAINT order_line_bundle_component_addons_item_fkey FOREIGN KEY (component_item_id) REFERENCES public.menu_items(id) ON DELETE RESTRICT;


--
-- Name: order_line_bundle_component_addons order_line_bundle_component_addons_order_line_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_line_bundle_component_addons
    ADD CONSTRAINT order_line_bundle_component_addons_order_line_fkey FOREIGN KEY (order_line_id) REFERENCES public.order_items(id) ON DELETE CASCADE;


--
-- Name: order_line_bundle_component_optionals order_line_bundle_component_optionals_order_line_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_line_bundle_component_optionals
    ADD CONSTRAINT order_line_bundle_component_optionals_order_line_fkey FOREIGN KEY (order_line_id) REFERENCES public.order_items(id) ON DELETE CASCADE;


--
-- Name: order_line_bundle_components order_line_bundle_components_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_line_bundle_components
    ADD CONSTRAINT order_line_bundle_components_item_id_fkey FOREIGN KEY (item_id) REFERENCES public.menu_items(id) ON DELETE RESTRICT;


--
-- Name: order_line_bundle_components order_line_bundle_components_order_line_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_line_bundle_components
    ADD CONSTRAINT order_line_bundle_components_order_line_id_fkey FOREIGN KEY (order_line_id) REFERENCES public.order_items(id) ON DELETE CASCADE;


--
-- Name: order_payments order_payments_order_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_payments
    ADD CONSTRAINT order_payments_order_id_fkey FOREIGN KEY (order_id) REFERENCES public.orders(id) ON DELETE CASCADE;


--
-- Name: orders orders_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id);


--
-- Name: orders orders_discount_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_discount_id_fkey FOREIGN KEY (discount_id) REFERENCES public.discounts(id);


--
-- Name: orders orders_shift_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_shift_id_fkey FOREIGN KEY (shift_id) REFERENCES public.shifts(id);


--
-- Name: orders orders_teller_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_teller_id_fkey FOREIGN KEY (teller_id) REFERENCES public.users(id);


--
-- Name: orders orders_voided_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_voided_by_fkey FOREIGN KEY (voided_by) REFERENCES public.users(id);


--
-- Name: org_ingredients org_ingredients_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_ingredients
    ADD CONSTRAINT org_ingredients_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- Name: org_ingredients org_ingredients_supplier_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_ingredients
    ADD CONSTRAINT org_ingredients_supplier_id_fkey FOREIGN KEY (supplier_id) REFERENCES public.suppliers(id) ON DELETE SET NULL;


--
-- Name: org_payment_methods org_payment_methods_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_payment_methods
    ADD CONSTRAINT org_payment_methods_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: permissions permissions_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.permissions
    ADD CONSTRAINT permissions_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: purchase_order_lines purchase_order_lines_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.purchase_order_lines
    ADD CONSTRAINT purchase_order_lines_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: purchase_order_lines purchase_order_lines_purchase_order_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.purchase_order_lines
    ADD CONSTRAINT purchase_order_lines_purchase_order_id_fkey FOREIGN KEY (purchase_order_id) REFERENCES public.purchase_orders(id) ON DELETE CASCADE;


--
-- Name: purchase_orders purchase_orders_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.purchase_orders
    ADD CONSTRAINT purchase_orders_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: purchase_orders purchase_orders_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.purchase_orders
    ADD CONSTRAINT purchase_orders_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id);


--
-- Name: purchase_orders purchase_orders_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.purchase_orders
    ADD CONSTRAINT purchase_orders_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: purchase_orders purchase_orders_received_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.purchase_orders
    ADD CONSTRAINT purchase_orders_received_by_fkey FOREIGN KEY (received_by) REFERENCES public.users(id);


--
-- Name: purchase_orders purchase_orders_supplier_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.purchase_orders
    ADD CONSTRAINT purchase_orders_supplier_id_fkey FOREIGN KEY (supplier_id) REFERENCES public.suppliers(id) ON DELETE SET NULL;


--
-- Name: shift_cash_movements shift_cash_movements_moved_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shift_cash_movements
    ADD CONSTRAINT shift_cash_movements_moved_by_fkey FOREIGN KEY (moved_by) REFERENCES public.users(id);


--
-- Name: shift_cash_movements shift_cash_movements_shift_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shift_cash_movements
    ADD CONSTRAINT shift_cash_movements_shift_id_fkey FOREIGN KEY (shift_id) REFERENCES public.shifts(id) ON DELETE CASCADE;


--
-- Name: shifts shifts_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shifts
    ADD CONSTRAINT shifts_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id);


--
-- Name: shifts shifts_closed_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shifts
    ADD CONSTRAINT shifts_closed_by_fkey FOREIGN KEY (closed_by) REFERENCES public.users(id);


--
-- Name: shifts shifts_force_closed_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shifts
    ADD CONSTRAINT shifts_force_closed_by_fkey FOREIGN KEY (force_closed_by) REFERENCES public.users(id);


--
-- Name: shifts shifts_teller_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shifts
    ADD CONSTRAINT shifts_teller_id_fkey FOREIGN KEY (teller_id) REFERENCES public.users(id);


--
-- Name: stocktake_items stocktake_items_branch_inventory_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktake_items
    ADD CONSTRAINT stocktake_items_branch_inventory_id_fkey FOREIGN KEY (branch_inventory_id) REFERENCES public.branch_inventory(id) ON DELETE SET NULL;


--
-- Name: stocktake_items stocktake_items_counted_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktake_items
    ADD CONSTRAINT stocktake_items_counted_by_fkey FOREIGN KEY (counted_by) REFERENCES public.users(id);


--
-- Name: stocktake_items stocktake_items_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktake_items
    ADD CONSTRAINT stocktake_items_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: stocktake_items stocktake_items_stocktake_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktake_items
    ADD CONSTRAINT stocktake_items_stocktake_id_fkey FOREIGN KEY (stocktake_id) REFERENCES public.stocktakes(id) ON DELETE CASCADE;


--
-- Name: stocktakes stocktakes_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktakes
    ADD CONSTRAINT stocktakes_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: stocktakes stocktakes_finalized_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktakes
    ADD CONSTRAINT stocktakes_finalized_by_fkey FOREIGN KEY (finalized_by) REFERENCES public.users(id);


--
-- Name: stocktakes stocktakes_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktakes
    ADD CONSTRAINT stocktakes_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: stocktakes stocktakes_started_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktakes
    ADD CONSTRAINT stocktakes_started_by_fkey FOREIGN KEY (started_by) REFERENCES public.users(id);


--
-- Name: suppliers suppliers_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.suppliers
    ADD CONSTRAINT suppliers_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: user_branch_assignments user_branch_assignments_assigned_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_branch_assignments
    ADD CONSTRAINT user_branch_assignments_assigned_by_fkey FOREIGN KEY (assigned_by) REFERENCES public.users(id);


--
-- Name: user_branch_assignments user_branch_assignments_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_branch_assignments
    ADD CONSTRAINT user_branch_assignments_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id);


--
-- Name: user_branch_assignments user_branch_assignments_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_branch_assignments
    ADD CONSTRAINT user_branch_assignments_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id);


--
-- Name: users users_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- PostgreSQL database dump complete
--

