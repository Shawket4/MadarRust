-- The production schema as of the 2026-09-05 dump (madar_20260905_214531.dump):
-- `pg_restore -s --no-owner --no-privileges` plus the `_sqlx_migrations` rows
-- (version + checksum bookkeeping only). NO customer data. Used by the CI
-- schema-drift job; refresh it from a fresh prod dump (owner) with the same
-- two pg_restore commands.
--
-- PostgreSQL database dump
--


-- Dumped from database version 17.11 (Debian 17.11-0+deb13u1)
-- Dumped by pg_dump version 17.11 (Debian 17.11-0+deb13u1)

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
-- Name: _sqlx_test; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA _sqlx_test;


--
-- Name: pg_trgm; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA public;


--
-- Name: EXTENSION pg_trgm; Type: COMMENT; Schema: -; Owner: -
--

COMMENT ON EXTENSION pg_trgm IS 'text similarity measurement and index searching based on trigrams';


--
-- Name: pgcrypto; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS pgcrypto WITH SCHEMA public;


--
-- Name: EXTENSION pgcrypto; Type: COMMENT; Schema: -; Owner: -
--

COMMENT ON EXTENSION pgcrypto IS 'cryptographic functions';


--
-- Name: booking_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.booking_status AS ENUM (
    'requested',
    'confirmed',
    'notified',
    'arrived',
    'seated',
    'completed',
    'no_show',
    'cancelled'
);


--
-- Name: bundle_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.bundle_status AS ENUM (
    'draft',
    'active',
    'archived'
);


--
-- Name: delivery_channel; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.delivery_channel AS ENUM (
    'in_mall',
    'outside',
    'umbrella',
    'pickup'
);


--
-- Name: delivery_order_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.delivery_order_status AS ENUM (
    'received',
    'confirmed',
    'preparing',
    'ready',
    'out_for_delivery',
    'delivered',
    'cancelled',
    'rejected'
);


--
-- Name: discount_type; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.discount_type AS ENUM (
    'percentage',
    'fixed'
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
    'stock_count',
    'purchase_return'
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
-- Name: kitchen_routing_mode; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.kitchen_routing_mode AS ENUM (
    'kds',
    'till',
    'both',
    'off'
);


--
-- Name: kitchen_ticket_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.kitchen_ticket_status AS ENUM (
    'firing',
    'ready',
    'voided'
);


--
-- Name: open_ticket_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.open_ticket_status AS ENUM (
    'open',
    'ready',
    'settled',
    'voided'
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
    'purchase_orders',
    'delivery_settings',
    'delivery_orders',
    'kitchen_stations',
    'kitchen_orders',
    'open_tickets',
    'floor_plan',
    'reservations',
    'staff',
    'work_shifts',
    'attendance',
    'leave',
    'payroll',
    'held_orders',
    'table_transfers'
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
-- Name: timezone_name; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.timezone_name AS ENUM (
    'Africa/Abidjan',
    'Africa/Accra',
    'Africa/Addis_Ababa',
    'Africa/Algiers',
    'Africa/Asmara',
    'Africa/Asmera',
    'Africa/Bamako',
    'Africa/Bangui',
    'Africa/Banjul',
    'Africa/Bissau',
    'Africa/Blantyre',
    'Africa/Brazzaville',
    'Africa/Bujumbura',
    'Africa/Cairo',
    'Africa/Casablanca',
    'Africa/Ceuta',
    'Africa/Conakry',
    'Africa/Dakar',
    'Africa/Dar_es_Salaam',
    'Africa/Djibouti',
    'Africa/Douala',
    'Africa/El_Aaiun',
    'Africa/Freetown',
    'Africa/Gaborone',
    'Africa/Harare',
    'Africa/Johannesburg',
    'Africa/Juba',
    'Africa/Kampala',
    'Africa/Khartoum',
    'Africa/Kigali',
    'Africa/Kinshasa',
    'Africa/Lagos',
    'Africa/Libreville',
    'Africa/Lome',
    'Africa/Luanda',
    'Africa/Lubumbashi',
    'Africa/Lusaka',
    'Africa/Malabo',
    'Africa/Maputo',
    'Africa/Maseru',
    'Africa/Mbabane',
    'Africa/Mogadishu',
    'Africa/Monrovia',
    'Africa/Nairobi',
    'Africa/Ndjamena',
    'Africa/Niamey',
    'Africa/Nouakchott',
    'Africa/Ouagadougou',
    'Africa/Porto-Novo',
    'Africa/Sao_Tome',
    'Africa/Timbuktu',
    'Africa/Tripoli',
    'Africa/Tunis',
    'Africa/Windhoek',
    'America/Adak',
    'America/Anchorage',
    'America/Anguilla',
    'America/Antigua',
    'America/Araguaina',
    'America/Argentina/Buenos_Aires',
    'America/Argentina/Catamarca',
    'America/Argentina/ComodRivadavia',
    'America/Argentina/Cordoba',
    'America/Argentina/Jujuy',
    'America/Argentina/La_Rioja',
    'America/Argentina/Mendoza',
    'America/Argentina/Rio_Gallegos',
    'America/Argentina/Salta',
    'America/Argentina/San_Juan',
    'America/Argentina/San_Luis',
    'America/Argentina/Tucuman',
    'America/Argentina/Ushuaia',
    'America/Aruba',
    'America/Asuncion',
    'America/Atikokan',
    'America/Atka',
    'America/Bahia',
    'America/Bahia_Banderas',
    'America/Barbados',
    'America/Belem',
    'America/Belize',
    'America/Blanc-Sablon',
    'America/Boa_Vista',
    'America/Bogota',
    'America/Boise',
    'America/Buenos_Aires',
    'America/Cambridge_Bay',
    'America/Campo_Grande',
    'America/Cancun',
    'America/Caracas',
    'America/Catamarca',
    'America/Cayenne',
    'America/Cayman',
    'America/Chicago',
    'America/Chihuahua',
    'America/Ciudad_Juarez',
    'America/Coral_Harbour',
    'America/Cordoba',
    'America/Costa_Rica',
    'America/Coyhaique',
    'America/Creston',
    'America/Cuiaba',
    'America/Curacao',
    'America/Danmarkshavn',
    'America/Dawson',
    'America/Dawson_Creek',
    'America/Denver',
    'America/Detroit',
    'America/Dominica',
    'America/Edmonton',
    'America/Eirunepe',
    'America/El_Salvador',
    'America/Ensenada',
    'America/Fort_Nelson',
    'America/Fort_Wayne',
    'America/Fortaleza',
    'America/Glace_Bay',
    'America/Godthab',
    'America/Goose_Bay',
    'America/Grand_Turk',
    'America/Grenada',
    'America/Guadeloupe',
    'America/Guatemala',
    'America/Guayaquil',
    'America/Guyana',
    'America/Halifax',
    'America/Havana',
    'America/Hermosillo',
    'America/Indiana/Indianapolis',
    'America/Indiana/Knox',
    'America/Indiana/Marengo',
    'America/Indiana/Petersburg',
    'America/Indiana/Tell_City',
    'America/Indiana/Vevay',
    'America/Indiana/Vincennes',
    'America/Indiana/Winamac',
    'America/Indianapolis',
    'America/Inuvik',
    'America/Iqaluit',
    'America/Jamaica',
    'America/Jujuy',
    'America/Juneau',
    'America/Kentucky/Louisville',
    'America/Kentucky/Monticello',
    'America/Knox_IN',
    'America/Kralendijk',
    'America/La_Paz',
    'America/Lima',
    'America/Los_Angeles',
    'America/Louisville',
    'America/Lower_Princes',
    'America/Maceio',
    'America/Managua',
    'America/Manaus',
    'America/Marigot',
    'America/Martinique',
    'America/Matamoros',
    'America/Mazatlan',
    'America/Mendoza',
    'America/Menominee',
    'America/Merida',
    'America/Metlakatla',
    'America/Mexico_City',
    'America/Miquelon',
    'America/Moncton',
    'America/Monterrey',
    'America/Montevideo',
    'America/Montreal',
    'America/Montserrat',
    'America/Nassau',
    'America/New_York',
    'America/Nipigon',
    'America/Nome',
    'America/Noronha',
    'America/North_Dakota/Beulah',
    'America/North_Dakota/Center',
    'America/North_Dakota/New_Salem',
    'America/Nuuk',
    'America/Ojinaga',
    'America/Panama',
    'America/Pangnirtung',
    'America/Paramaribo',
    'America/Phoenix',
    'America/Port_of_Spain',
    'America/Port-au-Prince',
    'America/Porto_Acre',
    'America/Porto_Velho',
    'America/Puerto_Rico',
    'America/Punta_Arenas',
    'America/Rainy_River',
    'America/Rankin_Inlet',
    'America/Recife',
    'America/Regina',
    'America/Resolute',
    'America/Rio_Branco',
    'America/Rosario',
    'America/Santa_Isabel',
    'America/Santarem',
    'America/Santiago',
    'America/Santo_Domingo',
    'America/Sao_Paulo',
    'America/Scoresbysund',
    'America/Shiprock',
    'America/Sitka',
    'America/St_Barthelemy',
    'America/St_Johns',
    'America/St_Kitts',
    'America/St_Lucia',
    'America/St_Thomas',
    'America/St_Vincent',
    'America/Swift_Current',
    'America/Tegucigalpa',
    'America/Thule',
    'America/Thunder_Bay',
    'America/Tijuana',
    'America/Toronto',
    'America/Tortola',
    'America/Vancouver',
    'America/Virgin',
    'America/Whitehorse',
    'America/Winnipeg',
    'America/Yakutat',
    'America/Yellowknife',
    'Antarctica/Casey',
    'Antarctica/Davis',
    'Antarctica/DumontDUrville',
    'Antarctica/Macquarie',
    'Antarctica/Mawson',
    'Antarctica/McMurdo',
    'Antarctica/Palmer',
    'Antarctica/Rothera',
    'Antarctica/South_Pole',
    'Antarctica/Syowa',
    'Antarctica/Troll',
    'Antarctica/Vostok',
    'Arctic/Longyearbyen',
    'Asia/Aden',
    'Asia/Almaty',
    'Asia/Amman',
    'Asia/Anadyr',
    'Asia/Aqtau',
    'Asia/Aqtobe',
    'Asia/Ashgabat',
    'Asia/Ashkhabad',
    'Asia/Atyrau',
    'Asia/Baghdad',
    'Asia/Bahrain',
    'Asia/Baku',
    'Asia/Bangkok',
    'Asia/Barnaul',
    'Asia/Beirut',
    'Asia/Bishkek',
    'Asia/Brunei',
    'Asia/Calcutta',
    'Asia/Chita',
    'Asia/Choibalsan',
    'Asia/Chongqing',
    'Asia/Chungking',
    'Asia/Colombo',
    'Asia/Dacca',
    'Asia/Damascus',
    'Asia/Dhaka',
    'Asia/Dili',
    'Asia/Dubai',
    'Asia/Dushanbe',
    'Asia/Famagusta',
    'Asia/Gaza',
    'Asia/Harbin',
    'Asia/Hebron',
    'Asia/Ho_Chi_Minh',
    'Asia/Hong_Kong',
    'Asia/Hovd',
    'Asia/Irkutsk',
    'Asia/Istanbul',
    'Asia/Jakarta',
    'Asia/Jayapura',
    'Asia/Jerusalem',
    'Asia/Kabul',
    'Asia/Kamchatka',
    'Asia/Karachi',
    'Asia/Kashgar',
    'Asia/Kathmandu',
    'Asia/Katmandu',
    'Asia/Khandyga',
    'Asia/Kolkata',
    'Asia/Krasnoyarsk',
    'Asia/Kuala_Lumpur',
    'Asia/Kuching',
    'Asia/Kuwait',
    'Asia/Macao',
    'Asia/Macau',
    'Asia/Magadan',
    'Asia/Makassar',
    'Asia/Manila',
    'Asia/Muscat',
    'Asia/Nicosia',
    'Asia/Novokuznetsk',
    'Asia/Novosibirsk',
    'Asia/Omsk',
    'Asia/Oral',
    'Asia/Phnom_Penh',
    'Asia/Pontianak',
    'Asia/Pyongyang',
    'Asia/Qatar',
    'Asia/Qostanay',
    'Asia/Qyzylorda',
    'Asia/Rangoon',
    'Asia/Riyadh',
    'Asia/Saigon',
    'Asia/Sakhalin',
    'Asia/Samarkand',
    'Asia/Seoul',
    'Asia/Shanghai',
    'Asia/Singapore',
    'Asia/Srednekolymsk',
    'Asia/Taipei',
    'Asia/Tashkent',
    'Asia/Tbilisi',
    'Asia/Tehran',
    'Asia/Tel_Aviv',
    'Asia/Thimbu',
    'Asia/Thimphu',
    'Asia/Tokyo',
    'Asia/Tomsk',
    'Asia/Ujung_Pandang',
    'Asia/Ulaanbaatar',
    'Asia/Ulan_Bator',
    'Asia/Urumqi',
    'Asia/Ust-Nera',
    'Asia/Vientiane',
    'Asia/Vladivostok',
    'Asia/Yakutsk',
    'Asia/Yangon',
    'Asia/Yekaterinburg',
    'Asia/Yerevan',
    'Atlantic/Azores',
    'Atlantic/Bermuda',
    'Atlantic/Canary',
    'Atlantic/Cape_Verde',
    'Atlantic/Faeroe',
    'Atlantic/Faroe',
    'Atlantic/Jan_Mayen',
    'Atlantic/Madeira',
    'Atlantic/Reykjavik',
    'Atlantic/South_Georgia',
    'Atlantic/St_Helena',
    'Atlantic/Stanley',
    'Australia/ACT',
    'Australia/Adelaide',
    'Australia/Brisbane',
    'Australia/Broken_Hill',
    'Australia/Canberra',
    'Australia/Currie',
    'Australia/Darwin',
    'Australia/Eucla',
    'Australia/Hobart',
    'Australia/LHI',
    'Australia/Lindeman',
    'Australia/Lord_Howe',
    'Australia/Melbourne',
    'Australia/North',
    'Australia/NSW',
    'Australia/Perth',
    'Australia/Queensland',
    'Australia/South',
    'Australia/Sydney',
    'Australia/Tasmania',
    'Australia/Victoria',
    'Australia/West',
    'Australia/Yancowinna',
    'Brazil/Acre',
    'Brazil/DeNoronha',
    'Brazil/East',
    'Brazil/West',
    'Canada/Atlantic',
    'Canada/Central',
    'Canada/Eastern',
    'Canada/Mountain',
    'Canada/Newfoundland',
    'Canada/Pacific',
    'Canada/Saskatchewan',
    'Canada/Yukon',
    'Chile/Continental',
    'Chile/EasterIsland',
    'Etc/GMT',
    'Etc/GMT-0',
    'Etc/GMT-1',
    'Etc/GMT-10',
    'Etc/GMT-11',
    'Etc/GMT-12',
    'Etc/GMT-13',
    'Etc/GMT-14',
    'Etc/GMT-2',
    'Etc/GMT-3',
    'Etc/GMT-4',
    'Etc/GMT-5',
    'Etc/GMT-6',
    'Etc/GMT-7',
    'Etc/GMT-8',
    'Etc/GMT-9',
    'Etc/GMT+0',
    'Etc/GMT+1',
    'Etc/GMT+10',
    'Etc/GMT+11',
    'Etc/GMT+12',
    'Etc/GMT+2',
    'Etc/GMT+3',
    'Etc/GMT+4',
    'Etc/GMT+5',
    'Etc/GMT+6',
    'Etc/GMT+7',
    'Etc/GMT+8',
    'Etc/GMT+9',
    'Etc/GMT0',
    'Etc/Greenwich',
    'Etc/UCT',
    'Etc/Universal',
    'Etc/UTC',
    'Etc/Zulu',
    'Europe/Amsterdam',
    'Europe/Andorra',
    'Europe/Astrakhan',
    'Europe/Athens',
    'Europe/Belfast',
    'Europe/Belgrade',
    'Europe/Berlin',
    'Europe/Bratislava',
    'Europe/Brussels',
    'Europe/Bucharest',
    'Europe/Budapest',
    'Europe/Busingen',
    'Europe/Chisinau',
    'Europe/Copenhagen',
    'Europe/Dublin',
    'Europe/Gibraltar',
    'Europe/Guernsey',
    'Europe/Helsinki',
    'Europe/Isle_of_Man',
    'Europe/Istanbul',
    'Europe/Jersey',
    'Europe/Kaliningrad',
    'Europe/Kiev',
    'Europe/Kirov',
    'Europe/Kyiv',
    'Europe/Lisbon',
    'Europe/Ljubljana',
    'Europe/London',
    'Europe/Luxembourg',
    'Europe/Madrid',
    'Europe/Malta',
    'Europe/Mariehamn',
    'Europe/Minsk',
    'Europe/Monaco',
    'Europe/Moscow',
    'Europe/Nicosia',
    'Europe/Oslo',
    'Europe/Paris',
    'Europe/Podgorica',
    'Europe/Prague',
    'Europe/Riga',
    'Europe/Rome',
    'Europe/Samara',
    'Europe/San_Marino',
    'Europe/Sarajevo',
    'Europe/Saratov',
    'Europe/Simferopol',
    'Europe/Skopje',
    'Europe/Sofia',
    'Europe/Stockholm',
    'Europe/Tallinn',
    'Europe/Tirane',
    'Europe/Tiraspol',
    'Europe/Ulyanovsk',
    'Europe/Uzhgorod',
    'Europe/Vaduz',
    'Europe/Vatican',
    'Europe/Vienna',
    'Europe/Vilnius',
    'Europe/Volgograd',
    'Europe/Warsaw',
    'Europe/Zagreb',
    'Europe/Zaporozhye',
    'Europe/Zurich',
    'Indian/Antananarivo',
    'Indian/Chagos',
    'Indian/Christmas',
    'Indian/Cocos',
    'Indian/Comoro',
    'Indian/Kerguelen',
    'Indian/Mahe',
    'Indian/Maldives',
    'Indian/Mauritius',
    'Indian/Mayotte',
    'Indian/Reunion',
    'Mexico/BajaNorte',
    'Mexico/BajaSur',
    'Mexico/General',
    'Pacific/Apia',
    'Pacific/Auckland',
    'Pacific/Bougainville',
    'Pacific/Chatham',
    'Pacific/Chuuk',
    'Pacific/Easter',
    'Pacific/Efate',
    'Pacific/Enderbury',
    'Pacific/Fakaofo',
    'Pacific/Fiji',
    'Pacific/Funafuti',
    'Pacific/Galapagos',
    'Pacific/Gambier',
    'Pacific/Guadalcanal',
    'Pacific/Guam',
    'Pacific/Honolulu',
    'Pacific/Johnston',
    'Pacific/Kanton',
    'Pacific/Kiritimati',
    'Pacific/Kosrae',
    'Pacific/Kwajalein',
    'Pacific/Majuro',
    'Pacific/Marquesas',
    'Pacific/Midway',
    'Pacific/Nauru',
    'Pacific/Niue',
    'Pacific/Norfolk',
    'Pacific/Noumea',
    'Pacific/Pago_Pago',
    'Pacific/Palau',
    'Pacific/Pitcairn',
    'Pacific/Pohnpei',
    'Pacific/Ponape',
    'Pacific/Port_Moresby',
    'Pacific/Rarotonga',
    'Pacific/Saipan',
    'Pacific/Samoa',
    'Pacific/Tahiti',
    'Pacific/Tarawa',
    'Pacific/Tongatapu',
    'Pacific/Truk',
    'Pacific/Wake',
    'Pacific/Wallis',
    'Pacific/Yap',
    'US/Alaska',
    'US/Aleutian',
    'US/Arizona',
    'US/Central',
    'US/East-Indiana',
    'US/Eastern',
    'US/Hawaii',
    'US/Indiana-Starke',
    'US/Michigan',
    'US/Mountain',
    'US/Pacific',
    'US/Samoa',
    'UTC'
);


--
-- Name: user_role; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.user_role AS ENUM (
    'super_admin',
    'org_admin',
    'branch_manager',
    'teller',
    'waiter',
    'kitchen'
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
-- Name: branch_stock_on_hand_guard(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.branch_stock_on_hand_guard() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.on_hand IS DISTINCT FROM OLD.on_hand
       AND pg_trigger_depth() < 2
       AND COALESCE(current_setting('madar.stock_rebase', true), '') <> 'on'
    THEN
        RAISE EXCEPTION 'branch_stock.on_hand is derived from inventory_movements — post a movement instead'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;


--
-- Name: derive_branch_code(uuid, text); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.derive_branch_code(p_id uuid, p_name text) RETURNS text
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT CASE
        WHEN nullif(upper(substring(regexp_replace(coalesce(p_name, ''), '[^A-Za-z0-9]', '', 'g') from 1 for 6)), '') IS NOT NULL
            THEN upper(substring(regexp_replace(p_name, '[^A-Za-z0-9]', '', 'g') from 1 for 6))
        ELSE 'B' || upper(substring(replace(p_id::text, '-', '') from 1 for 5))
    END
$$;


--
-- Name: ingredient_category_id(uuid, text); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ingredient_category_id(p_org_id uuid, p_slug text) RETURNS uuid
    LANGUAGE plpgsql
    AS $$
DECLARE v_id uuid;
BEGIN
    INSERT INTO ingredient_categories (org_id, slug, name, sort_order)
    VALUES (p_org_id, p_slug,
            CASE WHEN p_slug = 'general' THEN 'General'
                 ELSE initcap(replace(p_slug, '_', ' ')) END,
            CASE WHEN p_slug = 'general' THEN 0 ELSE 10 END)
    ON CONFLICT (org_id, slug) DO UPDATE SET slug = EXCLUDED.slug
    RETURNING id INTO v_id;
    RETURN v_id;
END $$;


--
-- Name: inventory_movements_apply(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.inventory_movements_apply() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    v_stock_id uuid;
    v_balance  numeric(12,3);
BEGIN
    INSERT INTO branch_stock (branch_id, org_ingredient_id, on_hand, last_movement_at)
    VALUES (NEW.branch_id, NEW.org_ingredient_id, NEW.quantity, NEW.created_at)
    ON CONFLICT (branch_id, org_ingredient_id) DO UPDATE
        SET on_hand          = branch_stock.on_hand + EXCLUDED.on_hand,
            last_movement_at = GREATEST(branch_stock.last_movement_at, EXCLUDED.last_movement_at)
    RETURNING id, on_hand INTO v_stock_id, v_balance;

    NEW.branch_stock_id := v_stock_id;
    NEW.balance_after   := v_balance;
    NEW.below_zero      := v_balance < 0;
    RETURN NEW;
END $$;


--
-- Name: org_ingredients_default_category(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.org_ingredients_default_category() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.category_id IS NULL THEN
        NEW.category_id := ingredient_category_id(NEW.org_id, 'general');
    END IF;
    RETURN NEW;
END $$;


--
-- Name: organizations_seed_general_category(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.organizations_seed_general_category() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM ingredient_category_id(NEW.id, 'general');
    RETURN NEW;
END $$;


--
-- Name: set_branch_code(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.set_branch_code() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    candidate text;
BEGIN
    IF NEW.code IS NOT NULL THEN
        RETURN NEW;
    END IF;
    candidate := public.derive_branch_code(NEW.id, NEW.name);
    IF EXISTS (SELECT 1 FROM public.branches WHERE org_id = NEW.org_id AND code = candidate) THEN
        candidate := 'B' || upper(substring(replace(NEW.id::text, '-', '') from 1 for 5));
    END IF;
    NEW.code := candidate;
    RETURN NEW;
END;
$$;


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


--
-- Name: shifts_fill_default_till(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.shifts_fill_default_till() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    v_till uuid;
    v_org  uuid;
BEGIN
    IF NEW.till_id IS NOT NULL THEN
        RETURN NEW;
    END IF;
    SELECT id INTO v_till FROM tills
        WHERE branch_id = NEW.branch_id AND is_default AND deleted_at IS NULL
        LIMIT 1;
    IF v_till IS NULL THEN
        SELECT org_id INTO v_org FROM branches WHERE id = NEW.branch_id;
        INSERT INTO tills (org_id, branch_id, name, is_default, is_active)
            VALUES (v_org, NEW.branch_id, 'Till 1', true, true)
            ON CONFLICT DO NOTHING;
        SELECT id INTO v_till FROM tills
            WHERE branch_id = NEW.branch_id AND is_default AND deleted_at IS NULL
            LIMIT 1;
    END IF;
    NEW.till_id := v_till;
    RETURN NEW;
END;
$$;


--
-- Name: database_ids; Type: SEQUENCE; Schema: _sqlx_test; Owner: -
--

CREATE SEQUENCE _sqlx_test.database_ids
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: databases; Type: TABLE; Schema: _sqlx_test; Owner: -
--

CREATE TABLE _sqlx_test.databases (
    db_name text NOT NULL,
    test_path text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


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
-- Name: modifier_options; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.modifier_options (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    group_id uuid NOT NULL,
    name text NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    price integer DEFAULT 0 NOT NULL,
    sort integer DEFAULT 0 NOT NULL,
    is_default boolean DEFAULT false NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    replaces_ingredient_id uuid,
    legacy_source text DEFAULT 'addon'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT modifier_options_legacy_source_check CHECK ((legacy_source = ANY (ARRAY['addon'::text, 'optional'::text]))),
    CONSTRAINT modifier_options_price_nonneg CHECK ((price >= 0))
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
    supplier_id uuid,
    pack_unit text,
    pack_size numeric(12,4),
    yield_pct numeric(6,3),
    density_g_per_ml numeric(10,4),
    category_id uuid NOT NULL
);


--
-- Name: recipe_lines; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.recipe_lines (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    owner_type text NOT NULL,
    owner_id uuid NOT NULL,
    ingredient_id uuid NOT NULL,
    quantity numeric(12,3) NOT NULL,
    unit text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT recipe_lines_owner_type_check CHECK ((owner_type = ANY (ARRAY['item_size'::text, 'modifier_option'::text]))),
    CONSTRAINT recipe_lines_qty_nonneg CHECK ((quantity >= (0)::numeric))
);


--
-- Name: addon_item_ingredients; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.addon_item_ingredients WITH (security_invoker='true') AS
 SELECT rl.id,
    rl.owner_id AS addon_item_id,
    rl.quantity AS quantity_used,
    rl.created_at,
    rl.updated_at,
    oi.name AS ingredient_name,
    rl.unit AS ingredient_unit,
    rl.ingredient_id AS org_ingredient_id
   FROM ((public.recipe_lines rl
     JOIN public.modifier_options mo ON (((mo.id = rl.owner_id) AND (mo.legacy_source = 'addon'::text))))
     JOIN public.org_ingredients oi ON ((oi.id = rl.ingredient_id)))
  WHERE (rl.owner_type = 'modifier_option'::text);


--
-- Name: modifier_groups; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.modifier_groups (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    selection_type text DEFAULT 'multi'::text NOT NULL,
    min_selections integer DEFAULT 0 NOT NULL,
    max_selections integer,
    is_required boolean DEFAULT false NOT NULL,
    sort integer DEFAULT 0 NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    legacy_addon_type text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT modifier_groups_selcount_ck CHECK (((min_selections >= 0) AND ((max_selections IS NULL) OR (max_selections >= min_selections)))),
    CONSTRAINT modifier_groups_selection_type_check CHECK ((selection_type = ANY (ARRAY['single'::text, 'multi'::text])))
);


--
-- Name: addon_items; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.addon_items WITH (security_invoker='true') AS
 SELECT mo.id,
    mg.org_id,
    mo.name,
    mg.legacy_addon_type AS type,
    mo.price AS default_price,
    mo.is_active,
    mo.name_translations,
    mo.created_at,
    mo.updated_at
   FROM (public.modifier_options mo
     JOIN public.modifier_groups mg ON ((mg.id = mo.group_id)))
  WHERE (mo.legacy_source = 'addon'::text);


--
-- Name: ai_conversations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.ai_conversations (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    title text NOT NULL,
    locale text DEFAULT 'en'::text NOT NULL,
    summary text,
    summarized_through_seq integer DEFAULT 0 NOT NULL,
    turn_count integer DEFAULT 0 NOT NULL,
    last_turn_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    CONSTRAINT ai_conversations_summary_covers_existing_turns CHECK (((summarized_through_seq >= 0) AND (summarized_through_seq <= turn_count)))
);


--
-- Name: ai_messages; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.ai_messages (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    conversation_id uuid NOT NULL,
    org_id uuid NOT NULL,
    seq integer NOT NULL,
    question text NOT NULL,
    answer text,
    kind text NOT NULL,
    specs jsonb DEFAULT '[]'::jsonb NOT NULL,
    provider text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT ai_messages_seq_positive CHECK ((seq > 0)),
    CONSTRAINT ai_messages_specs_is_array CHECK ((jsonb_typeof(specs) = 'array'::text))
);


--
-- Name: attendance_records; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.attendance_records (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    work_shift_id uuid,
    business_date date NOT NULL,
    status text DEFAULT 'present'::text NOT NULL,
    scheduled_start_at timestamp with time zone,
    scheduled_end_at timestamp with time zone,
    check_in_at timestamp with time zone,
    check_in_latitude double precision,
    check_in_longitude double precision,
    check_in_distance_meters double precision,
    check_in_method text,
    check_out_at timestamp with time zone,
    check_out_latitude double precision,
    check_out_longitude double precision,
    check_out_distance_meters double precision,
    check_out_method text,
    late_minutes integer DEFAULT 0 NOT NULL,
    early_leave_minutes integer DEFAULT 0 NOT NULL,
    overtime_minutes integer DEFAULT 0 NOT NULL,
    worked_minutes integer DEFAULT 0 NOT NULL,
    is_manual boolean DEFAULT false NOT NULL,
    notes text,
    edit_reason text,
    created_by uuid,
    edited_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT attendance_records_in_method_chk CHECK (((check_in_method IS NULL) OR (check_in_method = ANY (ARRAY['mobile_gps'::text, 'manual'::text, 'auto'::text])))),
    CONSTRAINT attendance_records_minutes_nonneg CHECK (((late_minutes >= 0) AND (early_leave_minutes >= 0) AND (overtime_minutes >= 0) AND (worked_minutes >= 0))),
    CONSTRAINT attendance_records_order_chk CHECK (((check_out_at IS NULL) OR (check_in_at IS NULL) OR (check_out_at >= check_in_at))),
    CONSTRAINT attendance_records_out_method_chk CHECK (((check_out_method IS NULL) OR (check_out_method = ANY (ARRAY['mobile_gps'::text, 'manual'::text, 'auto'::text])))),
    CONSTRAINT attendance_records_out_needs_in CHECK (((check_out_at IS NULL) OR (check_in_at IS NOT NULL))),
    CONSTRAINT attendance_records_status_chk CHECK ((status = ANY (ARRAY['present'::text, 'late'::text, 'absent'::text, 'half_day'::text, 'on_leave'::text])))
);


--
-- Name: attendance_settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.attendance_settings (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid,
    late_deduction_tiers jsonb DEFAULT '[]'::jsonb NOT NULL,
    absence_deduction_days numeric(5,2) DEFAULT 1.00 NOT NULL,
    default_overtime_multiplier numeric(4,2) DEFAULT 1.50 NOT NULL,
    auto_checkout_buffer_minutes integer DEFAULT 120 NOT NULL,
    weekend_days smallint[] DEFAULT ARRAY[(5)::smallint, (6)::smallint] NOT NULL,
    working_days_per_month numeric(5,2) DEFAULT 30.00 NOT NULL,
    require_geofence boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    excused_time_paid_default boolean DEFAULT true NOT NULL,
    CONSTRAINT attendance_settings_absence_nonneg CHECK ((absence_deduction_days >= (0)::numeric)),
    CONSTRAINT attendance_settings_buffer_nonneg CHECK ((auto_checkout_buffer_minutes >= 0)),
    CONSTRAINT attendance_settings_days_pos CHECK ((working_days_per_month > (0)::numeric)),
    CONSTRAINT attendance_settings_mult_pos CHECK ((default_overtime_multiplier > (0)::numeric)),
    CONSTRAINT attendance_settings_tiers_is_array CHECK ((jsonb_typeof(late_deduction_tiers) = 'array'::text))
);


--
-- Name: menu_price_overrides; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_price_overrides (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    scope text NOT NULL,
    branch_id uuid,
    channel public.delivery_channel,
    target_type text NOT NULL,
    target_id uuid NOT NULL,
    price integer,
    is_available boolean,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT menu_price_overrides_scope_check CHECK ((scope = ANY (ARRAY['branch'::text, 'channel'::text, 'branch_channel'::text]))),
    CONSTRAINT menu_price_overrides_target_type_check CHECK ((target_type = ANY (ARRAY['menu_item_size'::text, 'modifier_option'::text]))),
    CONSTRAINT mpo_not_empty_ck CHECK (((price IS NOT NULL) OR (is_available IS NOT NULL))),
    CONSTRAINT mpo_price_nonneg_ck CHECK (((price IS NULL) OR (price >= 0))),
    CONSTRAINT mpo_scope_shape_ck CHECK ((((scope = 'branch'::text) AND (branch_id IS NOT NULL) AND (channel IS NULL)) OR ((scope = 'channel'::text) AND (branch_id IS NULL) AND (channel IS NOT NULL)) OR ((scope = 'branch_channel'::text) AND (branch_id IS NOT NULL) AND (channel IS NOT NULL))))
);


--
-- Name: branch_addon_overrides; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.branch_addon_overrides WITH (security_invoker='true') AS
 SELECT p.branch_id,
    p.target_id AS addon_item_id,
    p.price AS price_override,
    COALESCE(p.is_available, true) AS is_available,
    p.updated_at
   FROM (public.menu_price_overrides p
     JOIN public.modifier_options mo ON (((mo.id = p.target_id) AND (mo.legacy_source = 'addon'::text))))
  WHERE ((p.scope = 'branch'::text) AND (p.target_type = 'modifier_option'::text));


--
-- Name: branch_channel_addon_overrides; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.branch_channel_addon_overrides WITH (security_invoker='true') AS
 SELECT p.branch_id,
    p.target_id AS addon_item_id,
    p.channel,
    p.price AS price_override,
    p.is_available,
    p.updated_at
   FROM (public.menu_price_overrides p
     JOIN public.modifier_options mo ON (((mo.id = p.target_id) AND (mo.legacy_source = 'addon'::text))))
  WHERE ((p.scope = 'branch_channel'::text) AND (p.target_type = 'modifier_option'::text));


--
-- Name: menu_item_sizes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_item_sizes (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    menu_item_id uuid NOT NULL,
    label text NOT NULL,
    price integer NOT NULL,
    sort integer DEFAULT 0 NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    CONSTRAINT menu_item_sizes_price_nonneg CHECK ((price >= 0))
);


--
-- Name: branch_channel_menu_overrides; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.branch_channel_menu_overrides WITH (security_invoker='true') AS
 SELECT p.branch_id,
    ms.menu_item_id,
    p.channel,
    max(p.price) FILTER (WHERE (ms.label = 'one_size'::text)) AS price_override,
    bool_and(p.is_available) AS is_available,
    max(p.updated_at) AS updated_at
   FROM (public.menu_price_overrides p
     JOIN public.menu_item_sizes ms ON ((ms.id = p.target_id)))
  WHERE ((p.scope = 'branch_channel'::text) AND (p.target_type = 'menu_item_size'::text))
  GROUP BY p.branch_id, ms.menu_item_id, p.channel
 HAVING ((max(p.price) FILTER (WHERE (ms.label = 'one_size'::text)) IS NOT NULL) OR bool_or((p.is_available IS NOT NULL)));


--
-- Name: branch_delivery_settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.branch_delivery_settings (
    branch_id uuid NOT NULL,
    in_mall_enabled boolean DEFAULT false NOT NULL,
    outside_enabled boolean DEFAULT false NOT NULL,
    in_mall_override text DEFAULT 'auto'::text NOT NULL,
    outside_override text DEFAULT 'auto'::text NOT NULL,
    in_mall_open_time time without time zone,
    in_mall_close_time time without time zone,
    outside_open_time time without time zone,
    outside_close_time time without time zone,
    in_mall_fee integer DEFAULT 0 NOT NULL,
    prep_time_minutes integer DEFAULT 20 NOT NULL,
    max_road_distance_meters integer,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    in_mall_discount_id uuid,
    outside_discount_id uuid,
    otp_required boolean DEFAULT true NOT NULL,
    in_mall_require_location boolean DEFAULT true NOT NULL,
    umbrella_enabled boolean DEFAULT false NOT NULL,
    pickup_enabled boolean DEFAULT false NOT NULL,
    umbrella_override text DEFAULT 'auto'::text NOT NULL,
    pickup_override text DEFAULT 'auto'::text NOT NULL,
    umbrella_open_time time without time zone,
    umbrella_close_time time without time zone,
    pickup_open_time time without time zone,
    pickup_close_time time without time zone,
    umbrella_fee integer DEFAULT 0 NOT NULL,
    pickup_fee integer DEFAULT 0 NOT NULL,
    umbrella_discount_id uuid,
    pickup_discount_id uuid,
    CONSTRAINT bds_in_mall_fee_nonneg CHECK ((in_mall_fee >= 0)),
    CONSTRAINT bds_in_mall_override_chk CHECK ((in_mall_override = ANY (ARRAY['auto'::text, 'open'::text, 'closed'::text]))),
    CONSTRAINT bds_max_dist_pos CHECK (((max_road_distance_meters IS NULL) OR (max_road_distance_meters > 0))),
    CONSTRAINT bds_outside_override_chk CHECK ((outside_override = ANY (ARRAY['auto'::text, 'open'::text, 'closed'::text]))),
    CONSTRAINT bds_prep_nonneg CHECK ((prep_time_minutes >= 0))
);


--
-- Name: branch_menu_overrides; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.branch_menu_overrides WITH (security_invoker='true') AS
 SELECT p.branch_id,
    ms.menu_item_id,
    max(p.price) FILTER (WHERE (ms.label = 'one_size'::text)) AS price_override,
    COALESCE(bool_and(p.is_available), true) AS is_available,
    max(p.updated_at) AS updated_at
   FROM (public.menu_price_overrides p
     JOIN public.menu_item_sizes ms ON ((ms.id = p.target_id)))
  WHERE ((p.scope = 'branch'::text) AND (p.target_type = 'menu_item_size'::text))
  GROUP BY p.branch_id, ms.menu_item_id
 HAVING ((max(p.price) FILTER (WHERE (ms.label = 'one_size'::text)) IS NOT NULL) OR bool_or((p.is_available = false)));


--
-- Name: branch_menu_size_overrides; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.branch_menu_size_overrides WITH (security_invoker='true') AS
 SELECT p.branch_id,
    ms.menu_item_id,
    ms.label AS size_label,
    p.price AS price_override,
    p.updated_at
   FROM (public.menu_price_overrides p
     JOIN public.menu_item_sizes ms ON ((ms.id = p.target_id)))
  WHERE ((p.scope = 'branch'::text) AND (p.target_type = 'menu_item_size'::text) AND (p.price IS NOT NULL) AND (ms.label <> 'one_size'::text));


--
-- Name: branch_stock; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.branch_stock (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    org_ingredient_id uuid NOT NULL,
    on_hand numeric(12,3) DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    cost_per_unit numeric(15,2),
    par_min numeric(12,3),
    par_max numeric(12,3),
    last_counted_at timestamp with time zone,
    last_movement_at timestamp with time zone,
    CONSTRAINT chk_branch_stock_par_nonneg CHECK ((((par_min IS NULL) OR (par_min >= (0)::numeric)) AND ((par_max IS NULL) OR (par_max >= (0)::numeric)))),
    CONSTRAINT chk_branch_stock_par_order CHECK (((par_min IS NULL) OR (par_max IS NULL) OR (par_max >= par_min)))
);


--
-- Name: branch_tables; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.branch_tables (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    label text NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    section_id uuid,
    seats smallint DEFAULT 2 NOT NULL,
    shape text DEFAULT 'rect'::text NOT NULL,
    pos_x double precision DEFAULT 0 NOT NULL,
    pos_y double precision DEFAULT 0 NOT NULL,
    width double precision DEFAULT 80 NOT NULL,
    height double precision DEFAULT 80 NOT NULL,
    rotation double precision DEFAULT 0 NOT NULL,
    status text DEFAULT 'free'::text NOT NULL,
    CONSTRAINT branch_tables_seats_pos CHECK ((seats >= 0)),
    CONSTRAINT branch_tables_shape_chk CHECK ((shape = ANY (ARRAY['rect'::text, 'circle'::text]))),
    CONSTRAINT branch_tables_status_chk CHECK ((status = ANY (ARRAY['free'::text, 'held'::text, 'seated'::text, 'dirty'::text])))
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
    timezone public.timezone_name,
    printer_ip inet,
    printer_port integer DEFAULT 9100,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    printer_brand public.printer_brand,
    latitude double precision,
    longitude double precision,
    geo_radius_meters integer DEFAULT 200,
    code text NOT NULL,
    kitchen_routing_mode public.kitchen_routing_mode,
    CONSTRAINT branches_code_format CHECK ((code ~ '^[A-Z0-9]{1,6}$'::text))
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
-- Name: catalog_revision; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.catalog_revision (
    org_id uuid NOT NULL,
    revision bigint DEFAULT 1 NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
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
-- Name: category_station_routes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.category_station_routes (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    category_id uuid NOT NULL,
    station_id uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: delivery_orders; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.delivery_orders (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    channel public.delivery_channel NOT NULL,
    status public.delivery_order_status DEFAULT 'received'::public.delivery_order_status NOT NULL,
    delivery_ref text,
    customer_name text NOT NULL,
    customer_phone text NOT NULL,
    place_name text,
    floor text,
    unit_number text,
    landmark text,
    address_line text,
    delivery_notes text,
    customer_lat double precision,
    customer_lng double precision,
    delivery_zone_id uuid,
    road_distance_meters integer,
    subtotal integer DEFAULT 0 NOT NULL,
    delivery_fee integer DEFAULT 0 NOT NULL,
    total integer DEFAULT 0 NOT NULL,
    cart jsonb NOT NULL,
    deductions_snapshot jsonb DEFAULT '[]'::jsonb NOT NULL,
    payment_method_hint text,
    extra_prep_minutes integer DEFAULT 0 NOT NULL,
    otp_verified boolean DEFAULT false NOT NULL,
    order_id uuid,
    receipt_printed_at timestamp with time zone,
    confirmed_at timestamp with time zone,
    preparing_at timestamp with time zone,
    ready_at timestamp with time zone,
    out_for_delivery_at timestamp with time zone,
    delivered_at timestamp with time zone,
    cancelled_at timestamp with time zone,
    rejected_at timestamp with time zone,
    cancel_reason text,
    cancelled_by uuid,
    cancel_restocked boolean,
    idempotency_key uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    discount_id uuid,
    discount_type public.discount_type,
    discount_value integer DEFAULT 0 NOT NULL,
    discount_amount integer DEFAULT 0 NOT NULL,
    CONSTRAINT delivery_orders_discount_nonneg CHECK (((discount_value >= 0) AND (discount_amount >= 0) AND (discount_amount <= subtotal))),
    CONSTRAINT delivery_orders_money_nonneg CHECK (((subtotal >= 0) AND (delivery_fee >= 0) AND (total >= 0) AND (extra_prep_minutes >= 0)))
);


--
-- Name: delivery_otp; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.delivery_otp (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    phone text NOT NULL,
    code_hash text NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    consumed_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: delivery_ref_counters; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.delivery_ref_counters (
    branch_id uuid NOT NULL,
    business_date date NOT NULL,
    last_seq integer DEFAULT 0 NOT NULL
);


--
-- Name: delivery_zones; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.delivery_zones (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    name text NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    max_road_distance_meters integer NOT NULL,
    fee integer NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT delivery_zones_dist_pos CHECK ((max_road_distance_meters > 0)),
    CONSTRAINT delivery_zones_fee_nonneg CHECK ((fee >= 0))
);


--
-- Name: departments; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.departments (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    manager_user_id uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT departments_name_len CHECK (((char_length(name) >= 1) AND (char_length(name) <= 120)))
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
-- Name: floor_sections; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.floor_sections (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    name text NOT NULL,
    ordering integer DEFAULT 0 NOT NULL,
    canvas_w integer DEFAULT 1000 NOT NULL,
    canvas_h integer DEFAULT 700 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: goods_receipt_lines; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.goods_receipt_lines (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    goods_receipt_id uuid NOT NULL,
    purchase_order_line_id uuid,
    org_ingredient_id uuid NOT NULL,
    quantity numeric(12,3) NOT NULL,
    unit_cost bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: goods_receipts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.goods_receipts (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    purchase_order_id uuid,
    supplier_id uuid,
    is_return boolean DEFAULT false NOT NULL,
    reference text,
    note text,
    received_by uuid NOT NULL,
    received_at timestamp with time zone DEFAULT now() NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: ingredient_categories; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.ingredient_categories (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    slug text NOT NULL,
    name text NOT NULL,
    sort_order integer DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT ingredient_categories_name_check CHECK (((length(TRIM(BOTH FROM name)) >= 1) AND (length(TRIM(BOTH FROM name)) <= 80))),
    CONSTRAINT ingredient_categories_slug_check CHECK ((slug ~ '^[a-z0-9_]{1,64}$'::text))
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
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    branch_id uuid
);


--
-- Name: integration_credentials; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.integration_credentials (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    name text NOT NULL,
    username text NOT NULL,
    secret_hash text NOT NULL,
    created_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    last_used_at timestamp with time zone,
    revoked_at timestamp with time zone,
    CONSTRAINT integration_credentials_name_len CHECK (((char_length(name) >= 1) AND (char_length(name) <= 120))),
    CONSTRAINT integration_credentials_username_len CHECK (((char_length(username) >= 3) AND (char_length(username) <= 64)))
);


--
-- Name: inventory_movements; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.inventory_movements (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    org_ingredient_id uuid NOT NULL,
    branch_stock_id uuid,
    type public.inventory_movement_type NOT NULL,
    quantity numeric(12,3) NOT NULL,
    balance_after numeric(12,3) NOT NULL,
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
-- Name: item_sizes; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.item_sizes WITH (security_invoker='true') AS
 SELECT id,
    menu_item_id,
    label,
    price AS price_override,
    is_active
   FROM public.menu_item_sizes z
  WHERE (NOT ((label = 'one_size'::text) AND (id = (md5(((menu_item_id)::text || ':one_size'::text)))::uuid)));


--
-- Name: kitchen_stations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.kitchen_stations (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    name text NOT NULL,
    name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
    sort_order integer DEFAULT 0 NOT NULL,
    printer_brand public.printer_brand,
    printer_ip text,
    printer_port integer,
    is_default boolean DEFAULT false NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    CONSTRAINT kitchen_stations_port_chk CHECK (((printer_port IS NULL) OR ((printer_port >= 1) AND (printer_port <= 65535))))
);


--
-- Name: kitchen_ticket_items; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.kitchen_ticket_items (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    kitchen_ticket_id uuid NOT NULL,
    station_id uuid,
    menu_item_id uuid,
    line jsonb NOT NULL,
    qty integer DEFAULT 1 NOT NULL,
    bumped_at timestamp with time zone,
    bumped_by uuid,
    voided_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: kitchen_tickets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.kitchen_tickets (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    source_type text NOT NULL,
    source_id uuid NOT NULL,
    table_label text,
    kitchen_ref text,
    round_number integer DEFAULT 1 NOT NULL,
    status public.kitchen_ticket_status DEFAULT 'firing'::public.kitchen_ticket_status NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    ready_at timestamp with time zone,
    voided_at timestamp with time zone,
    CONSTRAINT kitchen_tickets_source_chk CHECK ((source_type = ANY (ARRAY['order'::text, 'open_ticket'::text])))
);


--
-- Name: leave_balances; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.leave_balances (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    leave_type_id uuid NOT NULL,
    year integer NOT NULL,
    entitled_days numeric(6,2) DEFAULT 0 NOT NULL,
    used_days numeric(6,2) DEFAULT 0 NOT NULL,
    carried_over_days numeric(6,2) DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT leave_balances_nonneg CHECK (((entitled_days >= (0)::numeric) AND (used_days >= (0)::numeric) AND (carried_over_days >= (0)::numeric))),
    CONSTRAINT leave_balances_year_sane CHECK (((year >= 2000) AND (year <= 2200)))
);


--
-- Name: leave_types; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.leave_types (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    is_paid boolean DEFAULT true NOT NULL,
    annual_quota_days numeric(6,2),
    requires_approval boolean DEFAULT true NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT leave_types_name_len CHECK (((char_length(name) >= 1) AND (char_length(name) <= 120))),
    CONSTRAINT leave_types_quota_nonneg CHECK (((annual_quota_days IS NULL) OR (annual_quota_days >= (0)::numeric)))
);


--
-- Name: margin_targets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.margin_targets (
    org_id uuid NOT NULL,
    branch_id uuid,
    target_pct numeric(5,2) NOT NULL,
    updated_by uuid,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT margin_targets_target_pct_check CHECK (((target_pct > (0)::numeric) AND (target_pct < (100)::numeric)))
);


--
-- Name: menu_decisions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_decisions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid,
    menu_item_id uuid NOT NULL,
    size_label text DEFAULT 'one_size'::text NOT NULL,
    signal_kind text NOT NULL,
    action text NOT NULL,
    detail jsonb DEFAULT '{}'::jsonb NOT NULL,
    baseline jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT menu_decisions_action_check CHECK ((action = ANY (ARRAY['acted'::text, 'dismissed'::text, 'snoozed'::text]))),
    CONSTRAINT menu_decisions_signal_kind_check CHECK ((signal_kind = ANY (ARRAY['below_cost'::text, 'below_target'::text, 'cost_spike'::text, 'price_candidate'::text, 'removal_candidate'::text, 'recipe_incomplete'::text])))
);


--
-- Name: menu_item_modifier_groups; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_item_modifier_groups (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    menu_item_id uuid NOT NULL,
    group_id uuid NOT NULL,
    sort integer DEFAULT 0 NOT NULL,
    min_override integer,
    max_override integer,
    is_required_override boolean,
    included_option_ids uuid[],
    legacy_origin text,
    CONSTRAINT menu_item_modifier_groups_legacy_origin_check CHECK (((legacy_origin IS NULL) OR (legacy_origin = ANY (ARRAY['slot'::text, 'allowlist'::text, 'options'::text]))))
);


--
-- Name: menu_item_addon_slots; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.menu_item_addon_slots WITH (security_invoker='true') AS
 SELECT mimg.id,
    mimg.menu_item_id,
    mg.legacy_addon_type AS addon_type,
    COALESCE(mimg.is_required_override, mg.is_required) AS is_required,
    COALESCE(mimg.min_override, mg.min_selections) AS min_selections,
    COALESCE(mimg.max_override, mg.max_selections) AS max_selections,
    now() AS created_at,
    mg.name AS label,
    mg.name_translations AS label_translations
   FROM (public.menu_item_modifier_groups mimg
     JOIN public.modifier_groups mg ON ((mg.id = mimg.group_id)))
  WHERE ((mimg.legacy_origin = 'slot'::text) AND (mg.legacy_addon_type IS NOT NULL));


--
-- Name: menu_item_allowed_addons; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.menu_item_allowed_addons WITH (security_invoker='true') AS
 SELECT mimg.menu_item_id,
    u.opt AS addon_item_id,
    ((u.ord - 1))::integer AS sort_order,
    now() AS created_at
   FROM ((public.menu_item_modifier_groups mimg
     CROSS JOIN LATERAL unnest(mimg.included_option_ids) WITH ORDINALITY u(opt, ord))
     JOIN public.modifier_options mo ON (((mo.id = u.opt) AND (mo.legacy_source = 'addon'::text))))
  WHERE (mimg.included_option_ids IS NOT NULL);


--
-- Name: menu_item_optional_fields; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.menu_item_optional_fields WITH (security_invoker='true') AS
 SELECT mo.id,
    mimg.menu_item_id,
    mo.name,
    mo.price,
    rl.ingredient_id AS org_ingredient_id,
    oi.name AS ingredient_name,
    rl.unit AS ingredient_unit,
    rl.quantity AS quantity_used,
    NULL::text AS size_label,
    mo.is_active,
    mo.created_at,
    mo.updated_at,
    mo.name_translations
   FROM (((public.modifier_options mo
     JOIN public.menu_item_modifier_groups mimg ON (((mimg.group_id = mo.group_id) AND (mimg.legacy_origin = 'options'::text))))
     LEFT JOIN public.recipe_lines rl ON (((rl.owner_type = 'modifier_option'::text) AND (rl.owner_id = mo.id))))
     LEFT JOIN public.org_ingredients oi ON ((oi.id = rl.ingredient_id)))
  WHERE (mo.legacy_source = 'optional'::text);


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
-- Name: menu_item_recipes; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.menu_item_recipes WITH (security_invoker='true') AS
 SELECT rl.id,
    ms.menu_item_id,
    ms.label AS size_label,
    rl.quantity AS quantity_used,
    rl.created_at,
    rl.updated_at,
    oi.name AS ingredient_name,
    rl.unit AS ingredient_unit,
    rl.ingredient_id AS org_ingredient_id
   FROM ((public.recipe_lines rl
     JOIN public.menu_item_sizes ms ON ((ms.id = rl.owner_id)))
     JOIN public.org_ingredients oi ON ((oi.id = rl.ingredient_id)))
  WHERE (rl.owner_type = 'item_size'::text);


--
-- Name: menu_item_station_routes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.menu_item_station_routes (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    branch_id uuid NOT NULL,
    menu_item_id uuid NOT NULL,
    station_id uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
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
-- Name: open_ticket_items; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.open_ticket_items (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    open_ticket_id uuid NOT NULL,
    round_id uuid NOT NULL,
    menu_item_id uuid,
    line jsonb NOT NULL,
    deductions_snapshot jsonb DEFAULT '[]'::jsonb NOT NULL,
    line_total integer DEFAULT 0 NOT NULL,
    voided_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT oti_line_total_nonneg CHECK ((line_total >= 0))
);


--
-- Name: open_ticket_rounds; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.open_ticket_rounds (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    open_ticket_id uuid NOT NULL,
    round_number integer NOT NULL,
    fired_by uuid NOT NULL,
    idempotency_key uuid,
    fired_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: open_tickets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.open_tickets (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    table_id uuid,
    ticket_ref text,
    status public.open_ticket_status DEFAULT 'open'::public.open_ticket_status NOT NULL,
    opened_by uuid NOT NULL,
    customer_name text,
    notes text,
    guest_count integer,
    subtotal integer DEFAULT 0 NOT NULL,
    order_id uuid,
    settled_by uuid,
    settled_shift_id uuid,
    idempotency_key uuid,
    opened_at timestamp with time zone DEFAULT now() NOT NULL,
    ready_at timestamp with time zone,
    settled_at timestamp with time zone,
    voided_at timestamp with time zone,
    void_reason text,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    discount_id uuid,
    discount_type text,
    discount_value integer,
    CONSTRAINT open_tickets_money_nonneg CHECK ((subtotal >= 0))
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
    cost_missing boolean DEFAULT true NOT NULL,
    price_flagged boolean DEFAULT false NOT NULL
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
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    is_cash boolean
);


--
-- Name: order_ref_counters; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.order_ref_counters (
    branch_id uuid NOT NULL,
    business_date date NOT NULL,
    last_seq integer DEFAULT 0 NOT NULL
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
    tip_is_cash boolean,
    order_ref text NOT NULL,
    price_flagged boolean DEFAULT false NOT NULL,
    price_expected_total integer,
    order_type text DEFAULT 'dine_in'::text NOT NULL,
    delivery_fee integer DEFAULT 0 NOT NULL,
    delivery_order_id uuid,
    waiter_id uuid,
    CONSTRAINT chk_orders_voided_consistency CHECK ((((voided_at IS NULL) AND (voided_by IS NULL)) OR ((voided_at IS NOT NULL) AND (voided_by IS NOT NULL) AND (status = 'voided'::public.order_status)))),
    CONSTRAINT orders_delivery_fee_nonneg CHECK ((delivery_fee >= 0)),
    CONSTRAINT orders_order_type_chk CHECK ((order_type = ANY (ARRAY['dine_in'::text, 'delivery'::text])))
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
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    visible_in_integrations boolean DEFAULT true NOT NULL
);


--
-- Name: COLUMN org_payment_methods.visible_in_integrations; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.org_payment_methods.visible_in_integrations IS 'When false, orders tendered with this method are excluded entirely from GET /integrations/analytics/orders — both the rows and the aggregates.';


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
    onboarding_completed_at timestamp with time zone,
    stocktake_variance_threshold_pct numeric(6,3) DEFAULT 10 NOT NULL,
    timezone public.timezone_name DEFAULT 'Africa/Cairo'::public.timezone_name NOT NULL,
    lan_secret bytea DEFAULT public.gen_random_bytes(32) NOT NULL,
    is_demo boolean DEFAULT false NOT NULL,
    demo_expires_at timestamp with time zone
);


--
-- Name: payroll_bonuses; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.payroll_bonuses (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    amount_piastres bigint,
    percent_of_base numeric(5,2),
    reason text NOT NULL,
    effective_date date NOT NULL,
    source text DEFAULT 'manual'::text NOT NULL,
    status text DEFAULT 'approved'::text NOT NULL,
    created_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT payroll_bonuses_amount_pos CHECK (((amount_piastres IS NULL) OR (amount_piastres > 0))),
    CONSTRAINT payroll_bonuses_one_basis CHECK ((((amount_piastres IS NOT NULL) AND (percent_of_base IS NULL)) OR ((amount_piastres IS NULL) AND (percent_of_base IS NOT NULL)))),
    CONSTRAINT payroll_bonuses_percent_pos CHECK (((percent_of_base IS NULL) OR (percent_of_base > (0)::numeric))),
    CONSTRAINT payroll_bonuses_source_chk CHECK ((source = ANY (ARRAY['manual'::text, 'performance'::text, 'commission'::text]))),
    CONSTRAINT payroll_bonuses_status_chk CHECK ((status = ANY (ARRAY['pending'::text, 'approved'::text, 'rejected'::text])))
);


--
-- Name: payroll_deductions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.payroll_deductions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    amount_piastres bigint,
    percent_of_base numeric(5,2),
    reason text NOT NULL,
    effective_date date NOT NULL,
    source text DEFAULT 'manual'::text NOT NULL,
    attendance_record_id uuid,
    status text DEFAULT 'approved'::text NOT NULL,
    created_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    original_amount_piastres bigint,
    overridden_at timestamp with time zone,
    overridden_by uuid,
    override_reason text,
    waived_at timestamp with time zone,
    waived_by uuid,
    waive_reason text,
    CONSTRAINT payroll_deductions_amount_pos CHECK (((amount_piastres IS NULL) OR (amount_piastres > 0))),
    CONSTRAINT payroll_deductions_one_basis CHECK ((((amount_piastres IS NOT NULL) AND (percent_of_base IS NULL)) OR ((amount_piastres IS NULL) AND (percent_of_base IS NOT NULL)))),
    CONSTRAINT payroll_deductions_override_reason_chk CHECK (((overridden_at IS NULL) OR (override_reason IS NOT NULL))),
    CONSTRAINT payroll_deductions_percent_range CHECK (((percent_of_base IS NULL) OR ((percent_of_base > (0)::numeric) AND (percent_of_base <= (100)::numeric)))),
    CONSTRAINT payroll_deductions_source_chk CHECK ((source = ANY (ARRAY['manual'::text, 'late_penalty'::text, 'absence'::text]))),
    CONSTRAINT payroll_deductions_status_chk CHECK ((status = ANY (ARRAY['pending'::text, 'approved'::text, 'rejected'::text]))),
    CONSTRAINT payroll_deductions_waive_reason_chk CHECK (((waived_at IS NULL) OR (waive_reason IS NOT NULL)))
);


--
-- Name: payroll_periods; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.payroll_periods (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    start_date date NOT NULL,
    end_date date NOT NULL,
    status text DEFAULT 'draft'::text NOT NULL,
    employee_count integer DEFAULT 0 NOT NULL,
    total_net_piastres bigint DEFAULT 0 NOT NULL,
    generated_at timestamp with time zone,
    generated_by uuid,
    paid_at timestamp with time zone,
    closed_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT payroll_periods_dates_ordered CHECK ((end_date >= start_date)),
    CONSTRAINT payroll_periods_name_len CHECK (((char_length(name) >= 1) AND (char_length(name) <= 120))),
    CONSTRAINT payroll_periods_status_chk CHECK ((status = ANY (ARRAY['draft'::text, 'generated'::text, 'paid'::text, 'closed'::text])))
);


--
-- Name: payslips; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.payslips (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    payroll_period_id uuid NOT NULL,
    user_id uuid NOT NULL,
    base_salary_piastres bigint DEFAULT 0 NOT NULL,
    worked_days numeric(6,2) DEFAULT 0 NOT NULL,
    absent_days numeric(6,2) DEFAULT 0 NOT NULL,
    leave_days numeric(6,2) DEFAULT 0 NOT NULL,
    late_minutes integer DEFAULT 0 NOT NULL,
    overtime_minutes integer DEFAULT 0 NOT NULL,
    overtime_piastres bigint DEFAULT 0 NOT NULL,
    bonuses_piastres bigint DEFAULT 0 NOT NULL,
    deductions_piastres bigint DEFAULT 0 NOT NULL,
    advance_installment_piastres bigint DEFAULT 0 NOT NULL,
    net_piastres bigint DEFAULT 0 NOT NULL,
    breakdown jsonb DEFAULT '{}'::jsonb NOT NULL,
    generated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT payslips_components_nonneg CHECK (((base_salary_piastres >= 0) AND (overtime_piastres >= 0) AND (bonuses_piastres >= 0) AND (deductions_piastres >= 0) AND (advance_installment_piastres >= 0))),
    CONSTRAINT payslips_days_nonneg CHECK (((worked_days >= (0)::numeric) AND (absent_days >= (0)::numeric) AND (leave_days >= (0)::numeric))),
    CONSTRAINT payslips_minutes_nonneg CHECK (((late_minutes >= 0) AND (overtime_minutes >= 0)))
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
-- Name: qr_short_links; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.qr_short_links (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid,
    kind text NOT NULL,
    target_ref text NOT NULL,
    long_url text NOT NULL,
    short_code text NOT NULL,
    short_url text NOT NULL,
    label text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
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
-- Name: salary_advances; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.salary_advances (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    amount_piastres bigint NOT NULL,
    installments integer DEFAULT 1 NOT NULL,
    monthly_installment_piastres bigint NOT NULL,
    remaining_piastres bigint NOT NULL,
    reason text,
    status text DEFAULT 'pending'::text NOT NULL,
    decided_by uuid,
    decided_at timestamp with time zone,
    decision_note text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT salary_advances_amount_pos CHECK ((amount_piastres > 0)),
    CONSTRAINT salary_advances_installments_pos CHECK ((installments > 0)),
    CONSTRAINT salary_advances_monthly_pos CHECK ((monthly_installment_piastres > 0)),
    CONSTRAINT salary_advances_remaining_range CHECK (((remaining_piastres >= 0) AND (remaining_piastres <= amount_piastres))),
    CONSTRAINT salary_advances_status_chk CHECK ((status = ANY (ARRAY['pending'::text, 'approved'::text, 'rejected'::text, 'settled'::text, 'cancelled'::text])))
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
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    client_ref uuid
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
    force_close_reason text,
    till_id uuid NOT NULL
);


--
-- Name: staff_documents; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.staff_documents (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    kind text DEFAULT 'other'::text NOT NULL,
    title text NOT NULL,
    file_url text NOT NULL,
    expires_on date,
    uploaded_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT staff_documents_title_len CHECK (((char_length(title) >= 1) AND (char_length(title) <= 200)))
);


--
-- Name: staff_profiles; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.staff_profiles (
    user_id uuid NOT NULL,
    org_id uuid NOT NULL,
    department_id uuid,
    employee_code text,
    job_title text,
    hire_date date,
    termination_date date,
    employment_status text DEFAULT 'active'::text NOT NULL,
    base_salary_piastres bigint DEFAULT 0 NOT NULL,
    national_id text,
    photo_url text,
    emergency_contact_name text,
    emergency_contact_phone text,
    notes text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT staff_profiles_salary_nonneg CHECK ((base_salary_piastres >= 0)),
    CONSTRAINT staff_profiles_status_chk CHECK ((employment_status = ANY (ARRAY['active'::text, 'suspended'::text, 'terminated'::text]))),
    CONSTRAINT staff_profiles_termination_after_hire CHECK (((termination_date IS NULL) OR (hire_date IS NULL) OR (termination_date >= hire_date))),
    CONSTRAINT staff_profiles_termination_chk CHECK ((((employment_status = 'terminated'::text) AND (termination_date IS NOT NULL)) OR ((employment_status <> 'terminated'::text) AND (termination_date IS NULL))))
);


--
-- Name: staff_requests; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.staff_requests (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    kind text NOT NULL,
    on_date date NOT NULL,
    end_date date,
    from_time time without time zone,
    to_time time without time zone,
    leave_type_id uuid,
    is_half_day boolean DEFAULT false NOT NULL,
    title text,
    location text,
    reason text,
    status text DEFAULT 'pending'::text NOT NULL,
    is_paid boolean,
    decided_by uuid,
    decided_at timestamp with time zone,
    decision_note text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    attendance_record_id uuid,
    CONSTRAINT staff_requests_decision_stamped CHECK (((status = ANY (ARRAY['pending'::text, 'cancelled'::text])) OR (decided_at IS NOT NULL))),
    CONSTRAINT staff_requests_half_day_chk CHECK (((NOT is_half_day) OR ((kind = 'leave'::text) AND (on_date = end_date)))),
    CONSTRAINT staff_requests_kind_chk CHECK ((kind = ANY (ARRAY['leave'::text, 'late_arrival'::text, 'early_departure'::text, 'excuse'::text, 'mission'::text, 'correction'::text]))),
    CONSTRAINT staff_requests_shape_chk CHECK (
CASE kind
    WHEN 'leave'::text THEN ((leave_type_id IS NOT NULL) AND (end_date IS NOT NULL) AND (from_time IS NULL) AND (to_time IS NULL) AND (attendance_record_id IS NULL))
    WHEN 'late_arrival'::text THEN ((to_time IS NOT NULL) AND (from_time IS NULL) AND (end_date IS NULL) AND (leave_type_id IS NULL) AND (attendance_record_id IS NULL))
    WHEN 'early_departure'::text THEN ((from_time IS NOT NULL) AND (to_time IS NULL) AND (end_date IS NULL) AND (leave_type_id IS NULL) AND (attendance_record_id IS NULL))
    WHEN 'excuse'::text THEN ((from_time IS NOT NULL) AND (to_time IS NOT NULL) AND (end_date IS NULL) AND (leave_type_id IS NULL) AND (attendance_record_id IS NULL))
    WHEN 'mission'::text THEN ((title IS NOT NULL) AND (end_date IS NOT NULL) AND (leave_type_id IS NULL) AND (attendance_record_id IS NULL))
    WHEN 'correction'::text THEN ((attendance_record_id IS NOT NULL) AND ((from_time IS NOT NULL) OR (to_time IS NOT NULL)) AND (end_date IS NULL) AND (leave_type_id IS NULL))
    ELSE false
END),
    CONSTRAINT staff_requests_span_ordered CHECK (((end_date IS NULL) OR (end_date >= on_date))),
    CONSTRAINT staff_requests_status_chk CHECK ((status = ANY (ARRAY['pending'::text, 'approved'::text, 'rejected'::text, 'cancelled'::text]))),
    CONSTRAINT staff_requests_window_ordered CHECK (((from_time IS NULL) OR (to_time IS NULL) OR (to_time > from_time)))
);


--
-- Name: COLUMN staff_requests.attendance_record_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.staff_requests.attendance_record_id IS 'The record a `correction` proposes to fix. NULL for every other kind.';


--
-- Name: staff_schedule_overrides; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.staff_schedule_overrides (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    on_date date NOT NULL,
    work_shift_id uuid,
    reason text,
    created_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: staff_schedules; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.staff_schedules (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    work_shift_id uuid NOT NULL,
    day_of_week smallint,
    effective_from date DEFAULT CURRENT_DATE NOT NULL,
    effective_to date,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT staff_schedules_dates_ordered CHECK (((effective_to IS NULL) OR (effective_to >= effective_from))),
    CONSTRAINT staff_schedules_dow_range CHECK (((day_of_week IS NULL) OR ((day_of_week >= 0) AND (day_of_week <= 6))))
);


--
-- Name: stock_transfers; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.stock_transfers (
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
-- Name: stocktake_items; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.stocktake_items (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    stocktake_id uuid NOT NULL,
    org_ingredient_id uuid NOT NULL,
    branch_stock_id uuid,
    opening_qty numeric(12,3) NOT NULL,
    counted_qty numeric(12,3),
    unit_cost bigint,
    note text,
    counted_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    variance_reason public.stocktake_variance_reason,
    book_qty numeric(12,3),
    variance numeric(12,3) GENERATED ALWAYS AS ((counted_qty - COALESCE(book_qty, opening_qty))) STORED
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
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    scope jsonb DEFAULT '{"kind": "full"}'::jsonb NOT NULL
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
-- Name: table_transfer_requests; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.table_transfer_requests (
    id uuid NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    occupant_kind text DEFAULT 'open_ticket'::text NOT NULL,
    occupant_id uuid NOT NULL,
    from_table_id uuid,
    target_section_id uuid,
    target_table_id uuid,
    note text,
    status text DEFAULT 'waiting'::text NOT NULL,
    requested_by uuid,
    fulfilled_table_id uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    resolved_at timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT table_transfer_requests_occupant_kind_check CHECK ((occupant_kind = 'open_ticket'::text)),
    CONSTRAINT table_transfer_requests_status_check CHECK ((status = ANY (ARRAY['waiting'::text, 'fulfilled'::text, 'cancelled'::text]))),
    CONSTRAINT ttr_note_len CHECK (((note IS NULL) OR (char_length(note) <= 500))),
    CONSTRAINT ttr_target_present CHECK (((target_section_id IS NOT NULL) OR (target_table_id IS NOT NULL)))
);


--
-- Name: ticket_ref_counters; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.ticket_ref_counters (
    branch_id uuid NOT NULL,
    business_date date NOT NULL,
    last_seq integer DEFAULT 0 NOT NULL
);


--
-- Name: tills; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tills (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    name text NOT NULL,
    is_default boolean DEFAULT false NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone
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
    offline_pin_hash text,
    CONSTRAINT chk_login_method CHECK (((password_hash IS NOT NULL) OR (pin_hash IS NOT NULL))),
    CONSTRAINT chk_super_admin_no_org CHECK ((((role = 'super_admin'::public.user_role) AND (org_id IS NULL)) OR ((role <> 'super_admin'::public.user_role) AND (org_id IS NOT NULL))))
);


--
-- Name: whatsapp_gateway_settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.whatsapp_gateway_settings (
    id boolean DEFAULT true NOT NULL,
    paused boolean DEFAULT false NOT NULL,
    paused_at timestamp with time zone,
    paused_by uuid,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT whatsapp_gateway_settings_singleton CHECK (id)
);


--
-- Name: work_shifts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.work_shifts (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid,
    name text NOT NULL,
    start_time time without time zone NOT NULL,
    end_time time without time zone NOT NULL,
    crosses_midnight boolean GENERATED ALWAYS AS ((end_time <= start_time)) STORED,
    grace_minutes integer DEFAULT 15 NOT NULL,
    break_minutes integer DEFAULT 0 NOT NULL,
    paid_break boolean DEFAULT true NOT NULL,
    half_day_threshold_minutes integer,
    overtime_threshold_minutes integer DEFAULT 15 NOT NULL,
    overtime_multiplier numeric(4,2) DEFAULT 1.50 NOT NULL,
    checkin_window_minutes integer DEFAULT 120 NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT work_shifts_break_nonneg CHECK ((break_minutes >= 0)),
    CONSTRAINT work_shifts_grace_nonneg CHECK ((grace_minutes >= 0)),
    CONSTRAINT work_shifts_halfday_pos CHECK (((half_day_threshold_minutes IS NULL) OR (half_day_threshold_minutes > 0))),
    CONSTRAINT work_shifts_multiplier_pos CHECK ((overtime_multiplier > (0)::numeric)),
    CONSTRAINT work_shifts_name_len CHECK (((char_length(name) >= 1) AND (char_length(name) <= 120))),
    CONSTRAINT work_shifts_ot_nonneg CHECK ((overtime_threshold_minutes >= 0)),
    CONSTRAINT work_shifts_window_pos CHECK ((checkin_window_minutes > 0))
);


--
-- Name: databases databases_pkey; Type: CONSTRAINT; Schema: _sqlx_test; Owner: -
--

ALTER TABLE ONLY _sqlx_test.databases
    ADD CONSTRAINT databases_pkey PRIMARY KEY (db_name);


--
-- Name: _sqlx_migrations _sqlx_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public._sqlx_migrations
    ADD CONSTRAINT _sqlx_migrations_pkey PRIMARY KEY (version);


--
-- Name: ai_conversations ai_conversations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ai_conversations
    ADD CONSTRAINT ai_conversations_pkey PRIMARY KEY (id);


--
-- Name: ai_messages ai_messages_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ai_messages
    ADD CONSTRAINT ai_messages_pkey PRIMARY KEY (id);


--
-- Name: ai_messages ai_messages_seq_unique; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ai_messages
    ADD CONSTRAINT ai_messages_seq_unique UNIQUE (conversation_id, seq);


--
-- Name: attendance_records attendance_records_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.attendance_records
    ADD CONSTRAINT attendance_records_pkey PRIMARY KEY (id);


--
-- Name: attendance_settings attendance_settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.attendance_settings
    ADD CONSTRAINT attendance_settings_pkey PRIMARY KEY (id);


--
-- Name: branch_delivery_settings branch_delivery_settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_delivery_settings
    ADD CONSTRAINT branch_delivery_settings_pkey PRIMARY KEY (branch_id);


--
-- Name: branch_stock branch_stock_branch_id_org_ingredient_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_stock
    ADD CONSTRAINT branch_stock_branch_id_org_ingredient_id_key UNIQUE (branch_id, org_ingredient_id);


--
-- Name: branch_stock branch_stock_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_stock
    ADD CONSTRAINT branch_stock_pkey PRIMARY KEY (id);


--
-- Name: branch_tables branch_tables_branch_id_label_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_tables
    ADD CONSTRAINT branch_tables_branch_id_label_key UNIQUE (branch_id, label);


--
-- Name: branch_tables branch_tables_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_tables
    ADD CONSTRAINT branch_tables_pkey PRIMARY KEY (id);


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
-- Name: catalog_revision catalog_revision_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.catalog_revision
    ADD CONSTRAINT catalog_revision_pkey PRIMARY KEY (org_id);


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
-- Name: category_station_routes category_station_routes_branch_id_category_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.category_station_routes
    ADD CONSTRAINT category_station_routes_branch_id_category_id_key UNIQUE (branch_id, category_id);


--
-- Name: category_station_routes category_station_routes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.category_station_routes
    ADD CONSTRAINT category_station_routes_pkey PRIMARY KEY (id);


--
-- Name: delivery_orders delivery_orders_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_orders
    ADD CONSTRAINT delivery_orders_pkey PRIMARY KEY (id);


--
-- Name: delivery_otp delivery_otp_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_otp
    ADD CONSTRAINT delivery_otp_pkey PRIMARY KEY (id);


--
-- Name: delivery_ref_counters delivery_ref_counters_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_ref_counters
    ADD CONSTRAINT delivery_ref_counters_pkey PRIMARY KEY (branch_id, business_date);


--
-- Name: delivery_zones delivery_zones_dist_unique; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_zones
    ADD CONSTRAINT delivery_zones_dist_unique UNIQUE (branch_id, max_road_distance_meters);


--
-- Name: delivery_zones delivery_zones_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_zones
    ADD CONSTRAINT delivery_zones_pkey PRIMARY KEY (id);


--
-- Name: departments departments_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.departments
    ADD CONSTRAINT departments_pkey PRIMARY KEY (id);


--
-- Name: discounts discounts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.discounts
    ADD CONSTRAINT discounts_pkey PRIMARY KEY (id);


--
-- Name: floor_sections floor_sections_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.floor_sections
    ADD CONSTRAINT floor_sections_pkey PRIMARY KEY (id);


--
-- Name: goods_receipt_lines goods_receipt_lines_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.goods_receipt_lines
    ADD CONSTRAINT goods_receipt_lines_pkey PRIMARY KEY (id);


--
-- Name: goods_receipts goods_receipts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.goods_receipts
    ADD CONSTRAINT goods_receipts_pkey PRIMARY KEY (id);


--
-- Name: ingredient_categories ingredient_categories_org_id_slug_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ingredient_categories
    ADD CONSTRAINT ingredient_categories_org_id_slug_key UNIQUE (org_id, slug);


--
-- Name: ingredient_categories ingredient_categories_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ingredient_categories
    ADD CONSTRAINT ingredient_categories_pkey PRIMARY KEY (id);


--
-- Name: ingredient_cost_history ingredient_cost_history_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ingredient_cost_history
    ADD CONSTRAINT ingredient_cost_history_pkey PRIMARY KEY (id);


--
-- Name: integration_credentials integration_credentials_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.integration_credentials
    ADD CONSTRAINT integration_credentials_pkey PRIMARY KEY (id);


--
-- Name: inventory_movements inventory_movements_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inventory_movements
    ADD CONSTRAINT inventory_movements_pkey PRIMARY KEY (id);


--
-- Name: kitchen_stations kitchen_stations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_stations
    ADD CONSTRAINT kitchen_stations_pkey PRIMARY KEY (id);


--
-- Name: kitchen_ticket_items kitchen_ticket_items_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_ticket_items
    ADD CONSTRAINT kitchen_ticket_items_pkey PRIMARY KEY (id);


--
-- Name: kitchen_tickets kitchen_tickets_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_tickets
    ADD CONSTRAINT kitchen_tickets_pkey PRIMARY KEY (id);


--
-- Name: leave_balances leave_balances_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.leave_balances
    ADD CONSTRAINT leave_balances_pkey PRIMARY KEY (id);


--
-- Name: leave_types leave_types_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.leave_types
    ADD CONSTRAINT leave_types_pkey PRIMARY KEY (id);


--
-- Name: margin_targets margin_targets_org_id_branch_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.margin_targets
    ADD CONSTRAINT margin_targets_org_id_branch_id_key UNIQUE NULLS NOT DISTINCT (org_id, branch_id);


--
-- Name: menu_decisions menu_decisions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_decisions
    ADD CONSTRAINT menu_decisions_pkey PRIMARY KEY (id);


--
-- Name: menu_item_modifier_groups menu_item_modifier_groups_item_group_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_modifier_groups
    ADD CONSTRAINT menu_item_modifier_groups_item_group_key UNIQUE (menu_item_id, group_id);


--
-- Name: menu_item_modifier_groups menu_item_modifier_groups_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_modifier_groups
    ADD CONSTRAINT menu_item_modifier_groups_pkey PRIMARY KEY (id);


--
-- Name: menu_item_price_epochs menu_item_price_epochs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_price_epochs
    ADD CONSTRAINT menu_item_price_epochs_pkey PRIMARY KEY (id);


--
-- Name: menu_item_sizes menu_item_sizes_item_label_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_sizes
    ADD CONSTRAINT menu_item_sizes_item_label_key UNIQUE (menu_item_id, label);


--
-- Name: menu_item_sizes menu_item_sizes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_sizes
    ADD CONSTRAINT menu_item_sizes_pkey PRIMARY KEY (id);


--
-- Name: menu_item_station_routes menu_item_station_routes_branch_id_menu_item_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_station_routes
    ADD CONSTRAINT menu_item_station_routes_branch_id_menu_item_id_key UNIQUE (branch_id, menu_item_id);


--
-- Name: menu_item_station_routes menu_item_station_routes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_station_routes
    ADD CONSTRAINT menu_item_station_routes_pkey PRIMARY KEY (id);


--
-- Name: menu_items menu_items_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_items
    ADD CONSTRAINT menu_items_pkey PRIMARY KEY (id);


--
-- Name: menu_price_overrides menu_price_overrides_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_price_overrides
    ADD CONSTRAINT menu_price_overrides_pkey PRIMARY KEY (id);


--
-- Name: modifier_groups modifier_groups_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.modifier_groups
    ADD CONSTRAINT modifier_groups_pkey PRIMARY KEY (id);


--
-- Name: modifier_options modifier_options_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.modifier_options
    ADD CONSTRAINT modifier_options_pkey PRIMARY KEY (id);


--
-- Name: open_ticket_items open_ticket_items_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_ticket_items
    ADD CONSTRAINT open_ticket_items_pkey PRIMARY KEY (id);


--
-- Name: open_ticket_rounds open_ticket_rounds_open_ticket_id_round_number_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_ticket_rounds
    ADD CONSTRAINT open_ticket_rounds_open_ticket_id_round_number_key UNIQUE (open_ticket_id, round_number);


--
-- Name: open_ticket_rounds open_ticket_rounds_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_ticket_rounds
    ADD CONSTRAINT open_ticket_rounds_pkey PRIMARY KEY (id);


--
-- Name: open_tickets open_tickets_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_tickets
    ADD CONSTRAINT open_tickets_pkey PRIMARY KEY (id);


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
-- Name: order_ref_counters order_ref_counters_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_ref_counters
    ADD CONSTRAINT order_ref_counters_pkey PRIMARY KEY (branch_id, business_date);


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
-- Name: payroll_bonuses payroll_bonuses_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_bonuses
    ADD CONSTRAINT payroll_bonuses_pkey PRIMARY KEY (id);


--
-- Name: payroll_deductions payroll_deductions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_deductions
    ADD CONSTRAINT payroll_deductions_pkey PRIMARY KEY (id);


--
-- Name: payroll_periods payroll_periods_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_periods
    ADD CONSTRAINT payroll_periods_pkey PRIMARY KEY (id);


--
-- Name: payslips payslips_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payslips
    ADD CONSTRAINT payslips_pkey PRIMARY KEY (id);


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
-- Name: qr_short_links qr_short_links_org_id_kind_target_ref_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.qr_short_links
    ADD CONSTRAINT qr_short_links_org_id_kind_target_ref_key UNIQUE (org_id, kind, target_ref);


--
-- Name: qr_short_links qr_short_links_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.qr_short_links
    ADD CONSTRAINT qr_short_links_pkey PRIMARY KEY (id);


--
-- Name: recipe_lines recipe_lines_owner_ingredient_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.recipe_lines
    ADD CONSTRAINT recipe_lines_owner_ingredient_key UNIQUE (owner_type, owner_id, ingredient_id);


--
-- Name: recipe_lines recipe_lines_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.recipe_lines
    ADD CONSTRAINT recipe_lines_pkey PRIMARY KEY (id);


--
-- Name: role_permissions role_permissions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.role_permissions
    ADD CONSTRAINT role_permissions_pkey PRIMARY KEY (role, resource, action);


--
-- Name: salary_advances salary_advances_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.salary_advances
    ADD CONSTRAINT salary_advances_pkey PRIMARY KEY (id);


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
-- Name: staff_documents staff_documents_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_documents
    ADD CONSTRAINT staff_documents_pkey PRIMARY KEY (id);


--
-- Name: staff_profiles staff_profiles_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_profiles
    ADD CONSTRAINT staff_profiles_pkey PRIMARY KEY (user_id);


--
-- Name: staff_requests staff_requests_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_requests
    ADD CONSTRAINT staff_requests_pkey PRIMARY KEY (id);


--
-- Name: staff_schedule_overrides staff_schedule_overrides_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_schedule_overrides
    ADD CONSTRAINT staff_schedule_overrides_pkey PRIMARY KEY (id);


--
-- Name: staff_schedules staff_schedules_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_schedules
    ADD CONSTRAINT staff_schedules_pkey PRIMARY KEY (id);


--
-- Name: stock_transfers stock_transfers_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stock_transfers
    ADD CONSTRAINT stock_transfers_pkey PRIMARY KEY (id);


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
-- Name: table_transfer_requests table_transfer_requests_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.table_transfer_requests
    ADD CONSTRAINT table_transfer_requests_pkey PRIMARY KEY (id);


--
-- Name: ticket_ref_counters ticket_ref_counters_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ticket_ref_counters
    ADD CONSTRAINT ticket_ref_counters_pkey PRIMARY KEY (branch_id, business_date);


--
-- Name: tills tills_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tills
    ADD CONSTRAINT tills_pkey PRIMARY KEY (id);


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
-- Name: whatsapp_gateway_settings whatsapp_gateway_settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.whatsapp_gateway_settings
    ADD CONSTRAINT whatsapp_gateway_settings_pkey PRIMARY KEY (id);


--
-- Name: work_shifts work_shifts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.work_shifts
    ADD CONSTRAINT work_shifts_pkey PRIMARY KEY (id);


--
-- Name: databases_created_at; Type: INDEX; Schema: _sqlx_test; Owner: -
--

CREATE INDEX databases_created_at ON _sqlx_test.databases USING btree (created_at);


--
-- Name: ai_conversations_user_recent; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ai_conversations_user_recent ON public.ai_conversations USING btree (user_id, last_turn_at DESC NULLS LAST) WHERE (deleted_at IS NULL);


--
-- Name: ai_messages_conversation_seq; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX ai_messages_conversation_seq ON public.ai_messages USING btree (conversation_id, seq);


--
-- Name: attendance_records_branch_date_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX attendance_records_branch_date_idx ON public.attendance_records USING btree (branch_id, business_date DESC);


--
-- Name: attendance_records_open_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX attendance_records_open_idx ON public.attendance_records USING btree (scheduled_end_at) WHERE ((check_out_at IS NULL) AND (check_in_at IS NOT NULL));


--
-- Name: attendance_records_org_date_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX attendance_records_org_date_idx ON public.attendance_records USING btree (org_id, business_date DESC);


--
-- Name: attendance_records_unique_day; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX attendance_records_unique_day ON public.attendance_records USING btree (user_id, business_date, COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid));


--
-- Name: attendance_records_user_date_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX attendance_records_user_date_idx ON public.attendance_records USING btree (user_id, business_date DESC);


--
-- Name: attendance_settings_scope_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX attendance_settings_scope_key ON public.attendance_settings USING btree (org_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid));


--
-- Name: branches_org_code_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX branches_org_code_key ON public.branches USING btree (org_id, code);


--
-- Name: bundle_price_epochs_bundle_from_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX bundle_price_epochs_bundle_from_idx ON public.bundle_price_epochs USING btree (bundle_id, effective_from DESC);


--
-- Name: departments_manager_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX departments_manager_idx ON public.departments USING btree (manager_user_id);


--
-- Name: departments_org_name_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX departments_org_name_key ON public.departments USING btree (org_id, lower(name));


--
-- Name: idx_branch_stock_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_branch_stock_branch ON public.branch_stock USING btree (branch_id);


--
-- Name: idx_branch_stock_ingredient; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_branch_stock_ingredient ON public.branch_stock USING btree (org_ingredient_id);


--
-- Name: idx_branch_tables_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_branch_tables_branch ON public.branch_tables USING btree (branch_id);


--
-- Name: idx_branch_tables_section; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_branch_tables_section ON public.branch_tables USING btree (section_id);


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
-- Name: idx_csr_station; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_csr_station ON public.category_station_routes USING btree (station_id);


--
-- Name: idx_delivery_orders_branch_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_delivery_orders_branch_status ON public.delivery_orders USING btree (branch_id, status);


--
-- Name: idx_delivery_orders_order; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_delivery_orders_order ON public.delivery_orders USING btree (order_id) WHERE (order_id IS NOT NULL);


--
-- Name: idx_delivery_orders_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_delivery_orders_org ON public.delivery_orders USING btree (org_id);


--
-- Name: idx_delivery_orders_phone; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_delivery_orders_phone ON public.delivery_orders USING btree (customer_phone);


--
-- Name: idx_delivery_otp_expires; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_delivery_otp_expires ON public.delivery_otp USING btree (expires_at);


--
-- Name: idx_delivery_otp_phone; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_delivery_otp_phone ON public.delivery_otp USING btree (phone);


--
-- Name: idx_delivery_zones_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_delivery_zones_branch ON public.delivery_zones USING btree (branch_id);


--
-- Name: idx_discounts_org_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_discounts_org_id ON public.discounts USING btree (org_id);


--
-- Name: idx_floor_sections_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_floor_sections_branch ON public.floor_sections USING btree (branch_id);


--
-- Name: idx_goods_receipt_lines_receipt; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_goods_receipt_lines_receipt ON public.goods_receipt_lines USING btree (goods_receipt_id);


--
-- Name: idx_goods_receipts_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_goods_receipts_branch ON public.goods_receipts USING btree (branch_id, received_at DESC);


--
-- Name: idx_goods_receipts_po; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_goods_receipts_po ON public.goods_receipts USING btree (purchase_order_id);


--
-- Name: idx_ich_one_open_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_ich_one_open_branch ON public.ingredient_cost_history USING btree (org_ingredient_id, branch_id) WHERE ((effective_until IS NULL) AND (branch_id IS NOT NULL));


--
-- Name: idx_ich_one_open_org; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_ich_one_open_org ON public.ingredient_cost_history USING btree (org_ingredient_id) WHERE ((effective_until IS NULL) AND (branch_id IS NULL));


--
-- Name: idx_ingredient_categories_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_ingredient_categories_org ON public.ingredient_categories USING btree (org_id, sort_order, name);


--
-- Name: idx_ingredient_cost_history_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_ingredient_cost_history_branch ON public.ingredient_cost_history USING btree (org_ingredient_id, branch_id, effective_from DESC);


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
-- Name: idx_inventory_movements_stock_time; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_inventory_movements_stock_time ON public.inventory_movements USING btree (branch_id, org_ingredient_id, created_at DESC);


--
-- Name: idx_kitchen_stations_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kitchen_stations_branch ON public.kitchen_stations USING btree (branch_id) WHERE (deleted_at IS NULL);


--
-- Name: idx_kitchen_tickets_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kitchen_tickets_branch ON public.kitchen_tickets USING btree (branch_id, status);


--
-- Name: idx_kitchen_tickets_source; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kitchen_tickets_source ON public.kitchen_tickets USING btree (source_type, source_id);


--
-- Name: idx_kti_station_pending; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kti_station_pending ON public.kitchen_ticket_items USING btree (station_id) WHERE ((bumped_at IS NULL) AND (voided_at IS NULL));


--
-- Name: idx_kti_ticket; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kti_ticket ON public.kitchen_ticket_items USING btree (kitchen_ticket_id);


--
-- Name: idx_menu_item_sizes_item; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_menu_item_sizes_item ON public.menu_item_sizes USING btree (menu_item_id);


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
-- Name: idx_mimg_group; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_mimg_group ON public.menu_item_modifier_groups USING btree (group_id);


--
-- Name: idx_mimg_item; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_mimg_item ON public.menu_item_modifier_groups USING btree (menu_item_id);


--
-- Name: idx_misr_station; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_misr_station ON public.menu_item_station_routes USING btree (menu_item_id);


--
-- Name: idx_modifier_groups_legacy_type; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_modifier_groups_legacy_type ON public.modifier_groups USING btree (org_id, legacy_addon_type) WHERE (legacy_addon_type IS NOT NULL);


--
-- Name: idx_modifier_groups_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_modifier_groups_org ON public.modifier_groups USING btree (org_id);


--
-- Name: idx_modifier_options_group; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_modifier_options_group ON public.modifier_options USING btree (group_id);


--
-- Name: idx_mpo_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_mpo_branch ON public.menu_price_overrides USING btree (branch_id) WHERE (branch_id IS NOT NULL);


--
-- Name: idx_mpo_target; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_mpo_target ON public.menu_price_overrides USING btree (target_type, target_id);


--
-- Name: idx_oio_order_item; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_oio_order_item ON public.order_item_optionals USING btree (order_item_id);


--
-- Name: idx_open_tickets_branch_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_open_tickets_branch_status ON public.open_tickets USING btree (branch_id, status);


--
-- Name: idx_open_tickets_table; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_open_tickets_table ON public.open_tickets USING btree (table_id) WHERE (table_id IS NOT NULL);


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
-- Name: idx_orders_delivery_order; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_delivery_order ON public.orders USING btree (delivery_order_id) WHERE (delivery_order_id IS NOT NULL);


--
-- Name: idx_orders_order_type; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_order_type ON public.orders USING btree (order_type);


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
-- Name: idx_orders_waiter; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_orders_waiter ON public.orders USING btree (waiter_id) WHERE (waiter_id IS NOT NULL);


--
-- Name: idx_org_ingredients_category; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_org_ingredients_category ON public.org_ingredients USING btree (category_id);


--
-- Name: idx_org_ingredients_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_org_ingredients_org ON public.org_ingredients USING btree (org_id);


--
-- Name: idx_org_ingredients_supplier; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_org_ingredients_supplier ON public.org_ingredients USING btree (supplier_id);


--
-- Name: idx_organizations_demo_expiry; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_organizations_demo_expiry ON public.organizations USING btree (demo_expires_at) WHERE is_demo;


--
-- Name: idx_oti_round; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_oti_round ON public.open_ticket_items USING btree (round_id);


--
-- Name: idx_oti_ticket; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_oti_ticket ON public.open_ticket_items USING btree (open_ticket_id);


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
-- Name: idx_qr_short_links_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_qr_short_links_org ON public.qr_short_links USING btree (org_id, kind);


--
-- Name: idx_recipe_lines_ingredient; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_recipe_lines_ingredient ON public.recipe_lines USING btree (ingredient_id);


--
-- Name: idx_recipe_lines_owner; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_recipe_lines_owner ON public.recipe_lines USING btree (owner_type, owner_id);


--
-- Name: idx_shift_cash_movements_shift; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_shift_cash_movements_shift ON public.shift_cash_movements USING btree (shift_id);


--
-- Name: idx_shifts_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_shifts_branch ON public.shifts USING btree (branch_id);


--
-- Name: idx_shifts_one_open_per_teller; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_shifts_one_open_per_teller ON public.shifts USING btree (teller_id) WHERE (status = 'open'::public.shift_status);


--
-- Name: idx_shifts_one_open_per_till; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_shifts_one_open_per_till ON public.shifts USING btree (till_id) WHERE (status = 'open'::public.shift_status);


--
-- Name: idx_shifts_opened_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_shifts_opened_at ON public.shifts USING btree (opened_at);


--
-- Name: idx_shifts_teller; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_shifts_teller ON public.shifts USING btree (teller_id);


--
-- Name: idx_shifts_till; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_shifts_till ON public.shifts USING btree (till_id);


--
-- Name: idx_stock_transfers_destination; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_stock_transfers_destination ON public.stock_transfers USING btree (destination_branch_id);


--
-- Name: idx_stock_transfers_ingredient; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_stock_transfers_ingredient ON public.stock_transfers USING btree (org_ingredient_id);


--
-- Name: idx_stock_transfers_source; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_stock_transfers_source ON public.stock_transfers USING btree (source_branch_id);


--
-- Name: idx_stocktake_items_stocktake; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_stocktake_items_stocktake ON public.stocktake_items USING btree (stocktake_id);


--
-- Name: idx_stocktakes_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_stocktakes_branch ON public.stocktakes USING btree (branch_id, started_at DESC);


--
-- Name: idx_stocktakes_one_open_per_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_stocktakes_one_open_per_branch ON public.stocktakes USING btree (branch_id) WHERE (status = ANY (ARRAY['draft'::public.stocktake_status, 'in_progress'::public.stocktake_status]));


--
-- Name: idx_stocktakes_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_stocktakes_org ON public.stocktakes USING btree (org_id);


--
-- Name: idx_suppliers_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_suppliers_org ON public.suppliers USING btree (org_id) WHERE (deleted_at IS NULL);


--
-- Name: idx_tills_branch; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_tills_branch ON public.tills USING btree (branch_id) WHERE (deleted_at IS NULL);


--
-- Name: idx_ttr_branch_updated; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_ttr_branch_updated ON public.table_transfer_requests USING btree (branch_id, updated_at);


--
-- Name: idx_ttr_branch_waiting; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_ttr_branch_waiting ON public.table_transfer_requests USING btree (branch_id, created_at) WHERE (status = 'waiting'::text);


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
-- Name: integration_credentials_branch_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX integration_credentials_branch_idx ON public.integration_credentials USING btree (branch_id);


--
-- Name: integration_credentials_org_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX integration_credentials_org_idx ON public.integration_credentials USING btree (org_id);


--
-- Name: integration_credentials_username_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX integration_credentials_username_key ON public.integration_credentials USING btree (lower(username));


--
-- Name: leave_balances_unique; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX leave_balances_unique ON public.leave_balances USING btree (user_id, leave_type_id, year);


--
-- Name: leave_types_org_name_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX leave_types_org_name_key ON public.leave_types USING btree (org_id, lower(name));


--
-- Name: menu_decisions_org_created_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_decisions_org_created_idx ON public.menu_decisions USING btree (org_id, created_at DESC);


--
-- Name: menu_decisions_sku_kind_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_decisions_sku_kind_idx ON public.menu_decisions USING btree (org_id, menu_item_id, size_label, signal_kind, created_at DESC);


--
-- Name: menu_item_price_epochs_item_from_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX menu_item_price_epochs_item_from_idx ON public.menu_item_price_epochs USING btree (menu_item_id, effective_from DESC);


--
-- Name: menu_price_overrides_bc_uq; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX menu_price_overrides_bc_uq ON public.menu_price_overrides USING btree (target_type, target_id, branch_id, channel) WHERE (scope = 'branch_channel'::text);


--
-- Name: menu_price_overrides_branch_uq; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX menu_price_overrides_branch_uq ON public.menu_price_overrides USING btree (target_type, target_id, branch_id) WHERE (scope = 'branch'::text);


--
-- Name: menu_price_overrides_channel_uq; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX menu_price_overrides_channel_uq ON public.menu_price_overrides USING btree (target_type, target_id, channel) WHERE (scope = 'channel'::text);


--
-- Name: orders_idempotency_key_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX orders_idempotency_key_idx ON public.orders USING btree (idempotency_key) WHERE (idempotency_key IS NOT NULL);


--
-- Name: orders_order_ref_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX orders_order_ref_key ON public.orders USING btree (order_ref);


--
-- Name: payroll_bonuses_org_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX payroll_bonuses_org_idx ON public.payroll_bonuses USING btree (org_id, effective_date DESC);


--
-- Name: payroll_bonuses_user_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX payroll_bonuses_user_idx ON public.payroll_bonuses USING btree (user_id, effective_date DESC);


--
-- Name: payroll_deductions_auto_unique; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX payroll_deductions_auto_unique ON public.payroll_deductions USING btree (attendance_record_id, source) WHERE ((attendance_record_id IS NOT NULL) AND (source <> 'manual'::text));


--
-- Name: payroll_deductions_live_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX payroll_deductions_live_idx ON public.payroll_deductions USING btree (org_id, effective_date) WHERE ((waived_at IS NULL) AND (status = 'approved'::text));


--
-- Name: payroll_deductions_org_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX payroll_deductions_org_idx ON public.payroll_deductions USING btree (org_id, effective_date DESC);


--
-- Name: payroll_deductions_user_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX payroll_deductions_user_idx ON public.payroll_deductions USING btree (user_id, effective_date DESC);


--
-- Name: payroll_periods_org_span_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX payroll_periods_org_span_key ON public.payroll_periods USING btree (org_id, start_date, end_date);


--
-- Name: payslips_period_user_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX payslips_period_user_key ON public.payslips USING btree (payroll_period_id, user_id);


--
-- Name: payslips_user_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX payslips_user_idx ON public.payslips USING btree (user_id, generated_at DESC);


--
-- Name: salary_advances_active_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX salary_advances_active_idx ON public.salary_advances USING btree (user_id) WHERE ((status = 'approved'::text) AND (remaining_piastres > 0));


--
-- Name: salary_advances_org_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX salary_advances_org_idx ON public.salary_advances USING btree (org_id, status);


--
-- Name: salary_advances_user_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX salary_advances_user_idx ON public.salary_advances USING btree (user_id, created_at DESC);


--
-- Name: staff_documents_expiry_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_documents_expiry_idx ON public.staff_documents USING btree (org_id, expires_on) WHERE (expires_on IS NOT NULL);


--
-- Name: staff_documents_user_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_documents_user_idx ON public.staff_documents USING btree (user_id, created_at DESC);


--
-- Name: staff_profiles_department_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_profiles_department_idx ON public.staff_profiles USING btree (department_id);


--
-- Name: staff_profiles_org_code_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX staff_profiles_org_code_key ON public.staff_profiles USING btree (org_id, lower(employee_code)) WHERE (employee_code IS NOT NULL);


--
-- Name: staff_profiles_org_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_profiles_org_idx ON public.staff_profiles USING btree (org_id);


--
-- Name: staff_requests_live_correction_unique; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX staff_requests_live_correction_unique ON public.staff_requests USING btree (attendance_record_id) WHERE ((status = ANY (ARRAY['pending'::text, 'approved'::text])) AND (kind = 'correction'::text));


--
-- Name: staff_requests_live_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_requests_live_idx ON public.staff_requests USING btree (user_id, on_date, COALESCE(end_date, on_date)) WHERE (status = 'approved'::text);


--
-- Name: staff_requests_live_unique; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX staff_requests_live_unique ON public.staff_requests USING btree (user_id, kind, on_date) WHERE ((status = ANY (ARRAY['pending'::text, 'approved'::text])) AND (kind <> 'leave'::text));


--
-- Name: staff_requests_org_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_requests_org_idx ON public.staff_requests USING btree (org_id, status, on_date DESC);


--
-- Name: staff_requests_user_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_requests_user_idx ON public.staff_requests USING btree (user_id, on_date DESC);


--
-- Name: staff_schedule_overrides_user_date_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX staff_schedule_overrides_user_date_key ON public.staff_schedule_overrides USING btree (user_id, on_date);


--
-- Name: staff_schedules_shift_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_schedules_shift_idx ON public.staff_schedules USING btree (work_shift_id);


--
-- Name: staff_schedules_user_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_schedules_user_idx ON public.staff_schedules USING btree (user_id, effective_from DESC);


--
-- Name: uq_delivery_orders_idem; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_delivery_orders_idem ON public.delivery_orders USING btree (idempotency_key) WHERE (idempotency_key IS NOT NULL);


--
-- Name: uq_delivery_orders_ref; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_delivery_orders_ref ON public.delivery_orders USING btree (delivery_ref) WHERE (delivery_ref IS NOT NULL);


--
-- Name: uq_kitchen_stations_default; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_kitchen_stations_default ON public.kitchen_stations USING btree (branch_id) WHERE (is_default AND (deleted_at IS NULL));


--
-- Name: uq_kitchen_stations_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_kitchen_stations_name ON public.kitchen_stations USING btree (branch_id, lower(name)) WHERE (deleted_at IS NULL);


--
-- Name: uq_open_ticket_rounds_idem; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_open_ticket_rounds_idem ON public.open_ticket_rounds USING btree (idempotency_key) WHERE (idempotency_key IS NOT NULL);


--
-- Name: uq_open_tickets_idem; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_open_tickets_idem ON public.open_tickets USING btree (idempotency_key) WHERE (idempotency_key IS NOT NULL);


--
-- Name: uq_open_tickets_ref; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_open_tickets_ref ON public.open_tickets USING btree (ticket_ref) WHERE (ticket_ref IS NOT NULL);


--
-- Name: uq_shift_cash_movements_client_ref; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_shift_cash_movements_client_ref ON public.shift_cash_movements USING btree (client_ref) WHERE (client_ref IS NOT NULL);


--
-- Name: uq_tills_default; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_tills_default ON public.tills USING btree (branch_id) WHERE (is_default AND (deleted_at IS NULL));


--
-- Name: uq_tills_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_tills_name ON public.tills USING btree (branch_id, lower(name)) WHERE (deleted_at IS NULL);


--
-- Name: uq_ttr_waiting_occupant; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_ttr_waiting_occupant ON public.table_transfer_requests USING btree (occupant_kind, occupant_id) WHERE (status = 'waiting'::text);


--
-- Name: work_shifts_branch_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX work_shifts_branch_idx ON public.work_shifts USING btree (branch_id) WHERE (branch_id IS NOT NULL);


--
-- Name: work_shifts_org_branch_name_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX work_shifts_org_branch_name_key ON public.work_shifts USING btree (org_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid), lower(name));


--
-- Name: branches branches_set_code; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER branches_set_code BEFORE INSERT ON public.branches FOR EACH ROW EXECUTE FUNCTION public.set_branch_code();


--
-- Name: orders orders_set_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER orders_set_updated_at BEFORE UPDATE ON public.orders FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: branch_stock trg_branch_stock_on_hand_guard; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_branch_stock_on_hand_guard BEFORE UPDATE OF on_hand ON public.branch_stock FOR EACH ROW EXECUTE FUNCTION public.branch_stock_on_hand_guard();


--
-- Name: branch_stock trg_branch_stock_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_branch_stock_updated_at BEFORE UPDATE ON public.branch_stock FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


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
-- Name: ingredient_categories trg_ingredient_categories_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_ingredient_categories_updated_at BEFORE UPDATE ON public.ingredient_categories FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: inventory_movements trg_inventory_movements_apply; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_inventory_movements_apply BEFORE INSERT ON public.inventory_movements FOR EACH ROW EXECUTE FUNCTION public.inventory_movements_apply();


--
-- Name: menu_items trg_menu_items_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_menu_items_updated_at BEFORE UPDATE ON public.menu_items FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: orders trg_orders_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_orders_updated_at BEFORE UPDATE ON public.orders FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: org_ingredients trg_org_ingredients_default_category; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_org_ingredients_default_category BEFORE INSERT ON public.org_ingredients FOR EACH ROW EXECUTE FUNCTION public.org_ingredients_default_category();


--
-- Name: org_ingredients trg_org_ingredients_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_org_ingredients_updated_at BEFORE UPDATE ON public.org_ingredients FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: org_payment_methods trg_org_payment_methods_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_org_payment_methods_updated_at BEFORE UPDATE ON public.org_payment_methods FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: organizations trg_organizations_seed_general_category; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_organizations_seed_general_category AFTER INSERT ON public.organizations FOR EACH ROW EXECUTE FUNCTION public.organizations_seed_general_category();


--
-- Name: organizations trg_organizations_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_organizations_updated_at BEFORE UPDATE ON public.organizations FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: shifts trg_shifts_fill_default_till; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_shifts_fill_default_till BEFORE INSERT ON public.shifts FOR EACH ROW EXECUTE FUNCTION public.shifts_fill_default_till();


--
-- Name: shifts trg_shifts_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_shifts_updated_at BEFORE UPDATE ON public.shifts FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: users trg_users_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_users_updated_at BEFORE UPDATE ON public.users FOR EACH ROW EXECUTE FUNCTION public.set_updated_at();


--
-- Name: ai_conversations ai_conversations_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ai_conversations
    ADD CONSTRAINT ai_conversations_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: ai_conversations ai_conversations_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ai_conversations
    ADD CONSTRAINT ai_conversations_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: ai_messages ai_messages_conversation_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ai_messages
    ADD CONSTRAINT ai_messages_conversation_id_fkey FOREIGN KEY (conversation_id) REFERENCES public.ai_conversations(id) ON DELETE CASCADE;


--
-- Name: ai_messages ai_messages_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ai_messages
    ADD CONSTRAINT ai_messages_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: attendance_records attendance_records_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.attendance_records
    ADD CONSTRAINT attendance_records_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: attendance_records attendance_records_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.attendance_records
    ADD CONSTRAINT attendance_records_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: attendance_records attendance_records_edited_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.attendance_records
    ADD CONSTRAINT attendance_records_edited_by_fkey FOREIGN KEY (edited_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: attendance_records attendance_records_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.attendance_records
    ADD CONSTRAINT attendance_records_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: attendance_records attendance_records_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.attendance_records
    ADD CONSTRAINT attendance_records_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: attendance_records attendance_records_work_shift_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.attendance_records
    ADD CONSTRAINT attendance_records_work_shift_id_fkey FOREIGN KEY (work_shift_id) REFERENCES public.work_shifts(id) ON DELETE SET NULL;


--
-- Name: attendance_settings attendance_settings_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.attendance_settings
    ADD CONSTRAINT attendance_settings_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: attendance_settings attendance_settings_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.attendance_settings
    ADD CONSTRAINT attendance_settings_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: branch_delivery_settings branch_delivery_settings_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_delivery_settings
    ADD CONSTRAINT branch_delivery_settings_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: branch_delivery_settings branch_delivery_settings_in_mall_discount_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_delivery_settings
    ADD CONSTRAINT branch_delivery_settings_in_mall_discount_id_fkey FOREIGN KEY (in_mall_discount_id) REFERENCES public.discounts(id) ON DELETE SET NULL;


--
-- Name: branch_delivery_settings branch_delivery_settings_outside_discount_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_delivery_settings
    ADD CONSTRAINT branch_delivery_settings_outside_discount_id_fkey FOREIGN KEY (outside_discount_id) REFERENCES public.discounts(id) ON DELETE SET NULL;


--
-- Name: branch_delivery_settings branch_delivery_settings_pickup_discount_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_delivery_settings
    ADD CONSTRAINT branch_delivery_settings_pickup_discount_id_fkey FOREIGN KEY (pickup_discount_id) REFERENCES public.discounts(id) ON DELETE SET NULL;


--
-- Name: branch_delivery_settings branch_delivery_settings_umbrella_discount_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_delivery_settings
    ADD CONSTRAINT branch_delivery_settings_umbrella_discount_id_fkey FOREIGN KEY (umbrella_discount_id) REFERENCES public.discounts(id) ON DELETE SET NULL;


--
-- Name: branch_stock branch_stock_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_stock
    ADD CONSTRAINT branch_stock_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: branch_stock branch_stock_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_stock
    ADD CONSTRAINT branch_stock_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: branch_tables branch_tables_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_tables
    ADD CONSTRAINT branch_tables_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: branch_tables branch_tables_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_tables
    ADD CONSTRAINT branch_tables_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: branch_tables branch_tables_section_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.branch_tables
    ADD CONSTRAINT branch_tables_section_id_fkey FOREIGN KEY (section_id) REFERENCES public.floor_sections(id) ON DELETE SET NULL;


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
-- Name: catalog_revision catalog_revision_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.catalog_revision
    ADD CONSTRAINT catalog_revision_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: categories categories_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.categories
    ADD CONSTRAINT categories_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- Name: category_station_routes category_station_routes_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.category_station_routes
    ADD CONSTRAINT category_station_routes_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: category_station_routes category_station_routes_category_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.category_station_routes
    ADD CONSTRAINT category_station_routes_category_id_fkey FOREIGN KEY (category_id) REFERENCES public.categories(id) ON DELETE CASCADE;


--
-- Name: category_station_routes category_station_routes_station_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.category_station_routes
    ADD CONSTRAINT category_station_routes_station_id_fkey FOREIGN KEY (station_id) REFERENCES public.kitchen_stations(id) ON DELETE CASCADE;


--
-- Name: delivery_orders delivery_orders_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_orders
    ADD CONSTRAINT delivery_orders_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: delivery_orders delivery_orders_delivery_zone_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_orders
    ADD CONSTRAINT delivery_orders_delivery_zone_id_fkey FOREIGN KEY (delivery_zone_id) REFERENCES public.delivery_zones(id) ON DELETE SET NULL;


--
-- Name: delivery_orders delivery_orders_discount_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_orders
    ADD CONSTRAINT delivery_orders_discount_id_fkey FOREIGN KEY (discount_id) REFERENCES public.discounts(id) ON DELETE SET NULL;


--
-- Name: delivery_orders delivery_orders_order_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_orders
    ADD CONSTRAINT delivery_orders_order_id_fkey FOREIGN KEY (order_id) REFERENCES public.orders(id) ON DELETE SET NULL;


--
-- Name: delivery_orders delivery_orders_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_orders
    ADD CONSTRAINT delivery_orders_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: delivery_ref_counters delivery_ref_counters_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_ref_counters
    ADD CONSTRAINT delivery_ref_counters_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id);


--
-- Name: delivery_zones delivery_zones_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_zones
    ADD CONSTRAINT delivery_zones_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: departments departments_manager_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.departments
    ADD CONSTRAINT departments_manager_user_id_fkey FOREIGN KEY (manager_user_id) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: departments departments_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.departments
    ADD CONSTRAINT departments_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: discounts discounts_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.discounts
    ADD CONSTRAINT discounts_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- Name: floor_sections floor_sections_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.floor_sections
    ADD CONSTRAINT floor_sections_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: floor_sections floor_sections_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.floor_sections
    ADD CONSTRAINT floor_sections_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: goods_receipt_lines goods_receipt_lines_goods_receipt_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.goods_receipt_lines
    ADD CONSTRAINT goods_receipt_lines_goods_receipt_id_fkey FOREIGN KEY (goods_receipt_id) REFERENCES public.goods_receipts(id) ON DELETE CASCADE;


--
-- Name: goods_receipt_lines goods_receipt_lines_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.goods_receipt_lines
    ADD CONSTRAINT goods_receipt_lines_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: goods_receipt_lines goods_receipt_lines_purchase_order_line_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.goods_receipt_lines
    ADD CONSTRAINT goods_receipt_lines_purchase_order_line_id_fkey FOREIGN KEY (purchase_order_line_id) REFERENCES public.purchase_order_lines(id) ON DELETE SET NULL;


--
-- Name: goods_receipts goods_receipts_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.goods_receipts
    ADD CONSTRAINT goods_receipts_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: goods_receipts goods_receipts_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.goods_receipts
    ADD CONSTRAINT goods_receipts_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: goods_receipts goods_receipts_purchase_order_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.goods_receipts
    ADD CONSTRAINT goods_receipts_purchase_order_id_fkey FOREIGN KEY (purchase_order_id) REFERENCES public.purchase_orders(id) ON DELETE SET NULL;


--
-- Name: goods_receipts goods_receipts_received_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.goods_receipts
    ADD CONSTRAINT goods_receipts_received_by_fkey FOREIGN KEY (received_by) REFERENCES public.users(id);


--
-- Name: goods_receipts goods_receipts_supplier_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.goods_receipts
    ADD CONSTRAINT goods_receipts_supplier_id_fkey FOREIGN KEY (supplier_id) REFERENCES public.suppliers(id) ON DELETE SET NULL;


--
-- Name: ingredient_categories ingredient_categories_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ingredient_categories
    ADD CONSTRAINT ingredient_categories_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: ingredient_cost_history ingredient_cost_history_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ingredient_cost_history
    ADD CONSTRAINT ingredient_cost_history_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


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
-- Name: integration_credentials integration_credentials_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.integration_credentials
    ADD CONSTRAINT integration_credentials_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: integration_credentials integration_credentials_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.integration_credentials
    ADD CONSTRAINT integration_credentials_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: integration_credentials integration_credentials_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.integration_credentials
    ADD CONSTRAINT integration_credentials_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: inventory_movements inventory_movements_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inventory_movements
    ADD CONSTRAINT inventory_movements_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: inventory_movements inventory_movements_branch_stock_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inventory_movements
    ADD CONSTRAINT inventory_movements_branch_stock_id_fkey FOREIGN KEY (branch_stock_id) REFERENCES public.branch_stock(id) ON DELETE SET NULL;


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
-- Name: kitchen_stations kitchen_stations_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_stations
    ADD CONSTRAINT kitchen_stations_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: kitchen_stations kitchen_stations_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_stations
    ADD CONSTRAINT kitchen_stations_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: kitchen_ticket_items kitchen_ticket_items_bumped_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_ticket_items
    ADD CONSTRAINT kitchen_ticket_items_bumped_by_fkey FOREIGN KEY (bumped_by) REFERENCES public.users(id);


--
-- Name: kitchen_ticket_items kitchen_ticket_items_kitchen_ticket_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_ticket_items
    ADD CONSTRAINT kitchen_ticket_items_kitchen_ticket_id_fkey FOREIGN KEY (kitchen_ticket_id) REFERENCES public.kitchen_tickets(id) ON DELETE CASCADE;


--
-- Name: kitchen_ticket_items kitchen_ticket_items_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_ticket_items
    ADD CONSTRAINT kitchen_ticket_items_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id);


--
-- Name: kitchen_ticket_items kitchen_ticket_items_station_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_ticket_items
    ADD CONSTRAINT kitchen_ticket_items_station_id_fkey FOREIGN KEY (station_id) REFERENCES public.kitchen_stations(id) ON DELETE SET NULL;


--
-- Name: kitchen_tickets kitchen_tickets_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_tickets
    ADD CONSTRAINT kitchen_tickets_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: kitchen_tickets kitchen_tickets_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kitchen_tickets
    ADD CONSTRAINT kitchen_tickets_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: leave_balances leave_balances_leave_type_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.leave_balances
    ADD CONSTRAINT leave_balances_leave_type_id_fkey FOREIGN KEY (leave_type_id) REFERENCES public.leave_types(id) ON DELETE CASCADE;


--
-- Name: leave_balances leave_balances_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.leave_balances
    ADD CONSTRAINT leave_balances_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: leave_balances leave_balances_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.leave_balances
    ADD CONSTRAINT leave_balances_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: leave_types leave_types_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.leave_types
    ADD CONSTRAINT leave_types_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: margin_targets margin_targets_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.margin_targets
    ADD CONSTRAINT margin_targets_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: margin_targets margin_targets_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.margin_targets
    ADD CONSTRAINT margin_targets_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: margin_targets margin_targets_updated_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.margin_targets
    ADD CONSTRAINT margin_targets_updated_by_fkey FOREIGN KEY (updated_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: menu_decisions menu_decisions_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_decisions
    ADD CONSTRAINT menu_decisions_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: menu_decisions menu_decisions_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_decisions
    ADD CONSTRAINT menu_decisions_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: menu_decisions menu_decisions_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_decisions
    ADD CONSTRAINT menu_decisions_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id) ON DELETE CASCADE;


--
-- Name: menu_decisions menu_decisions_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_decisions
    ADD CONSTRAINT menu_decisions_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: menu_item_modifier_groups menu_item_modifier_groups_group_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_modifier_groups
    ADD CONSTRAINT menu_item_modifier_groups_group_id_fkey FOREIGN KEY (group_id) REFERENCES public.modifier_groups(id) ON DELETE CASCADE;


--
-- Name: menu_item_modifier_groups menu_item_modifier_groups_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_modifier_groups
    ADD CONSTRAINT menu_item_modifier_groups_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id) ON DELETE CASCADE;


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
-- Name: menu_item_sizes menu_item_sizes_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_sizes
    ADD CONSTRAINT menu_item_sizes_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id) ON DELETE CASCADE;


--
-- Name: menu_item_station_routes menu_item_station_routes_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_station_routes
    ADD CONSTRAINT menu_item_station_routes_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: menu_item_station_routes menu_item_station_routes_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_station_routes
    ADD CONSTRAINT menu_item_station_routes_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id) ON DELETE CASCADE;


--
-- Name: menu_item_station_routes menu_item_station_routes_station_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_item_station_routes
    ADD CONSTRAINT menu_item_station_routes_station_id_fkey FOREIGN KEY (station_id) REFERENCES public.kitchen_stations(id) ON DELETE CASCADE;


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
-- Name: menu_price_overrides menu_price_overrides_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.menu_price_overrides
    ADD CONSTRAINT menu_price_overrides_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: modifier_groups modifier_groups_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.modifier_groups
    ADD CONSTRAINT modifier_groups_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: modifier_options modifier_options_group_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.modifier_options
    ADD CONSTRAINT modifier_options_group_id_fkey FOREIGN KEY (group_id) REFERENCES public.modifier_groups(id) ON DELETE CASCADE;


--
-- Name: modifier_options modifier_options_replaces_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.modifier_options
    ADD CONSTRAINT modifier_options_replaces_ingredient_id_fkey FOREIGN KEY (replaces_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE SET NULL;


--
-- Name: open_ticket_items open_ticket_items_menu_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_ticket_items
    ADD CONSTRAINT open_ticket_items_menu_item_id_fkey FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id);


--
-- Name: open_ticket_items open_ticket_items_open_ticket_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_ticket_items
    ADD CONSTRAINT open_ticket_items_open_ticket_id_fkey FOREIGN KEY (open_ticket_id) REFERENCES public.open_tickets(id) ON DELETE CASCADE;


--
-- Name: open_ticket_items open_ticket_items_round_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_ticket_items
    ADD CONSTRAINT open_ticket_items_round_id_fkey FOREIGN KEY (round_id) REFERENCES public.open_ticket_rounds(id) ON DELETE CASCADE;


--
-- Name: open_ticket_rounds open_ticket_rounds_fired_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_ticket_rounds
    ADD CONSTRAINT open_ticket_rounds_fired_by_fkey FOREIGN KEY (fired_by) REFERENCES public.users(id);


--
-- Name: open_ticket_rounds open_ticket_rounds_open_ticket_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_ticket_rounds
    ADD CONSTRAINT open_ticket_rounds_open_ticket_id_fkey FOREIGN KEY (open_ticket_id) REFERENCES public.open_tickets(id) ON DELETE CASCADE;


--
-- Name: open_tickets open_tickets_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_tickets
    ADD CONSTRAINT open_tickets_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: open_tickets open_tickets_discount_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_tickets
    ADD CONSTRAINT open_tickets_discount_id_fkey FOREIGN KEY (discount_id) REFERENCES public.discounts(id);


--
-- Name: open_tickets open_tickets_opened_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_tickets
    ADD CONSTRAINT open_tickets_opened_by_fkey FOREIGN KEY (opened_by) REFERENCES public.users(id);


--
-- Name: open_tickets open_tickets_order_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_tickets
    ADD CONSTRAINT open_tickets_order_id_fkey FOREIGN KEY (order_id) REFERENCES public.orders(id) ON DELETE SET NULL;


--
-- Name: open_tickets open_tickets_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_tickets
    ADD CONSTRAINT open_tickets_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: open_tickets open_tickets_settled_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_tickets
    ADD CONSTRAINT open_tickets_settled_by_fkey FOREIGN KEY (settled_by) REFERENCES public.users(id);


--
-- Name: open_tickets open_tickets_settled_shift_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_tickets
    ADD CONSTRAINT open_tickets_settled_shift_id_fkey FOREIGN KEY (settled_shift_id) REFERENCES public.shifts(id);


--
-- Name: open_tickets open_tickets_table_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.open_tickets
    ADD CONSTRAINT open_tickets_table_id_fkey FOREIGN KEY (table_id) REFERENCES public.branch_tables(id) ON DELETE SET NULL;


--
-- Name: order_item_addons order_item_addons_order_item_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_item_addons
    ADD CONSTRAINT order_item_addons_order_item_id_fkey FOREIGN KEY (order_item_id) REFERENCES public.order_items(id) ON DELETE CASCADE;


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
-- Name: order_ref_counters order_ref_counters_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.order_ref_counters
    ADD CONSTRAINT order_ref_counters_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id);


--
-- Name: orders orders_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id);


--
-- Name: orders orders_delivery_order_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_delivery_order_id_fkey FOREIGN KEY (delivery_order_id) REFERENCES public.delivery_orders(id) ON DELETE SET NULL;


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
-- Name: orders orders_waiter_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_waiter_id_fkey FOREIGN KEY (waiter_id) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: org_ingredients org_ingredients_category_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_ingredients
    ADD CONSTRAINT org_ingredients_category_id_fkey FOREIGN KEY (category_id) REFERENCES public.ingredient_categories(id) ON DELETE RESTRICT;


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
-- Name: payroll_bonuses payroll_bonuses_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_bonuses
    ADD CONSTRAINT payroll_bonuses_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: payroll_bonuses payroll_bonuses_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_bonuses
    ADD CONSTRAINT payroll_bonuses_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: payroll_bonuses payroll_bonuses_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_bonuses
    ADD CONSTRAINT payroll_bonuses_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: payroll_deductions payroll_deductions_attendance_record_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_deductions
    ADD CONSTRAINT payroll_deductions_attendance_record_id_fkey FOREIGN KEY (attendance_record_id) REFERENCES public.attendance_records(id) ON DELETE SET NULL;


--
-- Name: payroll_deductions payroll_deductions_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_deductions
    ADD CONSTRAINT payroll_deductions_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: payroll_deductions payroll_deductions_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_deductions
    ADD CONSTRAINT payroll_deductions_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: payroll_deductions payroll_deductions_overridden_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_deductions
    ADD CONSTRAINT payroll_deductions_overridden_by_fkey FOREIGN KEY (overridden_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: payroll_deductions payroll_deductions_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_deductions
    ADD CONSTRAINT payroll_deductions_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: payroll_deductions payroll_deductions_waived_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_deductions
    ADD CONSTRAINT payroll_deductions_waived_by_fkey FOREIGN KEY (waived_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: payroll_periods payroll_periods_generated_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_periods
    ADD CONSTRAINT payroll_periods_generated_by_fkey FOREIGN KEY (generated_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: payroll_periods payroll_periods_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payroll_periods
    ADD CONSTRAINT payroll_periods_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: payslips payslips_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payslips
    ADD CONSTRAINT payslips_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: payslips payslips_payroll_period_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payslips
    ADD CONSTRAINT payslips_payroll_period_id_fkey FOREIGN KEY (payroll_period_id) REFERENCES public.payroll_periods(id) ON DELETE CASCADE;


--
-- Name: payslips payslips_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payslips
    ADD CONSTRAINT payslips_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


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
-- Name: qr_short_links qr_short_links_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.qr_short_links
    ADD CONSTRAINT qr_short_links_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: qr_short_links qr_short_links_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.qr_short_links
    ADD CONSTRAINT qr_short_links_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: recipe_lines recipe_lines_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.recipe_lines
    ADD CONSTRAINT recipe_lines_ingredient_id_fkey FOREIGN KEY (ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: salary_advances salary_advances_decided_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.salary_advances
    ADD CONSTRAINT salary_advances_decided_by_fkey FOREIGN KEY (decided_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: salary_advances salary_advances_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.salary_advances
    ADD CONSTRAINT salary_advances_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: salary_advances salary_advances_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.salary_advances
    ADD CONSTRAINT salary_advances_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


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
-- Name: shifts shifts_till_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shifts
    ADD CONSTRAINT shifts_till_id_fkey FOREIGN KEY (till_id) REFERENCES public.tills(id);


--
-- Name: staff_documents staff_documents_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_documents
    ADD CONSTRAINT staff_documents_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: staff_documents staff_documents_uploaded_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_documents
    ADD CONSTRAINT staff_documents_uploaded_by_fkey FOREIGN KEY (uploaded_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: staff_documents staff_documents_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_documents
    ADD CONSTRAINT staff_documents_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: staff_profiles staff_profiles_department_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_profiles
    ADD CONSTRAINT staff_profiles_department_id_fkey FOREIGN KEY (department_id) REFERENCES public.departments(id) ON DELETE SET NULL;


--
-- Name: staff_profiles staff_profiles_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_profiles
    ADD CONSTRAINT staff_profiles_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: staff_profiles staff_profiles_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_profiles
    ADD CONSTRAINT staff_profiles_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: staff_requests staff_requests_attendance_record_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_requests
    ADD CONSTRAINT staff_requests_attendance_record_id_fkey FOREIGN KEY (attendance_record_id) REFERENCES public.attendance_records(id) ON DELETE CASCADE;


--
-- Name: staff_requests staff_requests_decided_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_requests
    ADD CONSTRAINT staff_requests_decided_by_fkey FOREIGN KEY (decided_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: staff_requests staff_requests_leave_type_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_requests
    ADD CONSTRAINT staff_requests_leave_type_id_fkey FOREIGN KEY (leave_type_id) REFERENCES public.leave_types(id) ON DELETE RESTRICT;


--
-- Name: staff_requests staff_requests_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_requests
    ADD CONSTRAINT staff_requests_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: staff_requests staff_requests_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_requests
    ADD CONSTRAINT staff_requests_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: staff_schedule_overrides staff_schedule_overrides_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_schedule_overrides
    ADD CONSTRAINT staff_schedule_overrides_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: staff_schedule_overrides staff_schedule_overrides_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_schedule_overrides
    ADD CONSTRAINT staff_schedule_overrides_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: staff_schedule_overrides staff_schedule_overrides_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_schedule_overrides
    ADD CONSTRAINT staff_schedule_overrides_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: staff_schedule_overrides staff_schedule_overrides_work_shift_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_schedule_overrides
    ADD CONSTRAINT staff_schedule_overrides_work_shift_id_fkey FOREIGN KEY (work_shift_id) REFERENCES public.work_shifts(id) ON DELETE CASCADE;


--
-- Name: staff_schedules staff_schedules_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_schedules
    ADD CONSTRAINT staff_schedules_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: staff_schedules staff_schedules_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_schedules
    ADD CONSTRAINT staff_schedules_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: staff_schedules staff_schedules_work_shift_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_schedules
    ADD CONSTRAINT staff_schedules_work_shift_id_fkey FOREIGN KEY (work_shift_id) REFERENCES public.work_shifts(id) ON DELETE CASCADE;


--
-- Name: stock_transfers stock_transfers_destination_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stock_transfers
    ADD CONSTRAINT stock_transfers_destination_branch_id_fkey FOREIGN KEY (destination_branch_id) REFERENCES public.branches(id) ON DELETE RESTRICT;


--
-- Name: stock_transfers stock_transfers_initiated_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stock_transfers
    ADD CONSTRAINT stock_transfers_initiated_by_fkey FOREIGN KEY (initiated_by) REFERENCES public.users(id);


--
-- Name: stock_transfers stock_transfers_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stock_transfers
    ADD CONSTRAINT stock_transfers_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id);


--
-- Name: stock_transfers stock_transfers_org_ingredient_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stock_transfers
    ADD CONSTRAINT stock_transfers_org_ingredient_id_fkey FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id) ON DELETE RESTRICT;


--
-- Name: stock_transfers stock_transfers_source_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stock_transfers
    ADD CONSTRAINT stock_transfers_source_branch_id_fkey FOREIGN KEY (source_branch_id) REFERENCES public.branches(id) ON DELETE RESTRICT;


--
-- Name: stocktake_items stocktake_items_branch_stock_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.stocktake_items
    ADD CONSTRAINT stocktake_items_branch_stock_id_fkey FOREIGN KEY (branch_stock_id) REFERENCES public.branch_stock(id) ON DELETE SET NULL;


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
-- Name: table_transfer_requests table_transfer_requests_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.table_transfer_requests
    ADD CONSTRAINT table_transfer_requests_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: table_transfer_requests table_transfer_requests_from_table_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.table_transfer_requests
    ADD CONSTRAINT table_transfer_requests_from_table_id_fkey FOREIGN KEY (from_table_id) REFERENCES public.branch_tables(id) ON DELETE SET NULL;


--
-- Name: table_transfer_requests table_transfer_requests_fulfilled_table_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.table_transfer_requests
    ADD CONSTRAINT table_transfer_requests_fulfilled_table_id_fkey FOREIGN KEY (fulfilled_table_id) REFERENCES public.branch_tables(id) ON DELETE SET NULL;


--
-- Name: table_transfer_requests table_transfer_requests_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.table_transfer_requests
    ADD CONSTRAINT table_transfer_requests_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: table_transfer_requests table_transfer_requests_requested_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.table_transfer_requests
    ADD CONSTRAINT table_transfer_requests_requested_by_fkey FOREIGN KEY (requested_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: table_transfer_requests table_transfer_requests_target_section_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.table_transfer_requests
    ADD CONSTRAINT table_transfer_requests_target_section_id_fkey FOREIGN KEY (target_section_id) REFERENCES public.floor_sections(id) ON DELETE CASCADE;


--
-- Name: table_transfer_requests table_transfer_requests_target_table_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.table_transfer_requests
    ADD CONSTRAINT table_transfer_requests_target_table_id_fkey FOREIGN KEY (target_table_id) REFERENCES public.branch_tables(id) ON DELETE CASCADE;


--
-- Name: ticket_ref_counters ticket_ref_counters_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ticket_ref_counters
    ADD CONSTRAINT ticket_ref_counters_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id);


--
-- Name: tills tills_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tills
    ADD CONSTRAINT tills_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: tills tills_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tills
    ADD CONSTRAINT tills_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


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
-- Name: whatsapp_gateway_settings whatsapp_gateway_settings_paused_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.whatsapp_gateway_settings
    ADD CONSTRAINT whatsapp_gateway_settings_paused_by_fkey FOREIGN KEY (paused_by) REFERENCES public.users(id) ON DELETE SET NULL;


--
-- Name: work_shifts work_shifts_branch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.work_shifts
    ADD CONSTRAINT work_shifts_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES public.branches(id) ON DELETE CASCADE;


--
-- Name: work_shifts work_shifts_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.work_shifts
    ADD CONSTRAINT work_shifts_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


--
-- Name: ai_conversations; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.ai_conversations ENABLE ROW LEVEL SECURITY;

--
-- Name: ai_messages; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.ai_messages ENABLE ROW LEVEL SECURITY;

--
-- Name: attendance_records; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.attendance_records ENABLE ROW LEVEL SECURITY;

--
-- Name: attendance_settings; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.attendance_settings ENABLE ROW LEVEL SECURITY;

--
-- Name: branch_delivery_settings; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.branch_delivery_settings ENABLE ROW LEVEL SECURITY;

--
-- Name: branch_stock; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.branch_stock ENABLE ROW LEVEL SECURITY;

--
-- Name: branch_tables; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.branch_tables ENABLE ROW LEVEL SECURITY;

--
-- Name: branches; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.branches ENABLE ROW LEVEL SECURITY;

--
-- Name: bundle_branch_availability; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.bundle_branch_availability ENABLE ROW LEVEL SECURITY;

--
-- Name: bundle_components; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.bundle_components ENABLE ROW LEVEL SECURITY;

--
-- Name: bundle_price_epochs; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.bundle_price_epochs ENABLE ROW LEVEL SECURITY;

--
-- Name: bundles; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.bundles ENABLE ROW LEVEL SECURITY;

--
-- Name: catalog_revision; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.catalog_revision ENABLE ROW LEVEL SECURITY;

--
-- Name: categories; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.categories ENABLE ROW LEVEL SECURITY;

--
-- Name: category_station_routes; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.category_station_routes ENABLE ROW LEVEL SECURITY;

--
-- Name: delivery_orders; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.delivery_orders ENABLE ROW LEVEL SECURITY;

--
-- Name: delivery_otp; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.delivery_otp ENABLE ROW LEVEL SECURITY;

--
-- Name: delivery_ref_counters; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.delivery_ref_counters ENABLE ROW LEVEL SECURITY;

--
-- Name: delivery_zones; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.delivery_zones ENABLE ROW LEVEL SECURITY;

--
-- Name: departments; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.departments ENABLE ROW LEVEL SECURITY;

--
-- Name: discounts; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.discounts ENABLE ROW LEVEL SECURITY;

--
-- Name: floor_sections; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.floor_sections ENABLE ROW LEVEL SECURITY;

--
-- Name: role_permissions global_read; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY global_read ON public.role_permissions FOR SELECT USING (true);


--
-- Name: whatsapp_gateway_settings global_read; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY global_read ON public.whatsapp_gateway_settings FOR SELECT USING (true);


--
-- Name: goods_receipt_lines; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.goods_receipt_lines ENABLE ROW LEVEL SECURITY;

--
-- Name: goods_receipts; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.goods_receipts ENABLE ROW LEVEL SECURITY;

--
-- Name: ingredient_categories; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.ingredient_categories ENABLE ROW LEVEL SECURITY;

--
-- Name: ingredient_cost_history; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.ingredient_cost_history ENABLE ROW LEVEL SECURITY;

--
-- Name: integration_credentials; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.integration_credentials ENABLE ROW LEVEL SECURITY;

--
-- Name: inventory_movements; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.inventory_movements ENABLE ROW LEVEL SECURITY;

--
-- Name: kitchen_stations; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.kitchen_stations ENABLE ROW LEVEL SECURITY;

--
-- Name: kitchen_ticket_items; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.kitchen_ticket_items ENABLE ROW LEVEL SECURITY;

--
-- Name: kitchen_tickets; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.kitchen_tickets ENABLE ROW LEVEL SECURITY;

--
-- Name: leave_balances; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.leave_balances ENABLE ROW LEVEL SECURITY;

--
-- Name: leave_types; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.leave_types ENABLE ROW LEVEL SECURITY;

--
-- Name: margin_targets; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.margin_targets ENABLE ROW LEVEL SECURITY;

--
-- Name: menu_decisions; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.menu_decisions ENABLE ROW LEVEL SECURITY;

--
-- Name: menu_item_modifier_groups; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.menu_item_modifier_groups ENABLE ROW LEVEL SECURITY;

--
-- Name: menu_item_price_epochs; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.menu_item_price_epochs ENABLE ROW LEVEL SECURITY;

--
-- Name: menu_item_sizes; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.menu_item_sizes ENABLE ROW LEVEL SECURITY;

--
-- Name: menu_item_station_routes; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.menu_item_station_routes ENABLE ROW LEVEL SECURITY;

--
-- Name: menu_items; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.menu_items ENABLE ROW LEVEL SECURITY;

--
-- Name: menu_price_overrides; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.menu_price_overrides ENABLE ROW LEVEL SECURITY;

--
-- Name: modifier_groups; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.modifier_groups ENABLE ROW LEVEL SECURITY;

--
-- Name: modifier_options; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.modifier_options ENABLE ROW LEVEL SECURITY;

--
-- Name: open_ticket_items; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.open_ticket_items ENABLE ROW LEVEL SECURITY;

--
-- Name: open_ticket_rounds; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.open_ticket_rounds ENABLE ROW LEVEL SECURITY;

--
-- Name: open_tickets; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.open_tickets ENABLE ROW LEVEL SECURITY;

--
-- Name: order_item_addons; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.order_item_addons ENABLE ROW LEVEL SECURITY;

--
-- Name: order_item_optionals; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.order_item_optionals ENABLE ROW LEVEL SECURITY;

--
-- Name: order_items; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.order_items ENABLE ROW LEVEL SECURITY;

--
-- Name: order_line_bundle_component_addons; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.order_line_bundle_component_addons ENABLE ROW LEVEL SECURITY;

--
-- Name: order_line_bundle_component_optionals; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.order_line_bundle_component_optionals ENABLE ROW LEVEL SECURITY;

--
-- Name: order_line_bundle_components; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.order_line_bundle_components ENABLE ROW LEVEL SECURITY;

--
-- Name: order_payments; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.order_payments ENABLE ROW LEVEL SECURITY;

--
-- Name: order_ref_counters; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.order_ref_counters ENABLE ROW LEVEL SECURITY;

--
-- Name: orders; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.orders ENABLE ROW LEVEL SECURITY;

--
-- Name: org_ingredients; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.org_ingredients ENABLE ROW LEVEL SECURITY;

--
-- Name: org_payment_methods; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.org_payment_methods ENABLE ROW LEVEL SECURITY;

--
-- Name: organizations org_read; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY org_read ON public.organizations FOR SELECT USING ((id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: organizations org_update; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY org_update ON public.organizations FOR UPDATE USING ((id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: organizations; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.organizations ENABLE ROW LEVEL SECURITY;

--
-- Name: payroll_bonuses; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.payroll_bonuses ENABLE ROW LEVEL SECURITY;

--
-- Name: payroll_deductions; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.payroll_deductions ENABLE ROW LEVEL SECURITY;

--
-- Name: payroll_periods; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.payroll_periods ENABLE ROW LEVEL SECURITY;

--
-- Name: payslips; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.payslips ENABLE ROW LEVEL SECURITY;

--
-- Name: permissions; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.permissions ENABLE ROW LEVEL SECURITY;

--
-- Name: purchase_order_lines; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.purchase_order_lines ENABLE ROW LEVEL SECURITY;

--
-- Name: purchase_orders; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.purchase_orders ENABLE ROW LEVEL SECURITY;

--
-- Name: qr_short_links; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.qr_short_links ENABLE ROW LEVEL SECURITY;

--
-- Name: recipe_lines; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.recipe_lines ENABLE ROW LEVEL SECURITY;

--
-- Name: role_permissions; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.role_permissions ENABLE ROW LEVEL SECURITY;

--
-- Name: salary_advances; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.salary_advances ENABLE ROW LEVEL SECURITY;

--
-- Name: shift_cash_movements; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.shift_cash_movements ENABLE ROW LEVEL SECURITY;

--
-- Name: shifts; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.shifts ENABLE ROW LEVEL SECURITY;

--
-- Name: staff_documents; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.staff_documents ENABLE ROW LEVEL SECURITY;

--
-- Name: staff_profiles; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.staff_profiles ENABLE ROW LEVEL SECURITY;

--
-- Name: staff_requests; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.staff_requests ENABLE ROW LEVEL SECURITY;

--
-- Name: staff_schedule_overrides; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.staff_schedule_overrides ENABLE ROW LEVEL SECURITY;

--
-- Name: staff_schedules; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.staff_schedules ENABLE ROW LEVEL SECURITY;

--
-- Name: stock_transfers; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.stock_transfers ENABLE ROW LEVEL SECURITY;

--
-- Name: stocktake_items; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.stocktake_items ENABLE ROW LEVEL SECURITY;

--
-- Name: stocktakes; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.stocktakes ENABLE ROW LEVEL SECURITY;

--
-- Name: suppliers; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.suppliers ENABLE ROW LEVEL SECURITY;

--
-- Name: table_transfer_requests; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.table_transfer_requests ENABLE ROW LEVEL SECURITY;

--
-- Name: ai_conversations tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.ai_conversations USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: ai_messages tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.ai_messages USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: attendance_records tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.attendance_records USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: attendance_settings tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.attendance_settings USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: branch_delivery_settings tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.branch_delivery_settings USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: branch_stock tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.branch_stock USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: branch_tables tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.branch_tables USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: branches tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.branches USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: bundle_branch_availability tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.bundle_branch_availability USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: bundle_components tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.bundle_components USING ((EXISTS ( SELECT 1
   FROM public.bundles p
  WHERE (p.id = bundle_components.bundle_id))));


--
-- Name: bundle_price_epochs tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.bundle_price_epochs USING ((EXISTS ( SELECT 1
   FROM public.bundles p
  WHERE (p.id = bundle_price_epochs.bundle_id))));


--
-- Name: bundles tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.bundles USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: catalog_revision tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.catalog_revision USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: categories tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.categories USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: category_station_routes tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.category_station_routes USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: delivery_orders tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.delivery_orders USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: delivery_ref_counters tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.delivery_ref_counters USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: delivery_zones tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.delivery_zones USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: departments tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.departments USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: discounts tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.discounts USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: floor_sections tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.floor_sections USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: goods_receipt_lines tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.goods_receipt_lines USING ((EXISTS ( SELECT 1
   FROM public.goods_receipts p
  WHERE (p.id = goods_receipt_lines.goods_receipt_id))));


--
-- Name: goods_receipts tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.goods_receipts USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: ingredient_categories tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.ingredient_categories USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: ingredient_cost_history tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.ingredient_cost_history USING ((EXISTS ( SELECT 1
   FROM public.org_ingredients p
  WHERE (p.id = ingredient_cost_history.org_ingredient_id))));


--
-- Name: integration_credentials tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.integration_credentials USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: inventory_movements tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.inventory_movements USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: kitchen_stations tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.kitchen_stations USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: kitchen_ticket_items tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.kitchen_ticket_items USING ((EXISTS ( SELECT 1
   FROM public.kitchen_tickets p
  WHERE (p.id = kitchen_ticket_items.kitchen_ticket_id))));


--
-- Name: kitchen_tickets tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.kitchen_tickets USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: leave_balances tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.leave_balances USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: leave_types tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.leave_types USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: margin_targets tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.margin_targets USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: menu_decisions tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.menu_decisions USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: menu_item_modifier_groups tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.menu_item_modifier_groups USING ((EXISTS ( SELECT 1
   FROM public.menu_items p
  WHERE (p.id = menu_item_modifier_groups.menu_item_id))));


--
-- Name: menu_item_price_epochs tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.menu_item_price_epochs USING ((EXISTS ( SELECT 1
   FROM public.menu_items p
  WHERE (p.id = menu_item_price_epochs.menu_item_id))));


--
-- Name: menu_item_sizes tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.menu_item_sizes USING ((EXISTS ( SELECT 1
   FROM public.menu_items p
  WHERE (p.id = menu_item_sizes.menu_item_id))));


--
-- Name: menu_item_station_routes tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.menu_item_station_routes USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: menu_items tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.menu_items USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: menu_price_overrides tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.menu_price_overrides USING (((branch_id IN ( SELECT b.id
   FROM public.branches b)) OR ((branch_id IS NULL) AND (EXISTS ( SELECT 1
   FROM public.menu_item_sizes s
  WHERE (s.id = menu_price_overrides.target_id)
UNION ALL
 SELECT 1
   FROM public.modifier_options mo
  WHERE (mo.id = menu_price_overrides.target_id))))));


--
-- Name: modifier_groups tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.modifier_groups USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: modifier_options tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.modifier_options USING ((EXISTS ( SELECT 1
   FROM public.modifier_groups p
  WHERE (p.id = modifier_options.group_id))));


--
-- Name: open_ticket_items tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.open_ticket_items USING ((EXISTS ( SELECT 1
   FROM public.open_tickets p
  WHERE (p.id = open_ticket_items.open_ticket_id))));


--
-- Name: open_ticket_rounds tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.open_ticket_rounds USING ((EXISTS ( SELECT 1
   FROM public.open_tickets p
  WHERE (p.id = open_ticket_rounds.open_ticket_id))));


--
-- Name: open_tickets tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.open_tickets USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: order_item_addons tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.order_item_addons USING ((EXISTS ( SELECT 1
   FROM public.order_items p
  WHERE (p.id = order_item_addons.order_item_id))));


--
-- Name: order_item_optionals tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.order_item_optionals USING ((EXISTS ( SELECT 1
   FROM public.order_items p
  WHERE (p.id = order_item_optionals.order_item_id))));


--
-- Name: order_items tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.order_items USING ((EXISTS ( SELECT 1
   FROM public.orders p
  WHERE (p.id = order_items.order_id))));


--
-- Name: order_line_bundle_component_addons tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.order_line_bundle_component_addons USING ((EXISTS ( SELECT 1
   FROM public.order_items p
  WHERE (p.id = order_line_bundle_component_addons.order_line_id))));


--
-- Name: order_line_bundle_component_optionals tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.order_line_bundle_component_optionals USING ((EXISTS ( SELECT 1
   FROM public.order_items p
  WHERE (p.id = order_line_bundle_component_optionals.order_line_id))));


--
-- Name: order_line_bundle_components tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.order_line_bundle_components USING ((EXISTS ( SELECT 1
   FROM public.order_items p
  WHERE (p.id = order_line_bundle_components.order_line_id))));


--
-- Name: order_payments tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.order_payments USING ((EXISTS ( SELECT 1
   FROM public.orders p
  WHERE (p.id = order_payments.order_id))));


--
-- Name: order_ref_counters tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.order_ref_counters USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: orders tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.orders USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: org_ingredients tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.org_ingredients USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: org_payment_methods tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.org_payment_methods USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: payroll_bonuses tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.payroll_bonuses USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: payroll_deductions tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.payroll_deductions USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: payroll_periods tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.payroll_periods USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: payslips tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.payslips USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: permissions tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.permissions USING ((EXISTS ( SELECT 1
   FROM public.users p
  WHERE (p.id = permissions.user_id))));


--
-- Name: purchase_order_lines tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.purchase_order_lines USING ((EXISTS ( SELECT 1
   FROM public.purchase_orders p
  WHERE (p.id = purchase_order_lines.purchase_order_id))));


--
-- Name: purchase_orders tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.purchase_orders USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: qr_short_links tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.qr_short_links USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: recipe_lines tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.recipe_lines USING ((EXISTS ( SELECT 1
   FROM public.org_ingredients p
  WHERE (p.id = recipe_lines.ingredient_id))));


--
-- Name: salary_advances tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.salary_advances USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: shift_cash_movements tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.shift_cash_movements USING ((EXISTS ( SELECT 1
   FROM public.shifts p
  WHERE (p.id = shift_cash_movements.shift_id))));


--
-- Name: shifts tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.shifts USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: staff_documents tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.staff_documents USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: staff_profiles tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.staff_profiles USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: staff_requests tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.staff_requests USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: staff_schedule_overrides tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.staff_schedule_overrides USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: staff_schedules tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.staff_schedules USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: stock_transfers tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.stock_transfers USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: stocktake_items tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.stocktake_items USING ((EXISTS ( SELECT 1
   FROM public.stocktakes p
  WHERE (p.id = stocktake_items.stocktake_id))));


--
-- Name: stocktakes tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.stocktakes USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: suppliers tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.suppliers USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: table_transfer_requests tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.table_transfer_requests USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: ticket_ref_counters tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.ticket_ref_counters USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: tills tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.tills USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: user_branch_assignments tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.user_branch_assignments USING ((branch_id IN ( SELECT b.id
   FROM public.branches b)));


--
-- Name: users tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.users USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: work_shifts tenant_isolation; Type: POLICY; Schema: public; Owner: -
--

CREATE POLICY tenant_isolation ON public.work_shifts USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));


--
-- Name: ticket_ref_counters; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.ticket_ref_counters ENABLE ROW LEVEL SECURITY;

--
-- Name: tills; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.tills ENABLE ROW LEVEL SECURITY;

--
-- Name: user_branch_assignments; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.user_branch_assignments ENABLE ROW LEVEL SECURITY;

--
-- Name: users; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.users ENABLE ROW LEVEL SECURITY;

--
-- Name: whatsapp_gateway_settings; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.whatsapp_gateway_settings ENABLE ROW LEVEL SECURITY;

--
-- Name: work_shifts; Type: ROW SECURITY; Schema: public; Owner: -
--

ALTER TABLE public.work_shifts ENABLE ROW LEVEL SECURITY;

--
-- PostgreSQL database dump complete
--


--
-- PostgreSQL database dump
--


-- Dumped from database version 17.11 (Debian 17.11-0+deb13u1)
-- Dumped by pg_dump version 17.11 (Debian 17.11-0+deb13u1)

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
-- Data for Name: _sqlx_migrations; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public._sqlx_migrations (version, description, installed_on, success, checksum, execution_time) FROM stdin;
20260531200000	full schema	2026-06-10 17:00:44.83065+00	t	\\xf7fb6f78941d4f2763fe3f8c426c5b9c5353cf635328f826ccbe5fa1e645a4b8c83ddbd74229db2a2ecf9b1fe20d58f6	0
20260601000000	dynamic payment methods	2026-06-10 17:00:45.817981+00	t	\\x64d5df737f8700473312979ebeb68aaf4435d2aa3fd3c71b3d148f9a20eb620146f4271ff3bb1732d5f3367fb9e92ff3	0
20260601191116	add menu translations	2026-06-10 17:00:46.76796+00	t	\\x20008b2532b0539436f98185a1212659cad6f35964f33515ba51d30d0d6dcd296311e34cec8f812091caa0e9d6f5715e	0
20260601192023	add order translations	2026-06-10 17:00:47.743918+00	t	\\xb1a155146060195b0bc1bfa041a84e0b082c4235fb79357445d57cb9ed1347774efc7bfa55e20dcea25fe4030d656c0c	0
20260601194459	add nested translations	2026-06-10 17:00:48.737598+00	t	\\x4242d01a60640d055f5fe605ae29fa57bf7cd11994a9e7d722e6c33c03ffa5e8e054ca9870c6912471a7d147bee8a1c3	0
20260610090000	cost engine	2026-06-10 17:00:49.678367+00	t	\\x1bfe24adea6662de62f720883574be0bb085ea15476d4114e748db21c53501b446677751ee124a41c82fe2f48a084b61	0
20260610110000	org onboarding	2026-06-12 09:45:21.237436+00	t	\\x1d12be6b96abb155c740a9137932c94f43c7365ab554f2405d9ac9f4690768e85c469e4589984f3aeb4c1617260e858c	869004459
20260611000000	audit fixes	2026-06-12 09:45:22.308595+00	t	\\xd3879e3314f6ceeabcb6d7a293d9d79314bcf13510d4860e886bab4fc98832ce7ded8937e2f57dba9e14c48b1da84c4f	471493709
20260611010000	permissions discounts	2026-06-12 09:45:22.887112+00	t	\\x570c5f645dca51bbbfbae6826e9bac5a0e251b4af7fa2e1417f7b95864e7345cdad6d766b3841dc02f9ba210db21b382	406558209
20260612000000	teller pin branch scope	2026-06-12 09:45:23.425186+00	t	\\xc1eb277e4ce42b34d64fc7f4b1365c9307a12674c1505ddf71184e5f93aa094d19c64d86f115e20c4623636b51285105	713206667
20260612100000	menu advisor rebuild	2026-06-12 09:45:24.236024+00	t	\\x8d9b12fe1c45ccd1e44381e7426fc05f121e77b9e59457c21e220218bb43dbe8f2dde3a671eff23d177c798d3e0c3977	843813000
20260612110000	ingredient costs piastres	2026-06-12 09:45:25.16529+00	t	\\x4f0cb9c76f3c18d461dfdcda9eeddd06d7033a65f2b10cd35c3d8a09edbdc2e4f3bb1e3ad818ba08b08f4ee107ac788a	447921792
20260612130000	nullable ingredient costs	2026-06-12 09:45:25.698924+00	t	\\xc8cf2e8a34f1b154d43d6e46d66b50ad81ffeaa26a57badeba7ba8b9bb3ce1c822715ec8a13132f724e7daebdfb86117	376845250
20260613000000	inventory movements	2026-06-18 17:30:46.34712+00	t	\\xc906d12d4b717a5e1ad892e69e4b93c510e966883f505c5b4ebe04a1e4a81e8e24a6517a38d1b7b4b3c88e6568763e61	72216045
20260613001000	inventory permissions	2026-06-18 17:30:46.530207+00	t	\\xdffa34fce500f2a3311bfa0b9c49ee4374af14c755d8849f9645a4917939493c0d033c9198d56b86ab94b7c785dcfa7a	74432324
20260613002000	stocktakes	2026-06-18 17:30:46.708272+00	t	\\x0a9116e8d36e569af314cfd80a1589da5737854c5c8252cc8379c0c391f74a25754cccbc82fd50e5c9abcdada7a8dd95	66974839
20260613003000	remove shift inventory counts	2026-06-18 17:30:46.896162+00	t	\\x93e332ebef9b98cfdd3e6f073ea22811e59e62939e59d23f956d269aa978ca4b5a1e909a523e1dd71095c79b0bc1391b	70750246
20260613004000	purchasing	2026-06-18 17:30:47.074486+00	t	\\x4a93f6e9a40e8313e779c90da3d8d2d10cf4fcfd4e37fb4b79cc0e16400b2598cb10c6723095a129673f67f8d54fa371	69927657
20260613005000	void note	2026-06-18 17:30:47.232607+00	t	\\x2993bdb72a20e29d1ff95aa35ee4e790d382c31ff64d57afa1453432c518a4f7e2ed9bfc6b060ec9e3ea72aa40a061a2	49103386
20260613006000	drop display order	2026-06-18 17:30:47.402253+00	t	\\x7ccefcdcd4c69eae1446e9160529140eee9a8d8e357f6e9a4b98a43ccb3b4bf898180e624b784c53b6d95741df729c7f	60000195
20260613010000	inventory guardrail and gaps	2026-06-18 17:30:47.552313+00	t	\\xe7b1b7409ae7339f315876ede682aed48babf638f600919214a7439a66e08d80cbc0a0474b51b42f90971df570cddbbb	52815689
20260613011000	one open shift per teller	2026-06-18 17:30:47.711298+00	t	\\xc2729d8d443467ac220235a9025cb1527d1c58cf12d2477c2e7256f340900686bd197509f3e523d3c3128f59d6039153	48180628
20260614000000	one open stocktake per branch	2026-06-18 17:30:47.882066+00	t	\\xe5f3a26e15faa09c3b7766781e63b5d6ee9c76d5ac99b175618b81262c1d774b7926736065f02c1886de86c6940fcd1d	52980274
20260614010000	payment is cash snapshot	2026-06-18 17:30:48.078774+00	t	\\x7f3ec5ce04a66e99239a6e73ece97cfeba20853ad511d68e4473445a186466e25b7b4cd2c7b89d62c2fee1674c480ae9	84351364
20260614020000	order ref	2026-06-18 17:30:48.257714+00	t	\\x0a1471ca74811c2453ce2e24956c2e72bdde63f3096e35868d2bcea18d2cdc9eddb560c0cb59ec11cf9cc7a0a59d6137	70343350
20260614120000	branch menu overrides	2026-06-18 17:30:48.421958+00	t	\\xb09e7153d52079c7bd3ccc15658db32499d29a6d49c7427ba16bb41b4bc669d0e965f7efc1183ae3b3ca565fa6986ca3	56323365
20260614130000	branch menu size overrides	2026-06-18 17:30:48.591157+00	t	\\xdd7cfd8359d635e5b7c5054fcc83587c7c883a3d10f9c13c7a2d0edc27c90d68a41a93525bc344cacd77a746e9302b1c	54932429
20260614140000	branch addon overrides	2026-06-18 17:30:48.755056+00	t	\\x17ab4703caa7bdea423e9501f6ec97c18b21beea41b46b711769487eec8b14a018b82c34e1e097b9c9c9a2ab309ac91a	51259245
20260614150000	delivery core	2026-06-18 17:30:48.950302+00	t	\\x8bf7029fb6f1fc90ba17fa60317debd11f5445c6f45ef8df18a1365164893b2da50b9b9a81e131cda5be4f06b3930745	79346321
20260614150100	branch channel menu overrides	2026-06-18 17:30:49.120428+00	t	\\x7d5a408bc2cbadf0c7c94bf84594c8d38c1233352a31d8cec581ab835c93ee743bff1c1e004f84635bc45e239ce79f07	58598974
20260614150200	orders delivery link	2026-06-18 17:30:49.278157+00	t	\\xd4c054441ea955b0fc0514454728bf0b102f4f9c7ae36045b364fb5b76c836fff87f39f7b60f1b5796f1b4720f3af6e4	59230826
20260614150300	delivery permissions	2026-06-18 17:30:49.437174+00	t	\\x335205ece976b91a70b473a7ee41d6766f6372eb5cfadc4a0963e67e9ddc4798fc89c8ecf8945f5caaa6147358c45c82	54163391
20260615120000	delivery channel discounts	2026-06-18 17:30:49.587692+00	t	\\x27986f86c32bb9fabd1ea3195c72e227c1258bacc99391294a0ca25f34a821e1f8c079766e46786c4da2d51251302f7e	52909138
20260615130000	org timezone	2026-06-18 17:30:49.761009+00	t	\\xca216948a78c0113fde4a57ba1be6d838f0c8ad5c3da5b171ade574d96f30263be0a27580404d9cd6d3ec90a5744cee7	56591976
20260615140000	timezone enum	2026-06-18 17:30:49.96975+00	t	\\xef024e42cb98ae2e2e4f0a4a3377bb6fd2f81c287dcb0fd712ac6ad2fd8296ce0b5328bfc217c034fd3c6bc4d67e7fb4	85816212
20260615150000	branch otp required	2026-06-18 17:30:50.133973+00	t	\\x842e5b9af6d6bde8e299a6597692c32a00d7b570f3aa91ee71ccedad08b8b0dc289ed2909bb068bb474f8ce83fa88a4f	51489419
20260616000000	per branch costing	2026-06-18 17:30:50.323815+00	t	\\x48fd430df4281178612ad09c9d61a0a026839dbe0bb373e97c2651d7d72647a5ca40022dfbb0560a769e5fd47cab006a	66434133
20260616090000	whatsapp gateway settings	2026-06-18 17:30:50.476814+00	t	\\x5ac08db4288df975f2c41a83a3898ab6562d67439da3f65c9f8f916bda8f2e001563321950ee945d35d602829d5fdf3a	50429315
20260616100000	stocktake reconcile to live	2026-06-18 17:30:50.649969+00	t	\\xa40564f1d66136140ddbfdeafbc3537c4b2590a65d17ccd7fc95cc0cd014db47e4b6cae9f2bd768ecdaa679a571d845d	54534805
20260616110000	catalog depth	2026-06-18 17:30:50.796913+00	t	\\xdf060bfc2e54a060a5e1329f21e16bd542560e1e30304a7a29a7ce4377169a56778a63443a0267b7c55c169f5d33ae87	46329834
20260616120000	in mall require location	2026-06-18 17:30:50.952626+00	t	\\x5932873aa66b9b272609440a52f40ff1a3e1bf735d069bcc52f02b19f5972c8b5a4ae4ea912afa6d90ba60800bd8b1be	50113123
20260616130000	recipe depth	2026-06-18 17:30:51.131848+00	t	\\x1ed8d415ea007d3afc2883f671ad82a64c76317863d19c579df4ebb9ba929ec74cfe5e0a754df36b8674e86524d27efa	62562050
20260617000000	adjustment reasons	2026-06-18 17:30:51.294602+00	t	\\xa2a2bdda05ae2755fb9bf5e6724c34d3b5c84d6dba626738c7dcbf34e45b0b3bfcbbad52e2d67c5fc3db697ed8ce013f	54529568
20260617010000	goods receipts	2026-06-18 17:30:51.470927+00	t	\\x42cc058fc377a29e4add9336dadeeb4ce2614ba993bade61a4db8f02b73da32cb17b038cb493fe1d7a004bd4a2916ee4	62433649
20260617020000	remove adjustments	2026-06-18 17:30:51.659353+00	t	\\x0b78a9026923a72c8e3f78dfe30c639a95c9e2b55406e151e4aa131cc4ddcf025f4d8fff47be4dc3994c49171cb2351c	68106559
20260617100000	menu item allowed addons	2026-06-18 17:30:51.841323+00	t	\\x187a03a57bcd29e26edd417fa8fdd0b18817e56485da8febbec79c2172bf9d3205dca115eb3ccad5a2ae64ccfaaa8358	58306677
20260617100001	menu item allowed addons grant	2026-06-18 17:30:52.023476+00	t	\\x08dcd788d265fe9b658bebc6c51a01ee9a421fa5aa28b9a4fc2627a1355e31da233243c53dd26243ccc44a8c9e05f110	66548162
20260618010000	qr dynamic	2026-06-18 17:30:52.179849+00	t	\\x8bb5bdacd74ca790e96772fcbac2627f033cd5c6dc88f0cc9bd564ffcda360af0caeb14546eb25ca4437bde10f5eba7b	56902239
20260614030000	order ref finalize	2026-06-18 17:30:52.514463+00	t	\\xa3effe673294f07a3ec9d462a8a6e14191fecd920bd65996f56483db67eaeea98273c9debcb57cf2c776979321f33f6c	56196216
20260619000000	cash movement client ref	2026-06-21 17:57:51.624159+00	t	\\x8bbc089275b86fb7522c3676ccd4c191ef0079745fb9e1210d067e264677d865b3c40b822bbcf1341ae51f9cca9f379b	49867760
20260619100000	offline pin hash	2026-06-21 17:57:51.6754+00	t	\\xbe44ce3e962774a53cd41b903bfa1116dac449bebb2dfe43ae05eae0a8a881dd453cf55d3d676ec54ff86d792f997ae5	1564279
20260625000000	tills	2026-06-27 16:19:26.231889+00	t	\\x3a1abfedbbf6db2d86c0c2c24ad64d3874c2a2f6e003177c9ed98bb1c8276927cb1514a49f9f9cc3479709126d9503d1	24568447
20260625001000	shift tills	2026-06-27 16:19:26.257281+00	t	\\x4d5be89f07215df64f5d6e064cdf773a5f69ad0b53e40926bdabf67479fb666657ee1815e325fb29d411498b3273cde1	24107709
20260625002000	kitchen stations	2026-06-27 16:19:26.282231+00	t	\\xcf669d194ea1795554dc81cdec40a499069d437c0c21a064e9486a9bb1da7c00fd23f5e62c3039b85937e8af55eebdcb	12377022
20260625003000	kitchen tickets	2026-06-27 16:19:26.295207+00	t	\\x1d1059ae762662bb43a6aca58adf7405cad73dd22e09ebc1234945bd94cb1ab7367c68a5666bad097599fb3f93fe459d	7661651
20260625004000	open tickets	2026-06-27 16:19:26.30347+00	t	\\x3d387bdd80c8872c5869a9d1c7d551b3354c0a2bee0cf77cbb6d3d2debd2f37d0caca0cdd9b36937cc30027d7a0d0f80	13969440
20260625005000	waiter role	2026-06-27 16:19:26.318656+00	t	\\x74e67dde25965c892de31af2e079491051f6d89d63b1f45db693483aec5775b57064d2e0f9562abd042d047ef4760d6f	1129607
20260625006000	kitchen permissions	2026-06-27 16:19:26.320546+00	t	\\x07ac3e5eae369eb19c902f973700b47c5d13cbd3214c2fda8c49bf091bc53de2826ab6a2168f2290b357f31e12d9abde	838883
20260625007000	open ticket discount	2026-06-27 16:19:26.322001+00	t	\\xb5fe0427e3a7e3a2b41a189a2a5cd8e62ac66526cd775553b8f973503d143bec93f1b4016472e8d4ec1693edc7a3a4de	1358509
20260625008000	kitchen routing off	2026-06-27 16:19:26.323961+00	t	\\x9394313c7c1bd568ad1c1b5658c087ef3b10a5b9f63161df2bf9d729a8a49a2e0813333db87b659d15c52a8f22ba5e63	550101
20260625009000	kitchen role	2026-06-27 16:19:26.32505+00	t	\\xa20d73cd962f3c0d74a2a7447735f2248903884207a4a638805b838702cb707e0a06ee3fa779cf7b6263fe5eae8eb3fe	961956
20260625010000	org lan secret	2026-06-27 16:19:26.32648+00	t	\\x7d64bd1b9e4e2bb0a8761ca6c1a08cde084456d7e137c7e1f9dc3ca40054be2b0f3ea1a378542e94dcba8591139bad64	7699858
20260630000000	drop org onboarding	2026-06-30 21:40:48.37943+00	t	\\x3716a2b07360a1aa237280139ab16535b93d3d2d77df5b03796ba78ad7d0d1f5ce14809775c96ab7f1e830019d2e5551	9208200
20260630000100	onboarding rebuild	2026-06-30 21:40:48.389389+00	t	\\x5427edeec8ed049cc2f3c4f0cc5a98fdbf576c4c19e5e240ac80291020a03502e7ef5596c6aa2b5b1a858f73c8043b8f	3593436
20260630010000	demo orgs	2026-06-30 21:40:48.393478+00	t	\\xef1fa576cec14850356f3b78ac22514cb6e71fc88732f5cc7126cec77dd04a7bf53d6c173147a41fa842844e5e4a5317	6992731
20260630100000	item size enum to text	2026-06-30 21:40:48.401155+00	t	\\xb3f3f072a4cf480e9689e724368690989c8cb6ced6522848981ccf1e13c73cf51ea0d4cf32fdc3042726ce10c79882ca	41703271
20260630120000	reservations floor	2026-06-30 21:40:48.44395+00	t	\\x0a30c54f8ae59d889c67eaa12cdc3808c54d16c20fef47dd65f341f041414030e35077dea454d6667061bea2512978bf	30973599
20260630120100	reservations bookings	2026-06-30 21:40:48.475855+00	t	\\x2fdb85334e5c98f1cfb9e25539c09ad9e11a63db43b78aec20aa3b8afcb0ef9b5d0e03e0e760ac79aebd612c94d1fda7	12679483
20260630120200	reservations permissions	2026-06-30 21:40:48.489642+00	t	\\xb329ff58d05b83fbc826db3fd2f43bc017717ca61f37ae48d359e8e6d9a70796fc4068c74d26e7e2fb3e9ad48135e4f9	843820
20260701000000	add umbrella pickup channels	2026-06-30 23:05:24.325824+00	t	\\xf847434ccdcb9b50ed91f859f72f49129aaefe1d4923c98d720b86c7a7a04d0a5c221938d101389807202159d4a644a5	8342376
20260701010000	add waiter to orders	2026-07-01 04:08:47.134755+00	t	\\x53ca25741a58fac72d54b7931573bc451f6958b1ef5bf0635f4feeb80bc2b42ee05007af720f48ea6b03219ab750ffe2	39228603
20260703100000	menu unification expand	2026-07-04 00:20:15.99676+00	t	\\xd5452fdb33a9db94873406a70e0d30cf534093e67b258d3bb32b767ca2b0925652df6f55ef377a6b88eb79c1897a7383	102946309
20260704120000	drop menu advisor	2026-07-04 20:23:49.555663+00	t	\\x00356b8ea316d4ef165aafec320a71a8667c81ddd6782c078dc53327a928ef7212a4eac2acf88bbe1aea966e6ce94335	24564449
20260704140000	insights	2026-07-04 21:00:21.927076+00	t	\\x6e00cefc1326b28232523027e645af10da82d4e577651586d0b334538e325c6c20e36cffd66409b3f7984e0ff5a05630	21486792
20260708000000	rls role	2026-07-08 08:45:34.390662+00	t	\\x57ac37d63624788069226d6f345de94bf97a07b50fae70b4d5c8fcb45b0a04ae2405148ba7e64ee523db47b2a8534bb3	17726337
20260708000100	rls policies	2026-07-08 09:33:28.077156+00	t	\\xf7688f7003acf37284871cd9cc4d4cc91e5e5992f4e00224970d58aebe2480b8218ea89ac6f39ef008d335d95dedf78f	40687496
20260803000000	integration credentials	2026-08-03 13:42:55.81457+00	t	\\x5091cf745d98868ad405802940de3063431c408b51b380ba7230e8f205741760698401bf557ce9192652a90e72a7c180	66345573
20260803120000	payment method integration visibility	2026-08-03 14:10:51.255423+00	t	\\x905d2cb7176c3f480961242fe8cb5a43cccc65257fb2be51de09ca1616fd27ef8f3f708f4159beac9c7b347bcf7b4c28	11840135
20260808000000	staff directory	2026-09-03 10:53:27.719393+00	t	\\x955a8bfb8a7d744eba0e05adbadc2483e76c6d9a7ccc36ca794701346c91e764df44a2eae3553edda1cbb0c17a93a605	95942044
20260808000100	staff schedules	2026-09-03 10:53:27.817037+00	t	\\x60f9c55a14b9f4d9b46ff2c6743c821b230c440986ad5941eb7949e2ad739be95c52c2dd5189a4ac4da7f6b71c9b11e5	30989612
20260808000200	staff attendance	2026-09-03 10:53:27.849145+00	t	\\x56b429a683b9cd969caa5b170fa542d4a17dd270b8d5424d04b81777776dc31007dddbc72c820812b6c36f733219cc78	18075466
20260808000300	staff leave	2026-09-03 10:53:27.870268+00	t	\\x6c01e347f29bea5bdbca32f643a977da1da9c041d15ae8fce6515fea660b93d131844d26d7ff7136a3dae46129270878	32532325
20260808000400	staff payroll	2026-09-03 10:53:27.904114+00	t	\\x883db8f950a28b1ff14fe7284bdc86cd800b37cbe9da17830bfdea0b3fbd998b255fa680d4d9e1249730c95f8840fc9b	44215043
20260808000500	staff permissions	2026-09-03 10:53:27.949069+00	t	\\xf46d50f1c0f15eb530058ab280dd904007a33a9b7cbd97f89751c85a618122b64bcc4661158d8a70ffa639454269d6e3	2704048
20260810000000	staff requests	2026-09-03 10:53:27.952739+00	t	\\x8da7e3aeef70dd356c9b21534553868904683cb52e65a409f4e6b1bf9c119f3ea4963cdb0748baf97ac74a460d2d86a9	42796344
20260810000100	deduction overrides	2026-09-03 10:53:27.998822+00	t	\\xa37a84f12c1f9b0eacaa259d297e888498d71607089094067a9c587cee0e70afb601c32992612de0d2d410a30711a157	8301949
20260815000000	staff request corrections	2026-09-03 10:53:28.008993+00	t	\\xd7df090873aa2d236a33eb12231d0d87ee3f99848da41aa0a5af8f43e871f79fc28ec7f3b4f4569e6475ea4088c64f5c	10533051
20260903000000	ai conversations	2026-09-03 10:53:28.020005+00	t	\\x966a18f99354e2819b68e190924ac60b112d6dd4ffcec3007f0159de4ffc187710939b37fcc1100140671902136c6d4d	30093524
20260815100000	held orders	2026-09-05 12:45:16.69378+00	t	\\xd9cb3c9c9f2f246134a940dae03f6fd54ad334ac9ca98f523d6963b90e3dcb7a43ac47d7cf46685229535173cd48cfdd	37851963
20260815100100	held orders permissions	2026-09-05 12:45:16.732621+00	t	\\x8d747becd7d3c696910253a4cc487dad025cf1c9accbf60c3e42b71a893ec8fa64ec2b8ceb8757473cf15846e7883a02	1850305
20260905090000	drop bookings and held orders	2026-09-05 12:45:16.734959+00	t	\\xf56e000f3cbcb16319c4cbaee6b573301b6acc4ebc4a4e65a1349ecbb839c7440222672fede6243de129948a4f4121e1	17258431
20260906000000	inventory v2	2026-09-05 20:53:54.936282+00	t	\\x9d2c2517c367b2ba776845d2fd0d1901145f29ce4abf43d52982bd93ae1c8716b37f1ed0c4aaa7c59fce616504d99cfa	609060936
\.


--
-- PostgreSQL database dump complete
--


