import 'package:dio/dio.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../models/cart.dart';
import '../models/order.dart';
import 'client.dart';

class OrderApi {
  final DioClient _c;
  OrderApi(this._c);

  Future<Order> create({
    required String              branchId,
    required String              shiftId,
    required String              paymentMethod,
    required List<CartItem>      items,
    String?                      customerName,
    String?                      discountType,
    int?                         discountValue,
    String?                      discountId,
    int?                         amountTendered,
    int?                         tipAmount,
    String?                      tipPaymentMethod,
    List<PaymentSplit>?          paymentSplits,
    required String              idempotencyKey,
    DateTime?                    createdAt,
  }) async {
    final res = await _c.dio.post(
      '/orders',
      data: {
        'branch_id':      branchId,
        'shift_id':       shiftId,
        'payment_method': paymentMethod,
        'customer_name':  customerName,
        'discount_type':  discountType,
        'discount_value': discountValue,
        if (discountId       != null) 'discount_id':        discountId,
        if (amountTendered   != null) 'amount_tendered':    amountTendered,
        if (tipAmount        != null) 'tip_amount':         tipAmount,
        if (tipPaymentMethod != null) 'tip_payment_method': tipPaymentMethod,
        if (paymentSplits != null && paymentSplits.isNotEmpty)
          'payment_splits': paymentSplits.map((s) => s.toApiJson()).toList(),
        'items': items.map((i) => i.toApiJson()).toList(),
        if (createdAt != null) 'created_at': createdAt.toUtc().toIso8601String(),
      },
      options: Options(headers: {'Idempotency-Key': idempotencyKey}),
    );
    return Order.fromJson(res.data as Map<String, dynamic>);
  }

  /// Fetches all orders for the given filters.
  ///
  /// Backend (`GET /orders`) paginates (shift default `per_page=1000`) and returns:
  /// `{ data, total, page, per_page, total_pages, summary }`.
  Future<List<Order>> list({String? shiftId, String? branchId}) async {
    final baseParams = <String, dynamic>{
      // RueRust API allows up to 999999; use a high page size for POS shift loads.
      'per_page': 500,
    };
    if (shiftId  != null) baseParams['shift_id']  = shiftId;
    if (branchId != null) baseParams['branch_id'] = branchId;

    final all = <Order>[];
    var page = 1;

    while (true) {
      final params = {...baseParams, 'page': page};
      final res = await _c.dio.get('/orders', queryParameters: params);
      final body = res.data;

      final List<dynamic> items;
      int? totalPages;

      if (body is Map<String, dynamic>) {
        final raw = body['data'];
        if (raw is! List) break;
        items = raw;
        // RueRust: top-level total_pages (not Laravel-style meta.last_page)
        totalPages = _asInt(body['total_pages']);
        if (totalPages == null) {
          final meta = body['meta'];
          if (meta is Map) totalPages = _asInt(meta['last_page']);
        }
      } else if (body is List) {
        items = body;
        totalPages = 1;
      } else {
        break;
      }

      all.addAll(items.map((o) => Order.fromJson(o as Map<String, dynamic>)));

      if (totalPages == null || page >= totalPages || items.isEmpty) break;
      page++;
    }

    return all;
  }

  static int? _asInt(dynamic v) {
    if (v is int) return v;
    if (v is num) return v.toInt();
    return int.tryParse(v?.toString() ?? '');
  }

  Future<Order> get(String id) async {
    final res = await _c.dio.get('/orders/$id');
    return Order.fromJson(res.data as Map<String, dynamic>);
  }

  Future<Order> voidOrder(
    String id, {
    required String reason,
    bool     restoreInventory = false,
    DateTime? voidedAt,
  }) async {
    final res = await _c.dio.post('/orders/$id/void', data: {
      'reason':            reason,
      'restore_inventory': restoreInventory,
      if (voidedAt != null) 'voided_at': voidedAt.toUtc().toIso8601String(),
    });
    return Order.fromJson(res.data as Map<String, dynamic>);
  }
}

final orderApiProvider =
    Provider<OrderApi>((ref) => OrderApi(ref.watch(dioClientProvider)));
