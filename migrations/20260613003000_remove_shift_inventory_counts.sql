-- Shift-close inventory counting has been removed in favour of standalone
-- stocktakes (see 20260613002000_stocktakes.sql). The shift_inventory_counts
-- table now has no reader or writer; drop it (its FK indexes go with it).

DROP TABLE IF EXISTS shift_inventory_counts;
