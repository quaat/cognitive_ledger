use super::*;
use ledger_core::ContentId;
use ledger_rdf::apply_patch;

fn q(s: &str) -> Quad {
    s.parse().unwrap()
}

fn st(quads: &[&str]) -> BTreeSet<Quad> {
    quads.iter().map(|s| q(s)).collect()
}

const ALL: [Strategy; 4] = [
    Strategy::Abort,
    Strategy::TakeTarget,
    Strategy::TakeSource,
    Strategy::Union,
];

#[test]
fn one_sided_and_independent_changes_merge_without_conflict() {
    let base = st(&["<urn:a> <urn:p> \"1\" .", "<urn:b> <urn:p> \"1\" ."]);
    let target = st(&["<urn:a> <urn:p> \"2\" .", "<urn:b> <urn:p> \"1\" ."]);
    let source = st(&[
        "<urn:a> <urn:p> \"1\" .",
        "<urn:b> <urn:p> \"1\" .",
        "<urn:c> <urn:p> \"new\" .",
    ]);
    for strategy in ALL {
        let r = three_way(&base, &target, &source, strategy);
        assert_eq!(r.conflict_count, 0);
        assert_eq!(
            r.merged.unwrap(),
            st(&[
                "<urn:a> <urn:p> \"2\" .",
                "<urn:b> <urn:p> \"1\" .",
                "<urn:c> <urn:p> \"new\" ."
            ])
        );
    }
}

#[test]
fn same_slot_replacement_and_delete_vs_modify_conflict() {
    let base = st(&["<urn:s> <urn:p> \"X\" .", "<urn:t> <urn:p> \"X\" ."]);
    // Slot s: target X->Y, source X->Z. Slot t: target deletes, source modifies.
    let target = st(&["<urn:s> <urn:p> \"Y\" ."]);
    let source = st(&["<urn:s> <urn:p> \"Z\" .", "<urn:t> <urn:p> \"W\" ."]);
    let abort = three_way(&base, &target, &source, Strategy::Abort);
    assert_eq!(abort.merged, None);
    assert_eq!(abort.conflict_count, 2);
    assert_eq!(
        abort
            .conflicts
            .iter()
            .map(|c| c.key.subject.as_str())
            .collect::<Vec<_>>(),
        ["<urn:s>", "<urn:t>"]
    );
    assert_eq!(
        abort.conflicts[0].base.quads,
        vec![q("<urn:s> <urn:p> \"X\" .")]
    );
    assert_eq!(abort.conflicts[1].target.quads, vec![]);
    assert_eq!(
        three_way(&base, &target, &source, Strategy::TakeTarget).merged,
        Some(target.clone())
    );
    assert_eq!(
        three_way(&base, &target, &source, Strategy::TakeSource).merged,
        Some(source.clone())
    );
    assert_eq!(
        three_way(&base, &target, &source, Strategy::Union).merged,
        Some(st(&[
            "<urn:s> <urn:p> \"Y\" .",
            "<urn:s> <urn:p> \"Z\" .",
            "<urn:t> <urn:p> \"W\" ."
        ]))
    );
}

#[test]
fn convergent_changes_are_not_conflicts_and_named_graphs_are_separate_slots() {
    let base = st(&["<urn:s> <urn:p> \"X\" ."]);
    let both = st(&["<urn:s> <urn:p> \"Y\" ."]);
    let r = three_way(&base, &both, &both, Strategy::Abort);
    assert_eq!((r.conflict_count, r.merged), (0, Some(both)));
    // The same subject/predicate in another graph is another slot.
    let target = st(&["<urn:s> <urn:p> \"Y\" ."]);
    let source = st(&["<urn:s> <urn:p> \"X\" .", "<urn:s> <urn:p> \"Z\" <urn:g> ."]);
    let r = three_way(&base, &target, &source, Strategy::Abort);
    assert_eq!(r.conflict_count, 0);
    assert_eq!(
        r.merged.unwrap(),
        st(&["<urn:s> <urn:p> \"Y\" .", "<urn:s> <urn:p> \"Z\" <urn:g> ."])
    );
}

#[test]
fn fast_forward_is_the_source_and_no_change_is_detected() {
    let base = st(&["<urn:s> <urn:p> \"X\" ."]);
    let source = st(&["<urn:s> <urn:p> \"Y\" .", "<urn:u> <urn:p> \"1\" ."]);
    for strategy in ALL {
        // Fast-forward: base = target.
        assert_eq!(
            three_way(&base, &base, &source, strategy).merged,
            Some(source.clone())
        );
    }
    assert!(is_no_change(&source, &source));
    assert!(!is_no_change(&base, &source));
}

#[test]
fn conflict_report_is_bounded() {
    let mut base = BTreeSet::new();
    let mut target = BTreeSet::new();
    let mut source = BTreeSet::new();
    for i in 0..(MAX_REPORTED_CONFLICTS + 5) {
        base.insert(q(&format!("<urn:s{i}> <urn:p> \"b\" .")));
        target.insert(q(&format!("<urn:s{i}> <urn:p> \"t\" .")));
        source.insert(q(&format!("<urn:s{i}> <urn:p> \"s\" .")));
    }
    for j in 0..(MAX_REPORTED_QUADS_PER_SIDE + 3) {
        target.insert(q(&format!("<urn:s0> <urn:p> \"t{j}\" .")));
    }
    let r = three_way(&base, &target, &source, Strategy::Abort);
    assert_eq!(r.conflict_count, MAX_REPORTED_CONFLICTS + 5);
    assert_eq!(r.conflicts.len(), MAX_REPORTED_CONFLICTS);
    let first = &r.conflicts[0];
    assert_eq!(first.target.quads.len(), MAX_REPORTED_QUADS_PER_SIDE);
    assert!(first.target.truncated && !first.source.truncated);
}

// ---- property tests against an independent set-formula oracle (deterministic seeds) ----

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A random state over a small universe (3 subjects × 2 predicates × 3 objects × 2 graphs),
/// so slots collide often.
fn random_state(rng: &mut Rng) -> BTreeSet<Quad> {
    random_set(rng, 4)
}

/// Each universe quad with probability `1/one_in`.
fn random_set(rng: &mut Rng, one_in: u64) -> BTreeSet<Quad> {
    let mut s = BTreeSet::new();
    for subj in 0..3 {
        for pred in 0..2 {
            for obj in 0..3 {
                for graph in 0..2 {
                    if rng.below(one_in) == 0 {
                        let g = if graph == 0 { "" } else { " <urn:g>" };
                        s.insert(q(&format!("<urn:s{subj}> <urn:p{pred}> \"{obj}\"{g} .")));
                    }
                }
            }
        }
    }
    s
}

/// `X|k` for every key of `B ∪ T ∪ S`, computed independently of the implementation.
fn by_key(x: &BTreeSet<Quad>, k: &StructuralKey) -> BTreeSet<Quad> {
    x.iter()
        .filter(|q| &q.structural_key() == k)
        .cloned()
        .collect()
}

#[test]
fn generated_merges_match_the_set_formula_and_strategy_laws() {
    let (mut with_conflicts, mut clean) = (0, 0);
    for seed in 1..=600u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let base = random_state(&mut rng);
        // Derive target and source from base by random edits so slots overlap.
        let mut target = base.clone();
        let mut source = base.clone();
        for side in [&mut target, &mut source] {
            // Sparse edits (sometimes none) so clean and conflicting merges both occur.
            let density = [u64::MAX, 30, 15, 8][rng.below(4) as usize];
            let edits = random_set(&mut rng, density);
            for e in edits {
                if !side.remove(&e) {
                    side.insert(e);
                }
            }
        }
        let keys: BTreeSet<StructuralKey> = base
            .iter()
            .chain(&target)
            .chain(&source)
            .map(Quad::structural_key)
            .collect();
        let conflicting: BTreeSet<StructuralKey> = keys
            .iter()
            .filter(|k| {
                let (b, t, s) = (by_key(&base, k), by_key(&target, k), by_key(&source, k));
                t != b && s != b && t != s
            })
            .cloned()
            .collect();
        let td = diff(&base, &target);
        let sd = diff(&base, &source);
        for strategy in ALL {
            let r = three_way(&base, &target, &source, strategy);
            assert_eq!(r.conflict_count, conflicting.len(), "seed {seed}");
            assert_eq!(
                r.conflicts
                    .iter()
                    .map(|c| c.key.clone())
                    .collect::<BTreeSet<_>>(),
                conflicting,
                "seed {seed}"
            );
            // Determinism.
            assert_eq!(
                r,
                three_way(&base, &target, &source, strategy),
                "seed {seed}"
            );
            let Some(merged) = &r.merged else {
                assert!(
                    strategy == Strategy::Abort && !conflicting.is_empty(),
                    "seed {seed}"
                );
                continue;
            };
            // Merged quads come from the inputs only.
            assert!(
                merged
                    .iter()
                    .all(|q| base.contains(q) || target.contains(q) || source.contains(q)),
                "seed {seed}"
            );
            for k in &keys {
                let got = by_key(merged, k);
                if conflicting.contains(k) {
                    let (t, s) = (by_key(&target, k), by_key(&source, k));
                    let want = match strategy {
                        Strategy::TakeTarget => t,
                        Strategy::TakeSource => s,
                        Strategy::Union => t.union(&s).cloned().collect(),
                        Strategy::Abort => unreachable!("abort with conflicts has no state"),
                    };
                    assert_eq!(got, want, "seed {seed} {strategy:?} conflicting {k}");
                } else {
                    // Independent oracle: the classic set three-way formula restricted to k,
                    // (B − D_T − D_S) ∪ A_T ∪ A_S, which equals the per-key rules whenever
                    // the key does not conflict.
                    let want: BTreeSet<Quad> = by_key(&base, k)
                        .into_iter()
                        .filter(|q| !td.deletes.contains(q) && !sd.deletes.contains(q))
                        .chain(by_key(&td.adds, k))
                        .chain(by_key(&sd.adds, k))
                        .collect();
                    assert_eq!(got, want, "seed {seed} {strategy:?} clean {k}");
                }
            }
            // The candidate patch reconstructs the merged state from the target exactly.
            let mut rebuilt = target.clone();
            apply_patch(&mut rebuilt, &diff(&target, merged).to_patch());
            assert_eq!(&rebuilt, merged, "seed {seed}");
        }
        // Role symmetry: take-source(B, T, S) = take-target(B, S, T); union is symmetric.
        assert_eq!(
            three_way(&base, &target, &source, Strategy::TakeSource).merged,
            three_way(&base, &source, &target, Strategy::TakeTarget).merged,
            "seed {seed}"
        );
        assert_eq!(
            three_way(&base, &target, &source, Strategy::Union).merged,
            three_way(&base, &source, &target, Strategy::Union).merged,
            "seed {seed}"
        );
        if conflicting.is_empty() {
            clean += 1;
        } else {
            with_conflicts += 1;
        }
    }
    assert!(
        with_conflicts > 100 && clean > 100,
        "{with_conflicts}/{clean}"
    );
}

#[test]
fn preview_token_is_injective_normalized_and_stable() {
    let c = |n: &str| CommitId(ContentId::for_bytes(n.as_bytes()));
    let id = PreviewIdentity {
        graph: GraphId::new("graph-1").unwrap(),
        source_branch: "agent/task-17".into(),
        source_head: c("s"),
        target_branch: "main".into(),
        target_head: c("t"),
        merge_base: c("b"),
        classification: Classification::Divergent,
        strategy: Strategy::Union,
        merged_state_digest: ContentId::for_bytes(b"m"),
    };
    let token = id.token();
    assert!(token.starts_with("sha256:") && token.len() == 7 + 64);
    // Every bound field changes the token.
    let variants = [
        PreviewIdentity {
            source_branch: "agent/task-18".into(),
            ..id.clone()
        },
        PreviewIdentity {
            source_head: c("s2"),
            ..id.clone()
        },
        PreviewIdentity {
            target_branch: "release".into(),
            ..id.clone()
        },
        PreviewIdentity {
            target_head: c("t2"),
            ..id.clone()
        },
        PreviewIdentity {
            merge_base: c("b2"),
            ..id.clone()
        },
        PreviewIdentity {
            strategy: Strategy::TakeSource,
            ..id.clone()
        },
        PreviewIdentity {
            classification: Classification::FastForward,
            ..id.clone()
        },
        PreviewIdentity {
            merged_state_digest: ContentId::for_bytes(b"m2"),
            ..id.clone()
        },
        PreviewIdentity {
            graph: GraphId::new("graph-2").unwrap(),
            ..id.clone()
        },
    ];
    let mut seen = BTreeSet::from([token.clone()]);
    for v in &variants {
        assert!(seen.insert(v.token()), "{v:?}");
    }
    // Length prefixes keep field boundaries: moving bytes between adjacent fields differs.
    // graph_id and source_branch are adjacent fields: moving a byte between them changes
    // the token only because of the length prefixes.
    let shifted = PreviewIdentity {
        graph: GraphId::new("graph-1a").unwrap(),
        source_branch: "gent/task-17".into(),
        ..id.clone()
    };
    assert_ne!(shifted.token(), token);
    // A fast-forward's strategy is normalized away.
    let ff = PreviewIdentity {
        classification: Classification::FastForward,
        ..id.clone()
    };
    assert_eq!(
        ff.token(),
        PreviewIdentity {
            strategy: Strategy::Abort,
            ..ff.clone()
        }
        .token()
    );
    assert!(id.canonical_bytes().starts_with(PREVIEW_TOKEN_V1_HEADER));
}

#[test]
fn edge_cases_empty_states_and_delete_versus_delete() {
    let empty = BTreeSet::new();
    for strategy in ALL {
        let r = three_way(&empty, &empty, &empty, strategy);
        assert_eq!((r.merged, r.conflict_count), (Some(BTreeSet::new()), 0));
    }
    // B = {x, y}; target deletes y, source deletes x (different slots): both deletions merge.
    let x = "<urn:x> <urn:p> \"1\" .";
    let y = "<urn:y> <urn:p> \"1\" .";
    let r = three_way(&st(&[x, y]), &st(&[x]), &st(&[y]), Strategy::Abort);
    assert_eq!(r.merged, Some(BTreeSet::new()));
    // Same slot, one side deletes one value, the other side the other: a conflict; union
    // keeps what either side still has.
    let a = "<urn:s> <urn:p> \"a\" .";
    let b = "<urn:s> <urn:p> \"b\" .";
    let r = three_way(&st(&[a, b]), &st(&[a]), &st(&[b]), Strategy::Union);
    assert_eq!((r.conflict_count, r.merged), (1, Some(st(&[a, b]))));
    // Exact patch: every add absent from the target, every delete present.
    let (t, m) = (st(&[a]), st(&[b, x]));
    let d = diff(&t, &m);
    assert!(d.adds.iter().all(|q| !t.contains(q)) && d.deletes.iter().all(|q| t.contains(q)));
    // Typed and language-tagged literals and escapes share the slot key of their subject and
    // predicate, and stay distinct quads.
    let typed = st(&[
        "<urn:s> <urn:p> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer> .",
        "<urn:s> <urn:p> \"1\"@en .",
        "<urn:s> <urn:p> \"a\\\"quoted\\\" line\" .",
    ]);
    let keys: BTreeSet<_> = typed.iter().map(Quad::structural_key).collect();
    assert_eq!((typed.len(), keys.len()), (3, 1));
}
