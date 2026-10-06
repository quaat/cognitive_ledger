//! Deterministic state diff and structural keys (ADR-0024).
//!
//! `diff(A, B)` is pure set difference over canonical quads: `deletes = A − B`,
//! `adds = B − A`, both as `BTreeSet<Quad>`, i.e. in the bytewise order of the canonical
//! state serialization (`sculpin-rdf-state/v1`). The result never depends on hash-map
//! iteration, storage row order or scheduling.
//!
//! A quad's **structural key** is `(graph, subject, predicate)` in canonical N-Triples
//! terms. The object is deliberately not part of it: two statements about the same slot
//! with different values share a key, which is what three-way merge treats as overlap.

use crate::{Operation, OperationKind, Patch, Quad};
use oxttl::NQuadsParser;
use std::collections::BTreeSet;

/// `(graph, subject, predicate)`; `graph` is `None` for the default graph, otherwise the
/// canonical graph term (`<iri>`). Ordered: default graph first, then named graphs, each by
/// canonical term bytes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StructuralKey {
    pub graph: Option<String>,
    pub subject: String,
    pub predicate: String,
}

impl std::fmt::Display for StructuralKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.graph {
            Some(g) => write!(f, "{} {} {g}", self.subject, self.predicate),
            None => write!(f, "{} {}", self.subject, self.predicate),
        }
    }
}

impl Quad {
    /// The quad's structural key. A `Quad` always holds one canonical N-Quad, so this cannot
    /// fail.
    pub fn structural_key(&self) -> StructuralKey {
        let quad = NQuadsParser::new()
            .for_slice(self.0.as_bytes())
            .next()
            .expect("a Quad holds one canonical N-Quad")
            .expect("a Quad holds one canonical N-Quad");
        StructuralKey {
            graph: (!quad.graph_name.is_default_graph()).then(|| quad.graph_name.to_string()),
            subject: quad.subject.to_string(),
            predicate: quad.predicate.to_string(),
        }
    }
}

/// The difference between two states.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StateDiff {
    /// In the first state, not in the second.
    pub deletes: BTreeSet<Quad>,
    /// In the second state, not in the first.
    pub adds: BTreeSet<Quad>,
}

/// Summary counts of a diff.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiffSummary {
    pub adds: usize,
    pub deletes: usize,
    pub affected_keys: usize,
    pub affected_subjects: usize,
}

impl StateDiff {
    pub fn is_empty(&self) -> bool {
        self.adds.is_empty() && self.deletes.is_empty()
    }

    /// Every structural key touched by an add or a delete, ascending.
    pub fn affected_keys(&self) -> BTreeSet<StructuralKey> {
        self.deletes
            .iter()
            .chain(&self.adds)
            .map(Quad::structural_key)
            .collect()
    }

    /// Every `(graph, subject)` touched, ascending.
    pub fn affected_subjects(&self) -> BTreeSet<(Option<String>, String)> {
        self.affected_keys()
            .into_iter()
            .map(|k| (k.graph, k.subject))
            .collect()
    }

    pub fn summary(&self) -> DiffSummary {
        DiffSummary {
            adds: self.adds.len(),
            deletes: self.deletes.len(),
            affected_keys: self.affected_keys().len(),
            affected_subjects: self.affected_subjects().len(),
        }
    }

    /// The exact patch from the first state to the second (every add absent from it, every
    /// delete present). Adds and deletes are disjoint by construction, so this cannot fail.
    pub fn to_patch(&self) -> Patch {
        Patch::new(
            self.deletes
                .iter()
                .map(|q| Operation {
                    kind: OperationKind::Delete,
                    quad: q.clone(),
                })
                .chain(self.adds.iter().map(|q| Operation {
                    kind: OperationKind::Add,
                    quad: q.clone(),
                })),
        )
        .expect("adds and deletes of a set difference are disjoint")
    }
}

/// `diff(a, b)`: what turns state `a` into state `b`.
pub fn diff(a: &BTreeSet<Quad>, b: &BTreeSet<Quad>) -> StateDiff {
    StateDiff {
        deletes: a.difference(b).cloned().collect(),
        adds: b.difference(a).cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_patch;

    fn q(s: &str) -> Quad {
        s.parse().unwrap()
    }

    fn state(quads: &[&str]) -> BTreeSet<Quad> {
        quads.iter().map(|s| q(s)).collect()
    }

    #[test]
    fn diff_is_set_difference_in_canonical_order() {
        let a = state(&[
            "<urn:s> <urn:p> \"1\" .",
            "<urn:s> <urn:q> \"x\" .",
            "<urn:t> <urn:p> \"keep\" .",
        ]);
        let b = state(&[
            "<urn:s> <urn:p> \"2\" .",
            "<urn:t> <urn:p> \"keep\" .",
            "<urn:u> <urn:p> \"new\" <urn:g> .",
        ]);
        let d = diff(&a, &b);
        assert_eq!(
            d.deletes,
            state(&["<urn:s> <urn:p> \"1\" .", "<urn:s> <urn:q> \"x\" ."])
        );
        assert_eq!(
            d.adds,
            state(&[
                "<urn:s> <urn:p> \"2\" .",
                "<urn:u> <urn:p> \"new\" <urn:g> ."
            ])
        );
        // Applying the patch to A yields exactly B; the reverse diff is the mirror.
        let mut applied = a.clone();
        apply_patch(&mut applied, &d.to_patch());
        assert_eq!(applied, b);
        let r = diff(&b, &a);
        assert_eq!((r.adds, r.deletes), (d.deletes.clone(), d.adds.clone()));
        assert!(diff(&a, &a).is_empty());
        let summary = d.summary();
        assert_eq!(
            (
                summary.adds,
                summary.deletes,
                summary.affected_keys,
                summary.affected_subjects
            ),
            (2, 2, 3, 2)
        );
    }

    #[test]
    fn structural_keys_ignore_objects_and_separate_graphs() {
        let a = q("<urn:s> <urn:p> \"1\" .");
        let b = q("<urn:s> <urn:p> \"2\"@en .");
        let c = q("<urn:s> <urn:p> \"2\"^^<http://www.w3.org/2001/XMLSchema#integer> .");
        let d = q("<urn:s> <urn:p> <urn:o> <urn:g> .");
        assert_eq!(a.structural_key(), b.structural_key());
        assert_eq!(a.structural_key(), c.structural_key());
        assert_ne!(a.structural_key(), d.structural_key());
        assert_eq!(
            d.structural_key(),
            StructuralKey {
                graph: Some("<urn:g>".into()),
                subject: "<urn:s>".into(),
                predicate: "<urn:p>".into()
            }
        );
        // Default graph orders before named graphs.
        assert!(a.structural_key() < d.structural_key());
    }

    #[test]
    fn diff_is_independent_of_insertion_order() {
        let quads: Vec<String> = (0..50)
            .map(|i| format!("<urn:s{}> <urn:p{}> \"{i}\" .", i % 7, i % 3))
            .collect();
        let a: BTreeSet<Quad> = quads.iter().take(30).map(|s| q(s)).collect();
        let b: BTreeSet<Quad> = quads.iter().rev().take(35).map(|s| q(s)).collect();
        let d1 = diff(&a, &b);
        let a2: BTreeSet<Quad> = quads.iter().take(30).rev().map(|s| q(s)).collect();
        let b2: BTreeSet<Quad> = quads.iter().skip(15).map(|s| q(s)).collect();
        assert_eq!(d1, diff(&a2, &b2));
        assert_eq!(
            d1.to_patch().canonical_bytes(),
            diff(&a2, &b2).to_patch().canonical_bytes()
        );
    }
}
