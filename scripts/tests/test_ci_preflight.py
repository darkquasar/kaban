"""Public preflight admission with invented distributions; no Rust compilation."""
import hashlib
import json
import os
import platform
import re
import signal
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
CLI = ROOT / "scripts/ci/preflight.py"


class Preflight(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="kaban-preflight-")
        self.addCleanup(self.temporary.cleanup)
        self.scratch = Path(self.temporary.name).resolve()
        self.repo = self.scratch / "source"
        self.repo.mkdir()
        for name, content in (("Cargo.toml", "[workspace]\n"), ("Cargo.lock", "version = 4\n"),
                              ("rust-toolchain.toml", '[toolchain]\nchannel="1.99.0"\n')):
            (self.repo / name).write_text(content)
        self.bin = self.scratch / "trusted-tools/bin"
        self.bin.mkdir(parents=True)
        self.calls = self.scratch / "calls.jsonl"
        self.invocations = 0
        self.policy = {"version": 1, "profiles": {"functional": {"tools": ["cargo", "nextest"],
            "rust": False, "native_linux": False, "tmp_quota": False}}, "tools": {
            "cargo": {"command": "cargo", "override": "CARGO", "args": ["--version"],
                      "version": "cargo invented-1", "kind": "executable"},
            "nextest": {"command": "cargo-nextest", "args": ["--version"],
                        "version": "cargo-nextest invented-2", "kind": "executable"}}}
        self.tool("cargo", "cargo invented-1")
        self.tool("cargo-nextest", "cargo-nextest invented-2")
        self.env = {**os.environ, "PATH": str(self.bin), "CARGO": str(self.bin / "cargo"),
                    "CARGO_TARGET_DIR": str(self.scratch / "target"),
                    "CARGO_HOME": str(self.scratch / "cargo-home"), "PREFLIGHT_CALLS": str(self.calls)}
        for key in ("RUSTC", "RUSTDOC", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTUP_TOOLCHAIN",
                    "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTDOCFLAGS",
                    "RUSTC_BOOTSTRAP", "RUSTC_FORCE_UNSTABLE", "CC", "CXX", "AR", "RANLIB", "CFLAGS",
                    "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "CXXSTDLIB", "LD_PRELOAD", "LD_LIBRARY_PATH",
                    "DYLD_INSERT_LIBRARIES", "DYLD_LIBRARY_PATH", "REALGCC"):
            self.env.pop(key, None)
        for key in list(self.env):
            if key.startswith(("CARGO_PROFILE_", "CARGO_BUILD_", "CARGO_ALIAS_", "CARGO_UNSTABLE_", "CARGO_HOST_",
                               "CC_", "CXX_", "AR_", "RANLIB_",
                               "HOST_CC", "TARGET_CC", "HOST_CXX", "TARGET_CXX", "HOST_AR", "TARGET_AR",
                               "HOST_CFLAGS", "TARGET_CFLAGS")) or re.fullmatch(r"CARGO_TARGET_.+_(?:LINKER|RUSTFLAGS|RUSTDOCFLAGS|RUNNER)", key):
                self.env.pop(key)

    def tool(self, name, version, extra=""):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\nimport os,sys,json\n"
            "with open(os.environ['PREFLIGHT_CALLS'],'a') as stream:\n"
            " stream.write(json.dumps([sys.argv[0],sys.argv[1:]])+'\\n')\n" + extra +
            f"\nprint({version!r})\n")
        path.chmod(0o700)
        return path

    def invoke(self, *extra, inherited_fd=False, input_data=None, timeout=10):
        pins = self.scratch / "pins.json"
        pins.write_text(json.dumps(self.policy))
        self.invocations += 1
        output = self.scratch / ("output-" + str(self.invocations))
        command = [sys.executable, str(CLI), "--profile", "functional", "--root", str(self.repo),
            "--pins", str(pins), "--output", str(output), "--target-dir", str(self.scratch / "target"),
            "--scratch-dir", str(self.scratch / "job-scratch"), *map(str, extra)]
        if inherited_fd:
            command = [shutil.which("bash"), "-c", 'exec 142>/dev/null\nexec "$@"', "bash", *command]
        result = subprocess.run(command, env=self.env, input=input_data, capture_output=True, text=True, timeout=timeout)
        report = json.loads((output / "preflight.json").read_text()) if (output / "preflight.json").exists() else None
        return result, report

    def rust_tools(self, active=None, components="rustfmt\nclippy", compiler_host=None):
        machine = {"arm64": "aarch64", "amd64": "x86_64"}.get(platform.machine().lower(), platform.machine().lower())
        host = machine + ("-unknown-linux-gnu" if platform.system() == "Linux" else "-apple-darwin")
        self.policy["profiles"]["functional"].update(rust=True, tools=["cargo", "rustc", "rustup"])
        self.policy["tools"]["cargo"]["version"] = "cargo 1.99.0"
        self.tool("cargo", "cargo 1.99.0")
        self.policy["tools"].update(rustc={"command": "rustc", "args": ["-Vv"],
            "version": "rustc 1.99.0", "kind": "executable"}, rustup={"command": "rustup",
            "args": ["--version"], "version": "rustup invented", "kind": "executable"})
        self.tool("rustc", "rustc 1.99.0\nrelease: 1.99.0\nhost: " + (compiler_host or host))
        for name in ("rustdoc", "rustfmt", "cargo-clippy"):
            self.tool(name, name + " invented")
        self.tool("rustup", "rustup invented",
            "args=sys.argv[1:]\n"
            f"if args==['show','active-toolchain']: print({(active if active is not None else '1.99.0-' + host)!r});sys.exit(0)\n"
            f"if args[:1]==['which']: print({str(self.bin)!r}+'/'+args[1]);sys.exit(0)\n"
            f"if args==['component','list','--installed']: print({components!r});sys.exit(0)\n"
            "if args==['target','list','--installed']: print('aarch64-unknown-linux-musl\\nx86_64-unknown-linux-musl');sys.exit(0)\n")
        return host

    def native_node(self):
        self.native_fixture(self.bin / "node", "fixture-node 22.23.4")
        (self.bin.parent / "lib/node_modules").mkdir(parents=True)
        self.policy["profiles"]["functional"]["tools"] = ["node"]
        self.policy["tools"]["node"] = {"command": "node", "args": ["--version"], "version": "fixture-node 22.23.4",
            "kind": "native", "root_env": "KABAN_NODE_ROOT"}
        self.env["KABAN_NODE_ROOT"] = str(self.bin.parent)

    def native_fixture(self, path, output):
        compiler = shutil.which("cc")
        if not compiler:
            self.skipTest("native fixture requires the C compiler required by Linux CI")
        subprocess.run([compiler, "-x", "c", "-o", str(path), "-"],
            input='#include <stdio.h>\nint main(void) { puts(' + json.dumps(output) + '); return 0; }\n',
            text=True, check=True, capture_output=True, timeout=20)

    def test_admitted_tools_record_actual_files_and_versions(self):
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(report["status"], "pass")
        cargo = report["tool_identities"]["cargo"]
        self.assertEqual(cargo["launch_path"], str(self.bin / "cargo"))
        self.assertEqual(cargo["sha256"], hashlib.sha256((self.bin / "cargo").read_bytes()).hexdigest())
        self.assertEqual(cargo["probes"][0]["stdout"], "cargo invented-1\n")

    def test_missing_required_nextest_refuses_before_any_build(self):
        (self.bin / "cargo-nextest").unlink()
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stderr + result.stdout)
        self.assertEqual(report["status"], "incomplete")
        self.assertIn("nextest", json.dumps(report))
        if self.calls.exists():
            self.assertFalse(any("build" in json.loads(line)[1] for line in self.calls.read_text().splitlines()))

    def test_node_launcher_is_refused_before_its_version_canary_executes(self):
        self.policy["profiles"]["functional"]["tools"] = ["node"]
        self.policy["tools"]["node"] = {"command": "node", "version": "v22.23.4", "args": ["--version"],
                                            "kind": "native", "root_env": "KABAN_NODE_ROOT"}
        self.env["KABAN_NODE_ROOT"] = str(self.bin.parent)
        self.tool("node", "v22.23.4")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("native", json.dumps(report))
        self.assertFalse(self.calls.exists(), "a Node shim must be refused before execution")

    def test_writable_explicit_cargo_never_falls_back_or_executes(self):
        (self.bin / "cargo").chmod(0o777)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("writable", json.dumps(report))
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []
        self.assertFalse(any(Path(call[0]).name == "cargo" for call in calls))

    def test_probe_failure_retains_native_exit_and_both_streams(self):
        self.tool("cargo", "unused", "print('native output');print('native diagnostic',file=sys.stderr);sys.exit(37)\n")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 37, result.stderr + result.stdout)
        self.assertEqual(report["status"], "infrastructure_failure")
        self.assertIn("native output", result.stdout)
        self.assertIn("native diagnostic", result.stderr)

    def test_disagreeing_rust_override_is_refused_before_any_tool_executes(self):
        self.policy["profiles"]["functional"]["rust"] = True
        self.env["RUSTUP_TOOLCHAIN"] = "nightly"
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("RUSTUP_TOOLCHAIN", json.dumps(report))
        self.assertFalse(self.calls.exists())

    def test_directory_override_cannot_replace_the_pinned_rust_toolchain(self):
        self.rust_tools(active="1.98.0-other-host (directory override)")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("active toolchain", json.dumps(report))

    def test_matching_native_rust_context_records_effective_objects(self):
        host = self.rust_tools()
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["rust"]["active_toolchain"], "1.99.0-" + host)
        self.assertIn("rustdoc", report["rust"]["objects"])

    def test_measured_resource_envelope_refuses_before_tool_execution(self):
        envelope = self.scratch / "envelope.json"
        envelope.write_text(json.dumps({"runner_class": "invented-measured-runner", "basis": "invented test observation",
            "min_disk_bytes": 2 ** 62, "min_memory_bytes": 0}))
        result, report = self.invoke("--resource-envelope", envelope)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("disk", json.dumps(report))
        self.assertFalse(self.calls.exists())

    def test_system_tool_can_have_a_declared_version_pattern(self):
        self.policy["tools"]["cargo"].pop("version")
        self.policy["tools"]["cargo"]["version_regex"] = r"^cargo invented-[0-9]+$"
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["status"], "pass")

    def test_colored_version_does_not_hide_an_installed_tool(self):
        self.tool("cargo", "\x1b[1mcargo invented-1\x1b[0m")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("\x1b[1m", report["tool_identities"]["cargo"]["probes"][0]["stdout"])

    def test_restored_tool_must_match_its_trusted_acquisition_manifest_before_exec(self):
        expected = self.scratch / "expected-tools.json"
        expected.write_text(json.dumps({"pins_sha256": hashlib.sha256(json.dumps(self.policy).encode()).hexdigest(),
            "tools": {name: hashlib.sha256((self.bin / command).read_bytes()).hexdigest()
                      for name, command in (("cargo", "cargo"), ("nextest", "cargo-nextest"))}}))
        self.tool("cargo", "cargo invented-1", "# same version, changed restored object\n")
        result, report = self.invoke("--expected-tools", expected)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("acquisition", json.dumps(report))
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []
        self.assertFalse(any(Path(call[0]).name == "cargo" for call in calls))

    def test_preflight_interpreter_must_be_the_selected_python_object(self):
        self.policy["admit_interpreter"] = True
        self.policy["profiles"]["functional"]["tools"] = ["python3"]
        self.policy["tools"]["python3"] = {"command": "python3", "args": ["--version"],
            "version": "Python invented", "kind": "executable"}
        self.tool("python3", "Python invented")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("interpreter", json.dumps(report))

    def test_applicable_python_sqlite_probe_keeps_its_real_failure(self):
        self.policy["profiles"]["functional"].update(tools=["python3"], python_sqlite=True)
        self.policy["tools"]["python3"] = {"command": "python3", "args": ["--version"],
            "version": "Python invented", "kind": "executable"}
        self.tool("python3", "Python invented",
            "if sys.argv[1:2]==['-c']: print('SQLite unavailable',file=sys.stderr);sys.exit(37)\n")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertEqual(report["status"], "infrastructure_failure")
        self.assertIn("SQLite unavailable", result.stderr)

    def test_version_probes_disable_implicit_rustup_installation(self):
        self.tool("cargo", "cargo invented-1", "if os.environ.get('RUSTUP_AUTO_INSTALL')!='0': sys.exit(52)\n")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["tool_identities"]["cargo"]["probes"][0]["environment_overrides"],
                         {"RUSTUP_AUTO_INSTALL": "0"})

    def test_source_and_effective_flags_change_configuration_inputs(self):
        result, first = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("configuration_inputs_id", first)
        (self.repo / "Cargo.toml").write_text("[workspace]\nresolver='3'\n")
        result, second = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotEqual(first["configuration_inputs_id"], second["configuration_inputs_id"])
        self.env["RUSTFLAGS"] = "-C target-cpu=native"
        result, third = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotEqual(second["configuration_inputs_id"], third["configuration_inputs_id"])

    def test_musl_profile_refuses_a_foreign_linker_or_non_linux_host(self):
        self.rust_tools()
        self.policy["profiles"]["functional"]["native_musl"] = True
        self.policy["profiles"]["functional"]["tools"].append("musl-gcc")
        self.policy["tools"]["musl-gcc"] = {"command": "musl-gcc", "args": ["--version"],
            "version": "gcc invented", "kind": "executable"}
        self.tool("musl-gcc", "gcc invented",
            "if sys.argv[1:]==['-dumpmachine']: print('foreign-linux-gnu');sys.exit(0)\n")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("musl", json.dumps(report))

    def test_namespace_view_must_include_the_actual_distribution_root(self):
        self.native_node()
        view = self.scratch / "namespace-view.json"
        view.write_text(json.dumps({"version": 1, "basis": "invented fixture read-only view", "read_only_roots": []}))
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("namespace", json.dumps(report))
        view.write_text(json.dumps({"version": 1, "basis": "invented fixture read-only view",
                                    "read_only_roots": [str(self.bin.parent)]}))
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("declaration", report["namespace_view"]["execution_proof"])

    def test_tool_identity_cannot_change_during_its_version_probe(self):
        self.tool("cargo", "cargo invented-1",
            "with open(sys.argv[0],'a') as source: source.write('\\n# changed during probe\\n')\n")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("changed", json.dumps(report))

    def test_empty_active_toolchain_is_reported_as_incomplete(self):
        self.rust_tools(active="")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("active toolchain", json.dumps(report))

    def test_unknown_cargo_configuration_cannot_inject_a_compiler_wrapper(self):
        self.rust_tools()
        config = Path(self.env["CARGO_HOME"]) / "config.toml"
        config.parent.mkdir()
        config.write_text('[build]\nrustc-wrapper="/untrusted/wrapper"\n')
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("Cargo configuration", json.dumps(report))
        self.assertFalse(self.calls.exists())

    def test_node_distribution_content_is_part_of_the_admitted_identity(self):
        self.native_node()
        result, first = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("distribution_sha256", first["tool_identities"]["node"])
        (self.bin.parent / "lib/node_modules/fixture.js").write_text("fixture library bytes\n")
        result, second = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotEqual(first["configuration_inputs_id"], second["configuration_inputs_id"])

    def test_restored_distribution_requires_the_recorded_library_identity(self):
        self.native_node()
        result, first = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        node = first["tool_identities"]["node"]
        expected = self.scratch / "expected-tools.json"
        expected.write_text(json.dumps({"pins_sha256": first["pins_sha256"], "tools": {"node": {
            "sha256": node["sha256"], "distribution_sha256": node["distribution_sha256"]}}}))
        result, report = self.invoke("--expected-tools", expected)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        (self.bin.parent / "lib/node_modules/fixture.js").write_text("changed cached library\n")
        result, report = self.invoke("--expected-tools", expected)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("acquisition", json.dumps(report))

    def test_distribution_root_can_be_derived_from_an_admitted_real_bin_object(self):
        self.native_node()
        self.policy["tools"]["node"].pop("root_env")
        self.policy["tools"]["node"]["root_from_binary"] = True
        self.env.pop("KABAN_NODE_ROOT")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["tool_identities"]["node"]["distribution_root"], str(self.bin.parent))

    def test_signaled_probe_keeps_shell_status_and_raw_signal(self):
        self.tool("cargo", "unused", "import signal\nos.kill(os.getpid(),signal.SIGTERM)\n")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 143, result.stdout + result.stderr)
        self.assertEqual(report["child_exit"], 143)
        self.assertEqual(report["tool_identities"]["cargo"]["probes"][0]["raw_child_exit"], -15)

    def test_inherited_descriptor_is_closed_and_standard_input_is_preserved(self):
        self.tool("cargo", "cargo invented-1", "try: os.fstat(142)\nexcept OSError: pass\nelse: sys.exit(56)\n"
            "print('input: '+sys.stdin.readline().rstrip())\n")
        self.policy["tools"]["cargo"]["version_regex"] = r"(?m)^cargo invented-1$"
        result, report = self.invoke(inherited_fd=True, input_data="preserved input\n")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("input: preserved input", result.stdout)

    def test_explicit_cargo_path_with_spaces_is_one_executable(self):
        cargo = self.bin / "cargo with spaces"
        (self.bin / "cargo").rename(cargo)
        self.env["CARGO"] = str(cargo)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["tool_identities"]["cargo"]["launch_path"], str(cargo))

    def test_source_checkout_cargo_and_symlink_alias_are_refused(self):
        source_tool = self.repo / "cargo"
        (self.bin / "cargo").rename(source_tool)
        (self.bin / "cargo").symlink_to(source_tool)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("forbidden", json.dumps(report))

    def test_first_untrusted_path_hit_cannot_fall_back_to_a_later_good_tool(self):
        bad = self.scratch / "bad-bin"
        bad.mkdir()
        shutil.copyfile(self.bin / "cargo", bad / "cargo")
        (bad / "cargo").chmod(0o777)
        self.env.pop("CARGO")
        self.env["PATH"] = str(bad) + os.pathsep + str(self.bin)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []
        self.assertFalse(any(Path(call[0]).name == "cargo" for call in calls))

    def test_missing_rust_component_and_wrong_compiler_host_are_incomplete(self):
        self.rust_tools(components="rustfmt")
        (self.repo / "rust-toolchain.toml").write_text('[toolchain]\nchannel="1.99.0"\ncomponents=["rustfmt","clippy"]\n')
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("components", json.dumps(report))
        self.rust_tools(compiler_host="foreign-unknown-linux-gnu")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("release/host", json.dumps(report))

    def test_foreign_native_object_is_refused_before_exec(self):
        self.policy["profiles"]["functional"]["tools"] = ["node"]
        self.policy["tools"]["node"] = {"command": "node", "args": ["--version"], "version": "unused", "kind": "native"}
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        foreign = 62 if platform.machine().lower() in ("arm64", "aarch64") else 183
        header[18:20] = foreign.to_bytes(2, "little")
        (self.bin / "node").write_bytes(header)
        (self.bin / "node").chmod(0o700)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("native executable", json.dumps(report))

    def test_unknown_available_memory_cannot_satisfy_a_measured_large_minimum(self):
        envelope = self.scratch / "envelope.json"
        envelope.write_text(json.dumps({"runner_class": "invented runner", "basis": "invented resource observation",
                                        "min_disk_bytes": 0, "min_memory_bytes": 2 ** 62}))
        result, report = self.invoke("--resource-envelope", envelope)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("memory", json.dumps(report))
        self.assertFalse(self.calls.exists())

    def test_non_object_policy_is_an_incomplete_report(self):
        self.policy = ["invalid policy"]
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertEqual(report["status"], "incomplete")

    def test_verification_helper_bytes_are_bound_even_in_a_dirty_local_tree(self):
        result, first = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        helper = self.repo / "scripts/ci/fixture.py"
        helper.parent.mkdir(parents=True)
        helper.write_text("# changed verification implementation\n")
        result, second = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotEqual(first["configuration_inputs_id"], second["configuration_inputs_id"])
        self.assertIn("scripts/ci/fixture.py", second["configuration_inputs"]["verification_inputs"])

    def test_tool_identifier_cannot_escape_its_log_directory(self):
        self.policy["tools"]["../escaped"] = self.policy["tools"].pop("cargo")
        self.policy["profiles"]["functional"]["tools"][0] = "../escaped"
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertFalse((self.scratch / "output-1/escaped-version.stdout").exists())

    def test_declared_target_must_match_the_effective_cargo_target(self):
        self.rust_tools()
        self.env["CARGO_TARGET_DIR"] = str(self.scratch / "different-target")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("CARGO_TARGET_DIR", json.dumps(report))
        self.assertFalse(self.calls.exists())

    def test_node_root_cannot_expand_to_an_ancestor_of_home(self):
        self.native_node()
        self.env["HOME"] = str(self.bin.parent / "user-home")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("broad", json.dumps(report))
        self.assertNotIn("fixture-node", result.stdout)

    def test_declared_data_roots_are_forbidden_tool_locations(self):
        result, report = self.invoke("--data-root", self.bin.parent)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("forbidden", json.dumps(report))
        self.assertFalse(self.calls.exists())

    def test_namespace_tool_exposure_cannot_include_home(self):
        self.native_node()
        self.env["HOME"] = str(self.scratch / "private-home")
        view = self.scratch / "namespace-view.json"
        view.write_text(json.dumps({"version": 1, "basis": "invented overly broad fixture view",
                                    "read_only_roots": [str(self.scratch)]}))
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("broad", json.dumps(report))

    def test_unadmitted_native_and_profile_overrides_refuse_before_tool_probes(self):
        self.rust_tools()
        for name in ("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER", "CC", "CARGO_PROFILE_DEV_OPT_LEVEL",
                     "RUSTDOCFLAGS", "CARGO_BUILD_TARGET"):
            with self.subTest(variable=name):
                self.calls.unlink(missing_ok=True)
                self.env[name] = "unadmitted-value"
                try:
                    result, report = self.invoke()
                    self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
                    self.assertIn(name, json.dumps(report))
                    self.assertFalse(self.calls.exists())
                finally:
                    self.env.pop(name)

    def test_namespace_profile_requires_its_view_before_probing_tools(self):
        self.native_node()
        self.policy["profiles"]["functional"]["namespace_view_required"] = True
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("namespace view", json.dumps(report))
        self.assertNotIn("fixture-node", result.stdout)
        view = self.scratch / "required-view.json"
        view.write_text(json.dumps({"version": 1, "basis": "invented namespace tool exposures",
                                    "read_only_roots": [str(self.bin.parent)]}))
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_minimal_path_exposure_must_exist_and_match_the_admitted_object(self):
        minimal = self.scratch / "minimal-bin"
        self.policy["profiles"]["functional"]["fixture_execution"] = {
            "minimal_path": [str(minimal)], "same_primary": {
                "cargo": {"lookup": "minimal_path", "primary": "cargo"}}}
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("minimal", json.dumps(report))
        minimal.mkdir()
        shutil.copyfile(self.bin / "cargo", minimal / "cargo")
        (minimal / "cargo").chmod(0o700)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("same admitted", json.dumps(report))
        (minimal / "cargo").unlink()
        (minimal / "cargo").symlink_to(self.bin / "cargo")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["fixture_execution"]["same_primary"]["cargo"]["resolved_path"],
                         str(self.bin / "cargo"))

    def test_fixed_helper_is_independent_and_must_be_in_the_namespace_view(self):
        self.native_node()
        helper = self.scratch / "system-bin/python3"
        helper.parent.mkdir()
        shutil.copyfile(self.bin / "node", helper)
        helper.chmod(0o700)
        self.policy["profiles"]["functional"].update(namespace_view_required=True, fixture_execution={
            "minimal_path": [str(helper.parent)], "fixed_helpers": {
                "system_python": {"absolute": str(helper), "args": ["--version"],
                                  "version_regex": r"^fixture-node 22\.23\.4$"}}})
        view = self.scratch / "fixed-view.json"
        declaration = {"version": 1, "basis": "invented separate system interpreter",
                       "read_only_roots": [str(self.bin.parent)]}
        view.write_text(json.dumps(declaration))
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("fixture:system_python", json.dumps(report))
        declaration["read_only_roots"].append(str(helper.parent))
        view.write_text(json.dumps(declaration))
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["fixture_execution"]["fixed_helpers"]["system_python"]["resolved_path"], str(helper))
        self.assertIn("fixture:system_python", report["configuration_inputs"]["tool_objects"])

    def test_fixed_primary_path_cannot_point_at_a_different_object(self):
        fixed = self.scratch / "fixed-git"
        shutil.copyfile(self.bin / "cargo", fixed)
        fixed.chmod(0o700)
        self.policy["profiles"]["functional"]["fixture_execution"] = {
            "minimal_path": [str(self.bin)], "same_primary": {
                "fixed_git": {"absolute": str(fixed), "primary": "cargo"}}}
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("same admitted", json.dumps(report))
        fixed.unlink()
        fixed.symlink_to(self.bin / "cargo")
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_fixture_resources_need_explicit_trusted_read_only_exposure(self):
        self.native_node()
        library = self.scratch / "system-lib"
        library.mkdir()
        self.policy["profiles"]["functional"].update(namespace_view_required=True, fixture_execution={
            "minimal_path": [str(self.bin)], "read_only_resources": [str(library)]})
        view = self.scratch / "resource-view.json"
        declaration = {"version": 1, "basis": "invented fixture libraries",
                       "read_only_roots": [str(self.bin.parent)]}
        view.write_text(json.dumps(declaration))
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("fixture-resource", json.dumps(report))
        declaration["read_only_roots"].append(str(library))
        view.write_text(json.dumps(declaration))
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        library.chmod(0o777)
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)

    def test_optional_system_resource_is_checked_when_present(self):
        library = self.scratch / "optional-lib64"
        self.policy["profiles"]["functional"]["fixture_execution"] = {
            "minimal_path": [str(self.bin)],
            "read_only_resources": [{"path": str(library), "if_present": True}]}
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        library.mkdir(mode=0o777)
        library.chmod(0o777)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("untrusted fixture resource", json.dumps(report))

    def test_system_python_runtime_paths_need_visibility_independently_of_primary_python(self):
        self.native_node()
        library = self.scratch / "system-python-lib"
        library.mkdir()
        helper = self.scratch / "system-python"
        self.native_fixture(helper, json.dumps({"stdlib": str(library), "socket": str(library)}))
        self.policy["profiles"]["functional"].update(namespace_view_required=True, fixture_execution={
            "minimal_path": [str(self.bin)], "fixed_helpers": {
                "system_python": {"absolute": str(helper), "python_runtime": True}}})
        view = self.scratch / "python-runtime-view.json"
        declaration = {"version": 1, "basis": "invented independent Python runtime",
                       "read_only_roots": [str(self.bin.parent), str(helper)]}
        view.write_text(json.dumps(declaration))
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("fixture-resource", json.dumps(report))
        declaration["read_only_roots"].append(str(library))
        view.write_text(json.dumps(declaration))
        result, report = self.invoke("--namespace-view", view)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["fixture_execution"]["fixed_helpers"]["system_python"]["runtime_paths"]["stdlib"],
                         str(library))

    def test_system_python_builtin_extension_needs_no_imaginary_file(self):
        library = self.scratch / "system-python-lib"
        library.mkdir()
        helper = self.scratch / "system-python"
        self.native_fixture(helper, json.dumps({"stdlib": str(library), "socket_native": None}))
        self.policy["profiles"]["functional"]["fixture_execution"] = {
            "minimal_path": [str(self.bin)], "fixed_helpers": {
                "system_python": {"absolute": str(helper), "python_runtime": True}}}
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["fixture_execution"]["fixed_helpers"]["system_python"]["builtin_modules"],
                         ["socket_native"])

    def test_rust_context_timeout_preserves_partial_streams_and_infrastructure_report(self):
        self.rust_tools()
        self.tool("rustup", "rustup invented",
            "if sys.argv[1:]==['show','active-toolchain']:\n"
            " print('partial native output',flush=True)\n"
            " print('partial native error',file=sys.stderr,flush=True)\n"
            " import time;time.sleep(12)\n")
        result, report = self.invoke(timeout=20)
        self.assertEqual(result.returncode, 124, result.stdout + result.stderr)
        self.assertEqual(report["status"], "infrastructure_failure")
        probe = report["discrepancies"][0]["probe"]
        self.assertTrue(probe["timed_out"])
        self.assertIsNone(probe["raw_child_exit"])
        self.assertIn("partial native output", result.stdout)
        self.assertIn("partial native error", result.stderr)
        logs = self.scratch / "output-1/probes"
        self.assertEqual((logs / "rustup-active.stdout").read_bytes(), b"partial native output\n")
        self.assertEqual((logs / "rustup-active.stderr").read_bytes(), b"partial native error\n")

    def test_timed_out_probe_stops_its_descendant_process_group(self):
        pid_file = self.scratch / "probe-descendant.pid"
        self.tool("cargo", "unused",
            "import subprocess,time\n"
            f"child=subprocess.Popen([{sys.executable!r},'-c','import time;time.sleep(30)'], "
            "stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\n"
            f"open({str(pid_file)!r},'w').write(str(child.pid))\n"
            "print('probe child started',flush=True)\n"
            "time.sleep(30)\n")
        try:
            result, report = self.invoke(timeout=20)
            self.assertEqual(result.returncode, 124, result.stdout + result.stderr)
            self.assertEqual(report["status"], "infrastructure_failure")
            child = int(pid_file.read_text())
            # A killed orphan can briefly remain as a zombie under a container's
            # init; it must no longer be an executing process.
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                try:
                    os.kill(child, 0)
                except ProcessLookupError:
                    break
                proc_stat = Path("/proc") / str(child) / "stat"
                if proc_stat.exists() and proc_stat.read_text().split(")", 1)[1].lstrip().startswith("Z"):
                    break
                time.sleep(0.02)
            else:
                self.fail("timed-out probe left its descendant executing")
        finally:
            if pid_file.exists():
                try:
                    os.kill(int(pid_file.read_text()), signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_sqlite_timeout_keeps_its_partial_error_and_report(self):
        self.policy["profiles"]["functional"].update(tools=["python3"], python_sqlite=True)
        self.policy["tools"]["python3"] = {"command": "python3", "args": ["--version"],
            "version": "Python invented", "kind": "executable"}
        self.tool("python3", "Python invented",
            "if sys.argv[1:2]==['-c']:\n"
            " print('SQLite probe stalled',file=sys.stderr,flush=True)\n"
            " import time;time.sleep(12)\n")
        result, report = self.invoke(timeout=20)
        self.assertEqual(result.returncode, 124, result.stdout + result.stderr)
        self.assertEqual(report["status"], "infrastructure_failure")
        self.assertTrue(report["discrepancies"][0]["probe"]["timed_out"])
        self.assertIn("SQLite probe stalled", result.stderr)

    def test_hidden_cargo_and_musl_overrides_refuse_before_probes(self):
        self.rust_tools()
        for name in ("CARGO_ALIAS_CLIPPY", "CARGO_ALIAS_NEXTEST", "CARGO_UNSTABLE_BUILD_STD",
                     "CARGO_HOST_LINKER", "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTDOCFLAGS", "REALGCC"):
            with self.subTest(variable=name):
                self.calls.unlink(missing_ok=True)
                self.env[name] = "invented-override"
                try:
                    result, report = self.invoke()
                    self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
                    self.assertIn(name, json.dumps(report))
                    self.assertFalse(self.calls.exists())
                finally:
                    self.env.pop(name)

    def test_explicitly_admitted_target_documentation_flags_are_bound(self):
        self.rust_tools()
        name = "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTDOCFLAGS"
        self.env[name] = "--cfg invented_admitted"
        self.policy["profiles"]["functional"]["required_environment"] = {name: self.env[name]}
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["configuration_inputs"]["environment"][name], self.env[name])

    def test_root_distribution_configuration_changes_identity_and_refuses_stale_acquisition(self):
        self.native_fixture(self.bin / "go", "go version go1.26.0")
        self.policy["profiles"]["functional"]["tools"] = ["go"]
        self.policy["tools"]["go"] = {"command": "go", "args": ["version"], "version": "go version go1.26.0",
            "kind": "native", "root_env": "GOROOT", "distribution_paths": ["bin", "pkg", "src", "lib"]}
        self.env["GOROOT"] = str(self.bin.parent)
        config = self.bin.parent / "go.env"
        config.write_text("GOTOOLCHAIN=local\n")
        result, first = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        go = first["tool_identities"]["go"]
        expected = self.scratch / "acquisition.json"
        expected.write_text(json.dumps({"pins_sha256": first["pins_sha256"], "tools": {"go": {
            "sha256": go["sha256"], "distribution_sha256": go["distribution_sha256"]}}}))
        config.write_text("GOFLAGS=-toolexec=/invented/wrapper\n")
        result, changed = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotEqual(first["configuration_inputs_id"], changed["configuration_inputs_id"])
        result, report = self.invoke("--expected-tools", expected)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("acquisition", json.dumps(report))
        self.assertNotIn("go version", result.stdout)

    def test_managed_native_forwarder_cannot_replace_the_bootstrap_object(self):
        real = self.bin / "direct-nextest"
        self.native_fixture(real, "cargo-nextest invented-2")
        launcher = self.bin / "forwarder"
        subprocess.run([shutil.which("cc"), "-x", "c", "-o", str(launcher), "-"],
            input='#include <unistd.h>\nint main(void) { execl(' + json.dumps(str(real)) +
                  ', "cargo-nextest", "--version", (char *)0); return 1; }\n',
            text=True, check=True, capture_output=True, timeout=20)
        (self.bin / "cargo-nextest").unlink()
        (self.bin / "cargo-nextest").symlink_to(launcher)
        self.policy["profiles"]["functional"]["tools"] = ["nextest"]
        self.policy["tools"]["nextest"].update(kind="native", root_env="KABAN_NEXTEST_ROOT",
            executable_relative="bin/direct-nextest")
        self.env["KABAN_NEXTEST_ROOT"] = str(self.bin.parent)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertNotIn("cargo-nextest invented-2", result.stdout)
        (self.bin / "cargo-nextest").unlink()
        (self.bin / "cargo-nextest").symlink_to(real)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["tool_identities"]["nextest"]["resolved_path"], str(real))

    def test_native_mise_shim_is_refused_before_its_version_probe(self):
        mise = self.bin / "mise"
        self.native_fixture(mise, "cargo-nextest invented-2")
        (self.bin / "cargo-nextest").unlink()
        (self.bin / "cargo-nextest").symlink_to(mise)
        self.policy["profiles"]["functional"]["tools"] = ["nextest"]
        self.policy["tools"]["nextest"]["kind"] = "native"
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("shim", json.dumps(report))
        self.assertNotIn("cargo-nextest invented-2", result.stdout)

    def test_system_tool_must_match_its_fixed_object(self):
        system = self.scratch / "system-jq"
        self.native_fixture(system, "jq-invented-1")
        shutil.copyfile(system, self.bin / "jq")
        (self.bin / "jq").chmod(0o700)
        self.policy["profiles"]["functional"]["tools"] = ["jq"]
        self.policy["tools"]["jq"] = {"command": "jq", "args": ["--version"], "kind": "native",
            "version": "jq-invented-1", "system_path": str(system)}
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertNotIn("jq-invented-1", result.stdout)
        (self.bin / "jq").unlink()
        (self.bin / "jq").symlink_to(system)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_managed_flat_and_nested_binary_directories_are_supported(self):
        self.policy["profiles"]["functional"]["tools"] = ["nextest"]
        self.policy["tools"]["nextest"].update(kind="native", root_env="KABAN_NEXTEST_ROOT",
            executable_relative="cargo-nextest")
        for layout in ("flat", "nested/archive"):
            with self.subTest(layout=layout):
                directory = self.scratch / layout
                directory.mkdir(parents=True)
                real = directory / "cargo-nextest"
                self.native_fixture(real, "cargo-nextest invented-2")
                self.env["KABAN_NEXTEST_ROOT"] = str(directory)
                (self.bin / "cargo-nextest").unlink()
                (self.bin / "cargo-nextest").symlink_to(real)
                result, report = self.invoke()
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(report["tool_identities"]["nextest"]["sha256"],
                                 hashlib.sha256(real.read_bytes()).hexdigest())

    def test_empty_controlled_overrides_are_not_silently_ignored(self):
        self.rust_tools()
        for name in ("CARGO_ALIAS_NEXTEST", "REALGCC"):
            with self.subTest(variable=name):
                self.calls.unlink(missing_ok=True)
                self.env[name] = ""
                try:
                    result, report = self.invoke()
                    self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
                    self.assertIn(name, json.dumps(report))
                    self.assertFalse(self.calls.exists())
                finally:
                    self.env.pop(name)

    def test_musl_wrapper_override_is_refused_without_rust_profile_flag(self):
        self.policy["profiles"]["functional"]["tools"] = ["musl-gcc"]
        self.policy["tools"]["musl-gcc"] = {"command": "musl-gcc", "args": ["--version"],
            "version": "gcc invented", "kind": "executable"}
        self.tool("musl-gcc", "gcc invented")
        self.env["REALGCC"] = "/invented/other-compiler"
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("REALGCC", json.dumps(report))
        self.assertFalse(self.calls.exists())

    @unittest.skipUnless(platform.system() == "Linux", "musl admission requires native Linux")
    def test_plain_native_compiler_cannot_masquerade_as_musl_gcc(self):
        host = self.rust_tools()
        architecture = host.split("-", 1)[0]
        path = self.bin / "musl-gcc"
        subprocess.run([shutil.which("cc"), "-x", "c", "-o", str(path), "-"],
            input='#include <stdio.h>\n#include <string.h>\nint main(int argc, char **argv) { '
                  'puts(argc>1 && strcmp(argv[1],"-dumpmachine")==0 ? ' +
                  json.dumps(architecture + '-linux-gnu') + ' : "gcc invented"); return 0; }\n',
            text=True, check=True, capture_output=True, timeout=20)
        self.policy["profiles"]["functional"]["native_musl"] = True
        self.policy["profiles"]["functional"]["tools"].append("musl-gcc")
        self.policy["tools"]["musl-gcc"] = {"command": "musl-gcc", "args": ["--version"],
            "version": "gcc invented", "kind": "native"}
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("musl", json.dumps(report))

    @unittest.skipUnless(platform.system() == "Linux", "musl admission requires native Linux")
    def test_musl_library_inputs_change_identity_and_writable_inputs_refuse(self):
        architecture = self.rust_tools().split("-", 1)[0]
        library = self.scratch / "musl-libraries"
        library.mkdir()
        libc = library / "libc.a"
        libc.write_bytes(b"!<arch>\ninvented archive\n")
        for name in ("crt1.o", "crti.o", "crtn.o"):
            (library / name).write_bytes(b"invented startup object\n")
        specs = library / "musl-gcc.specs"
        specs.write_text("*link_libgcc:\n-L" + str(library) + "\n")
        backend = self.bin / "fixture-gcc"
        recipe = (f" {backend} -static -nostdlib -dynamic-linker /lib/ld-musl-{architecture}.so.1 "
                  f"{library}/crt1.o {library}/crti.o -L{library} -lc {library}/crtn.o\n")
        source = ('#include <stdio.h>\n#include <string.h>\nint main(int argc,char **argv) { '
            'if(argc>1 && strcmp(argv[1],"-###")==0) { fputs(' + json.dumps(recipe) + ',stderr); return 0; } '
            'if(argc>1 && strcmp(argv[1],"-dumpmachine")==0) puts(' + json.dumps(architecture + '-linux-gnu') + '); '
            'else if(argc>1 && strcmp(argv[1],"-print-prog-name=ld")==0) puts(' + json.dumps(str(backend)) + '); '
            'else puts("gcc invented"); return 0; }\n')
        subprocess.run([shutil.which("cc"), "-x", "c", "-o", str(backend), "-"], input=source,
                       text=True, check=True, capture_output=True, timeout=20)
        wrapper = self.bin / "musl-gcc"
        wrapper.write_text('#!/bin/sh\nexec "${REALGCC:-' + str(backend) + '}" "$@" -specs "' + str(specs) + '"\n')
        wrapper.chmod(0o700)
        self.policy["profiles"]["functional"]["native_musl"] = True
        self.policy["profiles"]["functional"]["tools"].append("musl-gcc")
        self.policy["tools"]["musl-gcc"] = {"command": "musl-gcc", "args": ["--version"],
            "version": "gcc invented", "kind": "executable"}
        result, first = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("musl-component:libc", first["configuration_inputs"]["tool_objects"])
        libc.write_bytes(b"!<arch>\nchanged invented archive\n")
        result, changed = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotEqual(first["configuration_inputs_id"], changed["configuration_inputs_id"])
        libc.chmod(0o666)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("untrusted", json.dumps(report))
        libc.chmod(0o644)
        gnu_first = recipe.replace(" -static", " -L /invented/gnu -static")
        subprocess.run([shutil.which("cc"), "-x", "c", "-o", str(backend), "-"],
                       input=source.replace(json.dumps(recipe), json.dumps(gnu_first)),
                       text=True, check=True, capture_output=True, timeout=20)
        result, report = self.invoke()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("selected first", json.dumps(report))


if __name__ == "__main__":
    unittest.main()
