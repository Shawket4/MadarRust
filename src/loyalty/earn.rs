//! What a sale is worth in points.
//!
//! Pure and dependency-free on purpose: this is the one piece of the program a
//! customer will argue about at the counter, and it is exercised from three
//! call sites (live checkout, `/sync/replay` of an offline till, and the
//! dashboard's "what would this earn?" preview). Keeping it a function of plain
//! integers means the same sale is worth the same points down every path.
//!
//! Money is piastres everywhere, as it is throughout the schema. The dashboard
//! renders EGP with the existing `piastresToEgp` / `fmtMoney` helpers.
//!
//! One import, `Uuid`, arrived with per-item stamps: the rule now has to know
//! WHICH item a line sold before it can say whether that line collects. A uuid
//! is a plain 16-byte value with no behaviour of its own, so the module is
//! still a function of its arguments and still testable without a database —
//! which is the property that mattered, rather than the import list.

use uuid::Uuid;

/// The parts of an order the rule may be applied to. All piastres.
#[derive(Debug, Clone, Copy)]
pub struct OrderAmounts {
    pub subtotal: i32,
    pub discount_amount: i32,
    pub tax_amount: i32,
}

/// What a scope collects. One or the other, never both — a card that counted two
/// things at once needs two progress lines and two answers to "how close am I".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Points from money spent, by the rule below.
    Points,
    /// Stamps. One per sale, or one per eligible item sold — see
    /// [`EarnRule::per_line_item`].
    Visits,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Points => "points",
            Mode::Visits => "visits",
        }
    }

    /// Unknown values read as points — the default, and the mode a program
    /// starts in. A settings row can only hold the two the CHECK allows.
    pub fn parse(s: &str) -> Self {
        match s {
            "visits" => Mode::Visits,
            _ => Mode::Points,
        }
    }
}

/// The earn rule, resolved for the branch that made the sale.
#[derive(Debug, Clone, Copy)]
pub struct EarnRule {
    pub mode: Mode,
    /// One point per this many piastres. 1000 = a point per 10 EGP.
    /// Ignored in [`Mode::Visits`].
    pub piastres_per_point: i32,
    /// Earn on what the customer actually paid rather than the list value.
    pub on_discounted: bool,
    /// Add tax to the basis. Off by default — tax is remitted, not revenue.
    pub include_tax: bool,
    /// Count stamps per LINE ITEM rather than per sale. [`Mode::Visits`] only.
    ///
    /// Off, an order of three lattes is one stamp — the original rule, and the
    /// one every programme written before this existed keeps until its owner
    /// flips the switch. On, it is three: a card that says "buy ten coffees"
    /// fills at the rate the customer counts coffees, not the rate they queue.
    ///
    /// Ignored in [`Mode::Points`]: points already scale with the bill, and a
    /// second thing scaling with it would be the same money counted twice.
    pub per_line_item: bool,
}

/// One line of an order, as the stamp rule sees it.
///
/// Deliberately not the order-items row: the rule needs what was sold, how many
/// of it, and how many of those the customer did not pay for. Everything else
/// about a line — price, modifiers, notes — cannot change the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderLine {
    pub menu_item_id: Uuid,
    /// Units sold on this line. A line of quantity three is three stamps.
    pub quantity: i32,
    /// Units of this line a reward paid for.
    ///
    /// A free latte taken with stamps must not hand a stamp back, or a full
    /// card would refill itself off its own reward and the programme would
    /// never settle. Only the covered UNITS are excluded: one free unit of a
    /// line of three leaves two paid ones, which did earn.
    pub redeemed_units: i32,
}

/// The menu items a programme collects stamps for.
///
/// An EMPTY list means every item counts. That is the state every existing
/// programme is in and the state a shop that never opens the picker stays in,
/// so the feature costs nothing to ignore. An empty list is not "nothing
/// earns" — a shop that wants that switches the programme off.
pub fn line_is_eligible(line: &OrderLine, eligible_item_ids: &[Uuid]) -> bool {
    eligible_item_ids.is_empty() || eligible_item_ids.contains(&line.menu_item_id)
}

/// Units of a line that collect a stamp: the ones sold, less the ones a reward
/// paid for, never below zero (a clamp, not an arithmetic claim — a reward can
/// only ever cover units the line holds, and `redeem::plan` enforces that).
pub fn earning_units(line: &OrderLine) -> i32 {
    (line.quantity - line.redeemed_units).max(0)
}

/// Stamps a set of lines collects, per item.
///
/// The arithmetic in one sentence: **add up the units of every eligible line
/// that the customer actually paid for.**
pub fn stamps_for_lines(lines: &[OrderLine], eligible_item_ids: &[Uuid]) -> i32 {
    lines
        .iter()
        .filter(|l| line_is_eligible(l, eligible_item_ids))
        .fold(0i32, |a, l| a.saturating_add(earning_units(l)))
}

/// The piastres the rule applies to, before conversion.
///
/// Tips are absent by construction: they are the staff's money, not a sale, and
/// there is no toggle that lets them earn.
pub fn basis_piastres(a: OrderAmounts, r: EarnRule) -> i32 {
    let mut basis = a.subtotal;
    if r.on_discounted {
        basis -= a.discount_amount;
    }
    if r.include_tax {
        basis += a.tax_amount;
    }
    // A discount larger than the subtotal (a comped order) must not earn
    // negative points, and must not panic the checkout path.
    basis.max(0)
}

/// Points earned by a sale. Rounds DOWN: a customer never earns a point for
/// money they did not spend, and rounding up would let a stream of tiny sales
/// mint points out of nothing.
///
/// Returns 0 when the program is off for the branch, when the sale is too small
/// to reach one point, or on a nonsensical rate (defensive: the column is
/// `CHECK (> 0)`, but a zero here would divide by zero at a till).
/// **Order-level only.** In [`Mode::Visits`] this is the per-SALE answer and
/// ignores `per_line_item`, because it has no lines to count. Every caller that
/// can see the sale's lines should call [`points_for_order`] instead; this stays
/// public because it is still the whole of the points rule, and because it is
/// the answer a per-order programme wants.
pub fn points_for(a: OrderAmounts, r: EarnRule) -> i32 {
    match r.mode {
        // A stamp is a stamp: one per sale, however large. There is deliberately
        // no minimum — a shop that wants one is asking for a different feature,
        // and a silent floor would be a rule customers could not see.
        Mode::Visits => 1,
        Mode::Points => {
            if r.piastres_per_point <= 0 {
                return 0;
            }
            basis_piastres(a, r) / r.piastres_per_point
        }
    }
}

/// What a sale is worth, lines and all. **The entry point every call site
/// should use** — live checkout, `/sync/replay` and the dashboard preview.
///
/// * [`Mode::Points`] — unchanged in every particular. The lines and the
///   eligible list are not consulted at all: the eligible-item picker is a
///   stamps feature, and points already follow the money.
/// * [`Mode::Visits`] with `per_line_item` off — unchanged: one stamp per sale.
/// * [`Mode::Visits`] with `per_line_item` on — the units of every eligible
///   line the customer paid for, added up.
///
/// ### When the lines are not known
/// An empty `lines` in line mode falls back to the per-sale answer of ONE
/// rather than to zero. This is the shape of a sale whose lines the server
/// could not see, and the whole of this module's temperament is that a gap in
/// the system is not the customer's fault: they are standing at a counter
/// holding a card. One stamp is at worst one too many for the shop; zero is a
/// customer told to their face that their coffee did not count.
///
/// The ceiling is NOT applied here. A member's balance cap is state, not a
/// property of the sale, and it is trimmed against the balance inside the
/// award transaction (`model::award_for_order`) — where it stops the accrual
/// without ever failing the sale, exactly as it did before this existed.
pub fn points_for_order(
    a: OrderAmounts,
    lines: &[OrderLine],
    eligible_item_ids: &[Uuid],
    r: EarnRule,
) -> i32 {
    match r.mode {
        Mode::Visits if r.per_line_item => {
            if lines.is_empty() {
                return 1;
            }
            stamps_for_lines(lines, eligible_item_ids)
        }
        _ => points_for(a, r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: EarnRule = EarnRule {
        mode: Mode::Points,
        piastres_per_point: 1000, // a point per 10 EGP
        on_discounted: true,
        include_tax: false,
        per_line_item: false,
    };
    const STAMPS: EarnRule = EarnRule {
        mode: Mode::Visits,
        ..RULE
    };

    fn amounts(subtotal: i32, discount: i32, tax: i32) -> OrderAmounts {
        OrderAmounts {
            subtotal,
            discount_amount: discount,
            tax_amount: tax,
        }
    }

    #[test]
    fn earns_a_point_per_ten_pounds() {
        // 130 EGP subtotal, no discount → 13 points.
        assert_eq!(points_for(amounts(13_000, 0, 1_820), RULE), 13);
    }

    #[test]
    fn rounds_down_never_up() {
        // 19.99 EGP → 1 point, not 2. The 9.99 left over is not a point.
        assert_eq!(points_for(amounts(1_999, 0, 0), RULE), 1);
        // Below the first point earns nothing at all.
        assert_eq!(points_for(amounts(999, 0, 0), RULE), 0);
    }

    #[test]
    fn discount_toggle_changes_the_basis() {
        let a = amounts(10_000, 4_000, 0);
        assert_eq!(points_for(a, RULE), 6); // paid 60 EGP
        let list_price = EarnRule {
            on_discounted: false,
            ..RULE
        };
        assert_eq!(points_for(a, list_price), 10); // menu value, 100 EGP
    }

    #[test]
    fn tax_toggle_changes_the_basis() {
        let a = amounts(10_000, 0, 1_400);
        assert_eq!(points_for(a, RULE), 10);
        let with_tax = EarnRule {
            include_tax: true,
            ..RULE
        };
        assert_eq!(points_for(a, with_tax), 11);
    }

    #[test]
    fn a_comped_order_earns_nothing_and_never_goes_negative() {
        // Discount exceeding the subtotal must not mint negative points.
        assert_eq!(basis_piastres(amounts(5_000, 9_000, 0), RULE), 0);
        assert_eq!(points_for(amounts(5_000, 9_000, 0), RULE), 0);
    }

    #[test]
    fn a_zero_rate_earns_nothing_rather_than_dividing_by_zero() {
        let broken = EarnRule {
            piastres_per_point: 0,
            ..RULE
        };
        assert_eq!(points_for(amounts(10_000, 0, 0), broken), 0);
    }

    #[test]
    fn a_stamp_is_one_per_sale_whatever_the_size() {
        // The whole point of the visits mode: the bill does not matter.
        assert_eq!(points_for(amounts(500, 0, 0), STAMPS), 1);
        assert_eq!(points_for(amounts(500_000, 0, 0), STAMPS), 1);
        // Even a fully comped order is a visit — the customer came in.
        assert_eq!(points_for(amounts(5_000, 9_000, 0), STAMPS), 1);
    }

    #[test]
    fn the_points_rate_is_ignored_in_visits_mode() {
        let odd = EarnRule {
            piastres_per_point: 1,
            ..STAMPS
        };
        assert_eq!(points_for(amounts(13_000, 0, 0), odd), 1);
    }

    // ── Stamps per line item ────────────────────────────────────────────────
    // The arithmetic a customer argues about at the counter. Every case here is
    // one somebody will stand at a till and dispute, so each is spelled out in
    // the units they would say it in.

    const LATTE: Uuid = Uuid::from_u128(1);
    const CROISSANT: Uuid = Uuid::from_u128(2);
    const WATER: Uuid = Uuid::from_u128(3);

    const PER_LINE: EarnRule = EarnRule {
        per_line_item: true,
        ..STAMPS
    };

    fn line(item: Uuid, quantity: i32) -> OrderLine {
        OrderLine {
            menu_item_id: item,
            quantity,
            redeemed_units: 0,
        }
    }

    /// An order big enough that no rounding or minimum could be confused for
    /// the answer — the counts below come from the lines and nothing else.
    const BILL: OrderAmounts = OrderAmounts {
        subtotal: 20_000,
        discount_amount: 0,
        tax_amount: 0,
    };

    #[test]
    fn three_lattes_are_three_stamps_in_line_mode_and_one_in_order_mode() {
        // The example the whole feature was asked for.
        let lines = [line(LATTE, 3)];
        assert_eq!(points_for_order(BILL, &lines, &[], PER_LINE), 3);
        assert_eq!(points_for_order(BILL, &lines, &[], STAMPS), 1);
    }

    #[test]
    fn quantity_multiplies_across_several_lines() {
        // Two lattes and a croissant on one bill: three items, three stamps.
        let lines = [line(LATTE, 2), line(CROISSANT, 1)];
        assert_eq!(points_for_order(BILL, &lines, &[], PER_LINE), 3);
    }

    #[test]
    fn an_empty_eligible_list_means_everything_counts() {
        // The state every existing programme is in. Nothing is filtered out.
        let lines = [line(LATTE, 1), line(WATER, 4)];
        assert_eq!(points_for_order(BILL, &lines, &[], PER_LINE), 5);
    }

    #[test]
    fn only_the_chosen_items_collect() {
        // Coffee earns, the bottled water on the same bill does not.
        let lines = [line(LATTE, 2), line(WATER, 4), line(CROISSANT, 1)];
        let eligible = [LATTE, CROISSANT];
        assert_eq!(points_for_order(BILL, &lines, &eligible, PER_LINE), 3);
    }

    #[test]
    fn a_bill_of_nothing_eligible_earns_no_stamp_at_all() {
        // Not one stamp for turning up: line mode counts items, and this
        // customer bought none that the programme collects.
        let lines = [line(WATER, 6)];
        assert_eq!(points_for_order(BILL, &lines, &[LATTE], PER_LINE), 0);
    }

    #[test]
    fn a_redeemed_line_earns_nothing_back() {
        // The free latte taken with stamps. A card that refilled itself off its
        // own reward would never settle.
        let free = OrderLine {
            menu_item_id: LATTE,
            quantity: 1,
            redeemed_units: 1,
        };
        assert_eq!(points_for_order(BILL, &[free], &[], PER_LINE), 0);
    }

    #[test]
    fn only_the_covered_units_of_a_part_redeemed_line_are_excluded() {
        // Three lattes, one of them free: the two they paid for still earn.
        let partly = OrderLine {
            menu_item_id: LATTE,
            quantity: 3,
            redeemed_units: 1,
        };
        assert_eq!(points_for_order(BILL, &[partly], &[], PER_LINE), 2);
    }

    #[test]
    fn a_line_cannot_earn_negative_units() {
        // Defensive: `redeem::plan` clamps units to the line, so this shape
        // should be unreachable. It must not subtract from the rest of the bill.
        let impossible = OrderLine {
            menu_item_id: LATTE,
            quantity: 1,
            redeemed_units: 5,
        };
        assert_eq!(earning_units(&impossible), 0);
        assert_eq!(
            points_for_order(BILL, &[impossible, line(CROISSANT, 2)], &[], PER_LINE),
            2
        );
    }

    #[test]
    fn an_existing_programme_keeps_one_stamp_per_order() {
        // `per_line_item` off is the old rule in every particular, including
        // the eligible list, which a per-order programme never consults.
        let lines = [line(LATTE, 3), line(WATER, 9)];
        assert_eq!(points_for_order(BILL, &lines, &[], STAMPS), 1);
        assert_eq!(points_for_order(BILL, &lines, &[CROISSANT], STAMPS), 1);
    }

    #[test]
    fn a_sale_whose_lines_are_unknown_still_earns_one() {
        // An old till, or any path that could not see the lines. One stamp is
        // at worst one too many; zero is a customer told their coffee did not
        // count. See `points_for_order`.
        assert_eq!(points_for_order(BILL, &[], &[], PER_LINE), 1);
        assert_eq!(points_for_order(BILL, &[], &[LATTE], PER_LINE), 1);
    }

    #[test]
    fn points_mode_ignores_the_lines_and_the_eligible_list_entirely() {
        // The eligible-item picker is a stamps feature. Points follow the money
        // and are not touched by any of this.
        let lines = [line(LATTE, 7)];
        let per_line_points = EarnRule {
            per_line_item: true,
            ..RULE
        };
        assert_eq!(points_for_order(BILL, &lines, &[], per_line_points), 20);
        assert_eq!(
            points_for_order(BILL, &lines, &[CROISSANT], per_line_points),
            20
        );
        assert_eq!(points_for_order(BILL, &lines, &[], RULE), 20);
    }

    #[test]
    fn a_comped_bill_still_collects_its_stamps_in_line_mode() {
        // The money is irrelevant to stamps — the items left the counter.
        let comped = amounts(5_000, 9_000, 0);
        assert_eq!(points_for_order(comped, &[line(LATTE, 2)], &[], PER_LINE), 2);
    }

    #[test]
    fn tips_can_never_earn() {
        // Tips are not part of `OrderAmounts` at all — the only way a tip could
        // earn is if someone added a field here, which this test exists to make
        // a deliberate act rather than an accident.
        let generous = amounts(10_000, 0, 0);
        assert_eq!(points_for(generous, RULE), 10);
    }
}
