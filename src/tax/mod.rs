//! Tax, service charge, and the policy that decides both.
//!
//! `engine` is the arithmetic and knows nothing about the database; `policy`
//! reads a branch's effective settings. See `engine`'s module docs for why the
//! maths lives in one place and how the till's copy is kept in step.
pub mod engine;
pub mod policy;
pub mod vectors;

pub use engine::{Breakdown, Minor, TaxPolicy, compute};
