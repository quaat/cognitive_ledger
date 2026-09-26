//! Effective-delta semantics (ADR-0008): what a write request *means* against a resolved
//! base state, and which patch is persisted. Pure and deterministic — the result depends
//! only on the supplied base set and the requested patch, so a state materialised from a
//! checkpoint and one reconstructed from full history must yield identical patches
//! (invariant 8; the checkpoint half of that property is asserted in Phase 6).

use crate::{OperationKind, Patch, Quad};
use std::collections::BTreeSet;
use thiserror::Error;

/// Which no-ops a request may contain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeltaPolicy {
    /// Protected/accepted refs: deleting an absent quad is a stale precondition.
    Strict,
    /// Import/unprotected workflows: no-ops are tolerated and dropped.
    Permissive,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DeltaError {
    #[error("BASE_MISMATCH: the request deletes a quad absent from the base state: {0}")]
    BaseMismatch(Quad),
    #[error("NO_EFFECTIVE_CHANGE: every requested operation is a no-op against the base state")]
    NoEffectiveChange,
}

/// Reduce `requested` to the operations that actually change `base`.
///
/// - an `Add` of a present quad is dropped (both policies);
/// - a `Delete` of an absent quad is `BaseMismatch` under `Strict`, dropped under
///   `Permissive`;
/// - an empty result is `NoEffectiveChange` under both policies: a request containing only
///   no-ops never becomes RDF history (the caller's intent lives in the proposal record).
pub fn effective_delta(
    base: &BTreeSet<Quad>,
    requested: &Patch,
    policy: DeltaPolicy,
) -> Result<Patch, DeltaError> {
    let mut kept = Vec::with_capacity(requested.operations().len());
    for op in requested.operations() {
        let present = base.contains(&op.quad);
        match (op.kind, present) {
            (OperationKind::Add, true) => {}
            (OperationKind::Add, false) => kept.push(op.clone()),
            (OperationKind::Delete, true) => kept.push(op.clone()),
            (OperationKind::Delete, false) => match policy {
                DeltaPolicy::Strict => return Err(DeltaError::BaseMismatch(op.quad.clone())),
                DeltaPolicy::Permissive => {}
            },
        }
    }
    if kept.is_empty() {
        return Err(DeltaError::NoEffectiveChange);
    }
    Ok(Patch::new(kept).expect("a subset of a normalized patch is conflict-free"))
}

/// Set-semantic application (invariant 8): adds insert, deletes remove, replay is
/// idempotent. Reconstruction uses this; the effective-delta rule governs what is
/// *persisted*, never how a stored patch replays.
pub fn apply_patch(state: &mut BTreeSet<Quad>, patch: &Patch) {
    for op in patch.operations() {
        match op.kind {
            OperationKind::Add => {
                state.insert(op.quad.clone());
            }
            OperationKind::Delete => {
                state.remove(&op.quad);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Operation;

    fn q(n: u32) -> Quad {
        format!("<urn:s:{n}> <urn:p> \"{n}\" .").parse().unwrap()
    }
    fn add(n: u32) -> Operation {
        Operation {
            kind: OperationKind::Add,
            quad: q(n),
        }
    }
    fn del(n: u32) -> Operation {
        Operation {
            kind: OperationKind::Delete,
            quad: q(n),
        }
    }
    fn base(ns: &[u32]) -> BTreeSet<Quad> {
        ns.iter().map(|n| q(*n)).collect()
    }

    /// Deterministic generator (LCG) so failures print a replayable seed.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A random base and a random conflict-free request over a small quad universe.
    fn scenario(rng: &mut Rng) -> (BTreeSet<Quad>, Patch) {
        let base: BTreeSet<Quad> = (0..12u32).filter(|_| rng.below(2) == 1).map(q).collect();
        let mut ops = Vec::new();
        for n in 0..12u32 {
            match rng.below(3) {
                0 => {}
                1 => ops.push(add(n)),
                _ => ops.push(del(n)),
            }
        }
        (base, Patch::new(ops).unwrap())
    }

    #[test]
    fn add_present_collapses_and_delete_present_is_kept() {
        let effective = effective_delta(
            &base(&[1, 2]),
            &Patch::new([add(1), add(3), del(2)]).unwrap(),
            DeltaPolicy::Strict,
        )
        .unwrap();
        assert_eq!(effective, Patch::new([add(3), del(2)]).unwrap());
    }

    #[test]
    fn delete_absent_is_a_base_mismatch_under_strict_and_dropped_under_permissive() {
        let request = Patch::new([add(3), del(9)]).unwrap();
        assert_eq!(
            effective_delta(&base(&[1]), &request, DeltaPolicy::Strict),
            Err(DeltaError::BaseMismatch(q(9)))
        );
        assert_eq!(
            effective_delta(&base(&[1]), &request, DeltaPolicy::Permissive).unwrap(),
            Patch::new([add(3)]).unwrap()
        );
    }

    #[test]
    fn a_request_of_only_no_ops_is_rejected_under_both_policies() {
        let request = Patch::new([add(1), add(2)]).unwrap();
        for policy in [DeltaPolicy::Strict, DeltaPolicy::Permissive] {
            assert_eq!(
                effective_delta(&base(&[1, 2]), &request, policy),
                Err(DeltaError::NoEffectiveChange)
            );
        }
        let only_absent_deletes = Patch::new([del(7)]).unwrap();
        assert_eq!(
            effective_delta(&base(&[1]), &only_absent_deletes, DeltaPolicy::Permissive),
            Err(DeltaError::NoEffectiveChange)
        );
        let empty = Patch::new([]).unwrap();
        assert_eq!(
            effective_delta(&base(&[1]), &empty, DeltaPolicy::Strict),
            Err(DeltaError::NoEffectiveChange)
        );
    }

    #[test]
    fn properties_hold_over_seeded_random_scenarios() {
        let seed = 0x5ca1_ab1e_2026_0926u64;
        let mut rng = Rng(seed);
        let mut strict_ok = 0;
        for i in 0..2_000 {
            let (base, request) = scenario(&mut rng);
            let context = format!("seed={seed:#x} iteration={i} base={base:?} request={request:?}");
            // Permissive always reduces to the same resulting state as set-semantic replay.
            let mut via_request = base.clone();
            apply_patch(&mut via_request, &request);
            match effective_delta(&base, &request, DeltaPolicy::Permissive) {
                Ok(effective) => {
                    let mut via_effective = base.clone();
                    apply_patch(&mut via_effective, &effective);
                    assert_eq!(via_effective, via_request, "{context}");
                    // Every kept operation changes the base; nothing kept is a no-op.
                    for op in effective.operations() {
                        let present = base.contains(&op.quad);
                        assert_eq!(present, op.kind == OperationKind::Delete, "{context}");
                    }
                    // Idempotence: reducing the effective patch again changes nothing.
                    assert_eq!(
                        effective_delta(&base, &effective, DeltaPolicy::Strict).unwrap(),
                        effective,
                        "{context}"
                    );
                    // The effective patch never has more operations than the request.
                    assert!(effective.operations().len() <= request.operations().len());
                }
                Err(DeltaError::NoEffectiveChange) => {
                    assert_eq!(via_request, base, "{context}");
                }
                Err(other) => panic!("permissive never reports {other:?}: {context}"),
            }
            // Strict agrees with permissive exactly when no absent quad is deleted.
            let deletes_absent = request
                .operations()
                .iter()
                .any(|op| op.kind == OperationKind::Delete && !base.contains(&op.quad));
            match effective_delta(&base, &request, DeltaPolicy::Strict) {
                Ok(effective) => {
                    strict_ok += 1;
                    assert!(!deletes_absent, "{context}");
                    assert_eq!(
                        effective,
                        effective_delta(&base, &request, DeltaPolicy::Permissive).unwrap(),
                        "{context}"
                    );
                }
                Err(DeltaError::BaseMismatch(quad)) => {
                    assert!(deletes_absent, "{context}");
                    assert!(!base.contains(&quad), "{context}");
                }
                Err(DeltaError::NoEffectiveChange) => assert!(!deletes_absent, "{context}"),
            }
        }
        assert!(strict_ok > 0, "seed {seed:#x} produced no strict successes");
    }

    #[test]
    fn identity_depends_only_on_base_content_not_on_how_the_base_was_built() {
        // The same set reached by different insertion orders and by replaying history
        // (adds then deletes) yields byte-identical effective patches and ids.
        let request = Patch::new([add(1), add(5), del(2)]).unwrap();
        let direct = base(&[1, 2, 3]);
        let mut reversed = BTreeSet::new();
        for n in [3, 2, 1] {
            reversed.insert(q(n));
        }
        let mut replayed = BTreeSet::new();
        apply_patch(
            &mut replayed,
            &Patch::new([add(1), add(2), add(3), add(9)]).unwrap(),
        );
        apply_patch(&mut replayed, &Patch::new([del(9)]).unwrap());
        let expected = effective_delta(&direct, &request, DeltaPolicy::Strict).unwrap();
        for other in [&reversed, &replayed] {
            let effective = effective_delta(other, &request, DeltaPolicy::Strict).unwrap();
            assert_eq!(effective.canonical_bytes(), expected.canonical_bytes());
            assert_eq!(effective.id(), expected.id());
        }
    }
}
