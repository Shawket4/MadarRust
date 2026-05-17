# Agent Prompt: Audit & Fix Order Item Optionals/Modifiers Handling

## 📋 Context & Objective
In the POS companion app (`rue_pos`), we recently audited and fixed the handling logic for order item optionals/modifiers (e.g., custom choices like "No Onions", "Extra Cheese", etc.). We discovered they were missing from the physical receipts, local offline fallbacks, and the order history detail views.

**Objective:** Audit the `RueDashboard` codebase and implement similar fixes so that optionals are correctly parsed, rendered in all order views (order list, detail cards, invoice templates), and included in any thermal printing/PDF generation logic.

---

## 🔍 Codebase Mappings to Check
Please search for and audit the following areas in the dashboard codebase:

1. **Order Detail Views / Tables:**
   * Look for where order items are rendered (e.g., `OrderDetails`, `InvoiceCard`, `ItemRow`, `OrderSummary`).
   * Verify if `item.addons` are rendered but `item.optionals` (or `item.modifiers` / `item.options`) are omitted.
   * **Fix:** Render optionals using a clean, distinctive style (e.g., warning/yellow accent badges, similar to the cart rows in the POS app) showing the option name and price (if any, e.g., `+ EGP 15`).

2. **Receipt / Invoice Thermal Printing:**
   * Search for receipt generator templates, PDF builders, or Esc/Pos print layouts (e.g., `printer_service`, `invoice_pdf`, `receipt_builder`).
   * **Fix:** Ensure optionals are listed underneath the main item alongside addons (e.g., `  + No Onions` or `  + Extra Cheese  +EGP 15`).

3. **Data Models / Serialization:**
   * Search for `OrderItem` or `Order` model definitions.
   * Ensure `optionals` list is fully supported in `fromJson`, `toJson`, and any state copy methods.
   * **Data Schema reference:**
     * `OrderItem` contains `List<OrderItemOptional> optionals` (or `List<SelectedOptional>`).
     * `OrderItemOptional` contains `fieldName` (or `name`) and `price`.

---

## 🛠️ Step-by-Step Execution Plan

### Step 1: Search & Audit
Perform a global codebase search to find how order items and their options/modifiers are handled:
```bash
# Search for addon references to see how they are rendered
grep -rn "addons" src/ lib/ web/

# Search for optionals or modifiers
grep -rn "optionals" src/ lib/ web/
```

### Step 2: Implement UI Fixes
Update the order details card, summary modals, and history pages to render optionals chips/badges under each item. 

*If using Flutter/Dart:*
```dart
if (item.optionals.isNotEmpty) ...[
  const SizedBox(height: 6),
  Wrap(
    spacing: 4,
    runSpacing: 4,
    children: item.optionals.map((o) {
      final hasPrice = o.price > 0;
      return Container(
        padding: const EdgeInsets.symmetric(horizontal: 7, vertical: 3),
        decoration: BoxDecoration(
          color: AppColors.warning.withOpacity(0.08),
          borderRadius: BorderRadius.circular(AppRadius.xs),
        ),
        child: Text(
          hasPrice ? '${o.fieldName}  +${egp(o.price)}' : o.fieldName,
          style: cairo(fontSize: 10, fontWeight: FontWeight.w600, color: AppColors.warning),
        ),
      );
    }).toList(),
  ),
]
```

### Step 3: Implement Print & PDF Fixes
Update any thermal printing layout or PDF invoice builder to include optionals under the printed items.

*Example print addition:*
```dart
...item.optionals.map((o) {
  final oPrice = o.price > 0 ? '+${egp(o.price)}' : '';
  return oPrice.isNotEmpty
      ? _row('  + ${o.fieldName}', oPrice, font: font, fontB: fontB, sz: 7.5, leftIndent: 4)
      : pw.Padding(
          padding: const pw.EdgeInsets.only(left: 4, bottom: 1.5),
          child: pw.Text('  + ${o.fieldName}', style: ts(font, sz: 7.5)),
        );
})
```

---

## 📈 Verification Checklist
* [ ] Order history and detail page shows all selected optionals/modifiers under each item.
* [ ] Optionals are clearly distinguished from addons (e.g., unique color coding).
* [ ] Thermal receipts/PDF invoices print both addons and optionals correctly with accurate pricing details.
* [ ] Codebase compiles cleanly without lint errors.
