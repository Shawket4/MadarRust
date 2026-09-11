-- A cash movement says WHAT KIND of movement it is, not just which way.
--
-- `shift_cash_movements` has always been a signed amount and a free-text note.
-- The POS offers two chips, In and Out, and the shift report adds up the
-- positives as "cash in" and the negatives as "cash out". That is the whole
-- vocabulary, and it is one bit too small. The live ledger shows what that
-- costs: 352 of 373 movements are pay-outs (supplies, electricity, an Uber for
-- the milk), and among the 21 positives are a teller's 8,500 "gourmet" tapped
-- as In, the 8,500 Out that cancels it, and the real 8,500 Out with a note
-- apologising for the pair. The report shows 8,500 of "cash in" for that shift
-- that nobody handed anyone. Nothing in the schema could have caught it,
-- because a positive amount is the only thing "In" means.
--
-- Worse, two movements with the same sign mean different things to a manager
-- reading the close. Taking 2,000 to the safe and spending 2,000 at the
-- supermarket both leave the drawer 2,000 lighter and both read as "cash out",
-- but one is money the shop still has and the other is money it spent. A
-- report that cannot separate them cannot say how much the shift cost to run.
--
-- So every movement now carries a `kind`, and the kind fixes the sign:
--
--   pay_in      cash placed in the drawer that is NOT a sale — an owner topping
--               up a thin float ("سيولة"), a delivery platform's settlement
--               handed over in notes. Always positive.
--   pay_out     cash taken to PAY for something; the shift's running costs.
--               Always negative. This is what almost every existing row is.
--   safe_drop   cash taken out of the drawer and put in the safe. Not spent —
--               the shop still has it — so a report must never count it as a
--               cost. Always negative.
--   correction  reverses a movement recorded by mistake. Either sign, because
--               the mistake can go either way; `corrects_id` says which row it
--               undoes, so a report can net the pair instead of showing phantom
--               cash in AND phantom cash out.
--
-- Two things are deliberately NOT kinds here. The opening float lives on
-- `shifts.opening_cash`, and `compute_system_cash` adds it before summing the
-- movements — a movement kind for it would count the float twice. And a REFUND
-- is not a cash movement: it is keyed to an order and belongs with the order,
-- where the report can tell "we gave the customer their money back" apart from
-- "we bought ice". The drawer maths should read cash refunds from wherever the
-- refund feature records them, not from this ledger.
--
-- Backfill is by sign: negative → pay_out, positive → pay_in. That is exactly
-- what the report has been calling these rows all along, so no figure anyone
-- has seen changes. The handful of reversal pairs above stay as they are — the
-- data cannot say which half of each pair was the mistake, and guessing would
-- rewrite history to look tidier than it was.
--
-- The kind is a plain text column with a CHECK rather than an enum, like
-- `order_type` and `recipe_steps.kind`: when the refund work (or anything else)
-- needs another value, it is one constraint swap inside a normal migration, not
-- an ALTER TYPE that cannot be used in the transaction that adds it.

ALTER TABLE shift_cash_movements
    ADD COLUMN kind text,
    ADD COLUMN corrects_id uuid REFERENCES shift_cash_movements(id);

UPDATE shift_cash_movements
SET kind = CASE WHEN amount < 0 THEN 'pay_out' ELSE 'pay_in' END;

ALTER TABLE shift_cash_movements
    ALTER COLUMN kind SET NOT NULL,
    ADD CONSTRAINT shift_cash_movements_kind_is_known
        CHECK (kind IN ('pay_in', 'pay_out', 'safe_drop', 'correction')),
    -- The kind decides the direction; the amount may not disagree with it.
    -- Every branch also rules out zero, which the handler rejects today and the
    -- table never held (0 of 373 rows).
    ADD CONSTRAINT shift_cash_movements_kind_matches_sign
        CHECK (
            CASE kind
                WHEN 'pay_in'    THEN amount > 0
                WHEN 'pay_out'   THEN amount < 0
                WHEN 'safe_drop' THEN amount < 0
                ELSE                  amount <> 0
            END
        ),
    ADD CONSTRAINT shift_cash_movements_only_a_correction_corrects
        CHECK (corrects_id IS NULL OR kind = 'correction');

CREATE INDEX idx_shift_cash_movements_corrects
    ON shift_cash_movements (corrects_id)
    WHERE corrects_id IS NOT NULL;

-- Safety net for the clients already in the field. The live POS and dashboard
-- send only a signed amount and a note; until they ship a kind picker, a row
-- that arrives without a kind gets the one its sign has always meant. Same
-- shape as `shifts_fill_default_till`: the app should set the column
-- explicitly, and this exists so the NOT NULL above can never turn a teller's
-- pay-out into a 500. For an insert that names its kind it is a no-op.
CREATE OR REPLACE FUNCTION shift_cash_movements_fill_kind() RETURNS trigger AS $$
BEGIN
    IF NEW.kind IS NULL THEN
        NEW.kind := CASE WHEN NEW.amount < 0 THEN 'pay_out' ELSE 'pay_in' END;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_shift_cash_movements_fill_kind
    BEFORE INSERT ON shift_cash_movements
    FOR EACH ROW EXECUTE FUNCTION shift_cash_movements_fill_kind();

COMMENT ON COLUMN shift_cash_movements.kind IS
    'What the movement IS, which fixes its sign: pay_in (+, non-sale cash placed
     in the drawer), pay_out (−, spent on something), safe_drop (−, moved to the
     safe, NOT a cost), correction (either sign, reverses `corrects_id`). The
     opening float is `shifts.opening_cash`, not a movement; a refund belongs
     to its order, not here.';

COMMENT ON COLUMN shift_cash_movements.corrects_id IS
    'For a correction: the movement it reverses, so the pair nets to nothing in
     a report instead of reading as cash in AND cash out. NULL for a correction
     of something that was never recorded as a row (a miscounted float).';


-- ── The till's standard float ────────────────────────────────────────────────
--
-- Closing a shift asks the teller for `closing_cash_declared`, and the next
-- shift on the same drawer opens with that figure (`previous_declared_closing`
-- is keyed on the till precisely so the float follows the drawer, not the
-- person). But what SHOULD be left in the drawer has never been written down
-- anywhere: it is a number the manager told the teller once. The closes in the
-- live data say what happens next — 75 shifts declared 0, the others scatter
-- across 11,000, 16,000, 3,000, 4,000, 13,000, 35,000 — a drawer that opens with
-- whatever the last person happened to leave in it.
--
-- `standard_float` is that number, on the till, where the drawer is. With it
-- the close can propose "leave 5,000, drop the rest into the safe" instead of
-- making someone remember, and the safe drop that leaves the drawer at the
-- standard float is exactly the `safe_drop` kind above. NULL means the shop has
-- not decided, and everything keeps behaving as it does today; no value is
-- invented from the closing history, because a mode of 0 is a shop that empties
-- the drawer nightly, not a float anyone chose.

ALTER TABLE tills
    ADD COLUMN standard_float integer
        CONSTRAINT tills_standard_float_is_not_negative
            CHECK (standard_float IS NULL OR standard_float >= 0);

COMMENT ON COLUMN tills.standard_float IS
    'The cash that should be in this drawer at the start of a shift, in minor
     units. The close defaults `closing_cash_declared` to it (drop the rest to
     the safe); the next open on this till inherits it as the carryover. NULL
     when the shop has not set one — no default is proposed.';
