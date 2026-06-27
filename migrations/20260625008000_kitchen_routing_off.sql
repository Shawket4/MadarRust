-- A fourth routing mode: 'off' — the branch creates NO kitchen tickets at all
-- (pure-retail / no-kitchen branches). ADD VALUE must be its own migration (the
-- value isn't used in the same transaction it's added in).
ALTER TYPE kitchen_routing_mode ADD VALUE IF NOT EXISTS 'off';
