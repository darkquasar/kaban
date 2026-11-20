//! Live scip-go 0.2.9 expectations relative to the archived 0.2.7 fixture.
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::Duration;

use kaban_core::process::{self, EnvAllowlist, Exec, InheritedFds, Spec};
use kaban_index::runner::GO_INTERNAL;
use protobuf::Message;
use rusqlite::{Connection, OpenFlags};
use scip::types::Index;

use super::{common, table_dumps};

/// Compare file rows, symbol and edge identities, and individual source sites and owners.
/// Symbol IDs are database-local. All other projected columns, including spans, are exact.
fn rows(table: &str, dump: &str) -> BTreeSet<String> {
    let mut rows = BTreeSet::new();
    for line in dump.lines() {
        let row = if table == "symbols" {
            line.split_once('\t').unwrap().1
        } else {
            line
        };
        assert!(rows.insert(format!("{table}\t{row}")), "duplicate {row}");
    }
    rows
}

pub(super) fn assert_snapshot(path: &Path, go_version: &str) {
    let fixture = common::fixtures_dir().join("go-drivers");
    let mut expected = BTreeSet::new();
    let mut actual = BTreeSet::new();
    for (table, observed) in table_dumps(path) {
        let golden = fs::read_to_string(fixture.join(format!("goldens/{table}.txt"))).unwrap();
        expected.extend(rows(table, &golden));
        actual.extend(rows(table, &observed));
    }
    let changes = fixture.join("scip-go-0.2.9");
    let mut delta = fs::read_to_string(changes.join("rows.delta")).unwrap();
    match go_version {
        "1.26.0" => {}
        "1.27.3" => delta.push_str(&fs::read_to_string(changes.join("go1.27.3.delta")).unwrap()),
        _ => panic!("scip-go 0.2.9 fixture requires Go 1.26.0 or 1.27.3, found {go_version}"),
    }
    for line in delta.lines() {
        let (op, row) = line.split_once('\t').unwrap();
        match op {
            "+" => assert!(expected.insert(row.to_owned()), "already in golden: {row}"),
            "-" => assert!(expected.remove(row), "absent from golden: {row}"),
            _ => panic!("invalid golden delta: {line}"),
        }
    }
    assert!(
        actual == expected,
        "Go {go_version}: missing rows {:#?}, unexpected rows {:#?}",
        expected.difference(&actual).collect::<Vec<_>>(),
        actual.difference(&expected).collect::<Vec<_>>(),
    );
}

type Relationship<'a> = (&'a str, &'a str, &'a str, [bool; 4]);

fn relationships(index: &Index) -> Vec<Relationship<'_>> {
    let mut rows = Vec::new();
    for (path, symbols) in index
        .documents
        .iter()
        .map(|doc| (doc.relative_path.as_str(), &doc.symbols))
        .chain([("<external>", &index.external_symbols)])
    {
        for symbol in symbols {
            for r in &symbol.relationships {
                rows.push((
                    path,
                    symbol.symbol.as_str(),
                    r.symbol.as_str(),
                    [
                        r.is_reference,
                        r.is_implementation,
                        r.is_type_definition,
                        r.is_definition,
                    ],
                ));
            }
        }
    }
    rows
}

/// The snapshot unions both passes and omits self-loops. Check each producer pass before
/// that reduction, including document, owner, target, all four flags and multiplicity.
pub(super) fn assert_relationships(scip_go: &Path, go_version: &str, repo: &Path, scratch: &Path) {
    let fixture = common::fixtures_dir().join("go-drivers");
    let conn = format!(
        "scip-go gomod example.com/drivers {} `example.com/drivers/drivers`/Conn#",
        common::GO_DRIVERS_COMMIT
    );
    let mut env = EnvAllowlist::with_trusted(GO_INTERNAL);
    for name in ["GOROOT", "PATH", "GOMAXPROCS"] {
        env.carry(name).unwrap();
    }
    env.insert("CGO_ENABLED", "0").unwrap();
    env.insert("GOFLAGS", "-mod=readonly").unwrap();
    env.insert("GOCACHE", scratch.join("gocache")).unwrap();
    env.insert("GOPATH", scratch.join("gopath")).unwrap();
    for (file, skip_tests) in [("index.scip", false), ("index-notest.scip", true)] {
        let output = scratch.join(file);
        let mut args: Vec<OsString> =
            ["index", "-q", "--module-version", common::GO_DRIVERS_COMMIT]
                .into_iter()
                .map(OsString::from)
                .collect();
        if skip_tests {
            args.push("--skip-tests".into());
        }
        args.extend(["--output".into(), output.clone().into_os_string()]);
        let result = process::run(Spec {
            exec: Exec::Path(scip_go.to_owned()),
            args,
            cwd: repo,
            env: env.clone(),
            deadline: Duration::from_secs(600),
            stdout_cap: 0,
            stderr_cap: 1 << 20,
            monitors: vec![],
            join_bound: Duration::from_secs(2),
            inherited_fds: InheritedFds::Tolerate,
        })
        .unwrap();
        assert!(
            result.killed.is_none() && result.status.is_some_and(|s| s.success()),
            "{file}: {:?}, {}",
            result.killed,
            String::from_utf8_lossy(&result.stderr)
        );
        let golden = Index::parse_from_bytes(&fs::read(fixture.join(file)).unwrap()).unwrap();
        let index = Index::parse_from_bytes(&fs::read(output).unwrap()).unwrap();
        let mut expected = relationships(&golden);
        assert_eq!(expected.len(), 48);
        if go_version == "1.27.3" {
            for owner in [
                "scip-go gomod github.com/golang/go/src go1.22 `net/http/internal/http2`/ClientConn#",
                "scip-go gomod github.com/golang/go/src go1.22 `net/http/internal/http2`/NetHTTPClientConn#",
                "scip-go gomod github.com/golang/go/src go1.22 `vendor/golang.org/x/net/http2/hpack`/Decoder#",
            ] {
                expected.push(("<external>", owner, &conn, [false, true, false, false]));
            }
        }
        let mut actual = relationships(&index);
        expected.sort_unstable();
        actual.sort_unstable();
        assert_eq!(
            actual, expected,
            "Go {go_version}, {file}: raw relationships"
        );
    }
}

pub(super) fn assert_dependency_call(path: &Path) {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let calls: i64 = db
        .query_row(
            "SELECT count(*) FROM occurrences o JOIN symbols s ON s.id=o.symbol_id
         JOIN files f ON f.id=o.file_id WHERE f.path='app.go'
         AND (o.sl,o.sc,o.el,o.ec)=(5,34,5,40) AND o.roles=8 AND s.external=1 AND s.ref=?1",
            ["scip:scip-go gomod example.com/dep v1.0.0 `example.com/dep`/Answer()."],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        calls, 1,
        "dep.Answer() resolves at its ReadAccess site in app.go"
    );
}
