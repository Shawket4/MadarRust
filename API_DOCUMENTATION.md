# Sufrix POS API Documentation

Welcome to the Sufrix POS Backend API documentation. This document provides a comprehensive guide to all endpoints, models, and authentication patterns used in the system.

## 🚀 General Information
- **Base URL**: `http://0.0.0.0:8080` (Development)
- **Content-Type**: `application/json`
- **Authentication**: JWT Bearer Token required for most endpoints.

---

## 🔐 Authentication

### POST `/auth/login`
Authenticates a user and returns a JWT token. Supports both Email/Password and PIN login.

**Request Body (Email Login):**
```json
{
  "email": "admin@sufrix.com",
  "password": "password123",
  "org_id": "optional-uuid"
}
```

**Request Body (PIN Login):**
```json
{
  "name": "Teller Name",
  "pin": "1234",
  "branch_id": "uuid"
}
```

**Response:**
```json
{
  "token": "jwt-token-string",
  "user": {
    "id": "uuid",
    "name": "User Name",
    "role": "org_admin | teller | ...",
    "branch_id": "uuid"
  }
}
```

### GET `/auth/me`
Returns the current authenticated user's profile.
- **Header**: `Authorization: Bearer <token>`

---

## 📂 Menu Management

### Categories (`/categories`)
- `GET /categories?org_id=<uuid>`: List all active categories.
- `POST /categories`: Create a new category.
- `PATCH /categories/{id}`: Update category metadata.
- `DELETE /categories/{id}`: Soft delete category.

### Menu Items (`/menu-items`)
- `GET /menu-items?org_id=<uuid>&full=true`: List menu items. Use `full=true` to embed sizes, addon slots, optional fields, and recipes.
- `POST /menu-items`: Create a menu item.
- `GET /menu-items/{id}`: Get full item details.

### Addon Items (`/addon-items`)
- `GET /addon-items?org_id=<uuid>`: List global addons (milk types, coffee beans, etc.).
- `POST /addon-items`: Create a global addon item.

---

## 🛒 Orders

### POST `/orders`
Submits a new order. Handles complex calculations for subtotal, taxes, and automatic inventory deduction.

**Request Body:**
```json
{
  "branch_id": "uuid",
  "shift_id": "uuid",
  "payment_method": "cash | card | mixed | ...",
  "items": [
    {
      "menu_item_id": "uuid",
      "size_label": "Large",
      "quantity": 2,
      "addons": [{"addon_item_id": "uuid", "quantity": 1}],
      "optional_field_ids": ["uuid"]
    }
  ],
  "discount_id": "optional-uuid",
  "amount_tendered": 500
}
```

### POST `/orders/preview-recipe`
Calculates exactly what ingredients will be deducted for a specific configuration without creating an order. Useful for "View Recipe" logic in the POS.

---

## ⏱️ Shifts

### POST `/shifts/branches/{branch_id}/current`
Returns the currently open shift for the branch.

### POST `/shifts/branches/{branch_id}/open`
Starts a new shift for a branch. Requires an opening cash balance.

### POST `/shifts/{shift_id}/close`
Ends a shift and records the actual cash at hand. Generates a discrepancy report if needed.

---

## 📦 Inventory

### Catalog (`/inventory/orgs/{org_id}/catalog`)
Manage the global list of raw ingredients (Milk, Beans, Syrups) that the organization tracks.

### Branch Stock (`/inventory/branches/{branch_id}/stock`)
Manage the actual quantities available at a specific location.
- `POST /inventory/branches/{branch_id}/stock`: Add stock to the branch.

---

## 📊 Reports

### GET `/reports/branches/{branch_id}/sales`
Returns a summary of sales, tax, and discounts for a date range.

### GET `/reports/shifts/{shift_id}/summary`
Full financial and operational breakdown of a completed shift.

---

## 🛠️ Permissions

### GET `/permissions/matrix/{user_id}`
Returns a full matrix of effective permissions for a user (Role defaults + User overrides).

---

## 🖼️ Uploads

### POST `/uploads/menu-items/{menu_item_id}`
Uploads an image for a menu item. Expects `multipart/form-data`.

---

## ⚠️ Standard Error Response
All errors follow this JSON format:
```json
{
  "error": "Short description of what went wrong"
}
```
**Common Status Codes:**
- `400`: Bad Request (Validation failed)
- `401`: Unauthorized (Invalid or missing token)
- `403`: Forbidden (No permission for this resource/action)
- `404`: Not Found
