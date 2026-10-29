"""Exercise CI tool selection with invented Cargo commands, without compiling."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
BASH = shutil.which("bash")


class CiToolSelection(unittest.TestCase):
    def run_gate(self, *, tools=("nextest", "deny"), fail="nextest", color="always"):
        with tempfile.TemporaryDirectory(prefix="kaban-ci-tools-") as scratch:
            root = Path(scratch)
            script = root / "scripts/ci/ci.sh"
            script.parent.mkdir(parents=True)
            shutil.copyfile(ROOT / "scripts/ci/ci.sh", script)
            # Unrelated dependency checks can pass before the sentinel release build.
            (script.parent / "deps_allowlist.sh").write_text("exit 0\n")
            cargo = root / "cargo"
            calls = root / "calls.jsonl"
            cargo.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "with open(os.environ['CALLS'], 'a') as log:\n"
                "    log.write(json.dumps(args) + '\\n')\n"
                "if args == ['--list']:\n"
                "    for tool in os.environ['TOOLS'].split(','):\n"
                "        if os.environ.get('CARGO_TERM_COLOR') == 'always':\n"
                "            tool = '\\x1b[1m\\x1b[96m' + tool + '\\x1b[0m'\n"
                "        print('    ' + tool)\n"
                "elif args[0] == os.environ['FAIL']:\n"
                "    sys.exit(37)\n"
                "elif args[0] == 'test':\n"
                "    sys.exit(41)\n"
                "elif args[0] == 'metadata':\n"
                "    print('{\"packages\": []}')\n"
                "elif args[0] == 'tree':\n"
                "    pass\n"
                "elif args[0] not in ('fmt', 'clippy', 'nextest'):\n"
                "    sys.exit(97)\n"
            )
            cargo.chmod(0o700)
            result = subprocess.run(
                [BASH, str(script)],
                env={
                    **os.environ,
                    "BASH_ENV": os.devnull,
                    "CARGO": str(cargo),
                    "CARGO_TERM_COLOR": color,
                    "CALLS": str(calls),
                    "TOOLS": ",".join(tools),
                    "FAIL": fail,
                },
                capture_output=True,
                text=True,
                timeout=10,
            )
            return result, [json.loads(line) for line in calls.read_text().splitlines()]

    def test_installed_nextest_runs_and_preserves_failure_with_forced_color(self):
        result, calls = self.run_gate()
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertIn(["nextest", "run", "--workspace", "--no-fail-fast", "--success-output", "immediate"], calls)
        self.assertFalse(any(call[0] == "test" for call in calls))

    def test_installed_nextest_runs_without_color(self):
        result, calls = self.run_gate(color="never")
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertFalse(any(call[0] == "test" for call in calls))

    def test_absent_nextest_uses_fallback_and_preserves_failure(self):
        result, calls = self.run_gate(tools=("deny",))
        self.assertEqual(result.returncode, 41, result.stdout + result.stderr)
        self.assertIn(["test", "--workspace"], calls)
        self.assertIn("not-run: cargo-nextest absent", result.stdout)

    def test_installed_deny_runs_and_preserves_failure_with_forced_color(self):
        result, calls = self.run_gate(fail="deny")
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertIn(["deny", "check"], calls)
        self.assertFalse(any(call[0] == "test" for call in calls))

    def test_absent_deny_is_reported_and_not_invoked(self):
        result, calls = self.run_gate(tools=("nextest",), fail="build")
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertIn("not-run: cargo-deny absent", result.stdout)
        self.assertFalse(any(call[0] == "deny" for call in calls))


if __name__ == "__main__":
    unittest.main()
