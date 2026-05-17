import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../api/order_api.dart';
import '../models/cart.dart';
import '../models/order.dart';
import '../storage/storage_service.dart';

class OrderRepository {
  final OrderApi _api;
  final StorageService _storage;
  OrderRepository(this._api, this._storage);

  Future<Order> create({
    required String branchId,
    required String shiftId,
    required CartState cart,
    required String idempotencyKey,
  }) =>
      _api.create(
        branchId: branchId,
        shiftId: shiftId,
        paymentMethod: cart.payment,
        items: cart.items,
        customerName: cart.customerName,
        discountType: cart.discountType?.apiValue,
        discountValue: cart.discountValue,
        idempotencyKey: idempotencyKey,
      );

  Future<List<Order>> listForShift(String shiftId) async {
    try {
      final orders = await _api.list(shiftId: shiftId);
      await _storage.saveOrders(
          shiftId, orders.map((o) => o.toJson()).toList());
      return orders;
    } catch (_) {
      final cached = _storage.loadOrders(shiftId);
      if (cached != null) return cached.map(Order.fromJson).toList();
      rethrow;
    }
  }

  Future<Order> get(String id) => _api.get(id);

  // Task 1.2: Remove force unwrapping on reason
  Future<Order> voidOrder(String id,
          {String? reason, bool restoreInventory = false}) =>
      _api.voidOrder(id, reason: reason ?? 'No reason provided', restoreInventory: restoreInventory);

  // Task 1.1: Replace append with save since the notifier handles the list merging
  void saveOrdersToCache(String shiftId, List<Order> current) {
    _storage.saveOrders(shiftId, current.map((o) => o.toJson()).toList());
  }

  List<Order>? loadCachedOrders(String shiftId) {
    final cached = _storage.loadOrders(shiftId);
    if (cached == null) return null;
    return cached.map(Order.fromJson).toList();
  }
}

final orderRepositoryProvider =
    Provider<OrderRepository>((ref) => OrderRepository(
          ref.watch(orderApiProvider),
          ref.watch(storageServiceProvider),
        ));
