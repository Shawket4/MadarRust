import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';
import 'package:lottie/lottie.dart';
import '../../core/api/order_api.dart';
import '../../core/models/order.dart';
import '../../core/providers/auth_notifier.dart';
import '../../core/providers/order_history_notifier.dart';
import '../../core/providers/shift_notifier.dart';
import '../../core/services/printer_service.dart';
import '../../core/theme/app_theme.dart';
import '../../core/utils/formatting.dart';
import '../../shared/widgets/error_banner.dart';
import '../../shared/widgets/label_value.dart';
import 'void_order_sheet.dart';
import 'helpers/payment_helpers.dart';

class OrderHistoryScreen extends ConsumerStatefulWidget {
  const OrderHistoryScreen({super.key});

  @override
  ConsumerState<OrderHistoryScreen> createState() => _OrderHistoryScreenState();
}

class _OrderHistoryScreenState extends ConsumerState<OrderHistoryScreen> {
  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) => _load());
  }

  Future<void> _load() async {
    final shiftId = ref.read(shiftProvider).shift?.id;
    if (shiftId == null) return;
    await ref
        .read(orderHistoryProvider.notifier)
        .loadForShift(shiftId, force: true);
  }

  @override
  Widget build(BuildContext context) {
    final history = ref.watch(orderHistoryProvider);
    final shift = ref.watch(shiftProvider).shift;
    final isTablet = MediaQuery.of(context).size.shortestSide >= 600;

    return Scaffold(
      backgroundColor: AppColors.bg,
      appBar: AppBar(
        leading: IconButton(
            icon: const Icon(Icons.arrow_back_rounded),
            onPressed: () => context.go('/home')),
        title: const Text('Order History'),
        backgroundColor: Colors.white,
        elevation: 0,
        surfaceTintColor: Colors.transparent,
        bottom: PreferredSize(
            preferredSize: const Size.fromHeight(1),
            child: Container(height: 1, color: AppColors.border)),
        actions: [
          if (shift != null)
            IconButton(
              icon: const Icon(Icons.refresh_rounded, size: 20),
              tooltip: 'Refresh',
              onPressed: () =>
                  ref.read(orderHistoryProvider.notifier).refresh(shift.id),
            ),
        ],
      ),
      body: shift == null
          ? _Placeholder(
              icon: Icons.lock_outline_rounded,
              message: 'No open shift',
              isTablet: isTablet)
          : history.isLoading
              ? const Center(
                  child: CircularProgressIndicator(color: AppColors.primary))
              : history.error != null
                  ? Padding(
                      padding: const EdgeInsets.all(24),
                      child:
                          ErrorBanner(message: history.error!, onRetry: _load))
                  : history.orders.isEmpty
                      ? _Placeholder(
                          icon: Icons.receipt_long_outlined,
                          message: 'No orders yet for this shift',
                          isTablet: isTablet,
                          useLottie: true)
                      : _OrderList(orders: history.orders, isTablet: isTablet),
    );
  }
}

class _OrderList extends StatelessWidget {
  final List<Order> orders;
  final bool isTablet;
  const _OrderList({required this.orders, required this.isTablet});

  @override
  Widget build(BuildContext context) {
    final active = orders.where((o) => o.status != 'voided').toList();
    final total = active.fold(0, (s, o) => s + o.totalAmount);
    final cash = active
        .where((o) => o.paymentMethod == 'cash')
        .fold(0, (s, o) => s + o.totalAmount);
    final card = active
        .where((o) => o.paymentMethod == 'card')
        .fold(0, (s, o) => s + o.totalAmount);

    return Column(children: [
      Container(
        color: Colors.white,
        padding:
            EdgeInsets.fromLTRB(isTablet ? 24 : 16, 12, isTablet ? 24 : 16, 12),
        child: Row(children: [
          _StatChip(
              label: 'Orders',
              value: '${active.length}',
              color: AppColors.primary),
          const SizedBox(width: 8),
          _StatChip(
              label: 'Total', value: egp(total), color: AppColors.success),
          if (cash > 0) ...[
            const SizedBox(width: 8),
            _StatChip(
                label: 'Cash',
                value: egp(cash),
                color: AppColors.textSecondary),
          ],
          if (card > 0) ...[
            const SizedBox(width: 8),
            _StatChip(
                label: 'Card',
                value: egp(card),
                color: const Color(0xFF7C3AED)),
          ],
        ]),
      ),
      Container(height: 1, color: AppColors.border),

      Expanded(
        child: isTablet
            ? GridView.builder(
                padding: const EdgeInsets.all(20),
                gridDelegate: const SliverGridDelegateWithMaxCrossAxisExtent(
                  maxCrossAxisExtent: 520,
                  mainAxisSpacing: 10,
                  crossAxisSpacing: 10,
                  childAspectRatio: 3.6,
                ),
                itemCount: orders.length,
                itemBuilder: (_, i) => _OrderTile(order: orders[i]))
            : ListView.builder(
                padding: const EdgeInsets.all(14),
                itemCount: orders.length,
                itemBuilder: (_, i) => _OrderTile(order: orders[i])),
      ),
    ]);
  }
}

class _StatChip extends StatelessWidget {
  final String label, value;
  final Color color;
  const _StatChip(
      {required this.label, required this.value, required this.color});

  @override
  Widget build(BuildContext context) => Container(
        padding: const EdgeInsets.symmetric(horizontal: 11, vertical: 6),
        decoration: BoxDecoration(
            color: color.withOpacity(0.08),
            borderRadius: BorderRadius.circular(AppRadius.xs)),
        child: Row(mainAxisSize: MainAxisSize.min, children: [
          Text(label,
              style: cairo(
                  fontSize: 10,
                  fontWeight: FontWeight.w600,
                  color: color.withOpacity(0.8))),
          const SizedBox(width: 6),
          Text(value,
              style: cairo(
                  fontSize: 13, fontWeight: FontWeight.w800, color: color)),
        ]),
      );
}

class _OrderTile extends ConsumerStatefulWidget {
  final Order order;
  const _OrderTile({required this.order});

  @override
  ConsumerState<_OrderTile> createState() => _OrderTileState();
}

class _OrderTileState extends ConsumerState<_OrderTile> {
  bool _loading = false;

  Future<void> _onTap() async {
    if (_loading) return;
    setState(() => _loading = true);
    try {
      final full = await ref.read(orderApiProvider).get(widget.order.id);
      if (mounted) {
        setState(() => _loading = false);
        _show(full);
      }
    } catch (_) {
      if (mounted) {
        setState(() => _loading = false);
        _show(widget.order);
      }
    }
  }

  void _show(Order o) {
    final isTablet = MediaQuery.of(context).size.shortestSide >= 600;
    showModalBottomSheet(
      context: context,
      isScrollControlled: true,
      backgroundColor: Colors.transparent,
      constraints: isTablet
          ? BoxConstraints(
              maxWidth: 600,
              maxHeight: MediaQuery.of(context).size.height * 0.88)
          : null,
      builder: (_) => _OrderDetailSheet(order: o),
    );
  }

  @override
  Widget build(BuildContext context) {
    final o = widget.order;
    final isVoided = o.status == 'voided';
    final isPending = o.status == 'pending_sync';

    return GestureDetector(
      onTap: _onTap,
      child: AnimatedContainer(
        duration: const Duration(milliseconds: 150),
        margin: const EdgeInsets.only(bottom: 8),
        decoration: BoxDecoration(
          color: isVoided ? AppColors.bg : Colors.white,
          borderRadius: BorderRadius.circular(AppRadius.md),
          border: Border.all(
              color: isVoided ? AppColors.border : AppColors.borderLight),
          boxShadow: isVoided ? [] : AppShadows.card,
        ),
        child: Stack(children: [
          Padding(
            padding: const EdgeInsets.all(14),
            child: Row(children: [
              Container(
                width: 48,
                height: 48,
                decoration: BoxDecoration(
                    color: isVoided
                        ? AppColors.borderLight
                        : AppColors.primary.withOpacity(0.08),
                    borderRadius: BorderRadius.circular(AppRadius.sm)),
                alignment: Alignment.center,
                child: isPending 
                    ? const Icon(Icons.sync_rounded, color: AppColors.primary, size: 20)
                    : Text('#${o.orderNumber}',
                    style: cairo(
                        fontSize: 11,
                        fontWeight: FontWeight.w800,
                        color: isVoided
                            ? AppColors.textMuted
                            : AppColors.primary)),
              ),
              const SizedBox(width: 12),

              Expanded(
                  child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                    Row(children: [
                      _PaymentBadge(method: o.paymentMethod, voided: isVoided),
                      if (isVoided) ...[
                        const SizedBox(width: 6),
                        const _VoidedBadge(),
                      ],
                      if (isPending) ...[
                        const SizedBox(width: 6),
                        const _PendingSyncBadge(),
                      ],
                      const Spacer(),
                      Text(timeShort(o.createdAt),
                          style:
                              cairo(fontSize: 11, color: AppColors.textMuted)),
                    ]),
                    const SizedBox(height: 5),
                    Text(egp(o.totalAmount),
                        style: cairo(
                            fontSize: 16,
                            fontWeight: FontWeight.w800,
                            color: isVoided
                                ? AppColors.textMuted
                                : AppColors.textPrimary,
                            decoration:
                                isVoided ? TextDecoration.lineThrough : null)),
                    if (o.customerName != null) ...[
                      const SizedBox(height: 2),
                      Text(o.customerName!,
                          style: cairo(
                              fontSize: 11, color: AppColors.textSecondary)),
                    ],
                  ])),

              const SizedBox(width: 4),
              const Icon(Icons.chevron_right_rounded,
                  size: 16, color: AppColors.textMuted),
            ]),
          ),

          if (_loading)
            Positioned.fill(
                child: Container(
              decoration: BoxDecoration(
                  color: Colors.white.withOpacity(0.7),
                  borderRadius: BorderRadius.circular(AppRadius.md)),
              alignment: Alignment.center,
              child: const SizedBox(
                  width: 20,
                  height: 20,
                  child: CircularProgressIndicator(
                      strokeWidth: 2.5, color: AppColors.primary)),
            )),
        ]),
      ),
    );
  }
}

class _PaymentBadge extends StatelessWidget {
  final String method;
  final bool voided;
  const _PaymentBadge({required this.method, required this.voided});

  @override
  Widget build(BuildContext context) {
    final label = methodLabel(method);
    final color = methodColor(method);

    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
      decoration: BoxDecoration(
          color: voided ? AppColors.borderLight : color.withOpacity(0.1),
          borderRadius: BorderRadius.circular(AppRadius.xs)),
      child: Text(label,
          style: cairo(
              fontSize: 10,
              fontWeight: FontWeight.w700,
              color: voided ? AppColors.textMuted : color,
              letterSpacing: 0.2)),
    );
  }
}

class _VoidedBadge extends StatelessWidget {
  const _VoidedBadge();

  @override
  Widget build(BuildContext context) => Container(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
      decoration: BoxDecoration(
          color: AppColors.danger.withOpacity(0.09),
          borderRadius: BorderRadius.circular(AppRadius.xs)),
      child: Text('VOIDED',
          style: cairo(
              fontSize: 10,
              fontWeight: FontWeight.w700,
              color: AppColors.danger,
              letterSpacing: 0.3)));
}

class _PendingSyncBadge extends StatelessWidget {
  const _PendingSyncBadge();

  @override
  Widget build(BuildContext context) => Container(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
      decoration: BoxDecoration(
          color: AppColors.warning.withOpacity(0.1),
          borderRadius: BorderRadius.circular(AppRadius.xs)),
      child: Text('PENDING SYNC',
          style: cairo(
              fontSize: 10,
              fontWeight: FontWeight.w700,
              color: AppColors.warning,
              letterSpacing: 0.3)));
}

class _OrderDetailSheet extends ConsumerStatefulWidget {
  final Order order;
  const _OrderDetailSheet({required this.order});

  @override
  ConsumerState<_OrderDetailSheet> createState() => _OrderDetailSheetState();
}

class _OrderDetailSheetState extends ConsumerState<_OrderDetailSheet> {
  late Order _order;
  bool _printing = false;
  String? _printError;

  @override
  void initState() {
    super.initState();
    _order = widget.order;
  }

  Future<void> _print() async {
    final branch = ref.read(authProvider).branch;
    if (branch == null || !branch.hasPrinter) {
      ScaffoldMessenger.of(context).showSnackBar(const SnackBar(
        content: Text('No printer configured for this branch'),
        backgroundColor: AppColors.warning,
      ));
      return;
    }
    setState(() {
      _printing = true;
      _printError = null;
    });
    final err = await PrinterService.print(
        ip: branch.printerIp!,
        port: branch.printerPort,
        brand: branch.printerBrand!,
        order: _order,
        branchName: branch.name);
    if (mounted)
      setState(() {
        _printing = false;
        _printError = err;
      });
  }

  void _onVoided(Order voided) {
    setState(() => _order = voided);
    ref.read(orderHistoryProvider.notifier).updateOrder(voided);
  }

  @override
  Widget build(BuildContext context) {
    final order = _order;
    final isVoided = order.status == 'voided';
    final isPending = order.status == 'pending_sync';

    return Container(
      constraints:
          BoxConstraints(maxHeight: MediaQuery.of(context).size.height * 0.88),
      decoration: BoxDecoration(
          color: Colors.white, borderRadius: AppRadius.sheetRadius),
      child: Column(children: [
        Padding(
            padding: const EdgeInsets.only(top: 12),
            child: Center(
                child: Container(
                    width: 36,
                    height: 4,
                    decoration: BoxDecoration(
                        color: AppColors.border,
                        borderRadius: BorderRadius.circular(2))))),

        Padding(
          padding: const EdgeInsets.fromLTRB(22, 14, 22, 14),
          child: Row(crossAxisAlignment: CrossAxisAlignment.start, children: [
            Expanded(
                child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                  Row(children: [
                    Text(isPending ? 'Pending Order' : 'Order #${order.orderNumber}',
                        style:
                            cairo(fontSize: 18, fontWeight: FontWeight.w800)),
                    if (isVoided) ...[
                      const SizedBox(width: 8),
                      const _VoidedBadge(),
                    ],
                    if (isPending) ...[
                      const SizedBox(width: 8),
                      const _PendingSyncBadge(),
                    ],
                  ]),
                  const SizedBox(height: 3),
                  Text(dateTime(order.createdAt),
                      style:
                          cairo(fontSize: 12, color: AppColors.textSecondary)),
                  if (order.tellerName.isNotEmpty) ...[
                    const SizedBox(height: 2),
                    Text('by ${order.tellerName}',
                        style: cairo(fontSize: 11, color: AppColors.textMuted)),
                  ],
                ])),
            if (!isVoided && !isPending) ...[
              _printing
                  ? const Padding(
                      padding: EdgeInsets.all(8),
                      child: SizedBox(
                          width: 20,
                          height: 20,
                          child: CircularProgressIndicator(
                              strokeWidth: 2, color: AppColors.primary)))
                  : _SheetAction(
                      icon: Icons.print_rounded,
                      label: _printError != null ? 'Retry' : 'Print',
                      color: _printError != null
                          ? AppColors.danger
                          : AppColors.primary,
                      onTap: _print),
              const SizedBox(width: 8),
              _SheetAction(
                  icon: Icons.cancel_outlined,
                  label: 'Void',
                  color: AppColors.danger,
                  onTap: () => VoidOrderSheet.show(context, order, _onVoided)),
            ],
            if (isPending)
               _SheetAction(
                  icon: Icons.cancel_outlined,
                  label: 'Void',
                  color: AppColors.danger,
                  onTap: () => VoidOrderSheet.show(context, order, _onVoided)),
          ]),
        ),

        Container(height: 1, color: AppColors.border),

        Expanded(
            child: ListView(
          padding: const EdgeInsets.fromLTRB(20, 16, 20, 24),
          children: [
            _SectionCard(
              child: order.items.isEmpty
                  ? Text('No item details available',
                      style: cairo(fontSize: 13, color: AppColors.textMuted))
                  : Column(children: [
                      for (int i = 0; i < order.items.length; i++) ...[
                        _ItemRow(item: order.items[i]),
                        if (i < order.items.length - 1)
                          const Divider(
                              height: 1,
                              color: AppColors.borderLight,
                              indent: 14,
                              endIndent: 14),
                      ],
                    ]),
            ),

            const SizedBox(height: 12),

            _SectionCard(
              child: Column(children: [
                LabelValue('Subtotal', egp(order.subtotal)),
                if (order.discountAmount > 0)
                  LabelValue('Discount', '− ${egp(order.discountAmount)}',
                      valueColor: AppColors.success),
                if (order.taxAmount > 0)
                  LabelValue('Tax (14%)', egp(order.taxAmount)),
                Padding(
                    padding: const EdgeInsets.symmetric(vertical: 10),
                    child: Container(height: 1, color: AppColors.borderLight)),
                Row(
                    mainAxisAlignment: MainAxisAlignment.spaceBetween,
                    children: [
                      Text('Total',
                          style:
                              cairo(fontSize: 15, fontWeight: FontWeight.w800)),
                      Text(egp(order.totalAmount),
                          style: cairo(
                              fontSize: 18,
                              fontWeight: FontWeight.w800,
                              color: isVoided
                                  ? AppColors.textMuted
                                  : AppColors.primary,
                              decoration: isVoided
                                  ? TextDecoration.lineThrough
                                  : null)),
                    ]),
              ]),
            ),

            const SizedBox(height: 12),

            _SectionCard(
              child: Column(children: [
                _MetaRow(Icons.payments_outlined, 'Payment',
                    methodLabel(order.paymentMethod)),
                if (order.customerName != null) ...[
                  const SizedBox(height: 10),
                  _MetaRow(Icons.person_outline_rounded, 'Customer',
                      order.customerName!),
                ],
                const SizedBox(height: 10),
                _MetaRow(Icons.access_time_rounded, 'Time',
                    timeShort(order.createdAt)),
                if (isVoided && order.voidReason != null) ...[
                  const SizedBox(height: 10),
                  _MetaRow(Icons.cancel_outlined, 'Void Reason',
                      _voidReasonLabel(order.voidReason!)),
                ],
              ]),
            ),
          ],
        )),
      ]),
    );
  }

  String _voidReasonLabel(String r) {
    if (r.startsWith('other: ')) return r.substring(7);
    return switch (r) {
        'customer_request' => 'Customer request',
        'wrong_order' => 'Wrong order',
        'quality_issue' => 'Quality issue',
        'other' => 'Other',
        _ => r,
      };
  }
}

class _SectionCard extends StatelessWidget {
  final Widget child;
  const _SectionCard({required this.child});

  @override
  Widget build(BuildContext context) => Container(
        decoration: BoxDecoration(
            color: Colors.white,
            borderRadius: BorderRadius.circular(AppRadius.md),
            border: Border.all(color: AppColors.border),
            boxShadow: AppShadows.card),
        padding: const EdgeInsets.all(16),
        child: child,
      );
}

class _SheetAction extends StatelessWidget {
  final IconData icon;
  final String label;
  final Color color;
  final VoidCallback onTap;
  const _SheetAction(
      {required this.icon,
      required this.label,
      required this.color,
      required this.onTap});

  @override
  Widget build(BuildContext context) => GestureDetector(
        onTap: onTap,
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
          decoration: BoxDecoration(
              color: color.withOpacity(0.08),
              borderRadius: BorderRadius.circular(AppRadius.xs)),
          child: Row(mainAxisSize: MainAxisSize.min, children: [
            Icon(icon, size: 14, color: color),
            const SizedBox(width: 5),
            Text(label,
                style: cairo(
                    fontSize: 13, fontWeight: FontWeight.w600, color: color)),
          ]),
        ),
      );
}

class _ItemRow extends StatelessWidget {
  final OrderItem item;
  const _ItemRow({required this.item});

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.all(14),
        child: Row(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Container(
            width: 28,
            height: 28,
            decoration: BoxDecoration(
                color: AppColors.primary.withOpacity(0.08),
                borderRadius: BorderRadius.circular(AppRadius.xs)),
            alignment: Alignment.center,
            child: Text('${item.quantity}',
                style: cairo(
                    fontSize: 12,
                    fontWeight: FontWeight.w800,
                    color: AppColors.primary)),
          ),
          const SizedBox(width: 10),
          Expanded(
              child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                Text(
                    item.itemName +
                        (item.sizeLabel != null
                            ? ' · ${normaliseName(item.sizeLabel!)}'
                            : ''),
                    style: cairo(fontSize: 13, fontWeight: FontWeight.w600)),
                if (item.addons.isNotEmpty) ...[
                  const SizedBox(height: 6),
                  Wrap(
                      spacing: 4,
                      runSpacing: 4,
                      children: item.addons.map((a) {
                        final hasPrice = a.unitPrice > 0;
                        return Container(
                          padding: const EdgeInsets.symmetric(
                              horizontal: 7, vertical: 3),
                          decoration: BoxDecoration(
                              color: AppColors.primary.withOpacity(0.07),
                              borderRadius:
                                  BorderRadius.circular(AppRadius.xs)),
                          child: Text(
                              hasPrice
                                  ? '${normaliseName(a.addonName)}  +${egp(a.lineTotal)}'
                                  : normaliseName(a.addonName),
                              style: cairo(
                                  fontSize: 10,
                                  fontWeight: FontWeight.w600,
                                  color: AppColors.primary)),
                        );
                      }).toList()),
                ],
                if (item.optionals.isNotEmpty) ...[
                  const SizedBox(height: 6),
                  Wrap(
                      spacing: 4,
                      runSpacing: 4,
                      children: item.optionals.map((o) {
                        final hasPrice = o.price > 0;
                        return Container(
                          padding: const EdgeInsets.symmetric(
                              horizontal: 7, vertical: 3),
                          decoration: BoxDecoration(
                              color: AppColors.warning.withOpacity(0.08),
                              borderRadius:
                                  BorderRadius.circular(AppRadius.xs)),
                          child: Text(
                              hasPrice
                                  ? '${normaliseName(o.fieldName)}  +${egp(o.price)}'
                                  : normaliseName(o.fieldName),
                              style: cairo(
                                  fontSize: 10,
                                  fontWeight: FontWeight.w600,
                                  color: AppColors.warning)),
                        );
                      }).toList()),
                ],
              ])),
          const SizedBox(width: 10),
          Text(egp(item.lineTotal),
              style: cairo(fontSize: 13, fontWeight: FontWeight.w700)),
        ]),
      );
}

class _MetaRow extends StatelessWidget {
  final IconData icon;
  final String label, value;
  const _MetaRow(this.icon, this.label, this.value);

  @override
  Widget build(BuildContext context) => Row(children: [
        Icon(icon, size: 14, color: AppColors.textMuted),
        const SizedBox(width: 8),
        Text(label, style: cairo(fontSize: 12, color: AppColors.textSecondary)),
        const Spacer(),
        Text(value, style: cairo(fontSize: 12, fontWeight: FontWeight.w700)),
      ]);
}

class _Placeholder extends StatelessWidget {
  final IconData icon;
  final String message;
  final bool isTablet;
  final bool useLottie;

  const _Placeholder({
    required this.icon,
    required this.message,
    required this.isTablet,
    this.useLottie = false,
  });

  @override
  Widget build(BuildContext context) => Center(
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          if (useLottie)
            SizedBox(
                width: isTablet ? 180 : 150,
                height: isTablet ? 180 : 150,
                child: Lottie.asset('assets/lottie/no_orders.json',
                    fit: BoxFit.contain, repeat: true))
          else ...[
            Icon(icon, size: isTablet ? 48 : 40, color: AppColors.border),
            const SizedBox(height: 12),
          ],
          Text(message,
              style: cairo(
                  fontSize: isTablet ? 15 : 14,
                  color: AppColors.textSecondary)),
        ]),
      );
}
