"""The Tart image stages authoritative setup inputs without overwriting builds."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
INPUTS = (
    "mise.toml", "mise.lock", "rust-toolchain.toml",
    "tools/kaban-rust-scip/rust-toolchain.toml", ".cargo/config.toml",
    "scripts/ci/install-go.sh", "scripts/ci/install-scip-go.sh",
    "scripts/ci/preflight.py",
)


class TartInputs(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.repo = self.directory / "checkout"
        shutil.copytree(ROOT / "runners/tart", self.repo / "runners/tart")
        for name in INPUTS:
            file = self.repo / name
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text("invented setup input: " + name + "\n")
        (self.repo / "private-token").write_text("must not enter the image")

    def prepare(self, output):
        return subprocess.run(
            [sys.executable, str(self.repo / "runners/tart/prepare-inputs.py"), str(output)],
            capture_output=True, text=True,
        )

    def test_stage_uses_current_checkout_inputs_and_excludes_other_source(self):
        output = self.directory / "build"
        result = self.prepare(output)
        self.assertEqual(result.returncode, 0, result.stderr)
        hashes = json.loads((output / "inputs/source-hashes.json").read_text())
        self.assertEqual(set(hashes), set(INPUTS))
        for name in INPUTS:
            self.assertEqual((output / "inputs" / name).read_bytes(), (self.repo / name).read_bytes())
            self.assertEqual(hashes[name], hashlib.sha256((self.repo / name).read_bytes()).hexdigest())
        self.assertFalse((output / "inputs/private-token").exists())
        self.assertEqual((output / "provision-tools.sh").read_bytes(), (ROOT / "runners/tart/provision-tools.sh").read_bytes())
        self.assertTrue((output / "kaban-tools.pkr.hcl").is_file())

    def test_existing_build_is_preserved(self):
        output = self.directory / "build"
        output.mkdir()
        (output / "sentinel").write_text("previous build")
        result = self.prepare(output)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(output.iterdir()), [output / "sentinel"])
        self.assertEqual((output / "sentinel").read_text(), "previous build")

    def test_missing_input_fails_before_creating_build_directory(self):
        (self.repo / "mise.lock").unlink()
        output = self.directory / "build"
        result = self.prepare(output)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
