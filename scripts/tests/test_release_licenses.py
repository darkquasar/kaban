"""Check release notices against invented upstream packages and package tampering."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "release_licenses", Path(__file__).resolve().parents[1] / "release/licenses.py"
)
LICENSES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LICENSES)


class ReleaseLicenses(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.package = {
            "name": "invented", "version": "1.0.0", "license": "MIT AND BSD-3-Clause",
            "manifest_path": str(self.root / "Cargo.toml"),
        }
        (self.root / "Cargo.toml").write_text('[package]\nname = "invented"\n')

    def test_cargo_tree_keeps_features_separate_from_package_identity(self):
        result = LICENSES.selected_packages(
            "wrapper v1.0.0|default,ingest\n"
            "helper v2.0.0 (proc-macro)|default\n"
            "wrapper v1.0.0|default,extra (*)\n"
        )
        self.assertEqual(result, {
            ("wrapper", "1.0.0"): {"default", "ingest", "extra"},
            ("helper", "2.0.0"): {"default"},
        })

    def test_inventory_refuses_empty_or_unparseable_cargo_output(self):
        for output in ("", "\x1b[1mkaban v1.0.0|default\x1b[0m\n", "unexpected cargo tree format\n"):
            with self.subTest(output=output):
                with self.assertRaisesRegex(ValueError, "Cargo tree"):
                    LICENSES.selected_packages(output)

    def test_preserves_notices_for_nested_code_and_all_licence_obligations(self):
        (self.root / "LICENSE-MIT").write_text("Permission from wrapper author\n")
        (self.root / "NOTICE").write_text("Required attribution\n")
        nested = self.root / "src/algorithm"
        nested.mkdir(parents=True)
        (nested / "BSD-LICENSE.txt").write_text("Algorithm BSD conditions\n")
        result = LICENSES.collect_notices(self.package, self.root / "upstream")
        self.assertEqual({item['path'] for item in result}, {
            'LICENSE-MIT', 'NOTICE', 'src/algorithm/BSD-LICENSE.txt',
        })
        self.assertIn('Algorithm BSD conditions\n', [item['text'] for item in result])

    def test_package_without_notices_requires_explicit_supplement(self):
        with self.assertRaisesRegex(ValueError, 'invented.*1.0.0'):
            LICENSES.collect_notices(self.package, self.root / "upstream")


    def test_retains_declared_license_file_with_nonstandard_name(self):
        (self.root / "terms.txt").write_text("Custom upstream terms\n")
        self.package["license_file"] = "terms.txt"
        result = LICENSES.collect_notices(self.package, self.root / "upstream")
        self.assertIn("terms.txt", [item["path"] for item in result])

    def test_upgraded_dependency_cannot_reuse_an_old_supplement(self):
        self.package.update(name="rmcp", version="99.0.0")
        (self.root / "LICENSE").write_text("New version terms\n")
        with self.assertRaisesRegex(ValueError, "supplement.*rmcp.*99.0.0"):
            LICENSES.collect_notices(self.package, self.root / "upstream")

    def test_qcms_preserves_complete_embedded_copyright_variants(self):
        self.package.update(name='qcms', license='MIT')
        source = self.root/'src'
        source.mkdir()
        expected = []
        for name in ('matrix.rs', 'chain.rs', 'transform.rs', 'iccread.rs', 'transform_util.rs'):
            text = '// Copyright ' + ('Corporation' if name == 'chain.rs' else 'Foundation') + '\n// Permission is hereby granted invented terms\n// THE SOFTWARE IS PROVIDED invented disclaimer\n'
            (source/name).write_text(text + '\nfn invented() {}\n')
            expected.append({'path': 'src/' + name + ' (licence header)', 'text': text})
        self.assertEqual(LICENSES.collect_notices(self.package, self.root/'upstream'), expected)
        (source/'chain.rs').write_text('// incomplete header\n')
        with self.assertRaisesRegex(ValueError, 'qcms.*header'):
            LICENSES.collect_notices(self.package, self.root/'upstream')

    def test_brotli_decompressor_keeps_google_attribution_and_complete_terms(self):
        self.package.update(name='brotli-decompressor', version='6.0.1')
        (self.root/'LICENSE').write_text('Invented Dropbox BSD terms')
        (self.root/'src').mkdir()
        header = '// Copyright 2013 Google Inc. All Rights Reserved.\n//\n// Distributed under MIT license.\n// See file LICENSE for detail or copy at https://opensource.org/licenses/MIT\n//\n'
        (self.root/'src/context.rs').write_text('#![allow(non_snake_case)]\n\n' + header + '\nfn example() {}')
        upstream = self.root/'upstream'
        upstream.mkdir()
        (upstream/'brotli-9.0.0-LICENSE.MIT').write_text('Invented complete MIT terms')
        result = LICENSES.collect_notices(self.package, upstream)
        self.assertIn(header, [item['text'] for item in result])
        self.assertIn('Invented complete MIT terms', [item['text'] for item in result])
        self.assertIn('Invented Dropbox BSD terms', [item['text'] for item in result])

    def test_generated_inventory_resolves_the_exact_full_profile(self):
        (self.root/'LICENSE').write_text('Invented complete licence')
        (self.root/'rust-toolchain.toml').write_text('[toolchain]\nchannel = "1.99.0"\n')
        upstream = self.root/'licenses/release/upstream'
        upstream.mkdir(parents=True)
        for name in ('rust-1.99.0-COMPILER-BUILTINS-LICENSE.txt', 'rust-1.99.0-COPYRIGHT-library.html'):
            (upstream/name).write_text('Invented runtime notice')
        packages = [{'name': n, 'version': '1.0.0', 'source': None} for n in ('kaban', 'kaban-index')]
        packages.append({**self.package, 'source': 'registry+https://example.invalid'})
        def cargo(root, *args):
            if args[0] == 'metadata':
                return json.dumps({'packages': packages})
            tree = 'kaban v1.0.0|default\nkaban-index v1.0.0|default\n'
            if '--features' in args and args[args.index('--features')+1] == 'kaban-ingest/pdf,kaban-ingest/md-splitter':
                tree += 'invented v1.0.0|pdf,splitter-data\n'
            return tree
        with patch.object(LICENSES, 'cargo_output', cargo):
            LICENSES.generate(self.root, 'aarch64-apple-darwin')
        manifest = json.loads((self.root/'licenses/release/aarch64-apple-darwin/manifest.json').read_text())
        self.assertEqual(manifest['features'], ['default', 'kaban-ingest/pdf', 'kaban-ingest/md-splitter'])
        self.assertEqual(manifest['dependencies'][0]['name'], 'invented')
        LICENSES.check(self.root, 'aarch64-apple-darwin')

    def make_checked_bundle(self):
        target = LICENSES.TARGETS[0]
        output = self.root / "licenses/release" / target
        output.mkdir(parents=True)
        data = b"Invented complete notices\n"
        (output / "THIRD_PARTY_LICENSES.txt").write_bytes(data)
        (output / "RUST_STANDARD_LIBRARY_NOTICES.html").write_text("<p>Runtime terms</p>\n")
        manifest = {"schema": 1, "target": target, "packages": ["kaban", "kaban-index"],
                    "features": ["default", "kaban-ingest/pdf", "kaban-ingest/md-splitter"], "dependency_edges": ["normal", "build"],
                    "inputs": LICENSES.input_hashes(self.root),
                    "bundle_sha256": LICENSES.digest(data), "dependencies": [],
                    "files": {path.name: LICENSES.digest(path.read_bytes()) for path in output.iterdir()}}
        path = output / "manifest.json"
        path.write_text(json.dumps(manifest))
        return target, output, manifest, path

    def test_check_rejects_changed_lock_and_damaged_notices(self):
        (self.root / "Cargo.lock").write_text("original locked versions\n")
        target, output, manifest, path = self.make_checked_bundle()
        LICENSES.check(self.root, target)
        (self.root / "Cargo.lock").write_text("different locked versions\n")
        with self.assertRaisesRegex(ValueError, "inputs changed"):
            LICENSES.check(self.root, target)
        (self.root / "Cargo.lock").write_text("original locked versions\n")
        (output / "THIRD_PARTY_LICENSES.txt").write_text("damaged")
        with self.assertRaisesRegex(ValueError, "checksum"):
            LICENSES.check(self.root, target)

    def test_check_rejects_damage_to_the_separate_runtime_notices(self):
        target, output, manifest, path = self.make_checked_bundle()
        (output / "RUST_STANDARD_LIBRARY_NOTICES.html").write_text("changed runtime terms")
        with self.assertRaisesRegex(ValueError, "checksum"):
            LICENSES.check(self.root, target)

    def test_check_refuses_inventory_with_unexpected_dependency_scope(self):
        target, output, manifest, path = self.make_checked_bundle()
        manifest["dependency_edges"] = ["dev"]
        path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "configuration"):
            LICENSES.check(self.root, target)

if __name__ == "__main__":
    unittest.main()
