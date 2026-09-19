# Rehearsal report — retire the '… staff' twins, org Drops

Generated 2026-09-19T13:58:20Z by running 2026-09-retire-staff-drink-items.sql
against a LOCAL restore of the prod dump manual-20260915T134318Z.dump.
The script ended in ROLLBACK: nothing was changed, here or on prod.

```
BEGIN
psql:2026-09-retire-staff-drink-items.sql:38: NOTICE:  schema "archive" already exists, skipping
CREATE SCHEMA
SELECT 16
SELECT 16

=== REPORT 1: twins that would be retired (soft-deleted + deactivated) ===
          twin_name           | twin_active | history_lines | history_qty | twin_recipe_rows |        real_item         | real_price | real_recipe_rows 
------------------------------+-------------+---------------+-------------+------------------+--------------------------+------------+------------------
 Tea Staff                    | t           |             1 |          10 |                0 | Tea                      |       7000 |                0
 Caramel macchiato Staff      | f           |             0 |           0 |                0 | (NO MATCH — map by hand) |            |                0
 Cortado STAFF DRINK          | t           |             0 |           0 |                0 | CORTADO                  |       9500 |                0
 Espresso staff               | t           |             0 |           0 |                0 | Espresso                 |       8500 |                0
 Flat white staff             | t           |             0 |           0 |                0 | Flat white               |      10500 |                0
 Hot spiced chai Staff        | t           |             0 |           0 |                0 | (NO MATCH — map by hand) |            |                0
 Iced caramel macchiato Staff | f           |             0 |           0 |                0 | (NO MATCH — map by hand) |            |                0
 Iced latte Staff             | t           |             0 |           0 |                0 | Iced latte               |      12500 |                0
 Iced salted caramel Staff    | t           |             0 |           0 |                0 | (NO MATCH — map by hand) |            |                0
 Iced spanish Staff           | t           |             0 |           0 |                0 | (NO MATCH — map by hand) |            |                0
 Iced spiced chai STAFF       | t           |             0 |           0 |                0 | (NO MATCH — map by hand) |            |                0
 Latte staff                  | t           |             0 |           0 |                0 | Latte                    |      11500 |                0
 Macchiato Staff              | t           |             0 |           0 |                0 | Macchiato                |       9500 |                0
 Salted caramel Staff         | t           |             0 |           0 |                0 | (NO MATCH — map by hand) |            |                0
 Spanish latte Staff          | t           |             0 |           0 |                0 | Spanish latte            |      14000 |                0
 Turkish single staff         | t           |             0 |           0 |                0 | (NO MATCH — map by hand) |            |                0
(16 rows)


=== REPORT 2: suggested staff_pool_settings.eligible_item_ids ===
                                                                                                                                            eligible_item_ids                                                                                                                                            
---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
 144aeec3-06e4-4b9d-99a7-e0384d1db371,36ce47c4-2e6a-4dff-84bd-1d62fcf89b34,4d5de4f4-c5c8-42be-8296-20aada41e3de,9c016a8a-3572-4a39-bc8d-7082f70e5cb9,b56ba935-3e84-4334-ace5-2ffc6270dd8f,c302f6e1-6184-4c8e-a56e-d656234266cd,ed55e82e-fa65-48d9-b9a6-d4ac17014b68,f75d5cb2-5cc6-45d4-bea0-74864ddc53e3
(1 row)

               real_id                |   real_name   
--------------------------------------+---------------
 c302f6e1-6184-4c8e-a56e-d656234266cd | CORTADO
 b56ba935-3e84-4334-ace5-2ffc6270dd8f | Espresso
 9c016a8a-3572-4a39-bc8d-7082f70e5cb9 | Flat white
 36ce47c4-2e6a-4dff-84bd-1d62fcf89b34 | Iced latte
 f75d5cb2-5cc6-45d4-bea0-74864ddc53e3 | Latte
 ed55e82e-fa65-48d9-b9a6-d4ac17014b68 | Macchiato
 4d5de4f4-c5c8-42be-8296-20aada41e3de | Spanish latte
 144aeec3-06e4-4b9d-99a7-e0384d1db371 | Tea
(8 rows)


=== REPORT 3: twins with NO real counterpart (retire only after mapping) ===
          twin_name           | history_lines | history_qty 
------------------------------+---------------+-------------
 Caramel macchiato Staff      |             0 |           0
 Hot spiced chai Staff        |             0 |           0
 Iced caramel macchiato Staff |             0 |           0
 Iced salted caramel Staff    |             0 |           0
 Iced spanish Staff           |             0 |           0
 Iced spiced chai STAFF       |             0 |           0
 Salted caramel Staff         |             0 |           0
 Turkish single staff         |             0 |           0
(8 rows)


=== REPORT 4: references that would dangle (resolve before COMMIT) ===
 kind | ref_id | twin_name 
------+--------+-----------
(0 rows)


=== counts ===
 twins_retired | mapped | unmapped | historical_units_kept 
---------------+--------+----------+-----------------------
            16 |      8 |        8 |                    10
(1 row)

ROLLBACK
```
