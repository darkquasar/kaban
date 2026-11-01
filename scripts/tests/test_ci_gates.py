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
    def run_gate(self, *, tools=("nextest", "deny"), fail="nextest", color="always", inherited_fd=False, versions=None, cargo_override=None, args=(), missing_tool=None, target_present=True, marker="", runner_fail="", helper_fail=False, smoke_skip=False, size_table=False):
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
            producer = root / "tools/kaban-rust-scip"
            producer.mkdir(parents=True)
            (producer / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.91.0"\n')
            (root / "scripts/tests").mkdir()
            (root / "scripts/tests/test_fixture.py").write_text(
                'import unittest\nclass Helpers(unittest.TestCase):\n    def test_helper(self):\n        self.assertTrue(' + str(not helper_fail) + ')\n')
            (root / "scripts/mcp").mkdir()
            (root / "scripts/mcp/test_mcp_catalog.py").write_text('import unittest\nclass Catalog(unittest.TestCase):\n    def test_catalog(self): pass\n')
            (root / "scripts/mcp/mcp_smoke.py").write_text('print("smoke fixture")\n')
            (root / "hooks").mkdir()
            for package, features in (("kaban", "code-graph"), ("kaban-ingest", "pdf"), ("kaban-index", "")):
                directory = root / "crates" / package
                directory.mkdir(parents=True)
                (directory / "Cargo.toml").write_text(features + ' = []\n')
            bin_dir = root / "bin"
            bin_dir.mkdir()
            (bin_dir / "python3").symlink_to(sys.executable)
            helper = bin_dir / "fixture-tool"
            helper.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\nfrom pathlib import Path\n"
                "tool = Path(sys.argv[0]).name\n"
                "if tool == 'uname': print('x86_64' if '-m' in sys.argv else 'Linux')\n"
                "elif tool == 'rustc' and '--print' in sys.argv: print(str(Path(os.environ['TARGET_LIB']).parents[3]) if 'sysroot' in sys.argv else os.environ['TARGET_LIB'])\n"
                "elif tool == 'rustc': print(json.loads(os.environ['VERSIONS']).get('rustc', 'rustc ' + ('1.91.0' if Path.cwd().name == 'kaban-rust-scip' else '1.90.0') + ' (fixture)'))\n"
                "elif tool == 'stat': print(Path(sys.argv[-1]).stat().st_size)\n"
                "elif tool == 'node': print(json.loads(os.environ['VERSIONS']).get('node', 'v22.0.0'))\n"
                "else: print(tool + ' fixture')\n")
            helper.chmod(0o700)
            for name in ('uname', 'rustc', 'node', 'go', 'scip-go', 'cc', 'musl-gcc', 'git', 'strace', 'stat'):
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
                "import json, os, sys\nfrom pathlib import Path\n"
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
                "    defaults = {'cargo': 'cargo ' + ('1.91.0' if Path.cwd().name == 'kaban-rust-scip' else '1.90.0') + ' (fixture)', 'nextest': 'cargo-nextest 0.9.100', 'deny': 'cargo-deny 0.18.0'}\n"
                "    print(json.loads(os.environ['VERSIONS']).get(key, defaults.get(key, key + ' fixture')))\n"
                "elif args == ['--list']:\n"
                "    if os.environ['FAIL'] == 'list-probe': print('discovery failed', file=sys.stderr); sys.exit(38)\n"
                "    for tool in os.environ['TOOLS'].split(','):\n"
                "        if os.environ.get('CARGO_TERM_COLOR') == 'always':\n"
                "            tool = '\\x1b[1m\\x1b[96m' + tool + '\\x1b[0m'\n"
                "        print('    ' + tool)\n"
                "elif args[0] in ('nextest', 'test'):\n"
                "    suite = ('workspace' if '--workspace' in args else 'pdf' if 'pdf' in args else 'no-default') if args[0] == 'nextest' else ('doc' if '--doc' in args else 'producer')\n"
                "    print('runner: ' + suite + ' cwd=' + str(Path.cwd()) + ' target=' + os.environ.get('CARGO_TARGET_DIR', 'target'))\n"
                "    print('visible success: ' + suite)\n"
                "    if os.environ['MARKER'] == suite: print('NOT RUN: invented_branch: unavailable fixture', file=sys.stderr)\n"
                "    if os.environ['RUNNER_FAIL'] == suite or args[0] == os.environ['FAIL']: sys.exit(37)\n"
                "elif args[0] == os.environ['FAIL']:\n"
                "    sys.exit(37)\n"
                "elif args[0] == 'metadata':\n"
                "    libraries = [{'id': 'fixture-lib', 'name': 'kaban-rust-scip' if Path.cwd().name == 'kaban-rust-scip' else 'kaban-ingest', 'targets': [{'kind': ['lib'], 'doctest': True}]}]\n"
                "    if Path.cwd().name != 'kaban-rust-scip': libraries.append({'id': 'second-lib', 'name': 'fixture-core', 'targets': [{'kind': ['lib'], 'doctest': True}]})\n"
                "    print(json.dumps({'workspace_members': ['fixture-lib', 'second-lib'], 'packages': libraries}))\n"
                "elif args[0] == 'tree':\n"
                "    pass\n"
                "elif args[0] == 'build':\n"
                "    target = Path(os.environ.get('CARGO_TARGET_DIR', 'target'))\n"
                "    if '--target' in args: target /= args[args.index('--target') + 1]\n"
                "    (target / 'release').mkdir(parents=True, exist_ok=True)\n"
                "    for name in ('kaban', 'kaban-index'): (target / 'release' / name).write_bytes(b'fixture')\n"
                "elif args[0] not in ('fmt', 'clippy', 'nextest', 'deny', 'fetch'):\n"
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
                    "MARKER": marker,
                    "RUNNER_FAIL": runner_fail,
                    "CI_SKIP_SMOKE": "1" if smoke_skip else "0",
                    "KABAN_SIZE_TABLE": "1" if size_table else "0",
                    "CARGO_TARGET_DIR": str(root / "private target"),
                    "CHECK_FD": "1" if inherited_fd else "0",
                    "PRESERVED_VALUE": "retained environment",
                },
                input="retained standard input\n" if inherited_fd else None,
                capture_output=True,
                text=True,
                timeout=10,
            )
            return result, [json.loads(line) for line in (calls.read_text().splitlines() if calls.exists() else [])]

    def test_functional_runs_exact_suites_and_library_docs_without_artifacts(self):
        result, calls = self.run_gate(args=("functional",), tools=("nextest",), fail="", missing_tool="musl-gcc")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual([c for c in calls if c[:2] == ["nextest", "run"]], [
            ["nextest", "run", "--workspace", "--no-fail-fast", "--success-output", "immediate"],
            ["nextest", "run", "-p", "kaban-ingest", "--features", "pdf", "--success-output", "immediate"],
            ["nextest", "run", "-p", "kaban", "--no-default-features", "--success-output", "immediate"],
        ])
        self.assertIn(["test", "--doc", "--locked", "-p", "kaban-ingest", "-p", "fixture-core", "--", "--show-output"], calls)
        self.assertIn(["test", "--doc", "--locked", "-p", "kaban-ingest", "--features", "pdf", "--", "--show-output"], calls)
        self.assertFalse(any(c[0] in ("build", "fmt", "clippy", "deny") and "--version" not in c for c in calls))

    def test_every_selected_rust_suite_exposes_mandatory_markers_and_preserves_failure(self):
        for suite in ("workspace", "pdf", "no-default", "doc", "producer"):
            group = "producer" if suite == "producer" else "functional"
            with self.subTest(suite=suite):
                result, _ = self.run_gate(args=(group,), fail="", marker=suite)
                self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
                self.assertIn("mandatory_skips: 1\n  invented_branch\n", result.stdout)
                self.assertIn("visible success: " + suite, result.stdout)
                failed, _ = self.run_gate(args=(group,), fail="", marker=suite, runner_fail=suite)
                self.assertEqual(failed.returncode, 37, failed.stdout + failed.stderr)

    def test_producer_uses_own_toolchain_private_target_and_locked_commands(self):
        result, calls = self.run_gate(args=("producer",), tools=(), fail="", missing_tool="musl-gcc")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("/tools/kaban-rust-scip target=", result.stdout)
        self.assertIn("/private target/producer", result.stdout)
        self.assertEqual([c for c in calls if "--version" not in c], [
            ["fetch", "--locked"], ["fmt", "--all", "--check"],
            ["clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"],
            ["test", "--workspace", "--locked", "--", "--show-output"],
        ])
        wrong, calls = self.run_gate(args=("producer",), versions={"cargo": "cargo 1.90.0"}, fail="")
        self.assertEqual(wrong.returncode, 3, wrong.stdout + wrong.stderr)
        self.assertFalse(any(c[0] == "fetch" for c in calls))

    def test_helpers_need_no_cargo_and_propagate_test_failure(self):
        result, calls = self.run_gate(args=("helpers",), cargo_override="/missing/cargo")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, [])
        failed, _ = self.run_gate(args=("helpers",), helper_fail=True)
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)

    def test_default_and_all_keep_mandatory_groups_and_exclude_producer(self):
        default, calls = self.run_gate(fail="")
        explicit, selected = self.run_gate(args=("all",), fail="")
        self.assertEqual(default.returncode, 0, default.stdout + default.stderr)
        self.assertEqual(explicit.returncode, 0, explicit.stdout + explicit.stderr)
        self.assertEqual(calls, selected)
        for group in ("quality", "helpers", "functional", "gnu", "musl"):
            self.assertIn("==> " + group + "\n", default.stdout)
        self.assertFalse(any(c[0] == "fetch" or (c[0] == "test" and "--doc" not in c) for c in calls))
        self.assertTrue(any(c[0] == "build" and "--target" in c for c in calls))
        self.assertIn("smoke fixture", default.stdout)

    def test_artifact_groups_exclude_tests_and_preserve_optional_switches(self):
        for group in ("gnu", "musl"):
            with self.subTest(group=group):
                result, calls = self.run_gate(args=(group,), tools=(), fail="")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse(any(c[0] in ("nextest", "test", "fmt", "clippy", "deny") for c in calls))
                self.assertTrue(any(c[0] == "build" for c in calls))
                self.assertEqual(any("--target" in c for c in calls), group == "musl")
        skipped, _ = self.run_gate(args=("gnu",), fail="", smoke_skip=True, size_table=True)
        self.assertEqual(skipped.returncode, 3, skipped.stdout + skipped.stderr)
        self.assertIn("size-table: kaban(--no-default-features)", skipped.stdout)
        self.assertNotIn("smoke fixture", skipped.stdout)

    def test_quality_selects_lints_and_boundaries_without_runtime_or_artifacts(self):
        result, calls = self.run_gate(args=("quality",), tools=("deny",), fail="", missing_tool="musl-gcc")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        checks = [c for c in calls if "--version" not in c and c != ["--list"]]
        self.assertEqual([c[0] for c in checks], ["fmt", "clippy", "clippy", "clippy", "deny", "metadata", "tree"])
        self.assertIn(["clippy", "-p", "kaban-ingest", "--all-targets", "--no-default-features", "--", "-D", "warnings"], calls)

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
