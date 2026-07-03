//! Menu / recipe / modifier unification (Wave 1 foundation).
//!
//! The EXPAND migration `20260703100000_menu_unification_expand.sql` created the new
//! unified tables; this module's [`backfill`] populates them from the legacy tables
//! with STABLE ids (so immutable order-history FKs keep resolving) and an
//! unmigratable-rows report. The compat shim (legacy views + write-translation) is a
//! Wave-2 artifact — see MadarRust/CONTRACT.md.

pub mod backfill;
