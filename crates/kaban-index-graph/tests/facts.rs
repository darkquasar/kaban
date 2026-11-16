//! Node facts (COD-01): the newest record wins across base, layers and sediment; a fact removal
//! hides older values; a node removal deletes the node's facts (the barrier); facts survive
//! fold and seal unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod oracle;

use kaban_index_graph::{
    Base, FactOp, Layer, LocalId, NodeOp, OpKind, Records, View, build_base, fold, seal,
};
use oracle::{Oracle, chain, view};
use tantivy::directory::OwnedBytes;

fn node(id: u32, kind: OpKind) -> NodeOp {
    NodeOp {
        node: LocalId(id),
        kind,
    }
}

fn fact(id: u32, key: u8, payload: &[u8]) -> FactOp {
    FactOp {
        node: LocalId(id),
        key,
        kind: OpKind::Add,
        payload: payload.to_vec(),
    }
}

fn unset(id: u32, key: u8) -> FactOp {
    FactOp {
        node: LocalId(id),
        key,
        kind: OpKind::Remove,
        payload: Vec::new(),
    }
}

fn open_base(bytes: Vec<u8>) -> Base {
    Base::open(OwnedBytes::new(bytes)).unwrap()
}

fn open_layer(bytes: Vec<u8>) -> Layer {
    Layer::open(OwnedBytes::new(bytes)).unwrap()
}

fn get<'v>(v: &'v View<'_>, id: u32, key: u8) -> Option<&'v [u8]> {
    v.fact(LocalId(id), key)
}

fn base_with_facts() -> Base {
    open_base(build_base(
        &Records {
            nodes: (1..=4).map(|n| node(n, OpKind::Add)).collect(),
            facts: vec![
                fact(1, 0, b"base"),
                fact(1, 1, b"kept"),
                fact(2, 0, b"two"),
                fact(3, 0, b"three"),
                fact(4, 9, b"four"),
            ],
            ..Records::default()
        },
        0,
    ))
}

#[test]
fn the_newest_record_of_a_fact_wins() {
    let base = base_with_facts();
    let layer = seal(
        &view::<Records>(Some(&base), &[], &[]),
        &Records {
            facts: vec![fact(1, 0, b"layer")],
            ..Records::default()
        },
    );
    let layers = vec![open_layer(layer)];
    let v = view::<Records>(Some(&base), &layers, &[]);
    assert_eq!(get(&v, 1, 0), Some(&b"layer"[..]));
    assert_eq!(get(&v, 1, 1), Some(&b"kept"[..]), "other keys are untouched");
    let sediment = [
        Records {
            facts: vec![fact(1, 0, b"first")],
            ..Records::default()
        },
        Records {
            facts: vec![fact(1, 0, b"second"), fact(1, 0, b"")],
            ..Records::default()
        },
    ];
    let v = view(Some(&base), &layers, &sediment);
    assert_eq!(
        get(&v, 1, 0),
        Some(&b""[..]),
        "the last record wins, an empty payload included"
    );
}

#[test]
fn a_fact_removal_hides_older_values_until_set_again() {
    let base = base_with_facts();
    let layer = seal(
        &view::<Records>(Some(&base), &[], &[]),
        &Records {
            facts: vec![unset(1, 0), unset(2, 5)],
            ..Records::default()
        },
    );
    let layers = vec![open_layer(layer)];
    let v = view::<Records>(Some(&base), &layers, &[]);
    assert_eq!(get(&v, 1, 0), None);
    assert_eq!(get(&v, 1, 1), Some(&b"kept"[..]));
    assert_eq!(get(&v, 2, 0), Some(&b"two"[..]), "removing an unset key changes nothing");
    let again = [Records {
        facts: vec![fact(1, 0, b"again")],
        ..Records::default()
    }];
    let v = view(Some(&base), &layers, &again);
    assert_eq!(get(&v, 1, 0), Some(&b"again"[..]));
}

#[test]
fn a_node_removal_deletes_its_facts_in_every_source() {
    let base = base_with_facts();
    let cycle2 = Records {
        nodes: vec![node(2, OpKind::Remove), node(2, OpKind::Add)],
        ..Records::default()
    };
    let drop3 = Records {
        nodes: vec![node(3, OpKind::Remove)],
        ..Records::default()
    };
    let back3 = Records {
        nodes: vec![node(3, OpKind::Add)],
        facts: vec![fact(3, 1, b"new")],
        ..Records::default()
    };
    // In sediment.
    let sed = [cycle2.clone(), drop3.clone(), back3.clone()];
    let v = view(Some(&base), &[], &sed);
    let check = |v: &View<'_>, ctx: &str| {
        assert_eq!(get(v, 2, 0), None, "{ctx}: removed then re-added in one batch");
        assert_eq!(get(v, 3, 0), None, "{ctx}: the old fact stays deleted");
        assert_eq!(get(v, 3, 1), Some(&b"new"[..]), "{ctx}: the new fact shows");
        assert_eq!(get(v, 4, 9), Some(&b"four"[..]), "{ctx}: other nodes keep theirs");
    };
    check(&v, "sediment");
    // Sealed into layers.
    let mut layers = Vec::new();
    for r in &sed {
        let bytes = seal(&view::<Records>(Some(&base), &layers, &[]), r);
        layers.push(open_layer(bytes));
    }
    check(&view::<Records>(Some(&base), &layers, &[]), "layers");
    // Folded between the removal and the re-add.
    let first = [cycle2, drop3];
    let folded = open_base(fold(&view(Some(&base), &[], &first), &[]));
    let rest = [back3];
    check(&view(Some(&folded), &[], &rest), "fold");
    // While a node is absent its facts read as absent.
    let gone = [Records {
        nodes: vec![node(4, OpKind::Remove)],
        ..Records::default()
    }];
    assert_eq!(get(&view(Some(&base), &[], &gone), 4, 9), None);
}

#[test]
fn facts_survive_fold_and_seal_unchanged() {
    for seed in 0..6 {
        let f = chain(seed, |r| r);
        let v = f.view();
        let folded = open_base(fold(&v, &[]));
        let after = view::<Records>(Some(&folded), &[], &[]);
        let ctx = format!("seed {seed}");
        assert_facts(&after, &f.oracle, &format!("{ctx} fold"));
        // More fact changes sealed on top of the folded base.
        let mut oracle = f.oracle.clone();
        let more = Records {
            facts: vec![fact(1, 0, b"sealed"), unset(2, 0), unset(2, 1)],
            ..Records::default()
        };
        oracle.apply(&more);
        let layer = seal(&after, &more);
        let layers = vec![open_layer(layer)];
        let sealed = view::<Records>(Some(&folded), &layers, &[]);
        assert_facts(&sealed, &oracle, &format!("{ctx} fold+seal"));
    }
}

fn assert_facts(v: &View<'_>, oracle: &Oracle, ctx: &str) {
    for id in 0..oracle::IDS + 2 {
        for key in 0..5 {
            assert_eq!(
                v.fact(LocalId(id), key),
                oracle.fact(LocalId(id), key),
                "{ctx}: fact {id}/{key}"
            );
        }
    }
}
