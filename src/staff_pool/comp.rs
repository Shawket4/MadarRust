//! What a staff drink is given for free, and what it still pays for.
//!
//! Owner's rule (2026-09-21). A staff drink is a NORMAL sale whose pooled line
//! is comped on its BASE CONFIGURATION only:
//!
//! 1. **Size** — the free amount is the price of the CHEAPEST ACTIVE size
//!    (branch price applied). A larger size pays the difference. An item with
//!    no sizes: its price is the free amount.
//! 2. **Optional add-ons / optionals** — never free.
//! 3. **Required choice groups** (`required_min >= 1`) — per group the free
//!    allowance is the DEFAULT option's price × `required_min`; with no active
//!    default, the CHEAPEST active option × `required_min`. The picks made in
//!    that group pay only what their total exceeds the allowance by. Never
//!    negative per group: a cheaper pick earns no credit anywhere else.
//! 4. Per unit: `comp = size_comp + Σ group comps`, each part capped by what
//!    that part actually rang at, so `charged = normal − comp >= 0`. A line of
//!    `n` units is `n` times that.
//!
//! This is a PURE function: no I/O, integers only (piastres), no rounding
//! anywhere — every figure is a sum, a product by a count, or a `min`.
//!
//! It is the SECOND copy of a rule the till runs offline. The two are pinned
//! together by `tests/fixtures/staff_comp_vectors.json`, committed VERBATIM to
//! both repos and executed by a test on each side, exactly as
//! `staff_pool_vectors.json` pins the pool decision and `tax_vectors.json` the
//! bill engine. **Change the rule in the fixture first.**
//!
//! ## What a "price" is here
//!
//! Every price handed to this function is the price as it RINGS ON THE LINE:
//! a size at its branch price, an option at its branch price. A swap group
//! (milk, beans) is priced by the line resolver as a DIFFERENCE over the
//! recipe's own ingredient — the base choice already rings at zero — so the
//! caller does not pass swap groups as required groups at all: their picks
//! arrive as ordinary extras that already are "the difference".

use serde::{Deserialize, Serialize};

fn yes() -> bool {
    true
}

fn one() -> i32 {
    1
}

/// One size of the item, as the catalogue prices it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CompSize {
    pub label: String,
    pub price: i32,
    #[serde(default = "yes")]
    pub is_active: bool,
    /// The branch's own price for this size, when it has one. Wins over `price`.
    #[serde(default)]
    pub branch_price: Option<i32>,
}

/// One option of a choice group.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CompOption {
    pub id: String,
    pub price: i32,
    #[serde(default)]
    pub branch_price: Option<i32>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default = "yes")]
    pub is_active: bool,
}

/// A choice group attached to the item. `required_min == 0` is an optional
/// group: it grants nothing and its picks are plain extras.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CompGroup {
    pub id: String,
    pub required_min: i32,
    pub options: Vec<CompOption>,
}

/// One pick on the line. `unit_price` is what ONE of it rang at; an option id
/// found in no required group is an optional extra.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CompPick {
    pub option_id: String,
    pub unit_price: i32,
    #[serde(default = "one")]
    pub quantity: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CompInput {
    /// The pool decision allowed this line. `false` comps nothing.
    #[serde(default = "yes")]
    pub eligible: bool,
    /// What one unit of the CHOSEN size rings at (the item's price when it has
    /// no sizes), before any pick.
    pub unit_price: i32,
    /// Every size of the item. EMPTY = the item has no sizes (or the line named
    /// none): `unit_price` itself is the free amount.
    #[serde(default)]
    pub sizes: Vec<CompSize>,
    #[serde(default)]
    pub groups: Vec<CompGroup>,
    #[serde(default)]
    pub picks: Vec<CompPick>,
    /// Priced optional fields per unit. Never free.
    #[serde(default)]
    pub optionals_per_unit: i32,
    #[serde(default = "one")]
    pub quantity: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GroupComp {
    pub group_id: String,
    /// Default (else cheapest active) option × `required_min`.
    pub allowance: i32,
    /// What the picks made in this group rang at, per unit.
    pub picked: i32,
    /// `min(picked, allowance)`.
    pub comp: i32,
}

/// How much of one pick the comp absorbed, per unit of the line. Allocated in
/// pick order within its group, so a receipt can show it beside the pick.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PickComp {
    pub option_id: String,
    pub comp: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CompBreakdown {
    /// One unit at normal prices: size + every pick + optionals.
    pub normal_per_unit: i32,
    /// The cheapest active size (or `unit_price` with no sizes).
    pub free_size: i32,
    /// `min(free_size, unit_price)` — what the size part actually gave.
    pub size_comp: i32,
    pub groups: Vec<GroupComp>,
    /// One entry per input pick, in input order.
    pub picks: Vec<PickComp>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CompResult {
    pub free_per_unit: i32,
    pub charged_per_unit: i32,
    /// `free_per_unit × quantity` — `order_items.staff_comp_minor`.
    pub line_comp: i32,
    /// `charged_per_unit × quantity` — `staff_drinks.extras_minor`.
    pub line_charged: i32,
    pub breakdown: CompBreakdown,
}

fn nn(v: i32) -> i32 {
    v.max(0)
}

fn size_price(s: &CompSize) -> i32 {
    nn(s.branch_price.unwrap_or(s.price))
}

fn option_price(o: &CompOption) -> i32 {
    nn(o.branch_price.unwrap_or(o.price))
}

/// The group's free allowance per unit.
pub fn group_allowance(g: &CompGroup) -> i32 {
    if g.required_min < 1 {
        return 0;
    }
    let active = || g.options.iter().filter(|o| o.is_active);
    let per_pick = active()
        .find(|o| o.is_default)
        .map(option_price)
        .or_else(|| active().map(option_price).min())
        .unwrap_or(0);
    per_pick.saturating_mul(g.required_min)
}

/// THE rule. See the module note.
pub fn comp(input: &CompInput) -> CompResult {
    let quantity = nn(input.quantity);
    let unit_price = nn(input.unit_price);
    let picks_total: i32 = input
        .picks
        .iter()
        .map(|p| nn(p.unit_price).saturating_mul(nn(p.quantity)))
        .sum();
    let normal_per_unit = unit_price + picks_total + nn(input.optionals_per_unit);

    let free_size = input
        .sizes
        .iter()
        .filter(|s| s.is_active)
        .map(size_price)
        .min()
        .unwrap_or(unit_price);

    let mut pick_comps: Vec<PickComp> = input
        .picks
        .iter()
        .map(|p| PickComp {
            option_id: p.option_id.clone(),
            comp: 0,
        })
        .collect();
    let mut groups = Vec::new();
    let mut size_comp = 0;
    // A pick belongs to the FIRST required group that lists its option, so an
    // option shared by two groups is never comped twice.
    let mut taken = vec![false; input.picks.len()];

    if input.eligible {
        size_comp = free_size.min(unit_price);
        for g in input.groups.iter().filter(|g| g.required_min >= 1) {
            let allowance = group_allowance(g);
            let mut picked = 0;
            let mut left = allowance;
            for (i, p) in input.picks.iter().enumerate() {
                if taken[i] || !g.options.iter().any(|o| o.id == p.option_id) {
                    continue;
                }
                taken[i] = true;
                let rang = nn(p.unit_price).saturating_mul(nn(p.quantity));
                picked += rang;
                let absorbed = rang.min(left);
                pick_comps[i].comp += absorbed;
                left -= absorbed;
            }
            groups.push(GroupComp {
                group_id: g.id.clone(),
                allowance,
                picked,
                comp: allowance - left,
            });
        }
    }

    let free_per_unit =
        (size_comp + groups.iter().map(|g| g.comp).sum::<i32>()).min(normal_per_unit);
    let charged_per_unit = normal_per_unit - free_per_unit;
    CompResult {
        free_per_unit,
        charged_per_unit,
        line_comp: free_per_unit.saturating_mul(quantity),
        line_charged: charged_per_unit.saturating_mul(quantity),
        breakdown: CompBreakdown {
            normal_per_unit,
            free_size,
            size_comp,
            groups,
            picks: pick_comps,
        },
    }
}
