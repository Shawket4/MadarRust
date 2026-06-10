-- Add translation columns to categories
ALTER TABLE public.categories 
ADD COLUMN IF NOT EXISTS name_translations jsonb DEFAULT '{}'::jsonb NOT NULL;

-- Add translation columns to menu_items
ALTER TABLE public.menu_items 
ADD COLUMN IF NOT EXISTS name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
ADD COLUMN IF NOT EXISTS description_translations jsonb DEFAULT '{}'::jsonb NOT NULL;

-- Add translation columns to addon_items
ALTER TABLE public.addon_items 
ADD COLUMN IF NOT EXISTS name_translations jsonb DEFAULT '{}'::jsonb NOT NULL;
