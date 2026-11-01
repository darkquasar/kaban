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
    def run_gate(self, *, tools=("nextest", "deny"), fail="nextest", color="always", inherited_fd=False, versions=None, cargo_override=None, args=(), missing_tool=None, target_present=True):
        with tempfile.TemporaryDirectory(prefix="kaban-ci-tools-") as scratch:
            root = Path(scratch)
            script = root / "scripts/ci/ci.sh"
            script.parent.mkdir(parents=True)
            shutil.copyfile(ROOT / "scripts/ci/ci.sh", script)
            shutil.copyfile(ROOT / "scripts/ci/preflight.py", script.parent / "preflight.py")
            (root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.90.0"\n')
            (root / "mise.toml").write_text(
                '[tools]\npython = "' + sys.version.split()[0] + '"\nnode = "22.0.0"\n'
                '"aqua:nextest-rs/nextest/cargo-nextest" = "0.9.100"\n'
                '"aqua:EmbarkStudios/cargo-deny" = "0.18.0"\n')
            (root / ".cargo").mkdir()
            (root / ".cargo/config.toml").write_text(
                '[target.x86_64-unknown-linux-musl]\nlinker = ' + json.dumps(str(root / 'bin/musl-gcc')) + '\n')
            if target_present:
                (root / "sysroot/lib/rustlib/x86_64-unknown-linux-musl/lib").mkdir(parents=True)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            (bin_dir / "python3").symlink_to(sys.executable)
            helper = bin_dir / "fixture-tool"
            helper.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\nfrom pathlib import Path\n"
                "tool = Path(sys.argv[0]).name\n"
                "if tool == 'uname': print('x86_64' if '-m' in sys.argv else 'Linux')\n"
                "elif tool == 'rustc' and '--print' in sys.argv: print(os.environ['TARGET_LIB'])\n"
                "elif tool == 'rustc': print(json.loads(os.environ['VERSIONS']).get('rustc', 'rustc 1.90.0 (fixture)'))\n"
                "elif tool == 'node': print(json.loads(os.environ['VERSIONS']).get('node', 'v22.0.0'))\n"
                "else: print(tool + ' fixture')\n")
            helper.chmod(0o700)
            for name in ('uname', 'rustc', 'node', 'go', 'scip-go', 'cc', 'musl-gcc', 'git', 'strace'):
                (bin_dir / name).symlink_to(helper)
            if missing_tool:
                (bin_dir / missing_tool).unlink()
            # Dependency inspection is exercised elsewhere; keep this external jq boundary deterministic.
            (bin_dir / "jq").write_text('#!/bin/sh\ncat >/dev/null\n')
            (bin_dir / "jq").chmod(0o700)
            # Unrelated dependency checks can pass before the sentinel release build.
            (script.parent / "deps_allowlist.sh").write_text("exit 0\n")
            cargo = root / "effective cargo"
            calls = root / "calls.jsonl"
            cargo.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "with open(os.environ['CALLS'], 'a') as log:\n"
                "    log.write(json.dumps(args) + '\\n')\n"
                "if args[0] == 'fmt' and '--version' not in args and os.environ['CHECK_FD'] == '1':\n"
                "    try: os.fstat(142)\n"
                "    except OSError: pass\n"
                "    else: sys.exit(56)\n"
                "    print('input: ' + sys.stdin.readline().rstrip())\n"
                "    print('env: ' + os.environ['PRESERVED_VALUE'], file=sys.stderr)\n"
                "if '--version' in args:\n"
                "    key = 'cargo' if args == ['--version'] else args[0]\n"
                "    if os.environ['FAIL'] == key + '-version':\n"
                "        print('probe stdout'); print('probe stderr', file=sys.stderr); sys.exit(39)\n"
                "    defaults = {'cargo': 'cargo 1.90.0 (fixture)', 'nextest': 'cargo-nextest 0.9.100', 'deny': 'cargo-deny 0.18.0'}\n"
                "    print(json.loads(os.environ['VERSIONS']).get(key, defaults.get(key, key + ' fixture')))\n"
                "elif args == ['--list']:\n"
                "    if os.environ['FAIL'] == 'list-probe': print('discovery failed', file=sys.stderr); sys.exit(38)\n"
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
            command = [BASH, str(script), *args]
            if inherited_fd:
                command = [BASH, "-c", 'exec 142>/dev/null\nexec "$@"', "bash", *command]
            result = subprocess.run(
                command,
                env={
                    **os.environ,
                    "BASH_ENV": os.devnull,
                    "CARGO": str(cargo) if cargo_override is None else cargo_override,
                    "PATH": str(bin_dir) + os.pathsep + "/usr/bin:/bin",
                    "TARGET_LIB": str(root / "sysroot/lib/rustlib/x86_64-unknown-linux-musl/lib"),
                    "VERSIONS": json.dumps(versions or {}),
                    "CARGO_TERM_COLOR": color,
                    "RUSTC": str(bin_dir / "rustc"),
                    "SCIP_GO": str(bin_dir / "scip-go"),
                    "CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER": str(bin_dir / "musl-gcc"),
                    "CALLS": str(calls),
                    "TOOLS": ",".join(tools),
                    "FAIL": fail,
                    "CHECK_FD": "1" if inherited_fd else "0",
                    "PRESERVED_VALUE": "retained environment",
                },
                input="retained standard input\n" if inherited_fd else None,
                capture_output=True,
                text=True,
                timeout=10,
            )
            return result, [json.loads(line) for line in (calls.read_text().splitlines() if calls.exists() else [])]

    def test_installed_nextest_runs_and_preserves_failure_with_forced_color(self):
        result, calls = self.run_gate()
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertIn(["nextest", "run", "--workspace", "--no-fail-fast", "--success-output", "immediate"], calls)
        self.assertFalse(any(call[0] == "test" for call in calls))

    def test_installed_nextest_runs_without_color(self):
        result, calls = self.run_gate(color="never")
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertFalse(any(call[0] == "test" for call in calls))

    def test_inherited_high_descriptor_is_closed_without_losing_streams_environment_or_failure(self):
        result, calls = self.run_gate(inherited_fd=True)
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertIn("input: retained standard input", (result.stdout + result.stderr))
        self.assertIn("env: retained environment", result.stderr)
        self.assertIn(["fmt", "--all", "--check"], calls)
        self.assertTrue(any(call[0] == "nextest" for call in calls))

    def test_absent_nextest_stops_before_compilation_without_fallback(self):
        result, calls = self.run_gate(tools=("deny",))
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertFalse(any(call[0] in ("test", "fmt", "clippy", "build", "nextest") and "--version" not in call for call in calls))
        self.assertIn("not-run: cargo-nextest absent", (result.stdout + result.stderr))

    def test_installed_deny_runs_and_preserves_failure_with_forced_color(self):
        result, calls = self.run_gate(fail="deny")
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertIn(["deny", "check"], calls)
        self.assertFalse(any(call[0] == "test" for call in calls))

    def test_absent_deny_stops_before_compilation(self):
        result, calls = self.run_gate(tools=("nextest",), fail="build")
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("not-run: cargo-deny absent", (result.stdout + result.stderr))
        self.assertFalse(any(call[0] in ("test", "fmt", "clippy", "build", "deny") and "--version" not in call for call in calls))


    def test_wrong_required_version_stops_before_compilation(self):
        result, calls = self.run_gate(versions={"nextest": "cargo-nextest 0.0.1"})
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("0.9.100", result.stdout + result.stderr)
        self.assertFalse(any(call[0] in ("fmt", "clippy", "test", "build", "nextest")
                             and "--version" not in call for call in calls))



    def test_help_needs_no_tool_setup(self):
        result, calls = self.run_gate(cargo_override="/missing/cargo", args=("--help",))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Usage:", result.stdout)
        self.assertEqual(calls, [])

    def test_unknown_argument_runs_no_gates(self):
        result, calls = self.run_gate(args=("unknown",))
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertEqual(calls, [])

    def test_probe_child_failure_preserves_exit_and_both_streams(self):
        result, calls = self.run_gate(fail="nextest-version")
        self.assertEqual(result.returncode, 39, result.stdout + result.stderr)
        self.assertIn("probe stdout", result.stdout)
        self.assertIn("probe stderr", result.stderr)
        self.assertFalse(any(call[0] == "build" for call in calls))

    def test_invalid_explicit_cargo_never_falls_back(self):
        result, calls = self.run_gate(cargo_override="/missing/cargo")
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertEqual(calls, [])

    def test_wrong_pinned_versions_stop_before_compilation(self):
        for tool, version in (("cargo", "cargo 1.0.0"), ("deny", "cargo-deny 0.0.1"),
                              ("rustc", "rustc 1.0.0"), ("node", "v1.0.0")):
            with self.subTest(tool=tool):
                result, calls = self.run_gate(versions={tool: version})
                self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
                self.assertIn("expected", result.stderr)
                self.assertFalse(any(call[0] in ("fmt", "clippy", "test", "build", "nextest")
                                     and "--version" not in call for call in calls))

    def test_missing_musl_target_stops_before_compilation(self):
        result, calls = self.run_gate(target_present=False)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("musl target absent", result.stderr)
        self.assertFalse(any(call[0] == "build" for call in calls))

    def test_missing_musl_linker_stops_before_compilation(self):
        result, calls = self.run_gate(missing_tool="musl-gcc")
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("musl-gcc", result.stderr)
        self.assertFalse(any(call[0] == "build" for call in calls))


    def test_installed_cargo_discovery_failure_is_preserved(self):
        result, calls = self.run_gate(fail="list-probe")
        self.assertEqual(result.returncode, 38, result.stdout + result.stderr)
        self.assertIn("discovery failed", result.stderr)
        self.assertFalse(any(call[0] == "fmt" for call in calls))

    def test_coloured_version_output_is_accepted(self):
        result, calls = self.run_gate(versions={"nextest": "\x1b[32mcargo-nextest 0.9.100\x1b[0m"})
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertIn(["fmt", "--all", "--check"], calls)

if __name__ == "__main__":
    unittest.main()
