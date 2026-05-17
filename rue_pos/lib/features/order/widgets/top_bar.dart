import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';
import '../../../core/providers/auth_notifier.dart';
import '../../../core/providers/cart_notifier.dart';
import '../../../core/providers/menu_notifier.dart';
import '../../../core/providers/discount_notifier.dart';
import '../../../core/services/connectivity_service.dart';
import '../../../core/services/offline_queue.dart';
import '../../../core/theme/app_theme.dart';
import '../../../core/utils/formatting.dart';
import 'shared_widgets.dart';

// ─────────────────────────────────────────────────────────────────────────────
//  TOP BAR
// ─────────────────────────────────────────────────────────────────────────────
class TopBar extends ConsumerWidget {
  final TextEditingController ctrl;
  final String query;
  const TopBar({super.key, required this.ctrl, required this.query});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final cart = ref.watch(cartProvider);
    final sync = ref.watch(offlineQueueProvider);
    final isOnline = ref.watch(isOnlineProvider);
    final isTablet = MediaQuery.of(context).size.width >= 768;
    final cachedAt = ref.watch(menuProvider).cachedAt;
    final lastSynced = cachedAt != null ? timeShort(cachedAt) : '—';

    final bar = Container(
      color: Colors.white,
      padding: const EdgeInsets.fromLTRB(12, 10, 16, 10),
      child: Row(children: [
        SmallIconBtn(
            icon: Icons.arrow_back_rounded, onTap: () => context.go('/home')),
        const SizedBox(width: 6),
        SyncBtn(),
        const SizedBox(width: 6),
        Text(lastSynced,
            style: cairo(fontSize: 11, color: AppColors.textMuted)),
        const SizedBox(width: 14),
        Expanded(
          child: Container(
            height: 38,
            decoration: BoxDecoration(
                color: AppColors.bg,
                borderRadius: BorderRadius.circular(AppRadius.sm),
                border: Border.all(color: AppColors.border)),
            child: TextField(
              controller: ctrl,
              style: cairo(fontSize: 14),
              decoration: InputDecoration(
                hintText: 'Search menu…',
                hintStyle: cairo(fontSize: 14, color: AppColors.textMuted),
                prefixIcon: const Icon(Icons.search_rounded,
                    size: 17, color: AppColors.textMuted),
                suffixIcon: query.isNotEmpty
                    ? GestureDetector(
                        onTap: ctrl.clear,
                        child: const Icon(Icons.close_rounded,
                            size: 16, color: AppColors.textMuted))
                    : null,
                border: InputBorder.none,
                enabledBorder: InputBorder.none,
                focusedBorder: InputBorder.none,
                contentPadding: const EdgeInsets.symmetric(vertical: 10),
                isDense: true,
                filled: false,
              ),
            ),
          ),
        ),
        if (isTablet) ...[
          const SizedBox(width: 12),
          AnimatedSwitcher(
            duration: const Duration(milliseconds: 250),
            transitionBuilder: (child, anim) => ScaleTransition(
                scale: anim,
                child: FadeTransition(opacity: anim, child: child)),
            child: cart.isEmpty
                ? const SizedBox.shrink(key: ValueKey('empty'))
                : Container(
                    key: const ValueKey('pill'),
                    padding:
                        const EdgeInsets.symmetric(horizontal: 14, vertical: 7),
                    decoration: BoxDecoration(
                      color: AppColors.primary,
                      borderRadius: BorderRadius.circular(20),
                      boxShadow: AppShadows.primaryGlow(),
                    ),
                    child: Row(children: [
                      const Icon(Icons.shopping_bag_outlined,
                          size: 14, color: Colors.white),
                      const SizedBox(width: 6),
                      Text('${cart.count} · ${egp(cart.total)}',
                          style: cairo(
                              fontSize: 12,
                              fontWeight: FontWeight.w700,
                              color: Colors.white)),
                    ]),
                  ),
          ),
        ],
      ]),
    );

    if (!isOnline || sync.orderCount > 0) {
      return Column(mainAxisSize: MainAxisSize.min, children: [
        bar,
        if (!isOnline)
          const StatusBanner(
              color: Color(0xFFFFF3CD),
              icon: Icons.wifi_off_rounded,
              text: 'Offline — cached menu. Orders will sync when connected.',
              textColor: Color(0xFF856404)),
        if (isOnline && sync.orderCount > 0)
          StatusBanner(
              color: const Color(0xFFCFE2FF),
              icon: Icons.sync_rounded,
              text:
                  'Syncing ${sync.orderCount} offline order${sync.orderCount == 1 ? "" : "s"}…',
              textColor: const Color(0xFF084298),
              animate: true),
      ]);
    }
    return bar;
  }
}

// ─────────────────────────────────────────────────────────────────────────────
//  STATUS BANNER
// ─────────────────────────────────────────────────────────────────────────────
class StatusBanner extends StatelessWidget {
  final Color color, textColor;
  final IconData icon;
  final String text;
  final bool animate;
  const StatusBanner({
    super.key,
    required this.color,
    required this.icon,
    required this.text,
    required this.textColor,
    this.animate = false,
  });

  @override
  Widget build(BuildContext context) => Container(
        width: double.infinity,
        color: color,
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 6),
        child: Row(children: [
          animate
              ? SizedBox(
                  width: 11,
                  height: 11,
                  child: CircularProgressIndicator(
                      strokeWidth: 1.5, color: textColor))
              : Icon(icon, size: 12, color: textColor),
          const SizedBox(width: 8),
          Expanded(
              child: Text(text, style: cairo(fontSize: 11, color: textColor))),
        ]),
      );
}

// ─────────────────────────────────────────────────────────────────────────────
//  SYNC BUTTON
// ─────────────────────────────────────────────────────────────────────────────
class SyncBtn extends ConsumerStatefulWidget {
  @override
  ConsumerState<SyncBtn> createState() => _SyncBtnState();
}

class _SyncBtnState extends ConsumerState<SyncBtn>
    with SingleTickerProviderStateMixin {
  bool _syncing = false;
  late final AnimationController _spinCtrl;

  @override
  void initState() {
    super.initState();
    _spinCtrl = AnimationController(
        vsync: this, duration: const Duration(milliseconds: 900));
  }

  @override
  void dispose() {
    _spinCtrl.dispose();
    super.dispose();
  }

  Future<void> _sync() async {
    if (_syncing) return;
    setState(() => _syncing = true);
    _spinCtrl.repeat();
    try {
      final orgId = ref.read(authProvider).user?.orgId;
      if (orgId != null) {
        await Future.wait([
          ref.read(menuProvider.notifier).load(orgId, force: true),
          ref.read(discountProvider.notifier).load(orgId, force: true),
        ]);
      }
    } finally {
      if (mounted) {
        _spinCtrl.stop();
        _spinCtrl.reset();
        setState(() => _syncing = false);
      }
    }
  }

  @override
  Widget build(BuildContext context) => GestureDetector(
        onTap: _sync,
        child: Container(
          width: 36,
          height: 36,
          decoration: BoxDecoration(
              color: AppColors.bg,
              borderRadius: BorderRadius.circular(AppRadius.xs),
              border: Border.all(color: AppColors.border)),
          alignment: Alignment.center,
          child: RotationTransition(
            turns: _spinCtrl,
            child: Icon(Icons.sync_rounded,
                size: 18,
                color: _syncing ? AppColors.primary : AppColors.textSecondary),
          ),
        ),
      );
}
