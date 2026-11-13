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
        self.package.update(name="whatlang", version="99.0.0")
        (self.root / "LICENSE").write_text("New version terms\n")
        with self.assertRaisesRegex(ValueError, "supplement.*whatlang.*99.0.0"):
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

    def make_release_inputs(self):
        """Invent a repository, Rust sysroot and Cargo resolution for every target."""
        crate = self.root/'registry/invented'
        crate.mkdir(parents=True)
        (crate/'LICENSE').write_text('Invented complete licence')
        self.package['manifest_path'] = str(crate/'Cargo.toml')
        (self.root/'rust-toolchain.toml').write_text('[toolchain]\nchannel = "1.99.0"\n')
        (self.root/'Cargo.lock').write_text('original locked versions\n')
        upstream = self.root/'licenses/release/upstream'
        upstream.mkdir(parents=True)
        for name in ('rust-1.99.0-COMPILER-BUILTINS-LICENSE.txt', 'musl-1.2.5-COPYRIGHT'):
            (upstream/name).write_text('Invented runtime notice')
        docs = self.root/'sysroot/share/doc/rust'
        (docs/'licenses').mkdir(parents=True)
        (docs/'COPYRIGHT-library.html').write_text('<p>Invented standard library inventory</p>\n')
        (docs/'licenses/MIT.txt').write_text('Invented Rust MIT terms\n')
        self.extra = []
        packages = [{'name': n, 'version': '1.0.0', 'source': None} for n in ('kaban', 'kaban-index')]
        packages.append({**self.package, 'source': 'registry+https://example.invalid'})
        def cargo(root, *args):
            if args[0] == 'fetch':
                return ''
            if args[0] == 'metadata':
                return json.dumps({'packages': packages + self.extra})
            tree = 'kaban v1.0.0|default\nkaban-index v1.0.0|default\n'
            if '--features' in args and args[args.index('--features')+1] == 'kaban-ingest/pdf,kaban-ingest/md-splitter':
                tree += 'invented v1.0.0|pdf,splitter-data\n'
            return tree + ''.join(f"{p['name']} v{p['version']}|default\n" for p in self.extra)
        for patcher in (patch.object(LICENSES, 'cargo_output', cargo),
                        patch.object(LICENSES, 'rust_sysroot', lambda root: self.root/'sysroot')):
            patcher.start()
            self.addCleanup(patcher.stop)
        LICENSES.generate(self.root)
        return self.root/'licenses/release/inventory.json'

    def test_generated_inventory_resolves_the_exact_full_profile(self):
        path = self.make_release_inputs()
        inventory = json.loads(path.read_text())
        self.assertEqual(inventory['features'], ['default', 'kaban-ingest/pdf', 'kaban-ingest/md-splitter'])
        self.assertEqual(set(inventory['targets']), set(LICENSES.TARGETS))
        [dependency] = inventory['dependencies']
        self.assertEqual((dependency['name'], dependency['license']), ('invented', 'MIT AND BSD-3-Clause'))
        self.assertEqual(list(dependency['notices']), ['LICENSE'])
        self.assertNotIn('targets', dependency)  # present in every target
        for target in LICENSES.TARGETS:
            LICENSES.check(self.root, target)

    def test_committed_inventory_holds_no_generated_notice_texts(self):
        self.make_release_inputs()
        files = {p.relative_to(self.root/'licenses/release').as_posix()
                 for p in (self.root/'licenses/release').rglob('*') if p.is_file()}
        self.assertEqual(files, {'inventory.json', 'upstream/rust-1.99.0-COMPILER-BUILTINS-LICENSE.txt',
                                 'upstream/musl-1.2.5-COPYRIGHT'})

    def test_stage_writes_the_complete_archive_notices(self):
        self.make_release_inputs()
        output = self.root/'stage/third-party'
        LICENSES.stage(self.root, 'x86_64-unknown-linux-musl', output)
        self.assertEqual({p.name for p in output.iterdir()},
                         {'manifest.json', 'THIRD_PARTY_LICENSES.txt', 'RUST_STANDARD_LIBRARY_NOTICES.html'})
        manifest = json.loads((output/'manifest.json').read_text())
        self.assertEqual((manifest['schema'], manifest['target'], manifest['dependency_edges']),
                         (1, 'x86_64-unknown-linux-musl', ['normal', 'build']))
        self.assertEqual(manifest['files'], {name: LICENSES.digest((output/name).read_bytes())
                                             for name in ('THIRD_PARTY_LICENSES.txt', 'RUST_STANDARD_LIBRARY_NOTICES.html')})
        self.assertEqual((output/'RUST_STANDARD_LIBRARY_NOTICES.html').read_text(),
                         '<p>Invented standard library inventory</p>\n')
        bundle = (output/'THIRD_PARTY_LICENSES.txt').read_text()
        for text in ('Invented complete licence', 'Invented Rust MIT terms', 'Invented runtime notice',
                     '--- rust-1.99.0-LICENSE-MIT.txt ---', '--- musl-1.2.5-COPYRIGHT ---'):
            self.assertIn(text, bundle)

    def test_check_rejects_changed_lock(self):
        self.make_release_inputs()
        (self.root/'Cargo.lock').write_text('different locked versions\n')
        with self.assertRaisesRegex(ValueError, 'inputs changed'):
            LICENSES.check(self.root, LICENSES.TARGETS[0])

    def test_stage_refuses_notices_that_differ_from_the_reviewed_inventory(self):
        self.make_release_inputs()
        output = self.root/'stage/third-party'
        for change in ('licence text', 'new dependency', 'runtime notice'):
            with self.subTest(change=change):
                if change == 'licence text':
                    (self.root/'registry/invented/LICENSE').write_text('Changed upstream licence')
                elif change == 'new dependency':
                    (self.root/'registry/invented/LICENSE').write_text('Invented complete licence')
                    extra = self.root/'extra'
                    extra.mkdir()
                    (extra/'LICENSE').write_text('Unreviewed terms')
                    self.extra.append({'name': 'unreviewed', 'version': '2.0.0', 'license': 'MIT',
                                       'manifest_path': str(extra/'Cargo.toml'),
                                       'source': 'registry+https://example.invalid'})
                else:
                    self.extra.clear()
                    (self.root/'sysroot/share/doc/rust/COPYRIGHT-library.html').write_text('changed')
                with self.assertRaisesRegex(ValueError, 'reviewed inventory'):
                    LICENSES.stage(self.root, LICENSES.TARGETS[0], output)
                self.assertFalse(output.exists())

    def test_check_refuses_inventory_with_unexpected_dependency_scope(self):
        path = self.make_release_inputs()
        inventory = json.loads(path.read_text())
        inventory['dependency_edges'] = ['dev']
        path.write_text(json.dumps(inventory))
        with self.assertRaisesRegex(ValueError, 'configuration'):
            LICENSES.check(self.root, LICENSES.TARGETS[0])

    def test_repository_keeps_one_copy_of_each_licence_input(self):
        release = Path(__file__).resolve().parents[2]/'licenses/release'
        files = [p for p in release.rglob('*') if p.is_file()]
        self.assertFalse([p.name for p in files if p.suffix == '.html'])
        self.assertFalse([p.name for p in release.iterdir() if p.is_dir() and p.name != 'upstream'])
        contents = [p.read_bytes() for p in files]
        self.assertEqual(len(contents), len(set(contents)))
        self.assertLess(sum(map(len, contents)), 1_000_000)

if __name__ == "__main__":
    unittest.main()
