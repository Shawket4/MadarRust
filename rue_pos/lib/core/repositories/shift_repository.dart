import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../api/order_api.dart';
import '../api/shift_api.dart';
import '../api/inventory_api.dart';
import '../models/shift.dart';
import '../models/inventory.dart';
import '../storage/storage_service.dart';

class ShiftRepository {
  final ShiftApi       _shiftApi;
  final OrderApi       _orderApi;
  final InventoryApi   _inventoryApi;
  final StorageService _storage;
  ShiftRepository(this._shiftApi, this._orderApi, this._inventoryApi, this._storage);

  Future<ShiftPreFill> currentShift(String branchId) async {
    try {
      final preFill = await _shiftApi.current(branchId);
      if (preFill.openShift != null) {
        await _storage.saveShift(branchId, preFill.openShift!.toJson());
      } else {
        await _storage.removeShift(branchId);
      }
      return preFill;
    } catch (_) {
      final cached = _storage.loadShift(branchId);
      if (cached != null) {
        final shift = Shift.fromJson(cached);
        return ShiftPreFill(
            hasOpenShift: shift.isOpen, openShift: shift,
            suggestedOpeningCash: 0);
      }
      rethrow;
    }
  }

  Future<List<Shift>> listShifts(String branchId) =>
      _shiftApi.list(branchId);

  Future<Shift> openShift(String branchId, int openingCash) async {
    final shift = await _shiftApi.open(branchId, openingCash);
    await _storage.saveShift(branchId, shift.toJson());
    return shift;
  }

  Future<Shift> closeShift(String shiftId, {
    required String branchId,
    required int    closingCash,
    String?         note,
    required List<Map<String, dynamic>> inventoryCounts,
  }) async {
    final shift = await _shiftApi.close(shiftId,
        closingCash: closingCash, note: note, inventoryCounts: inventoryCounts);
    await _storage.removeShift(branchId);
    return shift;
  }

  Future<int> getSystemCash(String shiftId, int openingCash) async {
    final orders = await _orderApi.list(shiftId: shiftId);
    final cashFromOrders = orders
        .where((o) =>
            o.paymentMethod == 'cash' &&
            o.status != 'voided' &&
            o.status != 'refunded')
        .fold<int>(0, (s, o) => s + o.totalAmount);
    final movements = await _shiftApi.cashMovementsTotal(shiftId);
    return openingCash + cashFromOrders + movements;
  }

  Future<List<InventoryItem>> getInventory(String branchId) async {
    try { return await _inventoryApi.items(branchId); }
    catch (_) { return []; }
  }
}

final shiftRepositoryProvider = Provider<ShiftRepository>((ref) => ShiftRepository(
  ref.watch(shiftApiProvider),
  ref.watch(orderApiProvider),
  ref.watch(inventoryApiProvider),
  ref.watch(storageServiceProvider),
));
