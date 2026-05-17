import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:uuid/uuid.dart';
import '../../../core/api/client.dart';
import '../../../core/api/order_api.dart';
import '../../../core/models/cart.dart';
import '../../../core/models/discount.dart';
import '../../../core/models/order.dart';
import '../../../core/models/pending_action.dart';
import '../../../core/providers/auth_notifier.dart';
import '../../../core/providers/cart_notifier.dart';
import '../../../core/providers/discount_notifier.dart';
import '../../../core/providers/order_history_notifier.dart';
import '../../../core/providers/shift_notifier.dart';
import '../../../core/services/connectivity_service.dart';
import '../../../core/services/offline_queue.dart';
import '../../../core/theme/app_theme.dart';
import '../../../core/utils/formatting.dart';
import '../../../shared/widgets/app_button.dart';
import '../../../shared/widgets/label_value.dart';
import '../../../shared/widgets/responsive_sheet.dart';
import '../../../shared/widgets/sync_status_banner.dart';
import '../helpers/payment_helpers.dart';
import 'receipt_sheet.dart';
import 'shared_widgets.dart';

class CheckoutSheet extends ConsumerStatefulWidget {
  const CheckoutSheet({super.key});

  // Task 3.2: Use ResponsiveSheet
  static Future<void> show(BuildContext ctx) => ResponsiveSheet.show(
      context: ctx,
      builder: (_) => const CheckoutSheet());

  @override
  ConsumerState<CheckoutSheet> createState() => _CheckoutSheetState();
}

class _CheckoutSheetState extends ConsumerState<CheckoutSheet> {
  bool _loading = false;
  String? _error;
  final _customerCtrl = TextEditingController();

  Discount? _selectedDiscount;

  final _tenderedCtrl = TextEditingController();
  bool _showTendered = false;

  final _tipCtrl = TextEditingController();
  String _tipPaymentMethod = 'cash';

  bool _isSplit = false;
  final Map<String, TextEditingController> _splitCtrs = {};
  final Set<String> _activeSplitMethods = {};

  @override
  void initState() {
    super.initState();
    final cart = ref.read(cartProvider);
    _showTendered = cart.payment == 'cash' || cart.payment == 'talabat_cash';
  }

  @override
  void dispose() {
    _customerCtrl.dispose();
    _tenderedCtrl.dispose();
    _tipCtrl.dispose();
    for (final c in _splitCtrs.values) {
      c.dispose();
    }
    super.dispose();
  }

  void _toggleSplitMethod(String method) {
    setState(() {
      if (_activeSplitMethods.contains(method)) {
        _activeSplitMethods.remove(method);
        _splitCtrs[method]?.clear();
      } else {
        _activeSplitMethods.add(method);
        _splitCtrs.putIfAbsent(method, () => TextEditingController());
      }
    });
  }

  List<PaymentSplit> _buildSplits() {
    final splits = <PaymentSplit>[];
    for (final method in _activeSplitMethods) {
      final raw = double.tryParse(_splitCtrs[method]?.text ?? '');
      if (raw != null && raw > 0) {
        splits.add(PaymentSplit(method: method, amount: (raw * 100).round()));
      }
    }
    return splits;
  }

  int? get _parsedTip {
    final v = double.tryParse(_tipCtrl.text);
    if (v == null || v <= 0) return null;
    return (v * 100).round();
  }

  bool get _tipIsCash => isCashMethod(_tipPaymentMethod);

  Future<void> _place() async {
    final cart = ref.read(cartProvider);
    final shift = ref.read(shiftProvider).shift;
    final queue = ref.read(offlineQueueProvider.notifier);
    final isOnline = ref.read(isOnlineProvider);
    final customer =
        _customerCtrl.text.trim().isEmpty ? null : _customerCtrl.text.trim();

    if (_loading) return;
    if (cart.isEmpty) {
      setState(() => _error = 'Cart is empty');
      return;
    }
    if (shift == null) {
      setState(() => _error = 'No open shift');
      return;
    }
    if (!_isSplit && cart.payment.isEmpty) {
      setState(() => _error = 'Select a payment method');
      return;
    }

    final int? tip = _parsedTip;
    final String? tipMethod = tip != null ? _tipPaymentMethod : null;

    final int? tendered = _showTendered && !_isSplit
        ? (double.tryParse(_tenderedCtrl.text) != null
            ? (double.parse(_tenderedCtrl.text) * 100).round()
            : null)
        : null;

    if (_showTendered && !_isSplit) {
      if (tendered == null || tendered == 0) {
        setState(() => _error = 'Enter the cash amount tendered');
        return;
      }
      if (tendered < cart.total) {
        setState(() => _error =
            'Tendered ${egp(tendered)} is less than total ${egp(cart.total)}');
        return;
      }
      if (tip != null && _tipIsCash) {
        final change = tendered - cart.total;
        if (tip > change) {
          setState(() =>
              _error = 'Cash tip ${egp(tip)} exceeds change ${egp(change)}');
          return;
        }
      }
    }

    List<PaymentSplit>? splits;
    if (_isSplit) {
      if (_activeSplitMethods.isEmpty) {
        setState(() => _error = 'Select at least one payment method');
        return;
      }
      splits = _buildSplits();
      if (splits.isEmpty) {
        setState(() => _error = 'Enter amounts for selected payment methods');
        return;
      }
      final splitTotal = splits.fold(0, (s, p) => s + p.amount);
      final expectedSplitTotal = cart.total - (_tipIsCash ? (tip ?? 0) : 0);
      if (splitTotal != expectedSplitTotal) {
        setState(() => _error =
            'Split total ${egp(splitTotal)} must equal ${egp(expectedSplitTotal)}');
        return;
      }
    }

    setState(() {
      _loading = true;
      _error = null;
    });

    final discountType =
        _selectedDiscount?.dtype ?? cart.discountType?.apiValue;
    final discountValue = _selectedDiscount?.value ?? cart.discountValue;
    final discountId = _selectedDiscount?.id;
    final paymentMethod = _isSplit
        ? (splits!.length == 1 ? splits.first.method : 'mixed')
        : cart.payment;

    if (!isOnline) {
      final localId = const Uuid().v4();
      await queue.enqueueOrder(PendingOrder(
        localId: localId,
        branchId: shift.branchId,
        shiftId: shift.id,
        paymentMethod: paymentMethod,
        customerName: customer,
        discountType: discountType,
        discountValue: discountValue,
        discountId: discountId,
        amountTendered: tendered,
        tipAmount: tip,
        tipPaymentMethod: tipMethod,
        paymentSplits: splits,
        items: cart.items,
        orderedAt: DateTime.now(),
        createdAt: DateTime.now(),
      ));

      // Task 1.5: Optimistic offline order
      final optimistic = Order(
        id: localId,
        branchId: shift.branchId,
        shiftId: shift.id,
        tellerId: ref.read(authProvider).user!.id,
        tellerName: ref.read(authProvider).user!.name,
        orderNumber: -1,
        status: 'pending_sync',
        paymentMethod: paymentMethod,
        subtotal: cart.subtotal,
        discountType: discountType,
        discountValue: discountValue ?? 0,
        discountAmount: cart.discountAmount,
        taxAmount: 0,
        totalAmount: cart.total,
        customerName: customer,
        notes: cart.notes,
        amountTendered: tendered,
        tipAmount: tip,
        tipPaymentMethod: tipMethod,
        discountId: discountId,
        createdAt: DateTime.now(),
        items: cart.items.map((ci) => OrderItem(
          id: const Uuid().v4(),
          itemName: ci.itemName,
          sizeLabel: ci.sizeLabel,
          unitPrice: ci.unitPrice,
          quantity: ci.quantity,
          lineTotal: ci.lineTotal,
          addons: ci.addons.map((a) => OrderItemAddon(
            id: const Uuid().v4(),
            orderItemId: '',
            addonItemId: a.addonItemId,
            addonName: a.name,
            unitPrice: a.priceModifier,
            quantity: a.quantity,
            lineTotal: a.priceModifier * a.quantity,
          )).toList(),
          optionals: ci.optionals.map((o) => OrderItemOptional(
            id: const Uuid().v4(),
            orderItemId: '',
            optionalFieldId: o.optionalFieldId,
            fieldName: o.name,
            price: o.price,
          )).toList(),
        )).toList(),
      );
      ref.read(orderHistoryProvider.notifier).addOrder(optimistic);

      final total = cart.total;
      ref.read(cartProvider.notifier).clear();
      
      if (mounted) {
        Navigator.pop(context);
        ReceiptSheet.show(context,
            order: optimistic,
            total: total,
            changeGiven:
                tendered != null ? (tendered - total).clamp(0, 999999) : null);
      }
      return;
    }

    try {
      final order = await ref.read(orderApiProvider).create(
            branchId: shift.branchId,
            shiftId: shift.id,
            paymentMethod: paymentMethod,
            items: cart.items,
            customerName: customer,
            discountType: discountType,
            discountValue: discountValue,
            discountId: discountId,
            amountTendered: tendered,
            tipAmount: tip,
            tipPaymentMethod: tipMethod,
            paymentSplits: splits,
            idempotencyKey: const Uuid().v4(),
          );
      ref.read(orderHistoryProvider.notifier).addOrder(order);
      final total = cart.total;
      ref.read(cartProvider.notifier).clear();
      if (mounted) {
        Navigator.pop(context);
        ReceiptSheet.show(context,
            order: order,
            total: total,
            changeGiven:
                tendered != null ? (tendered - total).clamp(0, 999999) : null);
      }
    } catch (e) {
      if (isNetworkError(e)) {
        final localId = const Uuid().v4();
        await queue.enqueueOrder(PendingOrder(
          localId: localId,
          branchId: shift.branchId,
          shiftId: shift.id,
          paymentMethod: paymentMethod,
          customerName: customer,
          discountType: discountType,
          discountValue: discountValue,
          discountId: discountId,
          amountTendered: tendered,
          tipAmount: tip,
          tipPaymentMethod: tipMethod,
          paymentSplits: splits,
          items: cart.items,
          createdAt: DateTime.now(),
          orderedAt: DateTime.now(),
        ));

        // Task 1.5: Optimistic offline order
        final optimistic = Order(
          id: localId,
          branchId: shift.branchId,
          shiftId: shift.id,
          tellerId: ref.read(authProvider).user!.id,
          tellerName: ref.read(authProvider).user!.name,
          orderNumber: -1,
          status: 'pending_sync',
          paymentMethod: paymentMethod,
          subtotal: cart.subtotal,
          discountType: discountType,
          discountValue: discountValue ?? 0,
          discountAmount: cart.discountAmount,
          taxAmount: 0,
          totalAmount: cart.total,
          customerName: customer,
          notes: cart.notes,
          amountTendered: tendered,
          tipAmount: tip,
          tipPaymentMethod: tipMethod,
          discountId: discountId,
          createdAt: DateTime.now(),
          items: cart.items.map((ci) => OrderItem(
            id: const Uuid().v4(),
            itemName: ci.itemName,
            sizeLabel: ci.sizeLabel,
            unitPrice: ci.unitPrice,
            quantity: ci.quantity,
            lineTotal: ci.lineTotal,
            addons: ci.addons.map((a) => OrderItemAddon(
              id: const Uuid().v4(),
              orderItemId: '',
              addonItemId: a.addonItemId,
              addonName: a.name,
              unitPrice: a.priceModifier,
              quantity: a.quantity,
              lineTotal: a.priceModifier * a.quantity,
            )).toList(),
            optionals: ci.optionals.map((o) => OrderItemOptional(
              id: const Uuid().v4(),
              orderItemId: '',
              optionalFieldId: o.optionalFieldId,
              fieldName: o.name,
              price: o.price,
            )).toList(),
          )).toList(),
        );
        ref.read(orderHistoryProvider.notifier).addOrder(optimistic);

        final total = cart.total;
        ref.read(cartProvider.notifier).clear();
        if (mounted) {
          Navigator.pop(context);
          ReceiptSheet.show(context,
            order: optimistic,
            total: total,
            changeGiven:
                tendered != null ? (tendered - total).clamp(0, 999999) : null);
        }
      } else {
        setState(() {
          _error = 'Failed to place order — please retry';
          _loading = false;
        });
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    final cart = ref.watch(cartProvider);
    final discountState = ref.watch(discountProvider);
    final isOnline = ref.watch(isOnlineProvider);
    final mq = MediaQuery.of(context);
    final maxH = mq.size.height - mq.padding.top - 16;

    return Container(
      constraints: BoxConstraints(maxHeight: maxH),
      decoration: BoxDecoration(
          color: Colors.white, borderRadius: AppRadius.sheetRadius),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Padding(
            padding: const EdgeInsets.only(top: 12, bottom: 4),
            child: Center(
                child: Container(
                    width: 36,
                    height: 4,
                    decoration: BoxDecoration(
                        color: AppColors.border,
                        borderRadius: BorderRadius.circular(2)))),
          ),

          Padding(
            padding: const EdgeInsets.fromLTRB(24, 8, 24, 12),
            child: Row(children: [
              Text('Checkout',
                  style: cairo(fontSize: 20, fontWeight: FontWeight.w800)),
              const Spacer(),
              AnimatedSwitcher(
                duration: const Duration(milliseconds: 200),
                transitionBuilder: (child, anim) => SlideTransition(
                    position: Tween<Offset>(
                            begin: const Offset(0.2, 0), end: Offset.zero)
                        .animate(anim),
                    child: FadeTransition(opacity: anim, child: child)),
                child: Container(
                  key: ValueKey(cart.total),
                  padding:
                      const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
                  decoration: BoxDecoration(
                      color: AppColors.primary.withOpacity(0.08),
                      borderRadius: BorderRadius.circular(20)),
                  child: Text(egp(cart.total),
                      style: cairo(
                          fontSize: 14,
                          fontWeight: FontWeight.w800,
                          color: AppColors.primary)),
                ),
              ),
            ]),
          ),
          Container(height: 1, color: AppColors.border),

          Flexible(
            child: SingleChildScrollView(
              padding:
                  EdgeInsets.fromLTRB(24, 20, 24, mq.viewInsets.bottom + 8),
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  // Task 2.2: Show offline status
                  if (!isOnline)
                    const SyncStatusBanner(
                      variant: SyncBannerVariant.offline,
                      text: 'Offline — order will sync when reconnected.'
                    ),

                  _SummaryCard(cart: cart),
                  const SizedBox(height: 20),

                  const FieldLabel('CUSTOMER NAME (OPTIONAL)'),
                  const SizedBox(height: 8),
                  TextField(
                    controller: _customerCtrl,
                    textCapitalization: TextCapitalization.words,
                    style: cairo(fontSize: 15),
                    decoration: InputDecoration(
                      hintText: 'e.g. Ahmed',
                      hintStyle:
                          cairo(fontSize: 15, color: AppColors.textMuted),
                      prefixIcon: const Icon(Icons.person_outline_rounded,
                          size: 18, color: AppColors.textMuted),
                    ),
                  ),
                  const SizedBox(height: 20),

                  if (!discountState.isLoading && discountState.items.isNotEmpty) ...[
                    const FieldLabel('DISCOUNT (OPTIONAL)'),
                    const SizedBox(height: 8),
                    _DiscountPicker(
                      discounts: discountState.items,
                      selected: _selectedDiscount,
                      onSelect: (d) {
                        setState(() => _selectedDiscount = d);
                        if (d == null) {
                          ref
                              .read(cartProvider.notifier)
                              .setDiscount(null, null);
                        } else {
                          ref.read(cartProvider.notifier).setDiscount(
                              DiscountType.values.byName(d.dtype), d.value);
                        }
                      },
                    ),
                    const SizedBox(height: 20),
                  ],

                  Row(children: [
                    const FieldLabel('PAYMENT'),
                    const Spacer(),
                    _SplitToggle(
                      active: _isSplit,
                      onToggle: () => setState(() {
                        _isSplit = !_isSplit;
                        if (!_isSplit) {
                          for (final c in _splitCtrs.values) c.clear();
                          _activeSplitMethods.clear();
                          final pay = ref.read(cartProvider).payment;
                          _showTendered =
                              pay == 'cash' || pay == 'talabat_cash';
                        } else {
                          _showTendered = false;
                        }
                      }),
                    ),
                  ]),
                  const SizedBox(height: 10),

                  if (_isSplit)
                    _SplitPaymentSection(
                      activeMethods: _activeSplitMethods,
                      splitCtrs: _splitCtrs,
                      cartTotal: cart.total,
                      onToggleMethod: _toggleSplitMethod,
                      onAmountChanged: () => setState(() {}),
                      parsedTip: _parsedTip,
                      tipPaymentMethod: _tipPaymentMethod,
                    )
                  else ...[
                    _SinglePaymentGrid(
                      selected: cart.payment,
                      onSelect: (v) {
                        ref.read(cartProvider.notifier).setPayment(v);
                        setState(() =>
                            _showTendered = v == 'cash' || v == 'talabat_cash');
                      },
                    ),

                    if (_showTendered) ...[
                      const SizedBox(height: 20),
                      _CashTenderedSection(
                        tenderedCtrl: _tenderedCtrl,
                        cartTotal: cart.total,
                        onChanged: () => setState(() {}),
                        cashTip: _tipIsCash ? _parsedTip : null,
                      ),
                    ],
                  ],

                  const SizedBox(height: 20),
                  _TipSection(
                    tipCtrl: _tipCtrl,
                    tipPaymentMethod: _tipPaymentMethod,
                    parsedTip: _parsedTip,
                    onMethodChanged: (m) =>
                        setState(() => _tipPaymentMethod = m),
                    onAmountChanged: () => setState(() {}),
                  ),

                  AnimatedSize(
                    duration: const Duration(milliseconds: 200),
                    child: _error != null
                        ? Padding(
                            padding: const EdgeInsets.only(top: 16),
                            child: Container(
                              padding: const EdgeInsets.symmetric(
                                  horizontal: 14, vertical: 11),
                              decoration: BoxDecoration(
                                  color: AppColors.danger.withOpacity(0.07),
                                  borderRadius:
                                      BorderRadius.circular(AppRadius.xs)),
                              child: Row(children: [
                                const Icon(Icons.error_outline_rounded,
                                    size: 14, color: AppColors.danger),
                                const SizedBox(width: 8),
                                Expanded(
                                    child: Text(_error!,
                                        style: cairo(
                                            fontSize: 13,
                                            color: AppColors.danger))),
                              ]),
                            ))
                        : const SizedBox.shrink(),
                  ),

                  const SizedBox(height: 20),
                ],
              ),
            ),
          ),

          Container(
            padding: EdgeInsets.fromLTRB(24, 12, 24, mq.padding.bottom + 16),
            decoration: const BoxDecoration(
                color: Colors.white,
                border: Border(top: BorderSide(color: AppColors.border))),
            child: AppButton(
              label: 'Place Order',
              loading: _loading,
              width: double.infinity,
              height: 52,
              icon: Icons.check_rounded,
              onTap: _place,
            ),
          ),
        ],
      ),
    );
  }
}

class _SummaryCard extends StatelessWidget {
  final CartState cart;
  const _SummaryCard({required this.cart});
  @override
  Widget build(BuildContext context) => Container(
        padding: const EdgeInsets.all(14),
        decoration: BoxDecoration(
            color: AppColors.bg,
            borderRadius: BorderRadius.circular(AppRadius.sm),
            border: Border.all(color: AppColors.border)),
        child: Column(children: [
          LabelValue('Subtotal', egp(cart.subtotal)),
          if (cart.discountAmount > 0)
            LabelValue('Discount', '− ${egp(cart.discountAmount)}',
                valueColor: AppColors.success),
          const Divider(height: 16, color: AppColors.border),
          LabelValue('Total', egp(cart.total), bold: true),
        ]),
      );
}

class _SplitToggle extends StatelessWidget {
  final bool active;
  final VoidCallback onToggle;
  const _SplitToggle({required this.active, required this.onToggle});
  @override
  Widget build(BuildContext context) => GestureDetector(
        onTap: onToggle,
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 150),
          padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 4),
          decoration: BoxDecoration(
            color: active ? AppColors.primary.withOpacity(0.1) : AppColors.bg,
            borderRadius: BorderRadius.circular(AppRadius.xs),
            border: Border.all(
                color: active ? AppColors.primary : AppColors.border),
          ),
          child: Text('Split',
              style: cairo(
                  fontSize: 11,
                  fontWeight: FontWeight.w700,
                  color:
                      active ? AppColors.primary : AppColors.textSecondary)),
        ),
      );
}

class _DiscountPicker extends StatelessWidget {
  final List<Discount> discounts;
  final Discount? selected;
  final void Function(Discount?) onSelect;
  const _DiscountPicker(
      {required this.discounts,
      required this.selected,
      required this.onSelect});

  @override
  Widget build(BuildContext context) => Wrap(
        spacing: 8,
        runSpacing: 8,
        children: [
          _chip('None', null, selected == null, AppColors.primary),
          ...discounts.map((d) =>
              _chip(d.label, d, selected?.id == d.id, AppColors.success)),
        ],
      );

  Widget _chip(String label, Discount? d, bool sel, Color color) =>
      GestureDetector(
        onTap: () => onSelect(sel ? null : d),
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 150),
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
          decoration: BoxDecoration(
            color: sel ? color : AppColors.bg,
            borderRadius: BorderRadius.circular(AppRadius.xs),
            border: Border.all(
                color: sel ? color : AppColors.border, width: sel ? 1.5 : 1),
          ),
          child: Text(label,
              style: cairo(
                  fontSize: 13,
                  fontWeight: FontWeight.w600,
                  color: sel ? Colors.white : AppColors.textPrimary)),
        ),
      );
}

class _SinglePaymentGrid extends StatelessWidget {
  final String selected;
  final void Function(String) onSelect;
  const _SinglePaymentGrid({required this.selected, required this.onSelect});

  @override
  Widget build(BuildContext context) =>
      LayoutBuilder(builder: (ctx, constraints) {
        final btnW = (constraints.maxWidth - 8) / 2;
        // Task 4.1: Use new enum logic
        return Wrap(
          spacing: 8,
          runSpacing: 8,
          children: PaymentMethod.values.where((m) => m != PaymentMethod.mixed).map((m) {
            final sel = selected == m.wireFormat;
            return GestureDetector(
              onTap: () => onSelect(m.wireFormat),
              child: AnimatedContainer(
                duration: const Duration(milliseconds: 150),
                width: btnW,
                padding:
                    const EdgeInsets.symmetric(horizontal: 12, vertical: 11),
                decoration: BoxDecoration(
                    color: sel ? m.color : AppColors.bg,
                    borderRadius: BorderRadius.circular(AppRadius.sm),
                    border: Border.all(
                        color: sel ? m.color : AppColors.border,
                        width: sel ? 1.5 : 1)),
                child: Row(children: [
                  Icon(m.icon, size: 20, color: sel ? Colors.white : m.color),
                  const SizedBox(width: 10),
                  Expanded(
                      child: Text(m.label,
                          style: cairo(
                              fontSize: 13,
                              fontWeight: FontWeight.w600,
                              color:
                                  sel ? Colors.white : AppColors.textPrimary),
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis)),
                  if (sel) ...[
                    const SizedBox(width: 4),
                    const Icon(Icons.check_circle_rounded,
                        size: 15, color: Colors.white)
                  ],
                ]),
              ),
            );
          }).toList(),
        );
      });
}

class _CashTenderedSection extends StatelessWidget {
  final TextEditingController tenderedCtrl;
  final int cartTotal;
  final VoidCallback onChanged;
  final int? cashTip;
  const _CashTenderedSection({
    required this.tenderedCtrl,
    required this.cartTotal,
    required this.onChanged,
    this.cashTip,
  });

  @override
  Widget build(BuildContext context) => Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const FieldLabel('CASH TENDERED'),
          const SizedBox(height: 8),
          TextField(
            controller: tenderedCtrl,
            keyboardType: const TextInputType.numberWithOptions(decimal: true),
            onChanged: (_) => onChanged(),
            style: cairo(fontSize: 22, fontWeight: FontWeight.w700),
            decoration: InputDecoration(
              prefixText: 'EGP  ',
              prefixStyle: cairo(
                  fontSize: 16,
                  color: AppColors.textSecondary,
                  fontWeight: FontWeight.w500),
              hintText: '0',
              hintStyle: cairo(
                  fontSize: 22,
                  fontWeight: FontWeight.w700,
                  color: AppColors.border),
              border: InputBorder.none,
              enabledBorder: InputBorder.none,
              focusedBorder: InputBorder.none,
              isDense: true,
            ),
          ),
          Builder(builder: (_) {
            final tendered = double.tryParse(tenderedCtrl.text);
            if (tendered == null || tendered == 0) {
              return const SizedBox.shrink();
            }
            final tenderedP = (tendered * 100).round();
            final change = tenderedP - cartTotal - (cashTip ?? 0);
            return AnimatedContainer(
              duration: const Duration(milliseconds: 200),
              margin: const EdgeInsets.only(top: 8),
              padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
              decoration: BoxDecoration(
                color: change >= 0
                    ? AppColors.success.withOpacity(0.07)
                    : AppColors.danger.withOpacity(0.07),
                borderRadius: BorderRadius.circular(AppRadius.xs),
                border: Border.all(
                    color: change >= 0
                        ? AppColors.success.withOpacity(0.25)
                        : AppColors.danger.withOpacity(0.25)),
              ),
              child: Row(
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  children: [
                    Text(change >= 0 ? 'Change due:' : 'Insufficient:',
                        style: cairo(
                            fontSize: 13,
                            fontWeight: FontWeight.w600,
                            color: change >= 0
                                ? AppColors.success
                                : AppColors.danger)),
                    Text(egp(change.abs()),
                        style: cairo(
                            fontSize: 16,
                            fontWeight: FontWeight.w800,
                            color: change >= 0
                                ? AppColors.success
                                : AppColors.danger)),
                  ]),
            );
          }),
        ],
      );
}

class _TipSection extends StatelessWidget {
  final TextEditingController tipCtrl;
  final String tipPaymentMethod;
  final int? parsedTip;
  final void Function(String) onMethodChanged;
  final VoidCallback onAmountChanged;

  const _TipSection({
    required this.tipCtrl,
    required this.tipPaymentMethod,
    required this.parsedTip,
    required this.onMethodChanged,
    required this.onAmountChanged,
  });

  @override
  Widget build(BuildContext context) => AnimatedContainer(
        duration: const Duration(milliseconds: 200),
        padding: const EdgeInsets.all(16),
        decoration: BoxDecoration(
          color: AppColors.bg,
          borderRadius: BorderRadius.circular(AppRadius.md),
          border: Border.all(
              color: parsedTip != null
                  ? AppColors.primary.withOpacity(0.3)
                  : AppColors.border),
        ),
        child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Row(children: [
            const Icon(Icons.volunteer_activism_rounded,
                size: 14, color: AppColors.textMuted),
            const SizedBox(width: 6),
            const FieldLabel('TIP (OPTIONAL)'),
            const Spacer(),
            if (parsedTip != null)
              AnimatedSwitcher(
                duration: const Duration(milliseconds: 150),
                child: Container(
                  key: ValueKey(parsedTip),
                  padding:
                      const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
                  decoration: BoxDecoration(
                      color: AppColors.success.withOpacity(0.1),
                      borderRadius: BorderRadius.circular(10)),
                  child: Text(egp(parsedTip!),
                      style: cairo(
                          fontSize: 11,
                          fontWeight: FontWeight.w700,
                          color: AppColors.success)),
                ),
              ),
          ]),
          const SizedBox(height: 12),
          // Task 4.1: Enum usage
          Wrap(
            spacing: 6,
            runSpacing: 6,
            children: PaymentMethod.values.where((m) => m != PaymentMethod.mixed).map((method) {
              final sel = tipPaymentMethod == method.wireFormat;
              final color = method.color;
              return GestureDetector(
                onTap: () => onMethodChanged(method.wireFormat),
                child: AnimatedContainer(
                  duration: const Duration(milliseconds: 140),
                  padding:
                      const EdgeInsets.symmetric(horizontal: 11, vertical: 7),
                  decoration: BoxDecoration(
                    color: sel ? color : Colors.white,
                    borderRadius: BorderRadius.circular(AppRadius.xs),
                    border: Border.all(
                        color: sel ? color : AppColors.border,
                        width: sel ? 1.5 : 1),
                  ),
                  child: Row(mainAxisSize: MainAxisSize.min, children: [
                    if (sel) ...[
                      const Icon(Icons.check_rounded,
                          size: 11, color: Colors.white),
                      const SizedBox(width: 4)
                    ],
                    Text(method.label,
                        style: cairo(
                            fontSize: 12,
                            fontWeight: FontWeight.w600,
                            color:
                                sel ? Colors.white : AppColors.textSecondary)),
                  ]),
                ),
              );
            }).toList(),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: tipCtrl,
            keyboardType: const TextInputType.numberWithOptions(decimal: true),
            onChanged: (_) => onAmountChanged(),
            style: cairo(fontSize: 22, fontWeight: FontWeight.w700),
            decoration: InputDecoration(
              prefixText: 'EGP  ',
              prefixStyle: cairo(
                  fontSize: 16,
                  color: AppColors.textSecondary,
                  fontWeight: FontWeight.w500),
              hintText: '0',
              hintStyle: cairo(
                  fontSize: 22,
                  fontWeight: FontWeight.w700,
                  color: AppColors.border),
              border: InputBorder.none,
              enabledBorder: InputBorder.none,
              focusedBorder: InputBorder.none,
              isDense: true,
            ),
          ),
        ]),
      );
}

class _SplitPaymentSection extends StatelessWidget {
  final Set<String> activeMethods;
  final Map<String, TextEditingController> splitCtrs;
  final int cartTotal;
  final void Function(String) onToggleMethod;
  final VoidCallback onAmountChanged;
  final int? parsedTip;
  final String tipPaymentMethod;

  const _SplitPaymentSection({
    required this.activeMethods,
    required this.splitCtrs,
    required this.cartTotal,
    required this.onToggleMethod,
    required this.onAmountChanged,
    required this.parsedTip,
    required this.tipPaymentMethod,
  });

  List<PaymentSplit> _buildSplits() {
    final splits = <PaymentSplit>[];
    for (final method in activeMethods) {
      final raw = double.tryParse(splitCtrs[method]?.text ?? '');
      if (raw != null && raw > 0) {
        splits.add(PaymentSplit(method: method, amount: (raw * 100).round()));
      }
    }
    return splits;
  }

  @override
  Widget build(BuildContext context) => Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const FieldLabel('SELECT METHODS USED'),
          const SizedBox(height: 10),
          // Task 4.1: Enum usage
          Wrap(
            spacing: 8,
            runSpacing: 8,
            children: PaymentMethod.values.where((m) => m != PaymentMethod.mixed).map((method) {
              final color = method.color;
              final active = activeMethods.contains(method.wireFormat);
              return GestureDetector(
                onTap: () => onToggleMethod(method.wireFormat),
                child: AnimatedContainer(
                  duration: const Duration(milliseconds: 150),
                  padding:
                      const EdgeInsets.symmetric(horizontal: 14, vertical: 9),
                  decoration: BoxDecoration(
                    color: active ? color : AppColors.bg,
                    borderRadius: BorderRadius.circular(AppRadius.sm),
                    border: Border.all(
                        color: active ? color : AppColors.border,
                        width: active ? 1.5 : 1),
                  ),
                  child: Row(mainAxisSize: MainAxisSize.min, children: [
                    Icon(
                        active
                            ? Icons.check_circle_rounded
                            : Icons.radio_button_unchecked_rounded,
                        size: 14,
                        color: active ? Colors.white : color),
                    const SizedBox(width: 6),
                    Text(method.label,
                        style: cairo(
                            fontSize: 13,
                            fontWeight: FontWeight.w600,
                            color:
                                active ? Colors.white : AppColors.textPrimary)),
                  ]),
                ),
              );
            }).toList(),
          ),
          if (activeMethods.isNotEmpty) ...[
            const SizedBox(height: 16),
            const FieldLabel('ENTER AMOUNTS'),
            const SizedBox(height: 10),
            ...activeMethods.map((method) {
              final pm = PaymentMethod.fromWire(method);
              final color = pm.color;
              final ctrl = splitCtrs[method];
              return Padding(
                padding: const EdgeInsets.only(bottom: 10),
                child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Row(children: [
                        Container(
                            width: 8,
                            height: 8,
                            decoration: BoxDecoration(
                                color: color, shape: BoxShape.circle)),
                        const SizedBox(width: 6),
                        Text(pm.label,
                            style: cairo(
                                fontSize: 12,
                                fontWeight: FontWeight.w700,
                                color: color)),
                      ]),
                      const SizedBox(height: 6),
                      TextField(
                        controller: ctrl,
                        keyboardType: const TextInputType.numberWithOptions(
                            decimal: true),
                        onChanged: (_) => onAmountChanged(),
                        style: cairo(fontSize: 20, fontWeight: FontWeight.w700),
                        decoration: InputDecoration(
                          hintText: '0',
                          prefixText: 'EGP  ',
                          prefixStyle: cairo(
                              fontSize: 15,
                              color: AppColors.textSecondary,
                              fontWeight: FontWeight.w500),
                          contentPadding: const EdgeInsets.symmetric(
                              horizontal: 14, vertical: 12),
                          border: OutlineInputBorder(
                              borderRadius:
                                  BorderRadius.circular(AppRadius.sm),
                              borderSide:
                                  BorderSide(color: color.withOpacity(0.3))),
                          enabledBorder: OutlineInputBorder(
                              borderRadius:
                                  BorderRadius.circular(AppRadius.sm),
                              borderSide:
                                  BorderSide(color: color.withOpacity(0.3))),
                          focusedBorder: OutlineInputBorder(
                              borderRadius:
                                  BorderRadius.circular(AppRadius.sm),
                              borderSide:
                                  BorderSide(color: color, width: 2)),
                          filled: true,
                          fillColor: color.withOpacity(0.03),
                        ),
                      ),
                    ]),
              );
            }),

            Builder(builder: (context) {
              final splits = _buildSplits();
              final entered = splits.fold(0, (s, p) => s + p.amount);
              final isCashTip = PaymentMethod.fromWire(tipPaymentMethod).isCash;
              final tipOffset =
                  (isCashTip && parsedTip != null) ? parsedTip! : 0;
              final diff = cartTotal - entered - tipOffset;
              final ok = diff == 0;
              return Container(
                width: double.infinity,
                padding:
                    const EdgeInsets.symmetric(horizontal: 14, vertical: 12),
                decoration: BoxDecoration(
                  color: ok
                      ? AppColors.success.withOpacity(0.07)
                      : AppColors.warning.withOpacity(0.07),
                  borderRadius: BorderRadius.circular(AppRadius.sm),
                  border: Border.all(
                      color: ok
                          ? AppColors.success.withOpacity(0.3)
                          : AppColors.warning.withOpacity(0.3)),
                ),
                child: Row(
                    mainAxisAlignment: MainAxisAlignment.spaceBetween,
                    children: [
                      Row(children: [
                        Icon(
                            ok
                                ? Icons.check_circle_outline_rounded
                                : Icons.pending_outlined,
                            size: 16,
                            color:
                                ok ? AppColors.success : AppColors.warning),
                        const SizedBox(width: 8),
                        Text(ok ? 'Balanced ✓' : 'Remaining',
                            style: cairo(
                                fontSize: 13,
                                fontWeight: FontWeight.w600,
                                color: ok
                                    ? AppColors.success
                                    : AppColors.warning)),
                      ]),
                      if (!ok)
                        Text(egp(diff.abs()),
                            style: cairo(
                                fontSize: 15,
                                fontWeight: FontWeight.w800,
                                color: AppColors.warning)),
                    ]),
              );
            }),
          ],
        ],
      );
}
