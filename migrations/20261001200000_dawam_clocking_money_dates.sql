-- Dawam Phase B · clocking (AT-1, audit 08): a money date is the day where
-- it happened, in that branch's zone — never the database server's date.
-- Both writers of expense_advances name the day themselves (the dashboard's
-- log in the branch's zone, the till's pay-out in the till branch's zone), so
-- the server-date default goes: a writer that forgot the day now fails
-- instead of silently landing on the wrong one.
ALTER TABLE expense_advances ALTER COLUMN given_on DROP DEFAULT;
