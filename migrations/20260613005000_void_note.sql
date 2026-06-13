-- Free-text explanation to accompany a void. void_reason stays a categorized
-- enum (for void-rate analytics), but the 'other' category needs a place to
-- record what actually happened — and any void may carry an optional note.
-- The handler requires void_note when void_reason = 'other'.

ALTER TABLE orders ADD COLUMN IF NOT EXISTS void_note text;
