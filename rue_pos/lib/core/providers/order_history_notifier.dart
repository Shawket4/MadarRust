import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../models/order.dart';
import '../repositories/order_repository.dart';

class OrderHistoryState {
  final List<Order> orders;
  final bool isLoading;
  final bool fromCache;
  final String? error;
  final String? shiftId;

  const OrderHistoryState({
    this.orders = const [],
    this.isLoading = false,
    this.fromCache = false,
    this.error,
    this.shiftId,
  });

  OrderHistoryState copyWith({
    List<Order>? orders,
    bool? isLoading,
    bool? fromCache,
    String? error,
    String? shiftId,
    bool clearError = false,
  }) =>
      OrderHistoryState(
        orders: orders ?? this.orders,
        isLoading: isLoading ?? this.isLoading,
        fromCache: fromCache ?? this.fromCache,
        error: clearError ? null : (error ?? this.error),
        shiftId: shiftId ?? this.shiftId,
      );
}

class OrderHistoryNotifier extends Notifier<OrderHistoryState> {
  @override
  OrderHistoryState build() => const OrderHistoryState();

  Future<void> loadForShift(String shiftId, {bool force = false}) async {
    if (!force && state.shiftId == shiftId && state.orders.isNotEmpty) return;
    state = state.copyWith(isLoading: true, clearError: true, shiftId: shiftId);
    try {
      final orders =
          await ref.read(orderRepositoryProvider).listForShift(shiftId);
      state = state.copyWith(
          isLoading: false, orders: orders, shiftId: shiftId, fromCache: false);
    } catch (_) {
      final cached =
          ref.read(orderRepositoryProvider).loadCachedOrders(shiftId);
      if (cached != null) {
        state = state.copyWith(
            isLoading: false,
            orders: cached,
            shiftId: shiftId,
            fromCache: true,
            error: 'Showing cached orders — check connection');
      } else {
        state = state.copyWith(
            isLoading: false,
            fromCache: true,
            error: 'Could not load orders — check connection');
      }
    }
  }

  Future<void> refresh(String shiftId) => loadForShift(shiftId, force: true);

  void addOrder(Order order) {
    if (state.orders.any((o) => o.id == order.id)) return;
    final updated = [order, ...state.orders];
    state = state.copyWith(orders: updated);
    if (state.shiftId != null) {
      // Task 1.1: Pass the updated list directly, let repository save it without prepending
      ref
          .read(orderRepositoryProvider)
          .saveOrdersToCache(state.shiftId!, updated);
    }
  }

  // Task 1.5: Replace optimistic order
  void replaceOrder(String localId, Order synced) {
    final idx = state.orders.indexWhere((o) => o.id == localId);
    if (idx >= 0) {
      final updated = List<Order>.of(state.orders);
      updated[idx] = synced;
      state = state.copyWith(orders: updated);
      if (state.shiftId != null) {
        ref.read(orderRepositoryProvider).saveOrdersToCache(state.shiftId!, updated);
      }
    } else {
      addOrder(synced);
    }
  }

  void updateOrder(Order updated) {
    final newOrders = state.orders.map((o) => o.id == updated.id ? updated : o).toList();
    state = state.copyWith(orders: newOrders);
    if (state.shiftId != null) {
        ref.read(orderRepositoryProvider).saveOrdersToCache(state.shiftId!, newOrders);
    }
  }

  void clear() => state = const OrderHistoryState();
}

final orderHistoryProvider =
    NotifierProvider<OrderHistoryNotifier, OrderHistoryState>(
        OrderHistoryNotifier.new);
