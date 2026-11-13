#!/usr/bin/env python3
"""Collect the selected packages' upstream notices; check committed bundles offline.

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
SUPPLEMENTS = {
    "alloc-stdlib": ("alloc-stdlib-0.3.0-LICENSE",),
    "taffy": ("taffy-0.14.0-LICENSE",),
    "brotli-decompressor": ("brotli-9.0.0-LICENSE.MIT",),
    "datasketches": ("datasketches-0.2.0-LICENSE", "datasketches-0.2.0-NOTICE"),
    "htmlescape": ("htmlescape-0.3.1-LICENSE-APACHE",),
    "ownedbytes": ("ownedbytes-0.9.0-LICENSE",),
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


def generate(root, target):
    if not re.search(r'^channel\s*=\s*"1\.99\.0"', (root / "rust-toolchain.toml").read_text(), re.M):
        raise ValueError("Review Rust runtime notices for the changed toolchain before regeneration")
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
    runtime_names = ["rust-1.99.0-COMPILER-BUILTINS-LICENSE.txt"]
    runtime_names += sorted(path.name for path in upstream.glob("rust-1.99.0-LICENSE-*.txt"))
    if target.endswith("musl"):
        runtime_names.append("musl-1.2.5-COPYRIGHT")
    bundle += ["=" * 72 + "\nRuntime notices\n",
               "Rust runtime notices include the upstream standard-library dependency inventory conservatively.\n",
               "See the accompanying RUST_STANDARD_LIBRARY_NOTICES.html for its original notices.\n",
               "System shared libraries are supplied by the host; their binaries are not bundled.\n"]
    for name in runtime_names:
        bundle += [f"\n--- {name} ---\n", (upstream / name).read_text(), "\n"]
    output = root / "licenses/release" / target
    output.mkdir(parents=True, exist_ok=True)
    data = "".join(bundle).encode()
    (output / "THIRD_PARTY_LICENSES.txt").write_bytes(data)
    runtime_html = (upstream / "rust-1.99.0-COPYRIGHT-library.html").read_bytes()
    (output / "RUST_STANDARD_LIBRARY_NOTICES.html").write_bytes(runtime_html)
    manifest = {"schema": 1, "target": target, "packages": list(SELECTED), "features": FEATURES,
                "dependency_edges": ["normal", "build"], "inputs": input_hashes(root),
                "bundle_sha256": digest(data), "dependencies": packages,
                "files": {"THIRD_PARTY_LICENSES.txt": digest(data),
                          "RUST_STANDARD_LIBRARY_NOTICES.html": digest(runtime_html)},
                "runtime_notices": runtime_names}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"Generated {target}: {len(packages)} registry packages, {len(data)} notice bytes")


def check(root, target):
    """Refuse stale or damaged package notices without invoking Cargo or the network."""
    output = root / "licenses/release" / target
    manifest = json.loads((output / "manifest.json").read_text())
    if (manifest.get("schema"), manifest.get("target"), manifest.get("packages"), manifest.get("features"), manifest.get("dependency_edges")) != (1, target, list(SELECTED), FEATURES, ["normal", "build"]):
        raise ValueError("Licence bundle configuration does not match the release profile")
    if manifest["inputs"] != input_hashes(root):
        raise ValueError("Licence bundle inputs changed; regenerate and review before publishing")
    if manifest["bundle_sha256"] != digest((output / "THIRD_PARTY_LICENSES.txt").read_bytes()):
        raise ValueError("Licence bundle checksum does not match the reviewed manifest")
    actual_files = {path.name for path in output.iterdir() if path.is_file()} - {"manifest.json"}
    if actual_files != set(manifest["files"]):
        raise ValueError("Licence bundle file list does not match the reviewed manifest")
    for name, expected in manifest["files"].items():
        if expected != digest((output / name).read_bytes()):
            raise ValueError(f"Licence bundle checksum mismatch: {name}")
    print(f"Licence bundle verified: {target} ({len(manifest['dependencies'])} registry packages)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("generate", "check"))
    parser.add_argument("--target", choices=TARGETS, required=True)
    args = parser.parse_args()
    try:
        {"generate": generate, "check": check}[args.command](ROOT, args.target)
    except (ValueError, OSError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"licences: {error}\n")


if __name__ == "__main__":
    main()
