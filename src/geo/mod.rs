//! Geo helpers. Currently just the OSRM road-distance client used by the
//! delivery-quote endpoint. Kept in its own module so the OSRM box is reached
//! from exactly one place (backend-only — never exposed to clients).

pub mod osrm;
