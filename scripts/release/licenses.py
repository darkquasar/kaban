#!/usr/bin/env python3
"""Collect the selected packages' upstream notices against a small reviewed inventory.

The repository keeps only inputs the collector cannot derive (supplements and their
provenance) and `inventory.json`: resolved packages, licence expressions and notice
hashes per target. `check` refuses changed inputs offline; `stage` regenerates the
release notices from Cargo sources and the pinned toolchain and refuses any drift.

Generation uses the locked Cargo tree, including build/proc-macro dependencies
conservatively, and excludes dev-only dependencies. All supplied licence alternatives
are retained; the documented selection prefers MIT, then Apache-2.0, while retaining
additional conjunctive/native obligations. No dependency or native sources are modified.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[2]
TARGETS = ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin")
SELECTED = ("kaban", "kaban-index")
FEATURES = ["default", "kaban-ingest/pdf", "kaban-ingest/md-splitter"]
RUST = "1.99.0"
INVENTORY = "licenses/release/inventory.json"
SUPPLEMENTS = {
    "alloc-stdlib": ("alloc-stdlib-0.3.0-LICENSE",),
    "taffy": ("taffy-0.14.0-LICENSE",),
    "brotli-decompressor": ("brotli-9.0.0-LICENSE.MIT",),
    "datasketches": ("datasketches-0.2.0-LICENSE", "datasketches-0.2.0-NOTICE"),
    "htmlescape": ("htmlescape-0.3.1-LICENSE-APACHE",),
    # ownedbytes 0.9.0 records a tantivy revision whose LICENSE has identical bytes.
    "ownedbytes": ("tantivy-9e63fc5-LICENSE",),
    "whatlang": ("whatlang-0.18.0-LICENSE",),
    **{name: ("tantivy-9e63fc5-LICENSE",) for name in (
        "tantivy-bitpacker", "tantivy-columnar", "tantivy-common", "tantivy-query-grammar",
        "tantivy-sstable", "tantivy-stacker", "tantivy-tokenizer-api",
    )},
}
SUPPLEMENT_VERSIONS = {
    "alloc-stdlib": "0.3.0", "taffy": "0.14.0", "brotli-decompressor": "6.0.1",
    "datasketches": "0.2.0", "htmlescape": "0.3.1", "ownedbytes": "0.9.0",
    "whatlang": "0.18.0",
    **{name: "0.7.0" for name in SUPPLEMENTS if name.startswith("tantivy-")},
    "tantivy-bitpacker": "0.10.0", "tantivy-common": "0.11.0",
    "tantivy-query-grammar": "0.26.0",
}
NOTICE_NAME = re.compile(r"(^|[-_])(licen[sc]e|copying|copyright|notice|unlicen[sc]e)([-_.]|$)", re.I)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def collect_notices(package, upstream):
    """Read complete licence/notice texts, including notices nested with native code."""
    root = Path(package["manifest_path"]).parent
    result = []
    if package["name"] in SUPPLEMENT_VERSIONS and package["version"] != SUPPLEMENT_VERSIONS[package["name"]]:
        raise ValueError(f"Review licence supplement for {package['name']} {package['version']}")
    declared = package.get("license_file")
    declared = (root / declared).resolve() if declared else None
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        # SQLCipher and test corpora are not part of the selected binaries.
        if set(relative.parts) & {"sqlcipher", "test_data", "tests", "benches"}:
            continue
        if path.is_file() and (NOTICE_NAME.search(path.name) or path.resolve() == declared):
            result.append({"path": relative.as_posix(), "text": path.read_text()})
    for name in SUPPLEMENTS.get(package["name"], ()):
        result.append({"path": "upstream/" + name, "text": (upstream / name).read_text()})
    if package["name"] == "libsqlite3-sys":
        # SQLite's public-domain declaration lives in the amalgamation header,
        # rather than in a separate LICENSE file. Retain the exact source header.
        source = (root / "sqlite3/sqlite3.c").read_text()
        start = source.index("/*\n** 2001 September 15")
        end = source.index("*/", start) + 2
        result.append({"path": "sqlite3/sqlite3.c (copyright disclaimer)", "text": source[start:end] + "\n"})
    if package['name'] == 'brotli-decompressor':
        # Its BSD LICENSE omits the MIT-derived context table's Google attribution.
        source = (root/'src/context.rs').read_text()
        header = re.search(r'// Copyright[^\n]*\n(?://[^\n]*\n)+', source)
        if header is None or 'Distributed under MIT license.' not in header[0]:
            raise ValueError('brotli-decompressor has a missing MIT attribution header')
        result.append({'path': 'src/context.rs (MIT attribution)', 'text': header[0]})
    if package['name'] == 'qcms':
        # qcms ships complete MIT terms in source headers rather than a LICENSE file.
        # Keep both Mozilla copyright variants and their original source association.
        for name in ('matrix.rs', 'chain.rs', 'transform.rs', 'iccread.rs', 'transform_util.rs'):
            source = (root/'src'/name).read_text()
            header = re.match(r'(?:[^\S\n]*//[^\n]*\n)+', source)
            if header is None or not all(term in header[0] for term in (
                    'Copyright', 'Permission is hereby granted', 'THE SOFTWARE IS PROVIDED')):
                raise ValueError(f'qcms has a missing or incomplete licence header: {name}')
            result.append({'path': f'src/{name} (licence header)', 'text': header[0]})
    if not result:
        raise ValueError(f"No licence/notice text found for {package['name']} {package['version']}")
    return result


def input_hashes(root):
    paths = [root / "Cargo.lock", root / "Cargo.toml", root / "rust-toolchain.toml", root / ".cargo/config.toml"]
    paths += sorted((root / "crates").glob("*/Cargo.toml"))
    paths += sorted((root / "licenses/release/upstream").glob("*"))
    paths += [root / "scripts/release/licenses.py"]
    return {path.relative_to(root).as_posix(): digest(path.read_bytes()) for path in paths if path.is_file()}


def cargo_output(root, *args):
    return subprocess.check_output([os.environ.get("CARGO", "cargo"), *args], cwd=root, text=True,
                                   env={**os.environ, "CARGO_TERM_COLOR": "never"})


def selected_packages(tree):
    """Read package identities and enabled features from Cargo tree output."""
    selected = {}
    for line in tree.splitlines():
        if not line.strip():
            continue
        match = re.match(r"^([A-Za-z0-9_-]+) v([^\s|]+)", line.split("|", 1)[0])
        if not match or "|" not in line or "\x1b" in line:
            raise ValueError(f"Unparseable Cargo tree line: {line!r}")
        selected.setdefault(match.groups(), set()).update(line.split("|", 1)[1].replace(" (*)", "").split(","))
    if not selected:
        raise ValueError("Cargo tree produced an empty dependency inventory")
    return selected


def rust_sysroot(root):
    return Path(subprocess.check_output([os.environ.get("RUSTC", "rustc"), "--print", "sysroot"],
                                        cwd=root, text=True).strip())


def rust_runtime(root):
    """Read the standard-library notices that the pinned rustc component ships."""
    docs = rust_sysroot(root) / "share/doc/rust"
    files = {"COPYRIGHT-library.html": (docs / "COPYRIGHT-library.html").read_bytes()}
    files.update({f"licenses/{path.name}": path.read_bytes() for path in sorted((docs / "licenses").glob("*.txt"))})
    return files


def collect(root, target):
    """Resolve the target closure and render its notices; returns the shipped files."""
    if not re.search(rf'^channel\s*=\s*"{re.escape(RUST)}"', (root / "rust-toolchain.toml").read_text(), re.M):
        raise ValueError("Review Rust runtime notices for the changed toolchain before regeneration")
    cargo_output(root, "fetch", "--locked", "--target", target)
    metadata = json.loads(cargo_output(root, "metadata", "--locked", "--offline", "--format-version", "1", "--filter-platform", target, "--features", ",".join(FEATURES[1:])))
    tree = cargo_output(root, "tree", "--locked", "--offline", "--target", target,
                        "-p", "kaban", "-p", "kaban-index", "--edges", "normal,build",
                        "--prefix", "none", "--format", "{p}|{f}", "--features", ",".join(FEATURES[1:]))
    selected = selected_packages(tree)
    if not set(SELECTED) <= {name for name, _ in selected}:
        raise ValueError("Cargo tree did not include both selected release packages")
    available = {(package["name"], package["version"]) for package in metadata["packages"]}
    if not set(selected) <= available:
        raise ValueError("Cargo tree and metadata disagree about resolved package identities")
    upstream = root / "licenses/release/upstream"
    packages = []
    # Deduplicate identical complete texts while retaining every crate/path association.
    texts = {}
    associations = {}
    for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"])):
        if (package["name"], package["version"]) not in selected or not package.get("source"):
            continue
        if not package["source"].startswith("registry+"):
            raise ValueError(f"Unreviewed dependency source: {package['source']}")
        records = []
        for notice in collect_notices(package, upstream):
            text_hash = digest(notice["text"].encode())
            texts[text_hash] = notice["text"]
            associations.setdefault(text_hash, []).append(f"{package['name']} {package['version']}: {notice['path']}")
            records.append({"path": notice["path"], "sha256": text_hash})
        packages.append({"name": package["name"], "version": package["version"],
                         "license": package["license"], "repository": package.get("repository"),
                         "source": package["source"], "features": sorted(selected[(package["name"], package["version"])] - {""}), "notices": records})
    if not packages:
        raise ValueError("Cargo tree selected no registry dependencies")
    bundle = [
        "Kaban third-party licences and notices\n",
        f"Target: {target}; default + PDF + Markdown splitting; cargo build --release --locked -p kaban -p kaban-index --features {','.join(FEATURES[1:])}\n",
        "Inventory includes normal and build/proc-macro dependencies conservatively; dev-only dependencies are excluded.\n",
        "Licence alternatives: prefer MIT, otherwise Apache-2.0 where offered; retain all AND obligations.\n",
        "Additional terms for bundled native code are retained alongside crate terms.\n",
        "Retaining alternative texts does not select additional alternative licences.\n",
        "htmlescape: Apache-2.0 selected. rust-stemmers: MIT wrapper and BSD-3-Clause algorithms.\n",
        "All upstream source and licence texts are unmodified.\n\n",
    ]
    for text_hash, text in texts.items():
        bundle += ["=" * 72 + "\n", "\n".join(associations[text_hash]) + "\n",
                   f"Text SHA256: {text_hash}\n\n", text, "\n"]
    runtime = rust_runtime(root)
    runtime_texts = {f"rust-{RUST}-COMPILER-BUILTINS-LICENSE.txt": (upstream / f"rust-{RUST}-COMPILER-BUILTINS-LICENSE.txt").read_text()}
    runtime_texts.update({f"rust-{RUST}-LICENSE-{name.removeprefix('licenses/')}": data.decode()
                          for name, data in runtime.items() if name.startswith("licenses/")})
    if target.endswith("musl"):
        runtime_texts["musl-1.2.5-COPYRIGHT"] = (upstream / "musl-1.2.5-COPYRIGHT").read_text()
    bundle += ["=" * 72 + "\nRuntime notices\n",
               "Rust runtime notices include the upstream standard-library dependency inventory conservatively.\n",
               "See the accompanying RUST_STANDARD_LIBRARY_NOTICES.html for its original notices.\n",
               "System shared libraries are supplied by the host; their binaries are not bundled.\n"]
    for name, text in runtime_texts.items():
        bundle += [f"\n--- {name} ---\n", text, "\n"]
    data = "".join(bundle).encode()
    runtime_html = runtime["COPYRIGHT-library.html"]
    manifest = {"schema": 1, "target": target, "packages": list(SELECTED), "features": FEATURES,
                "dependency_edges": ["normal", "build"], "inputs": input_hashes(root),
                "bundle_sha256": digest(data), "dependencies": packages,
                "files": {"THIRD_PARTY_LICENSES.txt": digest(data),
                          "RUST_STANDARD_LIBRARY_NOTICES.html": digest(runtime_html)},
                "runtime_notices": list(runtime_texts)}
    files = {"THIRD_PARTY_LICENSES.txt": data, "RUST_STANDARD_LIBRARY_NOTICES.html": runtime_html,
             "manifest.json": (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()}
    return manifest, {name: digest(data) for name, data in runtime.items()}, files


def reviewed(manifest):
    """The reviewable part of one target's notices: identities, licences and notice hashes."""
    return {f"{item['name']} {item['version']}": {"license": item["license"],
            "notices": {notice["path"]: notice["sha256"] for notice in item["notices"]}}
            for item in manifest["dependencies"]}


def generate(root):
    """Record the reviewed inventory for all four targets from cached Cargo sources."""
    targets, dependencies, runtimes = {}, {}, set()
    for target in TARGETS:
        manifest, runtime, _ = collect(root, target)
        runtimes.add(json.dumps(runtime, sort_keys=True))
        targets[target] = {"bundle_sha256": manifest["bundle_sha256"], "dependencies": len(manifest["dependencies"])}
        for key, record in reviewed(manifest).items():
            dependencies.setdefault(key, {**record, "targets": []})["targets"].append(target)
        print(f"Collected {target}: {len(manifest['dependencies'])} registry packages, "
              f"{targets[target]['bundle_sha256'][:12]} notice bundle")
    if len(runtimes) != 1:
        raise ValueError("Rust runtime notices changed during generation")
    records = []
    for key, record in sorted(dependencies.items()):
        name, version = key.split(" ", 1)
        if record["targets"] == list(TARGETS):
            del record["targets"]
        records.append({"name": name, "version": version, **record})
    inventory = {"schema": 1, "packages": list(SELECTED), "features": FEATURES,
                 "dependency_edges": ["normal", "build"], "inputs": input_hashes(root),
                 "rust_runtime": json.loads(runtimes.pop()), "targets": targets, "dependencies": records}
    (root / INVENTORY).write_text(json.dumps(inventory, indent=2, sort_keys=True) + "\n")
    print(f"Wrote {INVENTORY}: {len(records)} registry packages across {len(TARGETS)} targets")


def check(root, target):
    """Refuse a stale inventory without invoking Cargo or the network."""
    inventory = json.loads((root / INVENTORY).read_text())
    if (inventory.get("schema"), inventory.get("packages"), inventory.get("features"), inventory.get("dependency_edges")) != (1, list(SELECTED), FEATURES, ["normal", "build"]):
        raise ValueError("Licence inventory configuration does not match the release profile")
    if target not in inventory.get("targets", {}):
        raise ValueError(f"Licence inventory has no reviewed closure for {target}")
    if inventory["inputs"] != input_hashes(root):
        raise ValueError("Licence inventory inputs changed; regenerate and review before publishing")
    print(f"Licence inventory verified: {target} ({inventory['targets'][target]['dependencies']} registry packages)")
    return inventory


def stage(root, target, output):
    """Regenerate one target's archive notices and refuse any drift from the inventory."""
    inventory = check(root, target)
    manifest, runtime, files = collect(root, target)
    expected = {f"{item['name']} {item['version']}": {"license": item["license"], "notices": item["notices"]}
                for item in inventory["dependencies"] if target in item.get("targets", TARGETS)}
    actual = reviewed(manifest)
    drift = sorted(f"{key}: {'unreviewed' if key not in expected else 'removed' if key not in actual else 'changed'}"
                   for key in expected.keys() | actual.keys() if expected.get(key) != actual.get(key))
    drift += [f"Rust runtime {name}: changed" for name in sorted(runtime.keys() | inventory["rust_runtime"].keys())
              if runtime.get(name) != inventory["rust_runtime"].get(name)]
    if not drift and manifest["bundle_sha256"] != inventory["targets"][target]["bundle_sha256"]:
        drift.append("notice bundle bytes changed")
    if drift:
        raise ValueError("Collected notices differ from the reviewed inventory: " + "; ".join(drift[:20]))
    output.mkdir(parents=True)
    for name, data in files.items():
        (output / name).write_bytes(data)
    print(f"Staged {target} notices: {', '.join(f'{name} ({len(data)} bytes)' for name, data in sorted(files.items()))}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("generate", "check", "stage"))
    parser.add_argument("--target", choices=TARGETS, help="required for check and stage")
    parser.add_argument("--output", type=Path, help="new third-party directory for stage")
    args = parser.parse_args()
    if args.command != "generate" and not args.target or (args.command == "stage") != bool(args.output):
        parser.error("check needs --target; stage needs --target and --output; generate covers all targets")
    try:
        if args.command == "generate":
            generate(ROOT)
        elif args.command == "check":
            check(ROOT, args.target)
        else:
            stage(ROOT, args.target, args.output)
    except (ValueError, OSError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"licences: {error}\n")


if __name__ == "__main__":
    main()
