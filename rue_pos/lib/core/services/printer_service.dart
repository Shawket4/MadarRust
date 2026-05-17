import 'dart:async';
import 'dart:io';
import 'dart:ui' as ui;
import 'package:flutter/services.dart';
import 'package:pdf/pdf.dart';
import 'package:pdf/widgets.dart' as pw;
import 'package:printing/printing.dart';
import 'package:starxpand_sdk_wrapper/starxpand_sdk_wrapper.dart';
import '../models/branch.dart';
import '../models/order.dart';
import '../models/shift_report.dart';
import '../utils/formatting.dart';

class PrinterService {
  static const _printerWidth = 576;
  static const _timeout      = Duration(seconds: 5);

  // ── Public entry points ────────────────────────────────────────────────────

  static Future<String?> print({
    required String       ip,
    required int          port,
    required PrinterBrand brand,
    required Order        order,
    required String       branchName,
    bool                  kickDrawer = false,
  }) async {
    final pdfBytes = await _buildReceiptPdf(order: order, branchName: branchName);
    return _send(
      ip: ip,
      port: port,
      brand: brand,
      pdfBytes: pdfBytes,
      kickDrawer: kickDrawer,
    );
  }

  static Future<String?> printShiftReport({
    required String       ip,
    required int          port,
    required PrinterBrand brand,
    required ShiftReport  report,
    required String       branchName,
  }) async {
    final pdfBytes = await _buildShiftReportPdf(report: report, branchName: branchName);
    return _send(ip: ip, port: port, brand: brand, pdfBytes: pdfBytes);
  }

  // ── Transport ──────────────────────────────────────────────────────────────

  static Future<String?> _send({
    required String       ip,
    required int          port,
    required PrinterBrand brand,
    required Uint8List    pdfBytes,
    bool                  kickDrawer = false,
  }) {
    final cleanIp = ip.split('/').first;
    return switch (brand) {
      PrinterBrand.star  => _printStar(
          ip: cleanIp,
          pdfBytes: pdfBytes,
          kickDrawer: kickDrawer,
        ),
      PrinterBrand.epson => _printEpson(
          ip: cleanIp,
          port: port,
          pdfBytes: pdfBytes,
          kickDrawer: kickDrawer,
        ),
    };
  }

  static Future<String?> _printStar({
    required String    ip,
    required Uint8List pdfBytes,
    bool               kickDrawer = false,
  }) async {
    // 1. Kick drawer if requested (Star BEL command: 0x07)
    if (kickDrawer) {
      try {
        final s = await Socket.connect(ip, 9100, timeout: _timeout);
        s.add(Uint8List.fromList([0x07]));
        await s.flush();
        await s.close();
      } catch (_) {
        // We don't fail the whole print job if the drawer kick fails
      }
    }

    // 2. Print PDF
    try {
      final device    = StarDevice(ip, StarInterfaceType.lan);
      final connected = await StarXpand.instance.connect(device, monitor: false);
      if (!connected) return 'Could not connect to Star printer';
      final ok = await StarXpand.instance.printPdf(pdfBytes, width: _printerWidth);
      return ok ? null : 'Star print failed';
    } catch (e) {
      return 'Star printer error: $e';
    } finally {
      await StarXpand.instance.disconnect();
    }
  }

  static Future<String?> _printEpson({
    required String    ip,
    required int       port,
    required Uint8List pdfBytes,
    bool               kickDrawer = false,
  }) async {
    Socket? socket;
    try {
      final page     = await Printing.raster(pdfBytes, dpi: 203).first;
      final png      = await page.toPng();
      final imgBytes = await _pngToEscPos(png, page.width, page.height);
      
      socket = await Socket.connect(ip, port, timeout: _timeout);
      socket.setOption(SocketOption.tcpNoDelay, true);

      // Kick drawer if requested (ESC/POS: ESC p m t1 t2)
      if (kickDrawer) {
        socket.add(Uint8List.fromList([0x1B, 0x70, 0x00, 0x19, 0xFA]));
      }

      socket.add(imgBytes);
      await socket.flush().timeout(_timeout);
      return null;
    } on TimeoutException {
      return 'Epson printer timeout';
    } on SocketException catch (e) {
      return 'Epson printer error: ${e.message}';
    } catch (e) {
      return 'Epson printer error: $e';
    } finally {
      await socket?.close();
    }
  }

  // ── ESC/POS rasteriser ─────────────────────────────────────────────────────

  static Future<Uint8List> _pngToEscPos(Uint8List png, int w, int h) async {
    final codec   = await ui.instantiateImageCodec(png);
    final frame   = await codec.getNextFrame();
    final imgData = await frame.image.toByteData(format: ui.ImageByteFormat.rawRgba);
    if (imgData == null) throw Exception('Failed to decode image');
    final pixels = imgData.buffer.asUint8List();
    final buf    = <int>[];
    buf.addAll([0x1B, 0x40]);
    final wB = (w + 7) ~/ 8;
    buf.addAll([
      0x1D, 0x76, 0x30, 0x00,
      wB & 0xFF, (wB >> 8) & 0xFF,
      h  & 0xFF, (h  >> 8) & 0xFF,
    ]);
    for (int y = 0; y < h; y++) {
      for (int xB = 0; xB < wB; xB++) {
        int byte = 0;
        for (int bit = 0; bit < 8; bit++) {
          final x = xB * 8 + bit;
          if (x < w) {
            final idx = (y * w + x) * 4;
            final r   = pixels[idx];
            final g   = pixels[idx + 1];
            final b   = pixels[idx + 2];
            final a   = pixels[idx + 3];
            final rW  = ((r * a) + (255 * (255 - a))) ~/ 255;
            final gW  = ((g * a) + (255 * (255 - a))) ~/ 255;
            final bW  = ((b * a) + (255 * (255 - a))) ~/ 255;
            if ((0.299 * rW + 0.587 * gW + 0.114 * bW).round() < 128) {
              byte |= (0x80 >> bit);
            }
          }
        }
        buf.add(byte);
      }
    }
    buf.addAll([0x1B, 0x64, 0x05, 0x1D, 0x56, 0x41, 0x05]);
    return Uint8List.fromList(buf);
  }

  // ── Shared PDF helpers ─────────────────────────────────────────────────────
  //
  //  _row()  — two-column layout using pw.Row + pw.Expanded so numbers are
  //             always flush-right regardless of Cairo glyph widths.
  //  _divider / _thinDivider — consistent separators.

  static pw.Widget _row(
    String    label,
    String    value, {
    required  pw.Font font,
    required  pw.Font fontB,
    double    sz        = 8,
    bool      bold      = false,
    bool      boldValue = false,
    PdfColor? valueColor,
    double    leftIndent = 0,
  }) {
    final labelStyle = pw.TextStyle(
      font:     bold ? fontB : font,
      fontSize: sz,
    );
    final valueStyle = pw.TextStyle(
      font:     (bold || boldValue) ? fontB : font,
      fontSize: sz,
      color:    valueColor,
    );
    return pw.Padding(
      padding: pw.EdgeInsets.only(left: leftIndent, bottom: 1.5),
      child: pw.Row(children: [
        pw.Expanded(child: pw.Text(label, style: labelStyle)),
        pw.Text(value, style: valueStyle, textAlign: pw.TextAlign.right),
      ]),
    );
  }

  static pw.Widget _divider() =>
      pw.Divider(thickness: 0.4, color: PdfColors.grey600, height: 6);

  static pw.Widget _thinDivider() =>
      pw.Divider(thickness: 0.2, color: PdfColors.grey400, height: 4);

  static String _fmtDt(DateTime dt) {
    final l = dt.toLocal();
    final d = l.day.toString().padLeft(2, '0');
    final m = l.month.toString().padLeft(2, '0');
    final hh = l.hour.toString().padLeft(2, '0');
    final mm = l.minute.toString().padLeft(2, '0');
    return '$d/$m/${l.year}  $hh:$mm';
  }

  /// Formats a payment method string for the order receipt footer.
  /// Handles all known Talabat variants and underscore-separated names.
  static String _fmtPayment(String raw) => switch (raw) {
    'cash'           => 'Cash',
    'card'           => 'Card',
    'digital_wallet' => 'Digital Wallet',
    'mixed'          => 'Mixed',
    'talabat_online' => 'Talabat Online',
    'talabat_cash'   => 'Talabat Cash',
    _                => raw[0].toUpperCase() +
                        raw.substring(1).replaceAll('_', ' '),
  };

  // ── Order receipt PDF ──────────────────────────────────────────────────────

  static Future<Uint8List> _buildReceiptPdf({
    required Order  order,
    required String branchName,
  }) async {
    final pdf   = pw.Document();
    final font  = pw.Font.ttf(
        (await rootBundle.load('assets/fonts/Cairo-Regular.ttf')).buffer.asByteData());
    final fontB = pw.Font.ttf(
        (await rootBundle.load('assets/fonts/Cairo-SemiBold.ttf')).buffer.asByteData());
    final logo  = pw.MemoryImage(
        (await rootBundle.load('assets/TheRue.png')).buffer.asUint8List());

    pw.TextStyle ts(pw.Font f, {double sz = 8, PdfColor? color}) =>
        pw.TextStyle(font: f, fontSize: sz, color: color);

    final dts = _fmtDt(order.createdAt);

    pdf.addPage(pw.Page(
      pageFormat: const PdfPageFormat(
          72 * PdfPageFormat.mm, double.infinity,
          marginTop:    2 * PdfPageFormat.mm,
          marginBottom: 2 * PdfPageFormat.mm,
          marginLeft:   2 * PdfPageFormat.mm,
          marginRight:  2 * PdfPageFormat.mm),
      build: (ctx) => pw.Column(
          crossAxisAlignment: pw.CrossAxisAlignment.stretch, children: [

        // Header
        pw.Center(child: pw.Image(logo, width: 56)),
        pw.SizedBox(height: 2),
        pw.Center(child: pw.Text(branchName, style: ts(font, sz: 7.5))),
        pw.SizedBox(height: 2),
        _divider(),

        // Order number + timestamp
        _row('Order #${order.orderNumber}', dts, font: font, fontB: fontB,
            bold: true, sz: 8),
        _divider(),

        // Items
        ...order.items.expand((item) {
          final sizePart = item.sizeLabel != null ? ' (${item.sizeLabel})' : '';
          return [
            _row('${item.quantity}x ${item.itemName}$sizePart',
                egp(item.lineTotal), font: font, fontB: fontB, bold: true, sz: 8),
            ...item.addons.map((a) {
              final aPrice = a.unitPrice > 0 ? '+${egp(a.unitPrice)}' : '';
              return aPrice.isNotEmpty
                  ? _row('  + ${a.addonName}', aPrice,
                      font: font, fontB: fontB, sz: 7.5, leftIndent: 4)
                  : pw.Padding(
                      padding: const pw.EdgeInsets.only(left: 4, bottom: 1.5),
                      child: pw.Text('  + ${a.addonName}', style: ts(font, sz: 7.5)));
            }),
            ...item.optionals.map((o) {
              final oPrice = o.price > 0 ? '+${egp(o.price)}' : '';
              return oPrice.isNotEmpty
                  ? _row('  + ${o.fieldName}', oPrice,
                      font: font, fontB: fontB, sz: 7.5, leftIndent: 4)
                  : pw.Padding(
                      padding: const pw.EdgeInsets.only(left: 4, bottom: 1.5),
                      child: pw.Text('  + ${o.fieldName}', style: ts(font, sz: 7.5)));
            }),
          ];
        }),
        _divider(),

        // Totals
        if (order.discountAmount > 0)
          _row('Subtotal', egp(order.subtotal), font: font, fontB: fontB, sz: 8),
        if (order.discountAmount > 0)
          _row('Discount', '- ${egp(order.discountAmount)}',
              font: font, fontB: fontB, sz: 8,
              valueColor: PdfColors.red700),
        if (order.taxAmount > 0)
          _row('Tax', egp(order.taxAmount), font: font, fontB: fontB, sz: 8),
        _row('TOTAL', egp(order.totalAmount),
            font: font, fontB: fontB, bold: true, boldValue: true, sz: 10),
        _divider(),

        // Footer metadata
        _row('Payment', _fmtPayment(order.paymentMethod),
            font: font, fontB: fontB, sz: 7.5),
        if (order.customerName != null && order.customerName!.isNotEmpty)
          _row('Customer', order.customerName!,
              font: font, fontB: fontB, sz: 7.5),
        if (order.tellerName.isNotEmpty)
          _row('Teller', order.tellerName, font: font, fontB: fontB, sz: 7.5),

        pw.SizedBox(height: 4),
        pw.Center(child: pw.Text('Thank you for visiting!', style: ts(font, sz: 7.5))),
        pw.SizedBox(height: 2),
        _divider(),
      ]),
    ));
    return pdf.save();
  }

  // ── Shift report PDF ───────────────────────────────────────────────────────

  static Future<Uint8List> _buildShiftReportPdf({
    required ShiftReport report,
    required String      branchName,
  }) async {
    final pdf   = pw.Document();
    final font  = pw.Font.ttf(
        (await rootBundle.load('assets/fonts/Cairo-Regular.ttf')).buffer.asByteData());
    final fontB = pw.Font.ttf(
        (await rootBundle.load('assets/fonts/Cairo-SemiBold.ttf')).buffer.asByteData());
    final logo  = pw.MemoryImage(
        (await rootBundle.load('assets/TheRue.png')).buffer.asUint8List());

    pw.TextStyle ts(pw.Font f, {double sz = 8, PdfColor? color}) =>
        pw.TextStyle(font: f, fontSize: sz, color: color);

    // Helpers that close over font/fontB
    pw.Widget row(String l, String v,
            {bool bold = false, bool boldVal = false,
             double sz = 8, PdfColor? color, double indent = 0}) =>
        _row(l, v,
            font: font, fontB: fontB,
            bold: bold, boldValue: boldVal,
            sz: sz, valueColor: color, leftIndent: indent);

    pw.Widget div()     => _divider();
    pw.Widget thinDiv() => _thinDivider();

    // Formatted timestamps
    final openDt  = report.openedAt.toLocal();
    final bizDate = '${openDt.day.toString().padLeft(2,'0')}/'
                    '${openDt.month.toString().padLeft(2,'0')}/${openDt.year}';
    final openTs  = _fmtDt(report.openedAt);
    final closeTs = report.closedAt != null ? _fmtDt(report.closedAt!) : null;

    // The timestamp shown at the top:
    //  • closed shift  → "Closed at: HH:MM" (the close time, not now)
    //  • open shift    → "Printed at: HH:MM" (now)
    final topTsLabel = report.isOpen ? 'Printed at' : 'Closed at';
    final topTsValue = report.closedAt != null
        ? _fmtDt(report.closedAt!)
        : _fmtDt(report.printedAt);

    // Cash shortage: positive = drawer short (declared < system), negative = over
    // shortage = system - declared  →  positive means you're missing money
    final shortage = (report.closingCashDeclared != null &&
                      report.closingCashSystem   != null)
        ? report.closingCashSystem! - report.closingCashDeclared!
        : null;

    pdf.addPage(pw.Page(
      pageFormat: const PdfPageFormat(
          72 * PdfPageFormat.mm, double.infinity,
          marginTop:    3 * PdfPageFormat.mm,
          marginBottom: 3 * PdfPageFormat.mm,
          marginLeft:   3 * PdfPageFormat.mm,
          marginRight:  3 * PdfPageFormat.mm),
      build: (ctx) => pw.Column(
          crossAxisAlignment: pw.CrossAxisAlignment.stretch, children: [

        // ── Header ────────────────────────────────────────────────────────────
        pw.Center(child: pw.Image(logo, width: 56)),
        pw.SizedBox(height: 3),
        pw.Center(child: pw.Text(branchName,         style: ts(fontB, sz: 8.5))),
        pw.Center(child: pw.Text('Till Close Report', style: ts(fontB, sz: 9.5))),
        pw.SizedBox(height: 4),
        pw.Center(child: pw.Text('Business Date: $bizDate', style: ts(font, sz: 7.5))),
        pw.Center(child: pw.Text('$topTsLabel: $topTsValue', style: ts(font, sz: 7.5))),
        pw.SizedBox(height: 4),
        div(),

        // ── Shift info ────────────────────────────────────────────────────────
        pw.SizedBox(height: 2),
        row('Teller', report.tellerName, sz: 8),
        row('Opened', openTs, sz: 7.5),
        if (closeTs != null)
          row('Closed', closeTs, sz: 7.5)
        else
          pw.Center(child: pw.Text(
              '— Interim Report (Shift Still Open) —',
              style: ts(font, sz: 7, color: PdfColors.grey600))),
        pw.SizedBox(height: 2),
        div(),

        // ── Payments breakdown ────────────────────────────────────────────────
        pw.SizedBox(height: 2),
        pw.Center(child: pw.Text('PAYMENTS', style: ts(fontB, sz: 7.5,
            color: PdfColors.grey700))),
        pw.SizedBox(height: 4),

        // Each payment method — label, order count sub-line, amount right-aligned
        ...report.paymentSummary.map((p) => pw.Padding(
          padding: const pw.EdgeInsets.only(bottom: 3),
          child: pw.Row(children: [
            pw.Expanded(child: pw.Column(
              crossAxisAlignment: pw.CrossAxisAlignment.start, children: [
                pw.Text(p.displayLabel,
                    style: ts(fontB, sz: 8)),
                pw.Text('${p.orderCount} order${p.orderCount == 1 ? '' : 's'}',
                    style: ts(font, sz: 7, color: PdfColors.grey600)),
              ],
            )),
            pw.Text(egp(p.total),
                style: ts(fontB, sz: 8), textAlign: pw.TextAlign.right),
          ]),
        )),

        pw.SizedBox(height: 2),
        thinDiv(),

        row('Total Collected', egp(report.totalPayments),
            bold: true, boldVal: true, sz: 8),

        pw.SizedBox(height: 1),

        pw.SizedBox(height: 2),
        div(),

        // ── Drawer operations ─────────────────────────────────────────────────
        pw.SizedBox(height: 2),
        pw.Center(child: pw.Text('DRAWER OPERATIONS', style: ts(fontB, sz: 7.5,
            color: PdfColors.grey700))),
        pw.SizedBox(height: 4),

        row('Pay In',  egp(report.cashMovementsIn),  sz: 8),
        row('Pay Out', egp(report.cashMovementsOut), sz: 8),

        if (report.cashMovements.isNotEmpty) ...[
          pw.SizedBox(height: 3),
          thinDiv(),
          pw.SizedBox(height: 2),
          ...report.cashMovements.map((m) {
            final sign   = m.isIn ? '+' : '−';
            final amount = '$sign ${egp(m.amount.abs())}';
            final time   = () {
              final l = m.createdAt.toLocal();
              return '${l.hour.toString().padLeft(2,'0')}:${l.minute.toString().padLeft(2,'0')}';
            }();
            // Two-line: note on top, time + amount on the row
            return pw.Padding(
              padding: const pw.EdgeInsets.only(bottom: 4),
              child: pw.Column(
                crossAxisAlignment: pw.CrossAxisAlignment.start,
                children: [
                  pw.Text(m.note, style: ts(font, sz: 7.5)),
                  pw.Row(children: [
                    pw.Text(time,
                        style: ts(font, sz: 7, color: PdfColors.grey600)),
                    pw.Spacer(),
                    pw.Text(amount,
                        style: ts(m.isIn ? fontB : font, sz: 7.5,
                            color: m.isIn ? PdfColors.green700 : PdfColors.red700),
                        textAlign: pw.TextAlign.right),
                  ]),
                ],
              ),
            );
          }),
        ],

        pw.SizedBox(height: 2),
        div(),

        // ── Cash reconciliation ───────────────────────────────────────────────
        pw.SizedBox(height: 2),
        pw.Center(child: pw.Text('CASH RECONCILIATION', style: ts(fontB, sz: 7.5,
            color: PdfColors.grey700))),
        pw.SizedBox(height: 4),

        row('Opening Cash', egp(report.openingCash), sz: 8),

        if (report.closingCashSystem != null)
          row('Expected in Drawer', egp(report.closingCashSystem!), sz: 8),

        if (report.closingCashDeclared != null)
          row('Actual in Drawer', egp(report.closingCashDeclared!),
              bold: true, sz: 8)
        else
          pw.Center(child: pw.Text('(Shift not yet closed)',
              style: ts(font, sz: 7.5, color: PdfColors.grey600))),

        if (shortage != null) ...[
          pw.SizedBox(height: 2),
          thinDiv(),
          pw.SizedBox(height: 2),
          if (shortage == 0)
            row('Difference', egp(0),
                bold: true, color: PdfColors.green700, sz: 8.5)
          else if (shortage > 0)
            row('Short by', egp(shortage),
                bold: true, color: PdfColors.red700, sz: 8.5)
          else
            row('Over by', egp(shortage.abs()),
                bold: true, color: PdfColors.orange700, sz: 8.5),
        ],

        pw.SizedBox(height: 4),
        div(),

        // ── Footer ────────────────────────────────────────────────────────────
        pw.SizedBox(height: 4),
        pw.Center(child: pw.Text('— End of Report —',
            style: ts(fontB, sz: 8, color: PdfColors.grey600))),
        pw.SizedBox(height: 2),
        div(),
      ]),
    ));
    return pdf.save();
  }
}
