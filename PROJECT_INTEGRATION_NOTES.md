# Integration Notes — Cross-Project Changes

Changes here require corresponding updates in the **POS app** (Flutter) and/or
the **dashboard** (React). Backend changes are deployed; client changes are
outstanding as of 2026-06-12.

---

## POS App — Required Changes

### [BREAKING] PIN login must now send `branch_id`

`POST /auth/login` — `branch_id` is now **required** when logging in with a PIN.

```
Old: { "name": "Ahmed", "pin": "1234" }
New: { "name": "Ahmed", "pin": "1234", "branch_id": "<uuid>" }
```

Returns `400 Bad Request` if `branch_id` is absent.  
Returns `401 Unauthorized` if the branch doesn't exist or the teller is not assigned to it.

> **Why:** Tellers at different orgs could share a name+PIN, producing a
> non-deterministic login. Scoping to a branch eliminates the collision.

### [NEW] Auto-detect branch by GPS — `POST /auth/resolve-branch`

Unauthenticated endpoint. Call at app startup to auto-select the branch if one
hasn't been manually configured.

**Request:**
```json
{ "org_id": "<uuid>", "latitude": 30.0626, "longitude": 31.2497 }
```

**Response (200):**
```json
{ "branch_id": "<uuid>", "branch_name": "Zamalek", "distance_meters": 42.3 }
```

**404** if no active branch for the org has coordinates set, or if the device is
outside every branch's `geo_radius_meters` (default 200 m).

### [NEW] Tellers can void orders

`orders:update` is now granted to the teller role. Show the void button in the
order detail screen for tellers.

### [NEW] Transfer deletion may return 409

`DELETE /inventory/transfers/:id` now returns `409 Conflict` if the destination
branch would go negative after the reversal. Handle this response; previously
it silently decremented below zero.

---

## Dashboard — Required Changes

### [NEW] Branch geofencing fields

`POST /branches` and `PUT /branches/:id` now accept:

| Field | Type | Notes |
|---|---|---|
| `latitude` | `f64 \| null` | WGS-84, clearable |
| `longitude` | `f64 \| null` | WGS-84, clearable |
| `geo_radius_meters` | `i32` | defaults to 200 |

Add these to the branch create/edit form. Saving `null` for lat/lng clears
the geofence (branch won't appear in `resolve-branch` results).

`GET /branches` and `GET /branches/:id` responses now include these three fields.

### [CHANGED] Discount permissions use `"discounts"` resource

The permissions matrix now includes a `discounts` resource (separate from
`menu_items`). Any UI that renders the permission matrix must include this row.
It is already seeded in the backend.

### [CHANGED] Payment method permissions use `"payment_methods"` resource

The permissions matrix now includes a `payment_methods` resource (was previously
checked against `"orgs"`). Update any hardcoded resource list in the dashboard.

---

## Backend — Pending Production Steps

Before deploying migration `20260612000000_teller_pin_branch_scope.sql`, run this
diagnostic on the production DB to find tellers that would violate the new
uniqueness index:

```sql
SELECT org_id, LOWER(name) AS lower_name, COUNT(*)
FROM users
WHERE role = 'teller' AND deleted_at IS NULL
GROUP BY org_id, LOWER(name)
HAVING COUNT(*) > 1;
```

Rename any duplicates before running the migration.
