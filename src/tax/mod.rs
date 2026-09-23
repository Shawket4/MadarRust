//! Tax, service charge, and the policy that decides both.
//!
//! `engine` is the arithmetic and knows nothing about the database; `policy`
//! reads a branch's effective settings. The maths itself is `madar_money::tax`
//! (madar-shared), which the till runs too; `engine` re-exports it.
pub mod engine;
pub mod policy;

pub use engine::{
    Breakdown, Discount, Minor, NegativePart, SaleChannel, TaxPolicy, compute, discount_amount,
    negative_part, refund_split,
};
