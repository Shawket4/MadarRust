# Deploying the customers unification — read before the first boot

## What happens on boot

There is no separate backfill step. The server applies pending migrations when
it starts (`sqlx::migrate!`, after `boot_config` has checked the environment),
and the unification IS eleven migrations, `20260925010000` … `20260925110000`.
On the first boot of this release, before it serves a request, the database:

1. gets one canonical phone form (`phone_canonical`, `customers_phone_key`) and
   every stored phone key is rewritten to it;
2. **folds duplicate customers**: live customers of one org with the same
   canonical phone become one — the survivor is the one with the most orders,
   then the oldest; the others get `merged_into` (their ids keep resolving) and
   their orders move. Then a unique index makes it a rule;
3. **creates a customer for every loyalty member** that has none (shared
   primary key: the membership's id is the customer's id), or links the member
   to the customer already holding that phone;
4. moves name / phone / locale / birthday / marketing opt-out from the loyalty
   card onto the customer and **drops those columns from the card**
   (`loyalty_members_v` keeps the old read shape);
5. adds `customer_id` to `delivery_orders`, `bookings`, `open_tickets`, and
   **creates a customer for every phone seen on a delivery order or a booking**
   (source `online` / `booking`), linking those rows, their sales and bills.
   A phone that is not a phone is left unlinked — never an error;
6. creates `customer_addresses` from past delivery orders (deduplicated);
7. adds capabilities 224 `customers.merge` and 225 `customers.addresses.view`,
   the voided-pass columns, and the OTP purge used by erase.

Nobody is renamed: where sources disagree about a name, the customer keeps the
first one and every order/booking keeps its own as a snapshot. Each migration
verifies itself and RAISES if its invariant does not hold — in which case the
server does not start and that migration's transaction is rolled back (earlier
ones in the set stay applied). That is why the dry run below exists.

It is not reversible by a down-migration. Take a backup first.

## The dry run — against a COPY of production, before deploying

```
# 1. a copy (any name containing copy / dry / test / staging / restore)
createdb madar_prodcopy
pg_dump "$PROD_DATABASE_URL" | psql madar_prodcopy      # or restore last night's backup into it

# 2. the report — writes nothing
DATABASE_URL=postgres://<user>@localhost:5432/madar_prodcopy \
  cargo run --bin customers-backfill-dry-run              # [--conflicts 1000]
```

It applies exactly the pending migrations (the SQL embedded in the binary — the
same the server would run) inside ONE transaction, reports, and rolls back:

- customers that would be **created**, per source (`loyalty`, `online`, `booking`, …);
- existing customers that would be **merged** by phone, per org;
- rows that would be linked, per table;
- **conflicts for manual review**: one canonical phone carrying materially
  different names ("Sara" vs "Omar" — not "Sara" vs "Sara Mostafa"), with each
  name, its source and how often it occurs; and how many rows hold a phone that
  is not a phone;
- any migration that would FAIL on this data fails here, with its message.

Never point it at the live database: the migrations' table locks are held until
the rollback, and the server would stall behind them (the tool refuses a
database not named like a copy unless given `--i-know-this-is-not-a-copy`).
The conflict list is personal data — read it, do not paste it anywhere.

What to do with a conflict: nothing is required. If it is one person, leave it.
If it is two people sharing a number or a mistyped number, fix the phone on the
wrong row in the COPY's source system (or after deploy, on the customer page)
— after the migration they are one customer and `customers.merge` cannot split
them.

## After the first boot

- `customers::repoint` runs daily (`CUSTOMERS_REPOINT_SWEEP_ENABLED=0` turns it
  off, `CUSTOMERS_REPOINT_SWEEP_INTERVAL_SECS` changes the tick): references to
  a customer merged more than 30 days ago move to the survivor.
- Mirror the scrub keys in the dashboard and the Flutter app
  (`scripts/check-scrub-parity.sh` fails until they match).
