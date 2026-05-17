import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../models/shift.dart';
import '../models/shift_report.dart';
import 'client.dart';

class ShiftApi {
  final DioClient _c;
  ShiftApi(this._c);

  Future<ShiftPreFill> current(String branchId) async {
    final res = await _c.dio.get('/shifts/branches/$branchId/current');
    return ShiftPreFill.fromJson(res.data as Map<String, dynamic>);
  }

  Future<List<Shift>> list(String branchId) async {
    final res = await _c.dio.get('/shifts/branches/$branchId');
    return (res.data as List).map((s) => Shift.fromJson(s)).toList();
  }

  /// Standard online open — server generates the UUID.
  Future<Shift> open(String branchId, int openingCash) async {
    final res = await _c.dio.post(
      '/shifts/branches/$branchId/open',
      data: {'opening_cash': openingCash},
    );
    return Shift.fromJson(res.data as Map<String, dynamic>);
  }

  /// Offline-aware open — client supplies its own UUID and timestamp.
  /// Idempotent: safe to call multiple times with the same shiftId.
  Future<Shift> openWithId({
    required String branchId,
    required String shiftId,
    required int openingCash,
    required DateTime openedAt,
  }) async {
    final res = await _c.dio.post(
      '/shifts/branches/$branchId/open',
      data: {
        'id': shiftId,
        'opening_cash': openingCash,
        'opened_at': openedAt.toUtc().toIso8601String(),
      },
    );
    return Shift.fromJson(res.data as Map<String, dynamic>);
  }

  /// Close shift. Accepts optional closedAt for offline scenarios.
  /// Idempotent: returns existing close data if already closed.
  Future<Shift> close(
    String shiftId, {
    required int closingCash,
    String? note,
    required List<Map<String, dynamic>> inventoryCounts,
    DateTime? closedAt,
  }) async {
    final res = await _c.dio.post('/shifts/$shiftId/close', data: {
      'closing_cash_declared': closingCash,
      'cash_note': note,
      'inventory_counts': inventoryCounts,
      if (closedAt != null) 'closed_at': closedAt.toUtc().toIso8601String(),
    });
    final body = res.data as Map<String, dynamic>;
    return Shift.fromJson(body['shift'] as Map<String, dynamic>);
  }

  Future<int> cashMovementsTotal(String shiftId) async {
    int movements = 0;
    try {
      final movRes = await _c.dio.get('/shifts/$shiftId/cash-movements');
      movements = (movRes.data as List)
          .fold<int>(0, (s, m) => s + (m['amount'] as int));
    } catch (_) {}
    return movements;
  }

  Future<ShiftReport> getReport(String shiftId) async {
    final res = await _c.dio.get('/shifts/$shiftId/report');
    return ShiftReport.fromJson(res.data as Map<String, dynamic>);
  }

  Future<void> addCashMovement(String shiftId, int amount, String note) async {
    await _c.dio.post('/shifts/$shiftId/cash-movements',
        data: {'amount': amount, 'note': note});
  }
}

final shiftApiProvider =
    Provider<ShiftApi>((ref) => ShiftApi(ref.watch(dioClientProvider)));
