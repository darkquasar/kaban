//! Typed query slices (stage1-plan CG2.3; stage1-spec G1-F12, G1-F16, RV-7; cg-join-v1 §6, §9)
//! over builder snapshots, through `CodeGraphReader` on `SqliteSnapshot`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::assert_is_empty
)] // test code: failures are the assertion

#[path = "common/builder.rs"]
mod builder;

use std::collections::BTreeSet;

use builder::{Snapshot, SymbolSpec, commit, lines, scratch};
use kaban_codegraph::reader::{MatchedBy, TestsTarget};
use kaban_codegraph::schema::{EdgeType, FileId, NodeId, NodeKind, Range};
use kaban_codegraph::{CodeGraphReader, Depth, Limits, Policy, SqliteSnapshot, reader_policy_flag};

/// One interface in `iface.go`, four implementers (one external), spread over two files so the
/// `(path, ref)` order differs from the `ref` order.
fn implementers_graph(commit: &str) -> (Snapshot, NodeId) {
    let mut s = Snapshot::new(commit);
    let a = s.file("a.go", false);
    let iface_file = s.file("iface.go", false);
    let z = s.file("z.go", false);
    // refs sorted: file:a.go, file:iface.go, file:z.go, scip:... in byte order
    s.symbol(SymbolSpec::file("file:a.go", "a.go", a));
    s.symbol(SymbolSpec::file("file:iface.go", "iface.go", iface_file));
    s.symbol(SymbolSpec::file("file:z.go", "z.go", z));
    let ext = s.symbol(
        SymbolSpec::symbol(
            "scip:s m p v ext/Remote#",
            "Struct",
            "Remote",
            a,
            lines(0, 0),
        )
        .external(),
    );
    let in_z = s.symbol(SymbolSpec::symbol(
        "scip:s m p v x/Alpha#",
        "Struct",
        "Alpha",
        z,
        lines(1, 2),
    ));
    let iface = s.symbol(SymbolSpec::symbol(
        "scip:s m p v x/Iface#",
        "Interface",
        "Iface",
        iface_file,
        lines(1, 3),
    ));
    let in_a1 = s.symbol(SymbolSpec::symbol(
        "scip:s m p v x/Mid#",
        "Struct",
        "Mid",
        a,
        lines(10, 12),
    ));
    let in_a2 = s.symbol(SymbolSpec::symbol(
        "scip:s m p v x/Zeta#",
        "Struct",
        "Zeta",
        a,
        lines(20, 22),
    ));
    // edges sorted by (src, type, dst)
    s.edge(ext, EdgeType::Implements, iface);
    s.edge(in_z, EdgeType::Implements, iface);
    s.edge(in_a1, EdgeType::Implements, iface);
    s.edge(in_a2, EdgeType::Implements, iface);
    (s, iface)
}

fn refs(rows: &[kaban_codegraph::NodeRow]) -> Vec<&str> {
    rows.iter().map(|r| r.r#ref.as_str()).collect()
}

#[test]
fn implementers_are_complete_and_sorted_by_path_then_ref() {
    let dir = scratch();
    let commit = commit(0x40);
    let (s, iface) = implementers_graph(&commit);
    let path = s.write_final(dir.path(), &commit);
    let snap = SqliteSnapshot::open_ro(&path).unwrap();

    let internal = snap.implementers(iface, false).unwrap();
    assert_eq!(
        refs(&internal),
        [
            "scip:s m p v x/Mid#",
            "scip:s m p v x/Zeta#",
            "scip:s m p v x/Alpha#"
        ],
        "a.go rows before z.go rows, then by ref"
    );
    assert!(internal.iter().all(|r| !r.external));
    assert_eq!(internal[0].path.as_deref(), Some("a.go"));
    assert_eq!(internal[2].path.as_deref(), Some("z.go"));

    let all = snap.implementers(iface, true).unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(
        all[0].r#ref, "scip:s m p v ext/Remote#",
        "a pathless external row sorts first"
    );
    assert!(all[0].external && all[0].path.is_none());
    assert_eq!(refs(&all[1..]), refs(&internal));

    assert!(snap.implementers(NodeId(999), true).unwrap().is_empty());
}

/// 60 implementers in 60 files plus a hub of 1,000 `REFERENCES` edges, all `CONTAINS`-children
/// of one package node, for the complete-set and traversal-bound slices.
fn wide_graph(commit: &str) -> (Snapshot, NodeId, NodeId) {
    let mut s = Snapshot::new(commit);
    let mut files = Vec::new();
    for i in 0..60 {
        files.push(s.file(&format!("f{i:03}.go"), false));
    }
    let hub_file = s.file("hub.go", false);
    let test_file = s.file("hub_test.go", true);
    for (i, f) in files.iter().enumerate() {
        s.symbol(SymbolSpec::file(&format!("file:f{i:03}.go"), "f.go", *f));
    }
    let hub_file_node = s.symbol(SymbolSpec::file("file:hub.go", "hub.go", hub_file));
    let test_file_node = s.symbol(SymbolSpec::file(
        "file:hub_test.go",
        "hub_test.go",
        test_file,
    ));
    // scip refs in byte order: Hub#, I000#..I059#, Pkg/, R0000()...R0999(), TestHub()
    let hub = s.symbol(SymbolSpec::symbol(
        "scip:s m p v h/Hub#",
        "Interface",
        "Hub",
        hub_file,
        lines(1, 2),
    ));
    let mut impls = Vec::new();
    for i in 0..files.len() {
        // reverse the file order against the ref order so (path, ref) differs from ref
        let file = files[59 - i];
        impls.push(s.symbol(SymbolSpec::symbol(
            &format!("scip:s m p v h/I{i:03}#"),
            "Struct",
            "Impl",
            file,
            lines(1, 1),
        )));
    }
    let pkg = s.symbol(SymbolSpec::symbol(
        "scip:s m p v h/Pkg/",
        "Package",
        "Pkg",
        hub_file,
        lines(0, 0),
    ));
    let mut refs = Vec::new();
    for i in 0..1000 {
        refs.push(s.symbol(SymbolSpec::symbol(
            &format!("scip:s m p v h/R{i:04}()."),
            "Function",
            "Ref",
            hub_file,
            lines(10 + i, 10 + i),
        )));
    }
    let tfn = s.symbol(
        SymbolSpec::symbol(
            "scip:s m p v h/TestHub().",
            "Function",
            "TestHub",
            test_file,
            lines(1, 5),
        )
        .is_test(),
    );
    // edges by (src, type, dst): the file node's naming-form TESTED_BY, hub TESTED_BY tfn,
    // impls IMPLEMENTS hub, pkg CONTAINS hub then impls (dst order), refs REFERENCES hub
    s.edge(hub_file_node, EdgeType::TestedBy, test_file_node);
    s.edge(hub, EdgeType::TestedBy, tfn);
    for i in &impls {
        s.edge(*i, EdgeType::Implements, hub);
    }
    s.edge(pkg, EdgeType::Contains, hub);
    for i in &impls {
        s.edge(pkg, EdgeType::Contains, *i);
    }
    for r in &refs {
        s.edge(*r, EdgeType::References, hub);
    }
    // occurrences by (file, sl, sc, el, ec, symbol, roles): hub referenced from the first 3 refs
    // and once from the file level, defined at its def range (role 1, not a reference); a test site
    s.occurrence(hub, hub_file, lines(1, 1), 1, None);
    s.occurrence(hub, hub_file, lines(3, 3), 8, None);
    for (i, r) in refs.iter().enumerate().take(3) {
        s.occurrence(
            hub,
            hub_file,
            lines(
                10 + u32::try_from(i).unwrap(),
                10 + u32::try_from(i).unwrap(),
            ),
            8,
            Some(*r),
        );
    }
    s.occurrence(hub, test_file, lines(2, 2), 8, Some(tfn));
    (s, hub, pkg)
}

#[test]
fn reader_returns_complete_candidates() {
    let dir = scratch();
    let commit = commit(0x41);
    let (s, hub, pkg) = wide_graph(&commit);
    let path = s.write_final(dir.path(), &commit);
    let snap = SqliteSnapshot::open_ro(&path).unwrap();

    // 60 matching rows: 60 returned, in (path, ref) order, no reader-side cut at 50 or 200
    let impls = snap.implementers(hub, false).unwrap();
    assert_eq!(impls.len(), 60, "complete candidate set");
    let paths: Vec<&str> = impls.iter().map(|r| r.path.as_deref().unwrap()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted, "ordered by path first");
    assert_eq!(impls[0].r#ref, "scip:s m p v h/I059#", "f000.go holds I059");

    // members: 61 CONTAINS children, labels filter exact
    let members = snap.members(pkg, &[]).unwrap();
    assert_eq!(members.len(), 61);
    let structs = snap.members(pkg, &["Struct"]).unwrap();
    assert_eq!(structs.len(), 60);
    let both = snap.members(pkg, &["Interface", "Struct"]).unwrap();
    assert_eq!(both.len(), 61);
    assert!(snap.members(pkg, &["Method"]).unwrap().is_empty());
    assert!(
        snap.members(pkg, &["struct"]).unwrap().is_empty(),
        "case-sensitive"
    );

    // by_label_name exact and complete: 60 `Struct Impl`, 1,000 `Function Ref`
    assert_eq!(snap.by_label_name("Struct", "Impl").unwrap().len(), 60);
    assert_eq!(snap.by_label_name("Function", "Ref").unwrap().len(), 1000);
    assert!(snap.by_label_name("Struct", "impl").unwrap().is_empty());
}

#[test]
fn references_group_sites_by_enclosing_node_and_skip_definitions() {
    let dir = scratch();
    let commit = commit(0x42);
    let (s, hub, _) = wide_graph(&commit);
    let path = s.write_final(dir.path(), &commit);
    let snap = SqliteSnapshot::open_ro(&path).unwrap();

    let groups = snap.references(hub, false).unwrap();
    // hub.go sites: one at file level (enclosing = file node) and three inside R0000..R0002;
    // the definition site (role 1) is not a reference; the test file is excluded
    assert_eq!(groups.len(), 4);
    assert_eq!(
        groups[0].enclosing.r#ref, "file:hub.go",
        "file node sorts before scip refs"
    );
    assert_eq!(groups[0].sites.len(), 1);
    assert_eq!(groups[0].sites[0].site.sl, 3);
    assert_eq!(groups[0].sites[0].path, "hub.go");
    assert_eq!(groups[0].sites[0].roles, 8);
    assert_eq!(groups[1].enclosing.r#ref, "scip:s m p v h/R0000().");
    assert_eq!(groups[3].enclosing.r#ref, "scip:s m p v h/R0002().");
    assert!(
        groups
            .iter()
            .all(|g| g.sites.iter().all(|s| s.site.sl != 1)),
        "no definition site"
    );

    let with_tests = snap.references(hub, true).unwrap();
    assert_eq!(with_tests.len(), 5);
    assert_eq!(with_tests[4].enclosing.r#ref, "scip:s m p v h/TestHub().");
    assert_eq!(with_tests[4].sites[0].path, "hub_test.go");

    // a 1,000-in-degree hub keeps its complete edge set reachable: no cap in the reader
    assert_eq!(snap.by_label_name("Function", "Ref").unwrap().len(), 1000);
}

#[test]
fn tests_for_both_forms_and_lookups() {
    let dir = scratch();
    let commit = commit(0x43);
    let (s, hub, _) = wide_graph(&commit);
    let path = s.write_final(dir.path(), &commit);
    let snap = SqliteSnapshot::open_ro(&path).unwrap();

    let by_symbol = snap.tests_for(TestsTarget::Symbol(hub)).unwrap();
    assert_eq!(by_symbol.len(), 1);
    assert_eq!(by_symbol[0].test.name, "TestHub");
    assert_eq!(by_symbol[0].matched_by, MatchedBy::Reference);
    assert!(!by_symbol[0].heuristic);
    assert!(by_symbol[0].test.is_test);

    let hub_file = FileId(61);
    let by_file = snap.tests_for(TestsTarget::File(hub_file)).unwrap();
    assert_eq!(by_file.len(), 1);
    assert_eq!(by_file[0].test.r#ref, "file:hub_test.go");
    assert_eq!(by_file[0].matched_by, MatchedBy::Naming);
    assert!(by_file[0].heuristic);
    assert!(
        snap.tests_for(TestsTarget::File(FileId(1)))
            .unwrap()
            .is_empty()
    );

    // node lookups
    let node = snap.node("scip:s m p v h/Hub#").unwrap().unwrap();
    assert_eq!(node.id, hub);
    assert_eq!(node.path.as_deref(), Some("hub.go"));
    assert_eq!(node.def, Some(lines(1, 2)));
    assert_eq!(snap.node_by_id(hub).unwrap().unwrap().r#ref, node.r#ref);
    assert!(snap.node("scip:nothing").unwrap().is_none());
    assert_eq!(
        snap.by_path_line("hub.go", 2).unwrap().len(),
        1,
        "Hub covers line 2"
    );
    assert_eq!(
        snap.by_path_line("hub.go", 12).unwrap()[0].r#ref,
        "scip:s m p v h/R0002()."
    );
    assert!(snap.by_path_line("hub.go", 5000).unwrap().is_empty());
    let in_ranges = snap.nodes_in_ranges(hub_file, &[(0, 2), (11, 12)]).unwrap();
    assert_eq!(
        refs(&in_ranges),
        [
            "scip:s m p v h/Hub#",
            "scip:s m p v h/Pkg/",
            "scip:s m p v h/R0001().",
            "scip:s m p v h/R0002()."
        ]
    );
    assert!(snap.nodes_in_ranges(hub_file, &[]).unwrap().is_empty());
    let in_file = snap.nodes_in_file("hub_test.go").unwrap();
    assert_eq!(
        refs(&in_file),
        ["file:hub_test.go", "scip:s m p v h/TestHub()."]
    );
    let meta = snap.meta().unwrap();
    assert_eq!(
        meta.get("commit").map(String::as_str),
        Some(commit.as_str())
    );
    assert_eq!(meta.len(), 6);
}

#[test]
fn references_exclude_inline_test_scopes_without_hiding_mixed_file_production() {
    let dir = scratch();
    let commit = commit(0x73);
    let mut s = Snapshot::new(&commit);
    let mixed = s.file("src/lib.rs", false);
    let integration = s.file("tests/driver.rs", true);
    let file_node = s.symbol(SymbolSpec::file("file:src/lib.rs", "src/lib.rs", mixed));
    s.symbol(SymbolSpec::file("file:tests/driver.rs", "tests/driver.rs", integration).is_test());
    let integration_test = s.symbol(
        SymbolSpec::symbol(
            "scip:s m p v Integration().",
            "Function",
            "Integration",
            integration,
            lines(0, 2),
        )
        .is_test(),
    );
    let production = s.symbol(SymbolSpec::symbol(
        "scip:s m p v Production().",
        "Function",
        "Production",
        mixed,
        lines(0, 2),
    ));
    let target = s.symbol(SymbolSpec::symbol(
        "scip:s m p v Target().",
        "Function",
        "Target",
        mixed,
        lines(4, 5),
    ));
    let test = s.symbol(
        SymbolSpec::symbol(
            "scip:s m p v Test().",
            "Function",
            "Test",
            mixed,
            lines(8, 11),
        )
        .is_test(),
    );
    s.occurrence(target, mixed, lines(1, 1), 8, Some(production));
    s.occurrence(target, mixed, lines(2, 2), 32 | 8, Some(production));
    s.occurrence(target, mixed, lines(4, 4), 1, Some(target));
    s.occurrence(target, mixed, lines(6, 6), 8, None);
    // The standard producer Test role works even without a test symbol/encloser.
    s.occurrence(target, mixed, lines(7, 7), 32 | 2 | 8, None);
    s.occurrence(target, mixed, lines(9, 9), 8, Some(test));
    s.occurrence(target, integration, lines(1, 1), 8, Some(integration_test));
    let path = s.write_final(dir.path(), &commit);
    let snapshot = SqliteSnapshot::open_ro(&path).unwrap();

    let production_refs = snapshot.references(target, false).unwrap();
    assert_eq!(
        production_refs
            .iter()
            .map(|group| group.enclosing.id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([file_node, production]),
    );
    assert_eq!(
        production_refs
            .iter()
            .map(|group| group.sites.len())
            .sum::<usize>(),
        2
    );
    let all_refs = snapshot.references(target, true).unwrap();
    assert_eq!(all_refs.len(), 4);
    assert_eq!(all_refs.iter().map(|g| g.sites.len()).sum::<usize>(), 6);
    assert!(
        all_refs
            .iter()
            .flat_map(|g| &g.sites)
            .any(|s| s.roles == 32 | 2 | 8)
    );
    assert!(all_refs.iter().any(|group| group.enclosing.id == test));
    assert!(
        all_refs
            .iter()
            .any(|group| group.enclosing.id == integration_test)
    );
}

#[test]
fn symbol_unique_distinguishes_absent_unique_and_ambiguous() {
    let dir = scratch();
    let commit = commit(0x44);
    let mut s = Snapshot::new(&commit);
    let a = s.file("a.go", false);
    s.symbol(SymbolSpec::file("file:a.go", "a.go", a));
    s.symbol(
        SymbolSpec::symbol("scip:s m p v1 x/Dup#", "Struct", "Dup", a, lines(1, 1))
            .key("s m p . x/Dup#"),
    );
    s.symbol(
        SymbolSpec::symbol("scip:s m p v1 x/Ext#", "Struct", "Ext", a, lines(0, 0))
            .key("s m p . x/Ext#")
            .external(),
    );
    s.symbol(
        SymbolSpec::symbol("scip:s m p v1 x/One#", "Struct", "One", a, lines(2, 2))
            .key("s m p . x/One#"),
    );
    s.symbol(
        SymbolSpec::symbol("scip:s m p v2 x/Dup#", "Struct", "Dup", a, lines(3, 3))
            .key("s m p . x/Dup#"),
    );
    s.symbol(
        SymbolSpec::symbol("scip:s m p v2 x/Ext#", "Struct", "Ext", a, lines(4, 4))
            .key("s m p . x/Ext#"),
    );
    let path = s.write_final(dir.path(), &commit);
    let snap = SqliteSnapshot::open_ro(&path).unwrap();

    assert_eq!(
        snap.symbol_unique("s m p . x/One#").unwrap().unwrap().name,
        "One"
    );
    assert!(snap.symbol_unique("s m p . x/None#").unwrap().is_none());
    assert!(matches!(
        snap.symbol_unique("s m p . x/Dup#"),
        Err(kaban_codegraph::ReadError::Ambiguous)
    ));
    // the external twin does not count: one non-external row shares the key
    assert_eq!(
        snap.symbol_unique("s m p . x/Ext#").unwrap().unwrap().r#ref,
        "scip:s m p v2 x/Ext#"
    );
}

#[test]
fn neighbours_hub_of_a_thousand_edges_returns_two_hundred_and_truncated() {
    let dir = scratch();
    let commit = commit(0x45);
    let (s, hub, pkg) = wide_graph(&commit);
    let path = s.write_final(dir.path(), &commit);
    let snap = SqliteSnapshot::open_ro(&path).unwrap();
    let limits = Limits::DEFAULT;

    let one = snap
        .neighbours(hub, Depth::new(1, &limits).unwrap(), &[])
        .unwrap();
    assert_eq!(
        one.edges.len(),
        limits.neighbours_per_hub,
        "200 edges from the hub"
    );
    assert!(one.truncated);
    assert_eq!(one.nodes[0].id, hub, "the start node first");
    assert_eq!(one.nodes.len(), 201);
    let mut by_ref: Vec<&str> = one.nodes[1..].iter().map(|n| n.r#ref.as_str()).collect();
    let as_returned = by_ref.clone();
    by_ref.sort_unstable();
    assert_eq!(as_returned, by_ref, "depth-1 nodes by ref");
    assert_eq!(one.nodes[1].r#ref, "scip:s m p v h/I000#");

    // an edge-type filter is applied before the bound: TESTED_BY only
    let tested = snap
        .neighbours(hub, Depth::new(1, &limits).unwrap(), &[EdgeType::TestedBy])
        .unwrap();
    assert_eq!(tested.edges.len(), 1);
    assert!(!tested.truncated);
    assert_eq!(tested.edges[0].r#type, EdgeType::TestedBy);
    assert_eq!(tested.edges[0].from, "scip:s m p v h/Hub#");
    assert_eq!(tested.edges[0].to, "scip:s m p v h/TestHub().");

    // depth 3 from the package: pkg's 61 CONTAINS edges, then the hub's first 200 touches (the
    // 60 IMPLEMENTS, pkg -> hub already emitted, 139 REFERENCES) under the per-hub cut; the impls
    // and refs add no edge that was not emitted from the hub's end (RG2-3: distinct edges)
    let deep = snap
        .neighbours(pkg, Depth::new(3, &limits).unwrap(), &[])
        .unwrap();
    assert!(deep.truncated, "the hub's per-hub cut");
    assert_eq!(deep.edges.len(), 61 + 199);
    let mut distinct: Vec<_> = deep.edges.iter().collect();
    distinct.sort_by(|a, b| (&a.from, a.r#type, &a.to).cmp(&(&b.from, b.r#type, &b.to)));
    distinct.dedup();
    assert_eq!(distinct.len(), deep.edges.len(), "no edge twice");
    assert!(deep.nodes.len() <= limits.neighbours_total + 1);

    // a small neighbourhood is exact, both directions, with the far refs
    let small = snap
        .neighbours(NodeId(1), Depth::new(2, &limits).unwrap(), &[])
        .unwrap();
    assert!(
        small.edges.is_empty() && !small.truncated,
        "file:f000.go has no edges"
    );
    assert!(Depth::new(0, &limits).is_none() && Depth::new(4, &limits).is_none());
}

#[test]
fn reader_stale_policy_flag() {
    let dir = scratch();
    let commit = commit(0x46);
    let path = Snapshot::new(&commit).write_final(dir.path(), &commit);
    let snap = SqliteSnapshot::open_ro(&path).unwrap();
    let meta = snap.meta().unwrap();
    assert_eq!(reader_policy_flag(&meta, "pv1"), Policy::Current);
    assert_eq!(
        reader_policy_flag(&meta, "pv2"),
        Policy::Stale,
        "snapshot older than live"
    );
    assert_eq!(
        reader_policy_flag(&meta, "pv0"),
        Policy::Stale,
        "any difference is stale"
    );
    let mut without = meta.clone();
    without.remove("policy_version");
    assert_eq!(reader_policy_flag(&without, "pv1"), Policy::Stale);
    assert_eq!(Policy::Current.as_str(), "current");
    // the scan was off at index time (DF-3): neither current nor stale, whatever is live
    let mut off = meta.clone();
    off.insert("policy_version".into(), Policy::OFF.into());
    assert_eq!(reader_policy_flag(&off, "pv1"), Policy::Off);
    assert_eq!(reader_policy_flag(&off, Policy::OFF), Policy::Off);
    assert_eq!(Policy::Off.as_str(), "off");
}

/// A graph from `(src, type, dst)` triples over refs `scip:s m p v n/<name>#` in one file; the
/// builder's order rules are met by sorting names and edges here.
fn graph_of(commit: &str, names: &[&str], edges: &[(&str, EdgeType, &str)]) -> Snapshot {
    let mut s = Snapshot::new(commit);
    let f = s.file("g.go", false);
    s.symbol(SymbolSpec::file("file:g.go", "g.go", f));
    let mut sorted: Vec<&str> = names.to_vec();
    sorted.sort_unstable();
    let r = |n: &str| format!("scip:s m p v n/{n}#");
    let mut ids = std::collections::HashMap::new();
    for (i, n) in sorted.iter().enumerate() {
        let id = s.symbol(SymbolSpec::symbol(
            &r(n),
            "Struct",
            n,
            f,
            lines(u32::try_from(i).unwrap(), u32::try_from(i).unwrap()),
        ));
        ids.insert((*n).to_owned(), id);
    }
    let mut rows: Vec<(NodeId, EdgeType, NodeId)> = edges
        .iter()
        .map(|(a, t, b)| (ids[*a], *t, ids[*b]))
        .collect();
    rows.sort_unstable_by_key(|(a, t, b)| (*a, t.as_str(), *b));
    for (a, t, b) in rows {
        s.edge(a, t, b);
    }
    s
}

fn open(dir: &std::path::Path, s: &Snapshot, commit: &str) -> SqliteSnapshot {
    SqliteSnapshot::open_ro(&s.write_final(dir, commit)).unwrap()
}

fn id_of(snap: &SqliteSnapshot, name: &str) -> NodeId {
    snap.node(&format!("scip:s m p v n/{name}#"))
        .unwrap()
        .unwrap()
        .id
}

/// `GM2a` RG2-3: an edge is one relationship however many hubs touch it; the total bound counts
/// distinct `(src, type, dst)` edges.
#[test]
fn neighbours_emit_each_edge_once_and_count_distinct_edges() {
    let dir = scratch();
    let limits = Limits::DEFAULT;

    // A -> B at depth 2: expanding B must not emit A -> B again
    let c = commit(0xc0);
    let snap = open(
        dir.path(),
        &graph_of(&c, &["A", "B"], &[("A", EdgeType::Contains, "B")]),
        &c,
    );
    let g = snap
        .neighbours(id_of(&snap, "A"), Depth::new(2, &limits).unwrap(), &[])
        .unwrap();
    assert_eq!(
        g.edges.len(),
        1,
        "one relationship, emitted once: {:?}",
        g.edges
    );
    assert_eq!(g.nodes.len(), 2);
    assert!(!g.truncated);

    // a cycle A -> B -> C -> A at depth 3: three distinct edges, three nodes
    let c = commit(0xc1);
    let snap = open(
        dir.path(),
        &graph_of(
            &c,
            &["A", "B", "C"],
            &[
                ("A", EdgeType::References, "B"),
                ("B", EdgeType::References, "C"),
                ("C", EdgeType::References, "A"),
            ],
        ),
        &c,
    );
    let g = snap
        .neighbours(id_of(&snap, "A"), Depth::new(3, &limits).unwrap(), &[])
        .unwrap();
    let mut edges: Vec<(String, String)> = g
        .edges
        .iter()
        .map(|e| (e.from.clone(), e.to.clone()))
        .collect();
    edges.sort();
    edges.dedup();
    assert_eq!(edges.len(), 3, "a cycle has three distinct edges");
    assert_eq!(g.edges.len(), 3, "and none is emitted twice: {:?}", g.edges);
    assert_eq!(g.nodes.len(), 3);
    assert!(!g.truncated);

    // near the total bound: S -> C_i, C_i -> T_i, T_i -> U for i in 0..160 is 480 distinct
    // edges; counting an edge twice (from both ends) would cut the traversal at 500
    let c = commit(0xc2);
    let mut names = vec!["S".to_owned(), "U".to_owned()];
    let mut edges: Vec<(String, EdgeType, String)> = Vec::new();
    for i in 0..160 {
        let (ci, ti) = (format!("C{i:03}"), format!("T{i:03}"));
        edges.push(("S".into(), EdgeType::Contains, ci.clone()));
        edges.push((ci.clone(), EdgeType::References, ti.clone()));
        edges.push((ti.clone(), EdgeType::References, "U".into()));
        names.push(ci);
        names.push(ti);
    }
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let edges: Vec<(&str, EdgeType, &str)> = edges
        .iter()
        .map(|(a, t, b)| (a.as_str(), *t, b.as_str()))
        .collect();
    let snap = open(dir.path(), &graph_of(&c, &names, &edges), &c);
    let g = snap
        .neighbours(id_of(&snap, "S"), Depth::new(3, &limits).unwrap(), &[])
        .unwrap();
    assert_eq!(g.edges.len(), 480, "every distinct edge, each once");
    assert!(
        !g.truncated,
        "480 distinct edges are under the total bound of 500"
    );
    assert_eq!(g.nodes.len(), 322);

    // over the total bound with 170 chains (510 distinct edges): exactly 500, truncated
    let c = commit(0xc3);
    let mut names = vec!["S".to_owned(), "U".to_owned()];
    let mut edges: Vec<(String, EdgeType, String)> = Vec::new();
    for i in 0..170 {
        let (ci, ti) = (format!("C{i:03}"), format!("T{i:03}"));
        edges.push(("S".into(), EdgeType::Contains, ci.clone()));
        edges.push((ci.clone(), EdgeType::References, ti.clone()));
        edges.push((ti.clone(), EdgeType::References, "U".into()));
        names.push(ci);
        names.push(ti);
    }
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let edges: Vec<(&str, EdgeType, &str)> = edges
        .iter()
        .map(|(a, t, b)| (a.as_str(), *t, b.as_str()))
        .collect();
    let snap = open(dir.path(), &graph_of(&c, &names, &edges), &c);
    let g = snap
        .neighbours(id_of(&snap, "S"), Depth::new(3, &limits).unwrap(), &[])
        .unwrap();
    assert_eq!(g.edges.len(), limits.neighbours_total);
    assert!(g.truncated, "510 distinct edges exceed the total bound");
}

/// `GM2a` RG2-4: a `Depth` is valid relative to the `Limits` it was built under; the snapshot
/// re-checks it against its own limits at the query boundary.
#[test]
fn a_depth_built_under_looser_limits_is_out_of_range_for_a_default_snapshot() {
    let dir = scratch();
    let c = commit(0xc4);
    let snap = open(
        dir.path(),
        &graph_of(&c, &["A", "B"], &[("A", EdgeType::Contains, "B")]),
        &c,
    );
    let loose = Limits {
        depth_max: 255,
        ..Limits::DEFAULT
    };
    let deep = Depth::new(4, &loose).expect("valid under the looser limits");
    let err = snap
        .neighbours(id_of(&snap, "A"), deep, &[])
        .expect_err("refused by the snapshot's own limits");
    assert!(
        matches!(
            err,
            kaban_codegraph::ReadError::OutOfRange { argument: "depth" }
        ),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "argument out of range: depth");
    // the ceiling itself is admitted
    let max = Depth::new(snap.limits().depth_max, snap.limits()).unwrap();
    assert!(snap.neighbours(id_of(&snap, "A"), max, &[]).is_ok());
}

/// `GM2a` RG2-5: definition ranges are end-exclusive in SCIP coordinates, so a range ending at
/// column 0 of line `n` does not touch line `n`; `by_path_line` and `nodes_in_ranges` apply the
/// same half-open rule, and an inverted request range matches nothing.
#[test]
fn line_selectors_apply_half_open_ranges() {
    use kaban_codegraph::schema::Range;
    let dir = scratch();
    let c = commit(0xc5);
    let mut s = Snapshot::new(&c);
    let f = s.file("h.go", false);
    s.symbol(SymbolSpec::file("file:h.go", "h.go", f));
    let range = |sl, sc, el, ec| Range { sl, sc, el, ec };
    // refs in byte order: Adjacent1 [1:0, 3:0), Adjacent2 [3:0, 5:0), Single [7:3, 7:10),
    // ZeroEnd [10:0, 12:0)
    s.symbol(SymbolSpec::symbol(
        "scip:s m p v n/Adjacent1#",
        "Struct",
        "Adjacent1",
        f,
        range(1, 0, 3, 0),
    ));
    s.symbol(SymbolSpec::symbol(
        "scip:s m p v n/Adjacent2#",
        "Struct",
        "Adjacent2",
        f,
        range(3, 0, 5, 0),
    ));
    s.symbol(SymbolSpec::symbol(
        "scip:s m p v n/Single#",
        "Struct",
        "Single",
        f,
        range(7, 3, 7, 10),
    ));
    s.symbol(SymbolSpec::symbol(
        "scip:s m p v n/ZeroEnd#",
        "Struct",
        "ZeroEnd",
        f,
        range(10, 0, 12, 0),
    ));
    let snap = open(dir.path(), &s, &c);
    let at = |line| refs_owned(&snap.by_path_line("h.go", line).unwrap());
    let in_ranges = |ranges: &[(u32, u32)]| refs_owned(&snap.nodes_in_ranges(f, ranges).unwrap());

    // zero end column: [10:0, 12:0) touches lines 10 and 11, not 12
    assert_eq!(at(11), ["scip:s m p v n/ZeroEnd#"]);
    assert!(at(12).is_empty(), "line 12 is past the exclusive end");
    assert_eq!(in_ranges(&[(11, 11)]), ["scip:s m p v n/ZeroEnd#"]);
    assert!(in_ranges(&[(12, 12)]).is_empty());
    assert_eq!(in_ranges(&[(12, 20), (0, 0)]), Vec::<String>::new());

    // single line: [7:3, 7:10) touches line 7 only
    assert_eq!(at(7), ["scip:s m p v n/Single#"]);
    assert!(at(6).is_empty() && at(8).is_empty());
    assert_eq!(in_ranges(&[(7, 7)]), ["scip:s m p v n/Single#"]);

    // adjacent definitions: line 3 belongs to the second only
    assert_eq!(at(3), ["scip:s m p v n/Adjacent2#"]);
    assert_eq!(at(2), ["scip:s m p v n/Adjacent1#"]);
    assert_eq!(in_ranges(&[(3, 3)]), ["scip:s m p v n/Adjacent2#"]);
    assert_eq!(
        in_ranges(&[(2, 3)]),
        ["scip:s m p v n/Adjacent1#", "scip:s m p v n/Adjacent2#"]
    );

    // an inverted request range is empty and matches nothing
    assert!(in_ranges(&[(4, 2)]).is_empty());
    assert_eq!(in_ranges(&[(4, 2), (7, 7)]), ["scip:s m p v n/Single#"]);
}

fn refs_owned(rows: &[kaban_codegraph::NodeRow]) -> Vec<String> {
    rows.iter().map(|r| r.r#ref.clone()).collect()
}

/// G3 (Q-1): the members of a file are its top-level symbols, by definition range. A parent in
/// another file never hides (the Go package of `b.go` contains `Zed`); a parent with a body
/// hides what the body encloses (`mod m { Inner }`), a parent without a recorded body hides
/// unless it is a package or module (a Go struct and its field; the same-file `package` clause
/// containing `Zed`); locals and the file node are never members; `labels` restricts the set.
#[test]
fn file_members_are_the_top_level_symbols_in_range_order() {
    let dir = scratch();
    let c = commit(0xf1);
    let mut s = Snapshot::new(&c);
    let a = s.file("pkg/a.go", false);
    let b = s.file("pkg/b.go", false);
    s.symbol(SymbolSpec::file("file:pkg/a.go", "a.go", a));
    s.symbol(SymbolSpec::file("file:pkg/b.go", "b.go", b));
    // the builder keeps ref order (`n/` < `n/Alpha#` < `n/Zed().` < `n/m/` < `o/`)
    let local = s.symbol(SymbolSpec {
        kind: NodeKind::Local,
        ..SymbolSpec::symbol("local:pkg/a.go:local 1", "Variable", "1", a, lines(12, 12))
    });
    // a package clause in `a.go` at line 0, no body (the Go shape)
    let pkg_a = s.symbol(SymbolSpec {
        enc: None,
        ..SymbolSpec::symbol("scip:s m p v n/", "Package", "n", a, lines(0, 0))
    });
    // a struct without a body and its field (Go), a module with a body and its item (Rust)
    let alpha = s.symbol(SymbolSpec {
        enc: None,
        ..SymbolSpec::symbol("scip:s m p v n/Alpha#", "Struct", "Alpha", a, lines(5, 5))
    });
    let field = s.symbol(SymbolSpec::symbol(
        "scip:s m p v n/Alpha#f.",
        "Field",
        "f",
        a,
        lines(6, 6),
    ));
    let zed = s.symbol(SymbolSpec::symbol(
        "scip:s m p v n/Zed().",
        "Function",
        "Zed",
        a,
        lines(1, 3),
    ));
    // the module's definition is its name, its enclosing range the block (the producer shape)
    let module = s.symbol(SymbolSpec {
        enc: Some(lines(8, 10)),
        ..SymbolSpec::symbol("scip:s m p v n/m/", "Module", "m", a, lines(8, 8))
    });
    let inner = s.symbol(SymbolSpec::symbol(
        "scip:s m p v n/m/Inner#",
        "Struct",
        "Inner",
        a,
        lines(9, 9),
    ));
    let pkg_b = s.symbol(SymbolSpec {
        enc: None,
        ..SymbolSpec::symbol("scip:s m p v o/", "Package", "o", b, lines(0, 0))
    });
    s.edge(pkg_a, EdgeType::Contains, alpha);
    s.edge(pkg_a, EdgeType::Contains, module);
    s.edge(alpha, EdgeType::Contains, field);
    s.edge(module, EdgeType::Contains, inner);
    s.edge(pkg_b, EdgeType::Contains, zed);
    let snap = open(dir.path(), &s, &c);

    let all = snap.file_members(a, &[]).unwrap();
    assert_eq!(
        all.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![pkg_a, zed, alpha, module],
        "top level by range, not by ref: {:?}",
        refs_owned(&all)
    );
    assert!(
        !all.iter()
            .any(|r| r.id == local || r.id == field || r.id == inner)
    );
    let structs = snap.file_members(a, &["Struct"]).unwrap();
    assert_eq!(
        structs.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![alpha]
    );
    assert_eq!(
        snap.file_members(b, &[])
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        vec![pkg_b]
    );
    assert!(snap.file_members(FileId(99), &[]).unwrap().is_empty());
}

/// q1-1: hiding is a position test over the file's symbols, not a `CONTAINS` parent walk.
/// After R-2 an inherent method is a child of its type, whose item range does not enclose
/// the separate `impl`; in `mod m { struct T; impl T { fn f() {} } }` the module encloses
/// `f` and `f` is not a member. The module symbol the Rust producer records for the whole
/// file (definition and enclosing range both the file) hides nothing: what lies inside a
/// symbol's own definition range is not in its body.
#[test]
fn file_members_hide_every_same_file_descendant() {
    let dir = scratch();
    let c = commit(0xf2);
    let mut s = Snapshot::new(&c);
    let a = s.file("src/lib.rs", false);
    s.symbol(SymbolSpec::file("file:src/lib.rs", "lib.rs", a));
    let file_module = s.symbol(SymbolSpec::symbol(
        "scip:s m p v lib/",
        "Module",
        "lib",
        a,
        lines(0, 30),
    ));
    // the builder keeps ref order (`lib/` < `lib/g().` < `lib/m/` < `lib/m/T#` < `lib/m/T#f().`)
    let free = s.symbol(SymbolSpec {
        enc: Some(lines(24, 26)),
        ..SymbolSpec::symbol("scip:s m p v lib/g().", "Function", "g", a, lines(24, 24))
    });
    let module = s.symbol(SymbolSpec {
        enc: Some(lines(8, 20)),
        ..SymbolSpec::symbol("scip:s m p v lib/m/", "Module", "m", a, lines(8, 8))
    });
    let ty = s.symbol(SymbolSpec::symbol(
        "scip:s m p v lib/m/T#",
        "Struct",
        "T",
        a,
        lines(9, 9),
    ));
    let method = s.symbol(SymbolSpec {
        enc: Some(lines(12, 14)),
        ..SymbolSpec::symbol("scip:s m p v lib/m/T#f().", "Method", "f", a, lines(12, 12))
    });
    s.edge(file_module, EdgeType::Contains, free);
    s.edge(file_module, EdgeType::Contains, module);
    s.edge(module, EdgeType::Contains, ty);
    s.edge(ty, EdgeType::Contains, method);
    let snap = open(dir.path(), &s, &c);

    let all = snap.file_members(a, &[]).unwrap();
    assert_eq!(
        all.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![file_module, module, free],
        "the file module, `m` and `g`; `T` and `f` lie inside `m`: {:?}",
        refs_owned(&all)
    );
}

/// q1-2: enclosure compares full SCIP positions (line and column, half-open ranges), so the
/// members of a file do not change with its formatting. `mod m { struct T; }` on one line has
/// the same members as the block form (`m` alone: `T` lies in the body of `m`); a symbol
/// whose item starts exactly where a body ends is outside it (half-open); and in
/// `struct S; impl S { fn f() {} }` on one line `f` is a member like in the two-line form
/// (the item range of `S` ends before the `impl`).
#[test]
fn file_members_do_not_change_with_formatting() {
    let range = |sl: u32, sc: u32, el: u32, ec: u32| Range { sl, sc, el, ec };
    // (outer item range, outer name, inner item range, inner name, outer label, hides)
    let layouts = [
        // `mod m { struct T; }` on one line, then as a block
        (
            range(0, 0, 0, 19),
            range(0, 4, 0, 5),
            range(0, 8, 0, 17),
            range(0, 15, 0, 16),
            "Module",
            true,
        ),
        (
            range(0, 0, 2, 1),
            range(0, 4, 0, 5),
            range(1, 4, 1, 13),
            range(1, 11, 1, 12),
            "Module",
            true,
        ),
        // `mod m {} struct T;`: the item of `T` starts where the body of `m` ends
        (
            range(0, 0, 0, 8),
            range(0, 4, 0, 5),
            range(0, 8, 0, 18),
            range(0, 16, 0, 17),
            "Module",
            false,
        ),
        // `struct S; impl S { fn f() {} }` on one line, then on two
        (
            range(0, 0, 0, 9),
            range(0, 7, 0, 8),
            range(0, 19, 0, 30),
            range(0, 22, 0, 23),
            "Struct",
            false,
        ),
        (
            range(0, 0, 0, 9),
            range(0, 7, 0, 8),
            range(1, 9, 1, 20),
            range(1, 12, 1, 13),
            "Struct",
            false,
        ),
    ];
    for (i, (o_enc, o_def, i_enc, i_def, label, hides)) in layouts.into_iter().enumerate() {
        let dir = scratch();
        let c = commit(0xf3 + u8::try_from(i).unwrap());
        let mut s = Snapshot::new(&c);
        let a = s.file("src/lib.rs", false);
        s.symbol(SymbolSpec::file("file:src/lib.rs", "lib.rs", a));
        let outer = s.symbol(SymbolSpec {
            enc: Some(o_enc),
            ..SymbolSpec::symbol("scip:s m p v lib/O#", label, "O", a, o_def)
        });
        let inner = s.symbol(SymbolSpec {
            enc: Some(i_enc),
            ..SymbolSpec::symbol("scip:s m p v lib/O#i().", "Function", "i", a, i_def)
        });
        s.edge(outer, EdgeType::Contains, inner);
        let snap = open(dir.path(), &s, &c);
        let all = snap.file_members(a, &[]).unwrap();
        let want = if hides {
            vec![outer]
        } else {
            vec![outer, inner]
        };
        assert_eq!(
            all.iter().map(|r| r.id).collect::<Vec<_>>(),
            want,
            "layout {i}: {:?}",
            refs_owned(&all)
        );
    }
}

/// G4 (Q-1): `neighbours` hides document-local symbols (`kind = local`) unless asked and
/// reports how many edges it hid; the hidden edges do not count against the traversal bounds,
/// and a local is never expanded as a hub.
#[test]
fn neighbours_hide_locals_unless_asked() {
    let dir = scratch();
    let c = commit(0xc4);
    let mut s = Snapshot::new(&c);
    let file = s.file("g.go", false);
    s.symbol(SymbolSpec::file("file:g.go", "g.go", file));
    let l1 = s.symbol(SymbolSpec {
        kind: NodeKind::Local,
        ..SymbolSpec::symbol("local:g.go:local 1", "Variable", "1", file, lines(2, 2))
    });
    let l2 = s.symbol(SymbolSpec {
        kind: NodeKind::Local,
        ..SymbolSpec::symbol("local:g.go:local 2", "Variable", "2", file, lines(3, 3))
    });
    let fn_a = s.symbol(SymbolSpec::symbol(
        "scip:s m p v n/A().",
        "Function",
        "A",
        file,
        lines(1, 4),
    ));
    let fn_b = s.symbol(SymbolSpec::symbol(
        "scip:s m p v n/B().",
        "Function",
        "B",
        file,
        lines(6, 6),
    ));
    s.edge(l1, EdgeType::References, l2);
    s.edge(fn_a, EdgeType::References, l1);
    s.edge(fn_a, EdgeType::References, l2);
    s.edge(fn_a, EdgeType::References, fn_b);
    let snap = open(dir.path(), &s, &c);
    let depth = Depth::new(2, &Limits::DEFAULT).unwrap();

    let hidden = snap.neighbours_with(fn_a, depth, &[], false).unwrap();
    assert_eq!(
        hidden.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![fn_a, fn_b]
    );
    assert_eq!(hidden.edges.len(), 1);
    assert_eq!(
        hidden.locals_hidden, 2,
        "two edges to locals hidden at the hub"
    );
    assert!(!hidden.truncated);

    let shown = snap.neighbours_with(fn_a, depth, &[], true).unwrap();
    assert_eq!(
        shown.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![fn_a, l1, l2, fn_b],
        "(depth, ref) order: `local:` refs sort before `scip:`"
    );
    assert_eq!(
        shown.edges.len(),
        4,
        "the local-to-local edge is reached at depth 2"
    );
    assert_eq!(shown.locals_hidden, 0);

    // a type filter that excludes every edge to a local hides nothing
    let typed = snap
        .neighbours_with(fn_a, depth, &[EdgeType::Contains], false)
        .unwrap();
    assert_eq!(typed.locals_hidden, 0);
}
