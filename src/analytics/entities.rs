//! What the *values* in a result actually are.
//!
//! A dimension produces strings. `"Marina"` might be a branch; `"Ahmed Hassan"`
//! might be a waiter, a teller, or an employee on an attendance sheet. Until now
//! nothing in the system said which — there was a hand-maintained list of
//! "dimensions that name a person" and a fuzzy matcher that only understood
//! branches. That is enough to pseudonymise, and not enough to *resolve*: when a
//! merchant asks "how did Ahmed do?", something has to know whether Ahmed is a
//! waiter or a till operator, and look in the right place.
//!
//! This module is that missing layer. Each [`EntityKind`] declares:
//!
//!   * which dimensions produce values of that kind,
//!   * whether those values name a **person** (and so must be pseudonymised
//!     before any of it reaches a language model),
//!   * and the SQL that lists the real entities, so a name can be resolved to
//!     one of them rather than pattern-matched.
//!
//! # Adding a kind
//!
//! One entry in [`ENTITY_KINDS`]. Nothing else changes: the pseudonymiser, the
//! personal-dimension rule, the resolver and the model's prompt all read this
//! list. The tests below refuse a kind that names a dimension which does not
//! exist, and refuse a person-valued dimension that no kind claims — so a new
//! dimension cannot quietly start leaking names, and a new kind cannot quietly
//! reference nothing.
//!
//! # Why people share one code namespace
//!
//! `waiter`, `cashier`, `teller` and `employee` are four *roles*, but one human
//! can appear under several — the same person takes orders and clocks in. The
//! pseudonym directory therefore keys on the person, not the role, so a merchant
//! reading "E-3 took the most orders and was late twice" is reading about one
//! person rather than two coincidentally-numbered ones. The kind is what decides
//! *where to look them up*, not what to call them.

use crate::db::Db;
use crate::errors::AppError;

/// A class of thing a dimension value can name.
#[derive(Debug)]
pub struct EntityKind {
    pub id: &'static str,
    /// Shown to the merchant and to the model.
    pub label: &'static str,
    /// One line the model reads when deciding what a name refers to.
    pub help: &'static str,
    /// True when a value of this kind is a person's name. Drives
    /// pseudonymisation; see `ai::pseudonym`.
    pub personal: bool,
    /// Dimension ids (across all datasets) whose values are of this kind.
    pub dimensions: &'static [&'static str],
    /// SQL listing the real entities for the caller's organization as
    /// `(id, name)`. Runs on the RLS-scoped tenant pool, so it needs no
    /// `org_id` filter and cannot see another merchant's rows.
    ///
    /// Author-written like every other fragment in the analytics layer — no
    /// caller input reaches it.
    pub list_sql: &'static str,
}

impl EntityKind {
    /// True when `dimension` produces values of this kind.
    pub fn owns(&self, dimension: &str) -> bool {
        self.dimensions.contains(&dimension)
    }
}

/// Every kind of entity a dimension can name.
///
/// Ordered people-first only for readability; nothing depends on the order.
pub const ENTITY_KINDS: &[EntityKind] = &[
    EntityKind {
        id: "waiter",
        label: "Waiter",
        help: "A member of floor staff, as recorded on the order they took.",
        personal: true,
        dimensions: &["waiter"],
        // Restricted to the role, so "how did Ahmed do as a waiter?" cannot
        // resolve to a kitchen user who happens to share the name.
        list_sql: "SELECT id, name FROM users \
                   WHERE role = 'waiter' AND deleted_at IS NULL AND name <> '' \
                   ORDER BY id",
    },
    EntityKind {
        id: "cashier",
        label: "Cashier",
        help: "Whoever rang an order up on the till. The same people as \
               'teller' — the two dimensions differ only in which table they \
               are read from (orders versus shifts).",
        personal: true,
        // `cashier` on orders and `teller` on shifts are the SAME humans; both
        // resolve here so a name given for one finds the other.
        dimensions: &["cashier", "teller"],
        list_sql: "SELECT id, name FROM users \
                   WHERE role = 'teller' AND deleted_at IS NULL AND name <> '' \
                   ORDER BY id",
    },
    EntityKind {
        id: "employee",
        label: "Employee",
        help: "Anyone on the staff roster, in an attendance or payroll context. \
               Broader than waiter or cashier: it includes managers and kitchen \
               staff, and is the right kind for lateness, overtime and absence.",
        personal: true,
        dimensions: &["employee"],
        // Any role — attendance covers the whole roster, not one job.
        list_sql: "SELECT id, name FROM users \
                   WHERE deleted_at IS NULL AND name <> '' ORDER BY id",
    },
    EntityKind {
        id: "branch",
        label: "Branch",
        help: "A physical location. Not a person — a branch name is business \
               information and is sent to the model as-is.",
        personal: false,
        dimensions: &["branch"],
        list_sql: "SELECT id, name FROM branches \
                   WHERE deleted_at IS NULL ORDER BY name",
    },
    EntityKind {
        id: "product",
        label: "Product",
        help: "A menu item. Matched against the name printed on the order line, \
               which is a snapshot — a later rename does not rewrite history.",
        personal: false,
        dimensions: &["product"],
        list_sql: "SELECT id, name FROM menu_items \
                   WHERE deleted_at IS NULL ORDER BY name",
    },
    EntityKind {
        id: "category",
        label: "Category",
        help: "A menu category — the grouping a product sits in, such as \
               drinks or pastries. Not a product itself.",
        personal: false,
        dimensions: &["category"],
        list_sql: "SELECT id, name FROM categories \
                   WHERE deleted_at IS NULL ORDER BY name",
    },
    EntityKind {
        id: "ingredient",
        label: "Ingredient",
        help: "A stock item. Quantities are in each ingredient's own unit, so \
               only compare within one.",
        personal: false,
        dimensions: &["ingredient"],
        list_sql: "SELECT id, name FROM org_ingredients \
                   WHERE deleted_at IS NULL ORDER BY name",
    },
    EntityKind {
        id: "supplier",
        label: "Supplier",
        help: "A company goods are bought from. A business name, not a person's \
               — deliberately not pseudonymised, because a merchant asking who \
               they spend the most with expects to be told.",
        personal: false,
        dimensions: &["supplier"],
        list_sql: "SELECT id, name FROM suppliers \
                   WHERE deleted_at IS NULL ORDER BY name",
    },
    EntityKind {
        id: "department",
        label: "Department",
        help: "A grouping of staff. A department is not a person.",
        personal: false,
        dimensions: &["department"],
        list_sql: "SELECT id, name FROM departments ORDER BY name",
    },
];

/// Look a kind up by id.
pub fn kind(id: &str) -> Option<&'static EntityKind> {
    ENTITY_KINDS.iter().find(|k| k.id == id)
}

/// The kind a dimension produces, if any.
pub fn kind_of_dimension(dimension: &str) -> Option<&'static EntityKind> {
    ENTITY_KINDS.iter().find(|k| k.owns(dimension))
}

/// True when a dimension's values name a person.
///
/// This is the single source of truth for pseudonymisation. `schema` used to
/// carry a parallel hand-written list; it now defers here so the two cannot
/// disagree.
pub fn is_personal_dimension(dimension: &str) -> bool {
    kind_of_dimension(dimension).is_some_and(|k| k.personal)
}

/// Every kind whose values name a person.
pub fn personal_kinds() -> impl Iterator<Item = &'static EntityKind> {
    ENTITY_KINDS.iter().filter(|k| k.personal)
}

/// One real entity.
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub id: uuid::Uuid,
    pub name: String,
    /// Which kind it was found under.
    pub kind: &'static str,
}

/// List the real entities of a kind for this organization.
pub async fn list(db: &Db, kind: &'static EntityKind) -> Result<Vec<Entity>, AppError> {
    let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(kind.list_sql)
        .fetch_all(db.get_ref())
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| Entity {
            id,
            name,
            kind: kind.id,
        })
        .collect())
}

/// Every person in the organization, across all personal kinds, deduplicated by
/// user id.
///
/// This is what the pseudonym directory is built from. Deduplication is the
/// point: one human who is both a waiter and an attendance record must get ONE
/// code, or an answer that mentions them twice reads as two people.
pub async fn list_people(db: &Db) -> Result<Vec<Entity>, AppError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for kind in personal_kinds() {
        for entity in list(db, kind).await? {
            if seen.insert(entity.id) {
                out.push(entity);
            }
        }
    }
    // Stable order — the pseudonym codes are assigned from this, and a code
    // that moved between requests would break follow-ups.
    out.sort_by_key(|e| e.id);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::schema::{self, DATASETS, PERSON_JOINS};
    use std::collections::HashSet;

    #[test]
    fn kind_ids_and_dimension_claims_are_unique() {
        let mut ids = HashSet::new();
        let mut dims = HashSet::new();
        for k in ENTITY_KINDS {
            assert!(ids.insert(k.id), "duplicate kind id {}", k.id);
            for d in k.dimensions {
                assert!(
                    dims.insert(*d),
                    "dimension '{d}' is claimed by two kinds — resolution would be ambiguous"
                );
            }
        }
    }

    #[test]
    fn every_claimed_dimension_exists_in_the_registry() {
        // A kind naming a dimension that does not exist resolves nothing and
        // would fail silently at runtime.
        for k in ENTITY_KINDS {
            for d in k.dimensions {
                assert!(
                    DATASETS.iter().any(|ds| ds.dims.iter().any(|x| x.id == *d)),
                    "kind '{}' claims dimension '{d}', which no dataset has",
                    k.id
                );
            }
        }
    }

    /// The guard that matters: a new person-valued dimension cannot be added
    /// without a kind claiming it, because an unclaimed one would not be
    /// pseudonymised and staff names would reach the model.
    #[test]
    fn every_person_valued_dimension_belongs_to_a_personal_kind() {
        for ds in DATASETS {
            for dim in ds.dims {
                if dim.joins.iter().any(|j| PERSON_JOINS.contains(j)) {
                    let k = kind_of_dimension(dim.id).unwrap_or_else(|| {
                        panic!(
                            "{}/{} names a person but no EntityKind claims it — \
                             it would reach the model unpseudonymised",
                            ds.id, dim.id
                        )
                    });
                    assert!(
                        k.personal,
                        "{}/{} names a person but kind '{}' is not marked personal",
                        ds.id, dim.id, k.id
                    );
                }
            }
        }
    }

    #[test]
    fn schema_and_entities_agree_on_what_is_personal() {
        // `schema::is_personal_dimension` delegates here; this pins that they
        // cannot drift back apart.
        for ds in DATASETS {
            for dim in ds.dims {
                assert_eq!(
                    schema::is_personal_dimension(dim.id),
                    is_personal_dimension(dim.id),
                    "{}/{} disagrees between schema and entities",
                    ds.id,
                    dim.id
                );
            }
        }
    }

    #[test]
    fn business_kinds_are_not_personal() {
        // Over-marking is its own failure: the model cannot reason without
        // product and branch names.
        for id in [
            "branch",
            "product",
            "category",
            "ingredient",
            "supplier",
            "department",
        ] {
            assert!(!kind(id).expect(id).personal, "{id} must not be personal");
        }
    }

    #[test]
    fn people_kinds_cover_the_roles_that_actually_appear() {
        for id in ["waiter", "cashier", "employee"] {
            assert!(kind(id).expect(id).personal, "{id} must be personal");
        }
        // `teller` is a dimension of the cashier kind, not a kind of its own —
        // they are the same humans read from different tables.
        assert_eq!(kind_of_dimension("teller").map(|k| k.id), Some("cashier"));
        assert_eq!(kind_of_dimension("cashier").map(|k| k.id), Some("cashier"));
    }

    #[test]
    fn every_kind_documents_itself_and_has_a_query() {
        for k in ENTITY_KINDS {
            assert!(k.help.len() > 30, "{}: help too thin", k.id);
            assert!(
                k.list_sql.contains("SELECT id, name"),
                "{}: list_sql must yield (id, name)",
                k.id
            );
            // RLS scopes the org; an explicit filter would be a second,
            // divergent source of truth.
            assert!(
                !k.list_sql.contains("org_id"),
                "{}: list_sql must rely on RLS, not an org_id filter",
                k.id
            );
        }
    }
}
