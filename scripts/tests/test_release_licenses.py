"""Check release notices against invented upstream packages and package tampering."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

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

    def make_checked_bundle(self):
        target = LICENSES.TARGETS[0]
        output = self.root / "licenses/release" / target
        output.mkdir(parents=True)
        data = b"Invented complete notices\n"
        (output / "THIRD_PARTY_LICENSES.txt").write_bytes(data)
        (output / "RUST_STANDARD_LIBRARY_NOTICES.html").write_text("<p>Runtime terms</p>\n")
        manifest = {"schema": 1, "target": target, "packages": ["kaban", "kaban-index"],
                    "features": "default", "dependency_edges": ["normal", "build"],
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
