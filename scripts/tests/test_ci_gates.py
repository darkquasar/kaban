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
    def run_gate(self, *, tools=("nextest", "deny"), fail="nextest", color="always", inherited_fd=False, versions=None, cargo_override=None, args=(), missing_tool=None, target_present=True, marker="", runner_fail="", helper_fail=False, smoke_skip=False, size_table=False, artifact_scenario="", artifact_sizes=None, repeats=1, artifact_root=False, summary=False, isolation_split=False, hosted=True, isolation_probe="normal"):
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
            (root / "scripts/mcp/mcp_smoke.py").write_text(
                'import sys\nfrom pathlib import Path\ndata=Path(sys.argv[1]).read_bytes()\n'
                'print("smoke fixture: " + data[:24].decode())\n'
                'sys.exit(0 if data.startswith(b"default") else 51)\n')
            (root / "hooks").mkdir()
            for package, features in (("kaban", "code-graph"), ("kaban-ingest", "pdf"), ("kaban-index", "")):
                directory = root / "crates" / package
                directory.mkdir(parents=True)
                (directory / "Cargo.toml").write_text(features + ' = []\n' + ('md-splitter = []\n' if package == 'kaban-ingest' else ''))
            bin_dir = root / "bin"
            bin_dir.mkdir()
            (bin_dir / "python3").symlink_to(sys.executable)
            policy = root / "namespace-policy"
            policy.write_text("1")
            helper = bin_dir / "fixture-tool"
            helper.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\nfrom pathlib import Path\n"
                "tool = Path(sys.argv[0]).name\n"
                "if tool in ('sudo', 'sysctl', 'unshare'):\n"
                "    with open(os.environ['ISOLATION_CALLS'], 'a') as log: log.write(json.dumps([tool, *sys.argv[1:]]) + '\\n')\n"
                "if tool == 'uname': print('x86_64' if '-m' in sys.argv else 'Linux')\n"
                "elif tool == 'rustc' and '--print' in sys.argv: print(str(Path(os.environ['TARGET_LIB']).parents[3]) if 'sysroot' in sys.argv else os.environ['TARGET_LIB'])\n"
                "elif tool == 'rustc': print(json.loads(os.environ['VERSIONS']).get('rustc', 'rustc ' + ('1.91.0' if Path.cwd().name == 'kaban-rust-scip' else '1.90.0') + ' (fixture)'))\n"
                "elif tool == 'sudo':\n"
                "    args = sys.argv[1:]; args.remove('-n'); os.execvp(args[0], args)\n"
                "elif tool == 'sysctl':\n"
                "    policy = Path(os.environ['NAMESPACE_POLICY'])\n"
                "    if '-n' in sys.argv: print(policy.read_text())\n"
                "    else: policy.write_text(sys.argv[-1].split('=')[1])\n"
                "elif tool == 'unshare':\n"
                "    mode = os.environ['ISOLATION_PROBE']; restricted = Path(os.environ['NAMESPACE_POLICY']).read_text() == '1'\n"
                "    sys.exit(1 if mode == 'available-fails' or (restricted and mode != 'unavailable-works') else 0)\n"
                "elif tool == 'stat':\n"
                "    p=Path(sys.argv[-1]); print(p.stat().st_size)\n"
                "    if '/default/' in str(p):\n"
                "        if os.environ['ARTIFACT_SCENARIO'] == 'corrupt-stage': p.write_bytes(b'wrong'.ljust(p.stat().st_size, b'.'))\n"
                "        elif os.environ['ARTIFACT_SCENARIO'] == 'missing-stage': p.unlink()\n"
                "elif tool == 'node': print(json.loads(os.environ['VERSIONS']).get('node', 'v22.0.0'))\n"
                "else: print(tool + ' fixture')\n")
            helper.chmod(0o700)
            for name in ('uname', 'rustc', 'node', 'go', 'scip-go', 'cc', 'musl-gcc', 'git', 'strace', 'stat', 'sudo', 'sysctl', 'unshare'):
                (bin_dir / name).symlink_to(helper)
            if missing_tool:
                for name in ((missing_tool,) if isinstance(missing_tool, str) else missing_tool):
                    (bin_dir / name).unlink()
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
                "elif args[:2] == ['nextest', 'list']:\n"
                "    if os.environ['FAIL'] == 'isolation-build': sys.exit(37)\n"
                "    print(json.dumps({'fixture': 'compiled once'}))\n"
                "elif args[0] in ('nextest', 'test'):\n"
                "    suite = ('workspace' if '--workspace' in args else 'pdf' if 'pdf' in args else 'no-default') if args[0] == 'nextest' else ('doc' if '--doc' in args else 'producer')\n"
                "    if '--binaries-metadata' in args:\n"
                "        assert json.loads(Path(args[args.index('--binaries-metadata') + 1]).read_text()) == {'fixture': 'compiled once'}\n"
                "        suite = 'workspace' if args[args.index('-E') + 1].startswith('not ') else 'refusal'\n"
                "        assert Path(os.environ['NAMESPACE_POLICY']).read_text() == ('1' if suite == 'refusal' else '0')\n"
                "    print('runner: ' + suite + ' cwd=' + str(Path.cwd()) + ' target=' + os.environ.get('CARGO_TARGET_DIR', 'target'))\n"
                "    print('visible success: ' + suite)\n"
                "    if os.environ['MARKER'] == suite: print('NOT RUN: invented_branch: unavailable fixture', file=sys.stderr)\n"
                "    if os.environ['RUNNER_FAIL'] == 'both' and suite in ('refusal', 'workspace'): sys.exit(37 if suite == 'refusal' else 38)\n"
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
                "    config = 'default'\n"
                "    if '--target' in args: config = 'musl'\n"
                "    elif 'kaban-index' in args: config = 'index'\n"
                "    elif '--no-default-features' in args: config = 'graph-off' if '--features' in args else 'no-default'\n"
                "    elif '--features' in args: config = {'code-graph': 'graph-on', 'kaban-ingest/pdf': 'pdf', 'kaban-ingest/md-splitter': 'md-splitter'}[args[args.index('--features')+1]]\n"
                "    names = ('kaban', 'kaban-index') if config in ('default', 'musl') else ('kaban-index',) if config == 'index' else ('kaban',)\n"
                "    for name in names:\n"
                "        binary=target / 'release' / name\n"
                "        if config == 'graph-on' and os.environ['ARTIFACT_SCENARIO'] == 'missing-output':\n"
                "            binary.unlink(missing_ok=True); continue\n"
                "        cache=target / 'release/deps' / (name + '-' + config); cache.parent.mkdir(exist_ok=True)\n"
                "        if not cache.exists():\n"
                "            cache.write_bytes(config.encode().ljust(64,b'.')); cache.chmod(0o700)\n"
                "            size=json.loads(os.environ['ARTIFACT_SIZES']).get(config + '/' + name)\n"
                "            if size is not None:\n"
                "                with cache.open('r+b') as stream: stream.truncate(size)\n"
                "        else: print('Fresh ' + name + ' (' + config + ')')\n"
                "        binary.unlink(missing_ok=True); os.link(cache, binary)\n"
                "elif args[0] not in ('fmt', 'clippy', 'nextest', 'deny', 'fetch'):\n"
                "    sys.exit(97)\n"
            )
            cargo.chmod(0o700)
            command = [BASH, str(script), *args]
            if inherited_fd:
                command = [BASH, "-c", 'exec 142>/dev/null\nexec "$@"', "bash", *command]
            runs = []
            for _ in range(repeats):
                result = subprocess.run(
                    command,
                    env={
                        **os.environ,
                        "BASH_ENV": os.devnull,
                        "KABAN_CI_ISOLATION_SPLIT": "1" if isolation_split else "0",
                        "GITHUB_ACTIONS": "true" if hosted else "false",
                        "RUNNER_ENVIRONMENT": "github-hosted" if hosted else "self-hosted",
                        "NAMESPACE_POLICY": str(policy),
                        "ISOLATION_CALLS": str(root / "isolation-calls.jsonl"),
                        "ISOLATION_PROBE": isolation_probe,
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
                        "ARTIFACT_SCENARIO": artifact_scenario,
                        "ARTIFACT_SIZES": json.dumps(artifact_sizes or {}),
                        "CARGO_TARGET_DIR": str(root / "private target"),
                        "KABAN_CI_ARTIFACT_DIR": str(root / "outside cache") if artifact_root else "",
                        "GITHUB_STEP_SUMMARY": str(root / "summary") if summary else os.devnull,
                        "CHECK_FD": "1" if inherited_fd else "0",
                        "PRESERVED_VALUE": "retained environment",
                    },
                    input="retained standard input\n" if inherited_fd else None,
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                runs.append(result)
            result.namespace_policy = policy.read_text()
            isolation_calls = root / "isolation-calls.jsonl"
            result.isolation_calls = [json.loads(line) for line in isolation_calls.read_text().splitlines()] if isolation_calls.exists() else []
            result.summary = (root / "summary").read_text() if (root / "summary").exists() else ""
            result.stages_in_target = bool(list((root / "private target").glob("ci-artifacts.*")))
            result.run_codes = [run.returncode for run in runs]
            binary = root / "private target/release/kaban"
            result.final_kaban = binary.read_bytes()[:64] if binary.exists() else None
            index_binary = binary.with_name("kaban-index")
            result.final_index = index_binary.read_bytes()[:64] if index_binary.exists() else None
            result.cached_payloads = {path.name: path.read_bytes()[:64] for path in (root / "private target/release/deps").glob("*")}
            configs = [path for run in (root / ("outside cache" if artifact_root else "private target")).glob("ci-artifacts.*") for path in run.iterdir() if path.is_dir()]
            result.staged_configs = sorted(path.name for path in configs)
            result.staged_commands = {path.name: (path / "cargo-args.txt").read_text().splitlines()[1:] for path in configs}
            result.staged_payloads = {path.name + "/" + binary.name: binary.read_bytes()[:64]
                                      for path in configs for binary in path.iterdir()
                                      if binary.name in ("kaban", "kaban-index")}
            return result, [json.loads(line) for line in (calls.read_text().splitlines() if calls.exists() else [])]

    def test_hosted_isolation_split_reuses_build_and_restores_policy(self):
        result, calls = self.run_gate(args=("functional",), fail="", isolation_split=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(sum(c[:2] == ["nextest", "list"] for c in calls), 1)
        reused = [c for c in calls if "--binaries-metadata" in c]
        self.assertEqual(len(reused), 2, calls)
        self.assertEqual(reused[0][reused[0].index("--binaries-metadata") + 1],
                         reused[1][reused[1].index("--binaries-metadata") + 1])
        selection = "binary_id(kaban-index::cli) & (test(=cli_index_required_without_isolating_tier_is_exit_8) | test(=rust_required_unavailable_never_runs_native_producer_probes))"
        self.assertEqual(reused[0][reused[0].index("-E") + 1], selection)
        self.assertEqual(reused[1][reused[1].index("-E") + 1], "not (" + selection + ")")
        self.assertFalse(any("--workspace" in c for c in reused))
        self.assertFalse(Path(reused[0][reused[0].index("--binaries-metadata") + 1]).exists())
        self.assertEqual(result.namespace_policy, "1")
        self.assertNotIn("visible success: pdf", result.stdout)
        self.assertIn("visible success: no-default", result.stdout)
        self.assertIn("visible success: doc", result.stdout)

    def test_hosted_isolation_split_runs_both_selections_and_preserves_first_failure(self):
        for failed in ("refusal", "workspace", "both"):
            with self.subTest(failed=failed):
                result, calls = self.run_gate(args=("functional",), fail="", isolation_split=True,
                                              runner_fail=failed)
                self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
                reused = [c for c in calls if "--binaries-metadata" in c]
                self.assertEqual(len(reused), 2)
                self.assertFalse(Path(reused[0][reused[0].index("--binaries-metadata") + 1]).exists())
                self.assertEqual(result.namespace_policy, "1")
                self.assertIn("visible success: workspace", result.stdout)

    def test_hosted_isolation_split_keeps_mandatory_marker_semantics(self):
        for marker in ("refusal", "workspace"):
            with self.subTest(marker=marker):
                result, _ = self.run_gate(args=("functional",), fail="", isolation_split=True,
                                          marker=marker)
                self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
                self.assertIn("mandatory_skips: 1", result.stdout)
                self.assertIn("invented_branch", result.stdout)
                self.assertIn("visible success: doc", result.stdout)
                self.assertEqual(result.namespace_policy, "1")

    def test_hosted_isolation_split_fails_before_build_on_unsuitable_runner(self):
        for probe in ("available-fails", "unavailable-works"):
            with self.subTest(probe=probe):
                result, calls = self.run_gate(args=("functional",), fail="", isolation_split=True,
                                              isolation_probe=probe)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertFalse(any(c[:2] in (["nextest", "list"], ["nextest", "run"]) for c in calls))
                self.assertEqual(result.namespace_policy, "1")
        result, calls = self.run_gate(args=("functional",), fail="isolation-build", isolation_split=True)
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertFalse(any("--binaries-metadata" in c for c in calls))
        self.assertEqual(result.namespace_policy, "1")

    def test_isolation_policy_changes_require_opt_in_and_hosted_guard(self):
        result, calls = self.run_gate(args=("functional",), fail="", isolation_split=True, hosted=False)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertEqual(result.isolation_calls, [])
        self.assertFalse(any(c[:2] in (["nextest", "list"], ["nextest", "run"]) for c in calls))
        result, calls = self.run_gate(args=("functional",), fail="")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.isolation_calls, [])
        self.assertFalse(any(c[:2] == ["nextest", "list"] for c in calls))
        self.assertTrue(any(c[:2] == ["nextest", "run"] and "--workspace" in c for c in calls))

    def test_artifacts_can_stage_outside_cached_target_without_changing_consumers(self):
        result, calls = self.run_gate(args=("gnu",), fail="", artifact_root=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(result.stages_in_target)
        self.assertEqual(result.staged_payloads["default/kaban"], result.final_kaban)
        self.assertIn("smoke fixture: default", result.stdout)
        self.assertEqual(calls.count(["build", "--release"]), 1)

    def test_hosted_group_summary_keeps_incomplete_and_failure_status(self):
        for marker, fail, expected, verdict in (("", "", 0, "PASS"),
                ("workspace", "", 3, "INCOMPLETE"), ("workspace", "workspace", 37, "FAILED")):
            with self.subTest(marker=marker, fail=fail):
                result, _ = self.run_gate(args=("functional",), fail="", marker=marker,
                                          runner_fail=fail, summary=True)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                self.assertEqual(result.summary, f"Group `functional`: {verdict} (exit {expected})\n")

    def test_gnu_builds_default_once_and_restores_exact_bytes_after_optional_variants(self):
        result, calls = self.run_gate(args=("gnu",), fail="", size_table=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls.count(["build", "--release"]), 1, calls)
        self.assertEqual(result.final_kaban, b"default".ljust(64, b"."))
        self.assertIn("smoke fixture: default", result.stdout)
        self.assertEqual(result.staged_configs, ["default", "gnu-index", "graph-off", "graph-on", "md-splitter", "no-default", "pdf"])
        self.assertIn("sha256=", result.stdout)
        self.assertEqual(result.cached_payloads["kaban-pdf"], b"pdf".ljust(64, b"."))
        self.assertEqual(result.final_index, b"default".ljust(64, b"."))

    def test_warm_artifact_reuse_keeps_cached_variants_and_size_miss(self):
        result, _ = self.run_gate(args=("gnu",), fail="", repeats=2,
                                  artifact_sizes={"default/kaban": 200, "graph-on/kaban": 900164})
        self.assertEqual(result.run_codes, [3, 3], result.stdout + result.stderr)
        self.assertIn("Fresh kaban (graph-off)", result.stdout)
        self.assertEqual(result.staged_payloads["graph-off/kaban"], b"graph-off".ljust(64, b"."))
        self.assertEqual(result.cached_payloads["kaban-graph-off"], b"graph-off".ljust(64, b"."))
        self.assertEqual(result.final_kaban, b"default".ljust(64, b"."))
        self.assertEqual(result.final_index, b"default".ljust(64, b"."))

    def test_artifact_consumers_reject_missing_or_corrupt_bytes_and_preserve_child_failure(self):
        for scenario in ("corrupt-stage", "missing-stage", "missing-output"):
            with self.subTest(scenario=scenario):
                result, _ = self.run_gate(args=("gnu",), fail="", artifact_scenario=scenario)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                if scenario == "missing-output":
                    self.assertIn("cp:", result.stderr)
                    self.assertNotIn("size: kaban(code-graph on)", result.stdout)
                else:
                    self.assertNotIn("smoke fixture:", result.stdout)
        failed, calls = self.run_gate(args=("gnu",), fail="build")
        self.assertEqual(failed.returncode, 37, failed.stdout + failed.stderr)
        self.assertNotIn("smoke fixture:", failed.stdout)
        self.assertEqual(sum(c[0] == "build" for c in calls), 1)

    def test_artifact_configuration_bytes_and_arguments_stay_distinct(self):
        result, calls = self.run_gate(fail="", size_table=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        expected = {
            "default": ["build", "--release"],
            "gnu-index": ["build", "--release", "-p", "kaban-index"],
            "graph-on": ["build", "--release", "-p", "kaban", "--features", "code-graph"],
            "graph-off": ["build", "--release", "-p", "kaban", "--no-default-features", "--features", "ingest,text-index-tantivy"],
            "no-default": ["build", "--release", "-p", "kaban", "--no-default-features"],
            "md-splitter": ["build", "--release", "-p", "kaban", "--features", "kaban-ingest/md-splitter"],
            "pdf": ["build", "--release", "-p", "kaban", "--features", "kaban-ingest/pdf"],
            "musl": ["build", "--release", "--target", "x86_64-unknown-linux-musl", "-p", "kaban", "-p", "kaban-index"],
        }
        self.assertEqual(result.staged_commands, expected)
        self.assertEqual([c for c in calls if c[0] == "build"], list(expected.values()))
        for name, binary, contents in (("default", "kaban", "default"), ("gnu-index", "kaban-index", "index"),
                                       ("graph-on", "kaban", "graph-on"), ("graph-off", "kaban", "graph-off"),
                                       ("musl", "kaban-index", "musl")):
            self.assertEqual(result.staged_payloads[name + "/" + binary], contents.encode().ljust(64, b"."))
        self.assertEqual(result.final_kaban, b"default".ljust(64, b"."))
        self.assertIn("smoke fixture: default", result.stdout)

    def test_artifact_strict_budgets_keep_failure_and_incomplete_semantics(self):
        for group, table, sizes, code, diagnostic in (
            ("gnu", False, {"default/kaban": 25 * 1024 * 1024}, 1, "FAILED: binary size"),
            ("gnu", True, {"no-default/kaban": 17 * 1024 * 1024}, 1, "FAILED: --no-default-features"),
            ("gnu", False, {"graph-on/kaban": 900064}, 3, "N4 MISS"),
            ("musl", False, {"musl/kaban-index": 11999999}, 0, "gate: < 12000000"),
            ("musl", False, {"musl/kaban-index": 12000000}, 3, "N4 MISS"),
        ):
            with self.subTest(group=group, sizes=sizes):
                result, _ = self.run_gate(args=(group,), fail="", size_table=table, artifact_sizes=sizes)
                self.assertEqual(result.returncode, code, result.stdout + result.stderr)
                self.assertIn(diagnostic, result.stdout + result.stderr)

    def test_pdf_runs_its_complete_suite_and_doctest_without_language_tools(self):
        result, calls = self.run_gate(args=("pdf",), tools=("nextest",), fail="",
                                      missing_tool=("node", "go", "scip-go", "musl-gcc"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual([c for c in calls if c[0] not in ("--list",) and "--version" not in c], [
            ["nextest", "run", "-p", "kaban-ingest", "--features", "pdf", "--success-output", "immediate"],
            ["test", "--doc", "--locked", "-p", "kaban-ingest", "--features", "pdf", "--", "--show-output"],
        ])
        self.assertEqual(result.isolation_calls, [])
        self.assertIn("tool: cc =>", result.stdout)

    def test_pdf_keeps_pinned_nextest_prerequisites(self):
        for tools, versions in (((), None), (("nextest",), {"nextest": "cargo-nextest 0.0.1"})):
            with self.subTest(tools=tools, versions=versions):
                result, calls = self.run_gate(args=("pdf",), tools=tools, versions=versions, fail="")
                self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
                self.assertFalse(any(c[0] in ("test", "nextest") and "--version" not in c for c in calls))

    def test_functional_runs_exact_suites_and_library_docs_without_artifacts(self):
        result, calls = self.run_gate(args=("functional",), tools=("nextest",), fail="", missing_tool="musl-gcc")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual([c for c in calls if c[:2] == ["nextest", "run"]], [
            ["nextest", "run", "--workspace", "--no-fail-fast", "--success-output", "immediate"],
            ["nextest", "run", "-p", "kaban", "--no-default-features", "--success-output", "immediate"],
        ])
        self.assertIn(["test", "--doc", "--locked", "-p", "kaban-ingest", "-p", "fixture-core", "--", "--show-output"], calls)
        self.assertFalse(any("pdf" in c for c in calls))
        self.assertFalse(any(c[0] in ("build", "fmt", "clippy", "deny") and "--version" not in c for c in calls))

    def test_every_selected_rust_suite_exposes_mandatory_markers_and_preserves_failure(self):
        for group, suite in (("functional", "workspace"), ("functional", "no-default"),
                             ("functional", "doc"), ("pdf", "pdf"), ("pdf", "doc"),
                             ("producer", "producer")):
            with self.subTest(group=group, suite=suite):
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
        for group in ("quality", "helpers", "functional", "pdf", "gnu", "musl"):
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


class CleanRoomResults(unittest.TestCase):
    def test_incomplete_child_stays_incomplete_unless_another_step_failed(self):
        for ci_status, clean_status, expected, verdict in (
            (3, 0, 3, "INCOMPLETE"), (3, 1, 1, "FAIL"), (1, 0, 1, "FAIL"), (0, 0, 0, "PASS")
        ):
            with self.subTest(ci=ci_status, clean=clean_status), tempfile.TemporaryDirectory() as scratch:
                root = Path(scratch)
                ci = root / "scripts/ci"
                ci.mkdir(parents=True)
                shutil.copyfile(ROOT / "scripts/ci/clean-room.sh", ci / "clean-room.sh")
                (ci / "ci.sh").write_text(f"exit {ci_status}\n")
                (root / "crates/fixture/src").mkdir(parents=True)
                (root / "crates/fixture/src/lib.rs").touch()
                (root / "crates/kaban/src").mkdir(parents=True)
                bin_dir = root / "bin"
                bin_dir.mkdir()
                for name, body in {
                    "cargo": f'if [ "$1" = clean ]; then exit {clean_status}; fi\nexit 0\n',
                    "flock": 'shift 2\nexec "$@"\n',
                    "git": 'echo fixture-head\n',
                }.items():
                    tool = bin_dir / name
                    tool.write_text("#!/bin/sh\n" + body)
                    tool.chmod(0o700)
                env = {**os.environ, "CARGO": str(bin_dir / "cargo"),
                       "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"]}
                result = subprocess.run([BASH, str(ci / "clean-room.sh"), "fixture", str(root / "logs")],
                                        env=env, text=True, capture_output=True)
                summary = (root / "logs/fixture-summary.log").read_text()
                self.assertEqual(result.returncode, expected, summary + result.stderr)
                self.assertIn("result " + verdict, summary)

if __name__ == "__main__":
    unittest.main()
