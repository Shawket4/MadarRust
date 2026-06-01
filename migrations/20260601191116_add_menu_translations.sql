-- Add translation columns to categories
ALTER TABLE public.categories 
ADD COLUMN name_translations jsonb DEFAULT '{}'::jsonb NOT NULL;

-- Add translation columns to menu_items
ALTER TABLE public.menu_items 
ADD COLUMN name_translations jsonb DEFAULT '{}'::jsonb NOT NULL,
ADD COLUMN description_translations jsonb DEFAULT '{}'::jsonb NOT NULL;

-- Add translation columns to addon_items
ALTER TABLE public.addon_items 
ADD COLUMN name_translations jsonb DEFAULT '{}'::jsonb NOT NULL;
