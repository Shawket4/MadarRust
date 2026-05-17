import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';
import '../../core/models/shift.dart';
import '../../core/providers/auth_notifier.dart';
import '../../core/providers/order_history_notifier.dart';
import '../../core/providers/shift_notifier.dart';
import '../shift/cash_movement_sheet.dart';
import '../../core/services/connectivity_service.dart';
import '../../core/services/offline_queue.dart';
import '../../core/theme/app_theme.dart';
import '../../core/utils/formatting.dart';
import '../../shared/widgets/app_button.dart';
import '../../shared/widgets/card_container.dart';
import '../../shared/widgets/sync_status_banner.dart';

class HomeScreen extends ConsumerStatefulWidget {
  const HomeScreen({super.key});
  @override
  ConsumerState<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends ConsumerState<HomeScreen> {
  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) => _load());
  }

  Future<void> _load() async {
    final branchId = ref.read(authProvider).user?.branchId;
    if (branchId == null) return;
    await ref.read(shiftProvider.notifier).load(branchId);
    final shift = ref.read(shiftProvider).shift;
    if (shift != null) {
      await ref
          .read(orderHistoryProvider.notifier)
          .loadForShift(shift.id, force: true);
      await ref.read(shiftProvider.notifier).loadSystemCash();
    }
  }

  Future<void> _onSignOut() async {
    final canLeave = await ref.read(authProvider.notifier).canLogout();
    if (!mounted) return;
    if (canLeave) {
      await ref.read(authProvider.notifier).logout();
    } else {
      await showDialog(
        context: context,
        builder: (ctx) => AlertDialog(
          shape:
              RoundedRectangleBorder(borderRadius: BorderRadius.circular(20)),
          title: Text('Close Shift First',
              style: cairo(fontWeight: FontWeight.w800)),
          content: Text(
            'You have an open shift. You must close it before signing out.',
            style: cairo(fontSize: 14, color: AppColors.textSecondary),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(ctx),
              child:
                  Text('Cancel', style: cairo(color: AppColors.textSecondary)),
            ),
            TextButton(
              onPressed: () {
                Navigator.pop(ctx);
                context.go('/close-shift');
              },
              child: Text('Close Shift',
                  style: cairo(
                      color: AppColors.danger, fontWeight: FontWeight.w700)),
            ),
          ],
        ),
      );
    }
  }

  @override
  Widget build(BuildContext context) {
    final user = ref.watch(authProvider).user!;
    final shiftSt = ref.watch(shiftProvider);
    // Task 3.7
    final isTablet = MediaQuery.of(context).size.shortestSide >= 600;

    return Scaffold(
      backgroundColor: AppColors.bg,
      body: SafeArea(
        child: Padding(
          padding: EdgeInsets.all(isTablet ? 36 : 24),
          child:
              Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
            Row(crossAxisAlignment: CrossAxisAlignment.center, children: [
              Image.asset('assets/TheRue.png', height: isTablet ? 52 : 44),
              const Spacer(),
              Text(user.name,
                  style: cairo(
                      fontSize: isTablet ? 14 : 13,
                      fontWeight: FontWeight.w600,
                      color: AppColors.textSecondary)),
              const SizedBox(width: 12),
              _SignOutBtn(onTap: _onSignOut),
            ]),
            SizedBox(height: isTablet ? 36 : 28),
            Text(_greet(user.name.split(' ').first),
                style: cairo(
                    fontSize: isTablet ? 28 : 22, fontWeight: FontWeight.w800)),
            const SizedBox(height: 2),
            Text(user.role.replaceAll('_', ' ').toUpperCase(),
                style: cairo(
                    fontSize: 11,
                    fontWeight: FontWeight.w600,
                    color: AppColors.textMuted,
                    letterSpacing: 1)),
            SizedBox(height: isTablet ? 32 : 24),
            if (shiftSt.isLoading)
              const Center(
                  child: CircularProgressIndicator(color: AppColors.primary))
            else if (shiftSt.error != null && !shiftSt.hasOpenShift)
              _ErrorBanner(message: shiftSt.error!, onRetry: _load)
            else if (shiftSt.hasOpenShift)
              _OpenShiftView(
                  shift: shiftSt.shift!,
                  shiftState: shiftSt,
                  onRefresh: _load,
                  isTablet: isTablet)
            else
              _NoShiftView(
                  suggested: shiftSt.suggestedOpeningCash, isTablet: isTablet),
          ]),
        ),
      ),
    );
  }

  String _greet(String first) {
    final h = DateTime.now().hour;
    final w = h < 12
        ? 'Morning'
        : h < 17
            ? 'Afternoon'
            : 'Evening';
    return 'Good $w, $first';
  }
}

class _OpenShiftView extends ConsumerWidget {
  final Shift shift;
  final ShiftState shiftState;
  final VoidCallback onRefresh;
  final bool isTablet;

  const _OpenShiftView(
      {required this.shift,
      required this.shiftState,
      required this.onRefresh,
      required this.isTablet});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final sync = ref.watch(offlineQueueProvider);
    final history = ref.watch(orderHistoryProvider);
    final isOnline = ref.watch(isOnlineProvider);

    final active = history.orders.where((o) => o.status != 'voided').toList();
    final orderCount = active.length;
    final salesTotal = active.fold(0, (s, o) => s + o.totalAmount);
    final statsReady = !shiftState.systemCashLoading;

    return Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
      Container(
        width: double.infinity,
        padding: EdgeInsets.all(isTablet ? 30 : 24),
        decoration: BoxDecoration(
          gradient: const LinearGradient(
              colors: [AppColors.primary, AppColors.secondary],
              begin: Alignment.topLeft,
              end: Alignment.bottomRight),
          borderRadius: BorderRadius.circular(24),
          boxShadow: [
            BoxShadow(
                color: AppColors.primary.withOpacity(0.25),
                blurRadius: 24,
                offset: const Offset(0, 8))
          ],
        ),
        child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          // Task 3.1: Use shared banner
          if (!isOnline)
            const SyncStatusBanner(
              variant: SyncBannerVariant.offline,
              text: 'Offline — cached data shown. Orders queue until online.'
            ),
          if (isOnline && history.fromCache)
            const SyncStatusBanner(
              variant: SyncBannerVariant.offline,
              text: 'Showing cached stats — tap refresh to update.'
            ),
          if (isOnline && sync.orderCount > 0)
            SyncStatusBanner(
              variant: SyncBannerVariant.syncing,
              text: 'Syncing ${sync.orderCount} offline order${sync.orderCount == 1 ? "" : "s"}…'
            ),
          if (sync.hasStuck)
            SyncStatusBanner(
              variant: SyncBannerVariant.stuck,
              text: '${sync.stuckCount} order${sync.stuckCount == 1 ? "" : "s"} failed to sync — check connection or discard.'
            ),
            
          Row(children: [
            _StatusPill(),
            const Spacer(),
            Text('Since ${timeShort(shift.openedAt)}',
                style: cairo(fontSize: 11, color: Colors.white60)),
            const SizedBox(width: 8),
            GestureDetector(
                onTap: onRefresh,
                child: const Icon(Icons.refresh_rounded,
                    size: 16, color: Colors.white54)),
          ]),
          SizedBox(height: isTablet ? 26 : 20),
          Row(children: [
            _ShiftStat(
                label: 'Sales',
                value: egp(salesTotal),
                loading: !statsReady,
                isTablet: isTablet),
            _VertDivider(),
            _ShiftStat(
                label: 'Orders',
                value: '$orderCount',
                loading: !statsReady,
                isTablet: isTablet),
            _VertDivider(),
            _ShiftStat(
                label: 'System Cash',
                value: egp(shiftState.systemCash),
                sublabel: '${egp(shift.openingCash)} opening',
                loading: shiftState.systemCashLoading,
                isTablet: isTablet),
          ]),
          SizedBox(height: isTablet ? 28 : 22),
          Row(children: [
            Expanded(
                child: _CardBtn(
                    label: 'New Order',
                    icon: Icons.add_shopping_cart_rounded,
                    onTap: () => context.go('/order'),
                    isTablet: isTablet)),
            const SizedBox(width: 8),
            Expanded(
                child: _CardBtn(
                    label: 'History',
                    icon: Icons.receipt_long_rounded,
                    onTap: () => context.go('/order-history'),
                    isTablet: isTablet)),
            const SizedBox(width: 8),
            Expanded(
                child: _CardBtn(
                    label: 'Shifts',
                    icon: Icons.history_rounded,
                    onTap: () => context.go('/shift-history'),
                    isTablet: isTablet)),
            const SizedBox(width: 8),
            Expanded(
                child: _CardBtn(
              label: 'Pending',
              icon: Icons.pending_actions_rounded,
              onTap: () => context.go('/pending-orders'),
              isTablet: isTablet,
            )),
            const SizedBox(width: 8),
            Expanded(
                child: _CardBtn(
              label: 'Cash',
              icon: Icons.payments_outlined,
              isTablet: isTablet,
              onTap: () => CashMovementSheet.show(
                context,
                shiftId: shift.id,
                onSuccess: onRefresh,
              ))), // Task 2.3: Remove tooltip/disabled check so offline works
            const SizedBox(width: 8),
            Expanded(
                // Task 2.4: Added Tooltip 
                child: Tooltip(
                  message: !isOnline ? 'Internet connection required to close shift.' : '',
                  child: _CardBtn(
                                label: 'Close',
                                icon: Icons.lock_outline_rounded,
                                danger: true,
                                isTablet: isTablet,
                                disabled: !isOnline,
                                onTap: !isOnline
                                    ? () {}
                                    : () => _confirmClose(context),
                              ),
                )),
          ]),
        ]),
      ),
    ]);
  }

  void _confirmClose(BuildContext context) => showDialog(
        context: context,
        builder: (ctx) => AlertDialog(
          shape:
              RoundedRectangleBorder(borderRadius: BorderRadius.circular(20)),
          title:
              Text('Close Shift?', style: cairo(fontWeight: FontWeight.w800)),
          content: Text('You will count cash and inventory on the next screen.',
              style: cairo(fontSize: 14, color: AppColors.textSecondary)),
          actions: [
            TextButton(
                onPressed: () => Navigator.pop(ctx),
                child: Text('Cancel',
                    style: cairo(color: AppColors.textSecondary))),
            TextButton(
              onPressed: () {
                Navigator.pop(ctx);
                context.go('/close-shift');
              },
              child: Text('Continue',
                  style: cairo(
                      color: AppColors.danger, fontWeight: FontWeight.w700)),
            ),
          ],
        ),
      );
}

class _NoShiftView extends StatelessWidget {
  final int suggested;
  final bool isTablet;
  const _NoShiftView({required this.suggested, required this.isTablet});

  @override
  Widget build(BuildContext context) => Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          ConstrainedBox(
            constraints:
                BoxConstraints(maxWidth: isTablet ? 480 : double.infinity),
            child: CardContainer(
              padding: EdgeInsets.all(isTablet ? 28 : 20),
              child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Row(children: [
                      Container(
                          width: 46,
                          height: 46,
                          decoration: BoxDecoration(
                              color: AppColors.primary.withOpacity(0.08),
                              borderRadius: BorderRadius.circular(14)),
                          child: const Icon(Icons.wb_sunny_outlined,
                              color: AppColors.primary, size: 22)),
                      const SizedBox(width: 14),
                      Column(
                          crossAxisAlignment: CrossAxisAlignment.start,
                          children: [
                            Text('No Open Shift',
                                style: cairo(
                                    fontSize: isTablet ? 18 : 16,
                                    fontWeight: FontWeight.w700)),
                            if (suggested > 0)
                              Text('Last closing: ${egp(suggested)}',
                                  style: cairo(
                                      fontSize: 12,
                                      color: AppColors.textSecondary)),
                          ]),
                    ]),
                    const SizedBox(height: 22),
                    AppButton(
                        label: 'Open Shift',
                        width: double.infinity,
                        icon: Icons.play_arrow_rounded,
                        onTap: () => context.go('/open-shift')),
                  ]),
            ),
          ),
          const SizedBox(height: 16),
          ConstrainedBox(
            constraints:
                BoxConstraints(maxWidth: isTablet ? 480 : double.infinity),
            child: OutlinedButton.icon(
              onPressed: () => context.go('/shift-history'),
              icon: const Icon(Icons.history_rounded, size: 16),
              label: Text('View Shift History', style: cairo(fontSize: 14)),
              style: OutlinedButton.styleFrom(
                minimumSize: const Size(double.infinity, 48),
                shape: RoundedRectangleBorder(
                    borderRadius: BorderRadius.circular(12)),
                side: const BorderSide(color: AppColors.border),
                foregroundColor: AppColors.textSecondary,
              ),
            ),
          ),
        ],
      );
}

class _SignOutBtn extends StatelessWidget {
  final VoidCallback onTap;
  const _SignOutBtn({required this.onTap});
  @override
  Widget build(BuildContext context) => GestureDetector(
        onTap: onTap,
        child: Container(
          width: 36,
          height: 36,
          decoration: BoxDecoration(
              color: Colors.white,
              borderRadius: BorderRadius.circular(11),
              border: Border.all(color: AppColors.border)),
          alignment: Alignment.center,
          child: const Icon(Icons.logout_rounded,
              size: 15, color: AppColors.textSecondary),
        ),
      );
}

class _StatusPill extends StatelessWidget {
  @override
  Widget build(BuildContext context) => Container(
        padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 5),
        decoration: BoxDecoration(
            color: Colors.white.withOpacity(0.18),
            borderRadius: BorderRadius.circular(20)),
        child: Row(mainAxisSize: MainAxisSize.min, children: [
          Container(
              width: 7,
              height: 7,
              decoration: const BoxDecoration(
                  color: Color(0xFF4ADE80), shape: BoxShape.circle)),
          const SizedBox(width: 6),
          Text('SHIFT OPEN',
              style: cairo(
                  fontSize: 10,
                  fontWeight: FontWeight.w700,
                  color: Colors.white,
                  letterSpacing: 0.8)),
        ]),
      );
}

class _ShiftStat extends StatelessWidget {
  final String label, value;
  final String? sublabel;
  final bool loading, isTablet;
  const _ShiftStat(
      {required this.label,
      required this.value,
      this.sublabel,
      this.loading = false,
      this.isTablet = false});
  @override
  Widget build(BuildContext context) => Expanded(
        child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Text(label,
              style: cairo(
                  fontSize: 11,
                  color: Colors.white60,
                  fontWeight: FontWeight.w500)),
          const SizedBox(height: 4),
          loading
              ? Container(
                  width: 50,
                  height: 16,
                  decoration: BoxDecoration(
                      color: Colors.white.withOpacity(0.2),
                      borderRadius: BorderRadius.circular(4)))
              : Text(value,
                  style: cairo(
                      fontSize: isTablet ? 20 : 17,
                      fontWeight: FontWeight.w800,
                      color: Colors.white)),
          if (sublabel != null) ...[
            const SizedBox(height: 2),
            Text(sublabel!, style: cairo(fontSize: 10, color: Colors.white38)),
          ],
        ]),
      );
}

class _VertDivider extends StatelessWidget {
  @override
  Widget build(BuildContext context) => Container(
        width: 1,
        height: 44,
        color: Colors.white.withOpacity(0.15),
        margin: const EdgeInsets.symmetric(horizontal: 14),
      );
}

class _CardBtn extends StatelessWidget {
  final String label;
  final IconData icon;
  final VoidCallback onTap;
  final bool danger, isTablet, disabled;
  const _CardBtn(
      {required this.label,
      required this.icon,
      required this.onTap,
      this.danger = false,
      this.isTablet = false,
      this.disabled = false});
  @override
  Widget build(BuildContext context) {
    final bgColor = disabled 
        ? Colors.white.withOpacity(0.4) 
        : danger 
            ? Colors.white.withOpacity(0.12) 
            : Colors.white;
    final fgColor = disabled 
        ? Colors.black38
        : danger 
            ? Colors.white 
            : AppColors.primary;

    return GestureDetector(
        onTap: disabled ? null : onTap,
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 150),
          padding: EdgeInsets.symmetric(vertical: isTablet ? 14 : 11),
          decoration: BoxDecoration(
              color: bgColor,
              borderRadius: BorderRadius.circular(14)),
          child: Column(mainAxisSize: MainAxisSize.min, children: [
            Icon(icon,
                size: isTablet ? 18 : 16,
                color: fgColor),
            const SizedBox(height: 4),
            Text(label,
                style: cairo(
                    fontSize: isTablet ? 12 : 11,
                    fontWeight: FontWeight.w700,
                    color: fgColor)),
          ]),
        ),
      );
  }
}

class _ErrorBanner extends StatelessWidget {
  final String message;
  final VoidCallback onRetry;
  const _ErrorBanner({required this.message, required this.onRetry});
  @override
  Widget build(BuildContext context) => Container(
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 14),
        decoration: BoxDecoration(
            color: AppColors.danger.withOpacity(0.06),
            borderRadius: BorderRadius.circular(14),
            border: Border.all(color: AppColors.danger.withOpacity(0.2))),
        child: Row(children: [
          const Icon(Icons.error_outline_rounded,
              color: AppColors.danger, size: 18),
          const SizedBox(width: 10),
          Expanded(
              child: Text(message,
                  style: cairo(fontSize: 13, color: AppColors.danger))),
          TextButton(
              onPressed: onRetry,
              child: Text('Retry',
                  style: cairo(
                      fontSize: 13,
                      fontWeight: FontWeight.w600,
                      color: AppColors.primary))),
        ]),
      );
}
