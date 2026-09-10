-- A shop can insist that a dine-in sale belongs to a table.
--
-- Off, the till can ring up a dine-in sale that names no table, which is
-- right for a counter and wrong for a dining room: the sale exists, the
-- floor never knew about it, and nobody can tell from the room who has
-- ordered and who has not.
--
-- On, the till's own "ring it up" path is refused and the sale has to go
-- through a table — seat the party, add the rounds, settle. Two paths stay
-- open regardless, deliberately:
--
--   * settling a TICKET, which by definition already has its table;
--   * replaying a queued offline order, which was rung up on a device that
--     could not ask. Refusing that at replay would dead-letter a sale that
--     really happened, which is worse than the gap it closes.
--
-- Per organisation rather than per branch, and it costs nothing at a branch
-- with no tables: a shop cannot be made to seat somebody in a room that has
-- no seats, so the rule only bites where a floor is authored.
ALTER TABLE organizations
    ADD COLUMN require_table_for_orders boolean NOT NULL DEFAULT false;

COMMENT ON COLUMN organizations.require_table_for_orders IS
    'Dine-in sales must belong to a table. Enforced on the till''s direct '
    'order path only — ticket settle already has one, and offline replay is '
    'history. No effect at a branch with no tables.';
