"""Public CLI contracts using invented candidate/discovery/result files."""
import json
import hashlib
import re
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
CLI = ROOT / "scripts/ci/verify.py"
CANDIDATE = "a" * 40
BASE = "b" * 40


class CiContract(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="kaban-contract-")
        self.addCleanup(self.temporary.cleanup)
        self.scratch = Path(self.temporary.name)

    def invoke(self, *args):
        return subprocess.run([sys.executable, str(CLI), *map(str, args)],
                              capture_output=True, text=True, timeout=10)

    def select(self, changes, *, event="pull_request", base=BASE):
        changed = self.scratch / "changes.json"
        changed.write_text(json.dumps({"candidate": CANDIDATE, "base": base,
            "trusted": True, "changes": changes}))
        output = self.scratch / "selection.json"
        args = ["select", "--event", event, "--candidate", CANDIDATE,
                "--changed-paths", changed, "--output", output]
        if base is not None:
            args += ["--base", base]
        result = self.invoke(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(output.read_text())

    def test_prose_change_preserves_every_current_base_group(self):
        selection = self.select([{"status": "M", "path": "docs/README.md"}])
        self.assertEqual(set(selection["profiles"]),
                         {"quality", "functional", "gnu", "musl"})
        self.assertTrue(all(o["required"] for o in selection["obligations"]))
        self.assertIn("functional.workspace", {o["id"] for o in selection["obligations"]})

    def test_missing_required_results_are_incomplete(self):
        self.select([{"status": "M", "path": "docs/README.md"}])
        results = self.scratch / "results"
        results.mkdir()
        result = self.invoke("aggregate", "--selection", self.scratch / "selection.json",
                             "--results", results)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "incomplete")
        self.assertIn("functional.workspace", {o["id"] for o in report["obligations"]})

    def seed_results(self, *, changes=None, event="pull_request"):
        selection = self.select(changes if changes is not None else [{"status": "M", "path": "docs/README.md"}], event=event)
        directory = self.scratch / "results"
        directory.mkdir()
        configuration = {"source": CANDIDATE, "manifests": {"Cargo.toml": "c" * 64},
            "locks": {"Cargo.lock": "d" * 64}, "packages": ["invented-package"],
            "features": [], "target": "invented-linux-target", "runtime": "gnu",
            "compiler": "invented-rustc-1", "native_toolchain": "invented-cc-1",
            "profile": "test", "flags": [], "environment": {}}
        policy = json.loads((ROOT / "scripts/ci/obligations.json").read_text())
        configuration.update(source_inputs={s["source"].rsplit(":", 1)[0]: "e" * 64 for s in policy["subchecks"]},
            dependency_graph={"kaban-index": ["kaban-core"], "kaban": ["kaban-core"], "kaban-core": []},
            resolved_features={"kaban-index": ["default"], "kaban": ["default"], "kaban-core": []},
            package_roots={"kaban-index": "kaban-index", "kaban": "kaban"}, native_execution=True,
            tool_objects={name: "f" * 64 for name in ("cargo", "rustc", "cc", "python3", "bash", "go", "scip-go",
                                                    "rust-producer", "rust-sysroot", "kaban-index")})
        configuration_id = hashlib.sha256(json.dumps(configuration, sort_keys=True,
            separators=(",", ":")).encode()).hexdigest()
        for group in selection["profiles"]:
            folder = directory / group
            folder.mkdir()
            (folder / "commands.log").write_text("all selected invented checks ran\n")
            result = {"version": 1, "candidate": CANDIDATE,
                "policy_sha256": selection["policy_sha256"], "group": group,
                "configuration_id": configuration_id, "configuration": configuration,
                "status": "pass", "child_exit": 0,
                "tool_identities": {"compiler": "invented-rustc-1"},
                "capabilities": {"os": "linux"}, "artifacts": [],
                "timings": {}, "resources": {}, "logs": ["commands.log"],
                "checks": [{"id": o["id"], "status": "pass", "child_exit": 0,
                            "logs": ["commands.log"]}
                           for o in selection["obligations"] if o["group"] == group]}
            for check in result["checks"]:
                row = next(o for o in selection["obligations"] if o["id"] == check["id"])
                if isinstance(row["predicate"], dict):
                    check.update(predicate=row["predicate"], executed=True, capabilities=dict(row["predicate"]))
            (folder / "result.json").write_text(json.dumps(result))
        return directory

    def aggregate(self):
        return self.invoke("aggregate", "--selection", self.scratch / "selection.json",
                           "--results", self.scratch / "results")

    def test_complete_matching_candidate_results_pass(self):
        self.seed_results()
        result = self.aggregate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "pass")

    def modify_result(self, group, update):
        path = self.scratch / "results" / group / "result.json"
        value = json.loads(path.read_text())
        update(value)
        path.write_text(json.dumps(value))

    def test_another_candidate_cannot_supply_required_evidence(self):
        self.seed_results()
        self.modify_result("functional", lambda value: value.update(candidate="f" * 40))
        result = self.aggregate()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("candidate", result.stdout)

    def test_failed_first_attempt_remains_a_failure_after_retry_passes(self):
        self.seed_results()
        path = self.scratch / "results/functional/result.json"
        failed = json.loads(path.read_text())
        failed["checks"][0].update(status="assertion_failure", child_exit=37)
        failed.update(status="assertion_failure", child_exit=37)
        retry = path.parent / "first-attempt"
        retry.mkdir()
        (retry / "commands.log").write_text("assertion failed\n")
        (retry / "result.json").write_text(json.dumps(failed))
        result = self.aggregate()
        self.assertEqual(result.returncode, 37, result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "assertion_failure")

    def test_corrupt_or_incompatible_evidence_never_passes(self):
        updates = {
            "policy": lambda v: v.update(policy_sha256="e" * 64),
            "configuration": lambda v: v["configuration"].update(features=["unexpected"]),
            "empty checks": lambda v: v.update(checks=[]),
            "wrong group": lambda v: v.update(group="gnu"),
            "cancelled": lambda v: v.update(status="cancelled", child_exit=130),
            "exit 3": lambda v: v.update(status="incomplete", child_exit=3),
            "unknown status": lambda v: v["checks"][0].update(status="skipped"),
            "missing field": lambda v: v.pop("tool_identities"),
            "missing raw logs": lambda v: v.pop("logs"),
            "unexpected skip": lambda v: v["checks"][0].update(status="not_applicable"),
            "pass with failed child": lambda v: v["checks"][0].update(child_exit=37),
            "no logs": lambda v: v["checks"][0].update(logs=[]),
            "log escapes bundle": lambda v: v["checks"][0].update(logs=["../private.log"]),
        }
        directory = self.seed_results()
        original = (directory / "functional/result.json").read_text()
        for label, update in updates.items():
            with self.subTest(label=label):
                (directory / "functional/result.json").write_text(original)
                self.modify_result("functional", update)
                result = self.aggregate()
                self.assertEqual(result.returncode, 130 if label == "cancelled" else 3, result.stderr)
                self.assertNotEqual(json.loads(result.stdout)["status"], "pass")

    def test_malformed_json_is_reported_with_all_other_results_retained(self):
        directory = self.seed_results()
        (directory / "functional/result.json").write_text('{"broken":')
        result = self.aggregate()
        self.assertEqual(result.returncode, 3, result.stderr)
        report = json.loads(result.stdout)
        self.assertTrue(report["discrepancies"])
        self.assertEqual(next(o["status"] for o in report["obligations"]
                              if o["id"] == "quality.fmt"), "pass")

    def test_successful_parent_with_split_mandatory_marker_is_incomplete(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text(
            "test passed\nNOT RUN:\n  ns_checkout_is_readonly: unavailable\n")
        result = self.aggregate()
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertIn("ns_checkout_is_readonly", result.stdout)

    def test_unclassified_marker_cannot_disappear_inside_a_passing_test(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text("NOT RUN: invented_new_branch\n")
        result = self.aggregate()
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertIn("unclassified", result.stdout)

    def test_fixture_notices_do_not_become_mandatory_markers(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text(
            "NOT RUN real scip-go; committed fixture assertions still execute\n")
        self.assertEqual(self.aggregate().returncode, 0)

    def test_missing_discovery_and_doctest_captures_are_reported(self):
        output = self.scratch / "inventory.json"
        result = self.invoke("inventory", "--output", output, "--candidate", CANDIDATE)
        self.assertEqual(result.returncode, 3, result.stderr)
        report = json.loads(output.read_text())
        self.assertTrue(any("doctest" in d for d in report["discrepancies"]))
        self.assertTrue(any("workspace" in d for d in report["discrepancies"]))

    def add_namespace_counterpart(self, *, candidate=CANDIDATE,
                                  subcheck="subcheck.ns_checkout_is_readonly", folder_name="counterpart"):
        result = json.loads((self.scratch / "results/functional/result.json").read_text())
        result.update(group="qualification", candidate=candidate)
        policy = json.loads((ROOT / "scripts/ci/obligations.json").read_text())
        row = next(s for s in policy["subchecks"] if s["id"] == subcheck)
        result["capabilities"] = {"os": "linux", "namespaces_available": True, "isolation_tier": "namespaces"}
        if isinstance(row["predicate"], dict):
            result["capabilities"].update({k: v for k, v in row["predicate"].items() if k != "tmp_free_bytes_min"})
            if "tmp_free_bytes_min" in row["predicate"]:
                result["capabilities"]["tmp_free_bytes"] = row["predicate"]["tmp_free_bytes_min"]
        elif row["predicate"] == "selected tier none":
            result["capabilities"].update(isolation_tier="none", namespaces_available=False)
        result["checks"] = [{"id": subcheck, "status": "pass",
            "child_exit": 0, "logs": ["commands.log"], "predicate": row["predicate"],
            "parent_case": row["parent"], "executed": True}]
        folder = self.scratch / "results" / folder_name
        folder.mkdir()
        (folder / "commands.log").write_text("actual namespace assertion branch executed\n")
        (folder / "result.json").write_text(json.dumps(result))

    def test_same_candidate_specific_counterpart_discharges_only_its_subcheck(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text("NOT RUN: ns_checkout_is_readonly: unavailable\n")
        self.add_namespace_counterpart()
        self.assertEqual(self.aggregate().returncode, 0)
        (directory / "functional/commands.log").write_text(
            "NOT RUN: ns_checkout_is_readonly: unavailable\nNOT RUN: ns_tcp_connect_unreachable: unavailable\n")
        self.assertEqual(self.aggregate().returncode, 3)

    def test_counterpart_never_erases_parent_assertion_failure(self):
        self.seed_results()
        self.add_namespace_counterpart()
        self.modify_result("functional", lambda v: v["checks"][0].update(
            status="assertion_failure", child_exit=41))
        self.assertEqual(self.aggregate().returncode, 41)

    def seed_discovery(self):
        policy = json.loads((ROOT / "scripts/ci/obligations.json").read_text())
        samples = {
            "workspace": ("kaban-index", "namespaces", "ns_checkout_is_readonly"),
            "pdf": ("kaban-ingest", "golden_pdf", "a_cancelled_context_stops_the_extractor"),
            "no-default": ("kaban", "cli_pack", "two_forks_search_immediately_and_edit_independently"),
            "producer": ("kaban-rust-scip", "backend", "a_shadow_backend_first_on_the_child_path_never_executes"),
        }
        source_inputs = {}
        inputs = [ROOT / p for p in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")]
        for directory in (".cargo", "crates", "tools/kaban-rust-scip", "fixtures", "examples"):
            inputs.extend(p for p in (ROOT / directory).rglob("*") if p.is_file()
                          and "target" not in p.relative_to(ROOT).parts)
        for source in inputs:
            source_inputs[str(source.relative_to(ROOT))] = hashlib.sha256(source.read_bytes()).hexdigest()
        captures = {"version": 1, "candidate": CANDIDATE, "discovery": {}, "doctests": {},
                    "source_inputs": source_inputs}
        tools = {"rustc": "invented", "cargo_binary": {"path": "/admitted/toolchain/cargo", "sha256": "c" * 64},
            "cargo_launch": {"command": "cargo", "path": "/admitted/bin/cargo",
                "resolved_path": "/admitted/bin/rustup", "sha256": "b" * 64,
                "effective_path": "/admitted/toolchain/cargo", "effective_sha256": "c" * 64,
                "uid": 1000, "euid": 1000, "mode": 0o755, "regular": True}}
        for name, (package, binary, case) in samples.items():
            captures["discovery"][name] = {"candidate": CANDIDATE, "child_exit": 0,
                "configuration": policy["discovery"][name], "tool_identities": tools,
                "command": ["cargo", *policy["discovery"][name]["command"], "--locked"],
                "data": {"test-count": 1, "rust-suites": {package + "::" + binary: {
                    "package-name": package, "binary-name": binary, "kind": "test",
                    "testcases": {case: {"ignored": name == "producer"}}}}}}
        for package in policy["doctests"]:
            captures["doctests"][package] = {"candidate": CANDIDATE, "child_exit": 0,
                "tool_identities": tools,
                "command": ["cargo", "test", "--doc", "--locked", "-p", package.split("+")[0]],
                "stdout": "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored\n"}
            if package.endswith("+pdf"):
                captures["doctests"][package]["command"].extend(["--features", "pdf"])
        captures["metadata"] = {}
        for workspace in ("root", "producer"):
            packages = [p for p, w in policy["doctests"].items() if w == workspace and "+" not in p]
            if workspace == "root":
                packages.append("kaban")
            workspace_root = ROOT if workspace == "root" else ROOT / "tools/kaban-rust-scip"
            package_rows = []
            for package in packages:
                package_root = ROOT / "crates" / package if workspace == "root" else workspace_root
                targets = []
                if (package_root / "src/lib.rs").exists():
                    targets.append({"name": package.replace("-", "_"), "kind": ["lib"], "doctest": True,
                                    "src_path": str(package_root / "src/lib.rs")})
                for case_file in (package_root / "tests").glob("*.rs"):
                    targets.append({"name": case_file.stem, "kind": ["test"], "doctest": False,
                                    "src_path": str(case_file)})
                package_rows.append({"name": package, "targets": targets})
            captures["metadata"][workspace] = {"workspace_root": str(workspace_root), "packages": package_rows}
        file = self.scratch / "captures.json"
        file.write_text(json.dumps(captures))
        return captures, file

    def inventory(self, captures):
        output = self.scratch / "inventory.json"
        result = self.invoke("inventory", "--captures", captures, "--candidate", CANDIDATE,
                             "--output", output)
        return result, json.loads(output.read_text())

    def test_known_discoveries_and_doctest_captures_form_inventory(self):
        _, file = self.seed_discovery()
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 0, report["discrepancies"])
        self.assertEqual(len([c for c in report["cases"] if c["origin"] == "nextest"]), 4)
        self.assertTrue(any(c["ignored"] for c in report["cases"] if c["origin"] == "nextest"))

    def test_unclassified_generated_discovery_is_a_discrepancy(self):
        captures, file = self.seed_discovery()
        suite = next(iter(captures["discovery"]["workspace"]["data"]["rust-suites"].values()))
        suite["testcases"]["new_macro::invented_generated_case"] = {"ignored": False}
        captures["discovery"]["workspace"]["data"]["test-count"] += 1
        file.write_text(json.dumps(captures))
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("unclassified discovered case" in d for d in report["discrepancies"]))

    def test_new_marker_source_site_requires_an_obligation_mapping(self):
        _, file = self.seed_discovery()
        source = self.scratch / "source"
        shutil.copytree(ROOT / "crates", source / "crates")
        shutil.copytree(ROOT / "tools", source / "tools")
        added = source / "crates/kaban-index/tests/new_marker.rs"
        added.write_text('fn extra() { eprintln!("NOT RUN: new_unmapped_branch: unavailable"); }\n')
        output = self.scratch / "inventory.json"
        result = self.invoke("inventory", "--root", source, "--captures", file,
                             "--candidate", CANDIDATE, "--output", output)
        report = json.loads(output.read_text())
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("unclassified emitter" in d for d in report["discrepancies"]))

    def test_selection_keeps_base_and_broadens_shared_unknown_renamed_inputs(self):
        paths = ["crates/kaban-api/src/lib.rs", "Cargo.lock", "tools/kaban-rust-scip/src/lib.rs",
                 "crates/kaban-index/src/runner/mod.rs", "unknown-new-build-input"]
        for path in paths:
            with self.subTest(path=path):
                selection = self.select([{"status": "D", "path": path}])
                self.assertTrue({"quality", "functional", "gnu", "musl"} <= set(selection["profiles"]))
                if path != paths[0]:
                    self.assertTrue({"producer", "qualification"} <= set(selection["profiles"]))
        selection = self.select([{"status": "R", "path": "docs/old-tool.md",
                                  "old_path": "tools/removed-producer.rs"}])
        self.assertIn("producer", selection["profiles"])

    def test_schedule_selects_all_qualification_with_prerequisites(self):
        selection = self.select([], event="schedule", base=None)
        self.assertEqual(set(selection["profiles"]),
            {"quality", "functional", "gnu", "musl", "producer", "qualification"})
        self.assertIn("complementary-isolation", selection["prerequisites"]["qualification"]["capabilities"])

    def test_missing_and_unavailable_base_select_broader_scope(self):
        selection = self.select([], base=None)
        self.assertIn("qualification", selection["profiles"])
        output = self.scratch / "without-objects.json"
        result = self.invoke("select", "--event", "push", "--candidate", CANDIDATE,
                             "--base", BASE, "--output", output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("unavailable", json.loads(output.read_text())["impact_reason"])

    def test_invalid_change_input_broadens_instead_of_dropping_work(self):
        changed = self.scratch / "bad-changes.json"
        changed.write_text('{"bad":')
        output = self.scratch / "selection.json"
        result = self.invoke("select", "--event", "pull_request", "--candidate", CANDIDATE,
                             "--base", BASE, "--changed-paths", changed, "--output", output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("qualification", json.loads(output.read_text())["profiles"])

    def test_truncated_selection_cannot_remove_a_current_base_obligation(self):
        self.seed_results()
        file = self.scratch / "selection.json"
        selection = json.loads(file.read_text())
        selection["obligations"] = []
        file.write_text(json.dumps(selection))
        result = self.aggregate()
        self.assertEqual(result.returncode, 3)
        self.assertNotEqual(json.loads(result.stdout)["status"], "pass")

    def test_new_metadata_library_requires_its_own_doctest_capture(self):
        captures, file = self.seed_discovery()
        captures["metadata"]["root"]["packages"].append({"name": "new-doc-library",
            "targets": [{"name": "new_doc_library", "kind": ["lib"], "doctest": True}]})
        file.write_text(json.dumps(captures))
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("new-doc-library" in d for d in report["discrepancies"]))

    def test_captures_from_different_source_bytes_are_incomplete(self):
        captures, file = self.seed_discovery()
        captures["source_inputs"]["Cargo.lock"] = "0" * 64
        file.write_text(json.dumps(captures))
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("source" in d for d in report["discrepancies"]))

    def test_wrong_discovery_or_doctest_command_cannot_qualify_a_configuration(self):
        captures, file = self.seed_discovery()
        captures["discovery"]["pdf"]["command"] = ["cargo", "nextest", "list", "--workspace"]
        captures["doctests"]["kaban-core"]["command"] = ["cargo", "test", "-p", "kaban-core"]
        file.write_text(json.dumps(captures))
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("command" in d for d in report["discrepancies"]))

    def test_comments_between_test_attributes_and_function_keep_case_classified(self):
        captures, file = self.seed_discovery()
        suites = captures["discovery"]["workspace"]["data"]["rust-suites"]
        suite = suites.pop("kaban-index::namespaces")
        suite["binary-name"] = "runner"
        suites["kaban-index::runner"] = suite
        suite["testcases"] = {
            "publish_binds_sqlite_output_to_the_admitted_inode_under_a_renamed_directory": {"ignored": False}}
        file.write_text(json.dumps(captures))
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 0, report["discrepancies"])

    def test_marker_in_a_format_argument_also_requires_mapping(self):
        _, file = self.seed_discovery()
        source = self.scratch / "source"
        shutil.copytree(ROOT / "crates", source / "crates")
        shutil.copytree(ROOT / "tools", source / "tools")
        (source / "crates/kaban-index/tests/new_marker.rs").write_text(
            'fn extra() { eprintln!("{}", "NOT RUN: new_branch: unavailable"); }\n')
        output = self.scratch / "inventory.json"
        result = self.invoke("inventory", "--root", source, "--captures", file,
                             "--candidate", CANDIDATE, "--output", output)
        report = json.loads(output.read_text())
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("unclassified" in d for d in report["discrepancies"]))

    def test_inverse_marker_needs_its_specific_unavailable_host_counterpart(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text(
            "NOT RUN: required-unavailable Rust probes: isolating tier available\n")
        self.assertEqual(self.aggregate().returncode, 3)
        self.add_namespace_counterpart(subcheck="subcheck.required_unavailable_rust_probes")
        self.assertEqual(self.aggregate().returncode, 0)

    def test_failed_counterpart_is_not_hidden_by_a_passing_parent_or_retry(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text("NOT RUN: ns_checkout_is_readonly: unavailable\n")
        self.add_namespace_counterpart()
        original = directory / "counterpart/result.json"
        failed = json.loads(original.read_text())
        failed["checks"][0].update(status="assertion_failure", child_exit=37)
        retry = directory / "counterpart/first-attempt"
        retry.mkdir()
        (retry / "commands.log").write_text("assertion failure\n")
        (retry / "result.json").write_text(json.dumps(failed))
        self.assertEqual(self.aggregate().returncode, 37)

    def test_not_applicable_branch_requires_declared_successful_counterpart(self):
        directory = self.seed_results()
        self.add_namespace_counterpart()
        counterpart = directory / "counterpart/result.json"
        value = json.loads(counterpart.read_text())
        value.update(status="not_applicable", child_exit=0)
        value["capabilities"].update(namespaces_available=False, isolation_tier="none")
        value["checks"][0].update(status="not_applicable", executed=False,
            counterpart_ids=["subcheck.ns_checkout_is_readonly"], reason="namespace unavailable on this host")
        folder = directory / "not-applicable"
        folder.mkdir()
        (folder / "commands.log").write_text("unavailable host predicate observed\n")
        (folder / "result.json").write_text(json.dumps(value))
        self.assertEqual(self.aggregate().returncode, 0)
        counterpart.unlink()
        self.assertEqual(self.aggregate().returncode, 3)

    def test_interleaved_marker_and_unmarked_branch_accept_exact_counterparts(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text(
            "NOT RUN: ns_checkout_is_readonly: the namespaces tier is unavailable on this host (x)\n"
            "go_module_cache_is_an_input_on_every_tier: no namespaces tier on this host; the tier comparison did not run\n")
        self.add_namespace_counterpart()
        self.add_namespace_counterpart(subcheck="subcheck.go-module-cache-namespace-comparison",
                                       folder_name="go-comparison")
        result = self.aggregate()
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_go_prerequisite_markers_have_separate_dischargeable_branches(self):
        directory = self.seed_results()
        for reason, subcheck in [
            ("no scip-go reachable", "subcheck.go_module_cache_is_an_input_on_every_tier"),
            ("no go on this host", "subcheck.go_module_cache_is_an_input_on_every_tier.branch-3620"),
        ]:
            with self.subTest(reason=reason):
                folder = directory / "counterpart"
                if folder.exists():
                    shutil.rmtree(folder)
                (directory / "functional/commands.log").write_text(
                    "NOT RUN: go_module_cache_is_an_input_on_every_tier: " + reason + "\n")
                self.add_namespace_counterpart(subcheck=subcheck)
                result = self.aggregate()
                self.assertEqual(result.returncode, 0, result.stdout)

    def test_shared_security_dependencies_and_readers_select_qualification(self):
        for path in ("crates/kaban-core/src/lib.rs", "crates/kaban-policy/src/contain.rs",
                     "crates/kaban/tests/mcp_rust_acceptance.rs", "crates/new-unclassified/src/lib.rs"):
            with self.subTest(path=path):
                selection = self.select([{"status": "M", "path": path}])
                self.assertTrue({"producer", "qualification"} <= set(selection["profiles"]))

    def test_non_utf8_log_does_not_hide_another_groups_failure(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_bytes(b"raw child output\xff\n")
        self.modify_result("quality", lambda v: v.update(status="assertion_failure", child_exit=37))
        result = self.aggregate()
        self.assertEqual(result.returncode, 37, result.stdout)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "assertion_failure")
        self.assertTrue(report["obligations"])

    def test_raw_group_log_cannot_hide_a_marker_behind_check_summaries(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text("NOT RUN: ns_checkout_is_readonly: unavailable\n")
        (directory / "functional/summary.log").write_text("parent suite passed\n")
        self.modify_result("functional", lambda v: [c.update(logs=["summary.log"]) for c in v["checks"]])
        self.assertEqual(self.aggregate().returncode, 3)
        self.add_namespace_counterpart()
        self.assertEqual(self.aggregate().returncode, 0)

    def test_group_log_paths_must_exist_inside_the_result_bundle(self):
        self.seed_results()
        for path in ("../private.log", "missing.log"):
            with self.subTest(path=path):
                self.modify_result("functional", lambda v: v.update(logs=[path]))
                self.assertEqual(self.aggregate().returncode, 3)

    def test_cancelled_group_keeps_its_status_and_native_exit(self):
        self.seed_results()
        def cancel(value):
            value.update(status="cancelled", child_exit=130)
            for check in value["checks"]:
                check.update(status="cancelled", child_exit=130)
        self.modify_result("functional", cancel)
        result = self.aggregate()
        self.assertEqual(result.returncode, 130, result.stdout)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "cancelled")
        self.assertEqual(next(o["status"] for o in report["obligations"]
                              if o["id"] == "functional.workspace"), "cancelled")

    def test_failed_child_exit_three_keeps_failure_distinct_from_incomplete(self):
        self.seed_results()
        self.modify_result("functional", lambda v: v["checks"][0].update(
            status="assertion_failure", child_exit=3))
        result = self.aggregate()
        self.assertEqual(result.returncode, 1, result.stdout)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "assertion_failure")
        self.assertEqual(next(o["attempts"][0]["child_exit"] for o in report["obligations"]
                              if o["id"] == "functional.workspace"), 3)

    def test_pr_qualification_does_not_require_scheduled_scale_or_reproducibility(self):
        self.seed_results(changes=[{"status": "M", "path": "Cargo.lock"}])
        selection = json.loads((self.scratch / "selection.json").read_text())
        names = {o["id"] for o in selection["obligations"]}
        self.assertIn("qualification.available-isolation", names)
        self.assertNotIn("qualification.ignored-scale", names)
        self.assertNotIn("qualification.reproducibility", names)
        self.assertEqual(self.aggregate().returncode, 0)

    def test_schedule_keeps_scale_and_reproducibility_obligations(self):
        selection = self.select([], event="schedule", base=None)
        names = {o["id"] for o in selection["obligations"]}
        self.assertTrue({"qualification.ignored-scale", "qualification.reproducibility"} <= names)

    def test_proc_macro_library_also_requires_doctest_capture(self):
        captures, file = self.seed_discovery()
        captures["metadata"]["root"]["packages"].append({"name": "new-macro-library",
            "targets": [{"name": "new_macro_library", "kind": ["proc-macro"], "doctest": True}]})
        file.write_text(json.dumps(captures))
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("new-macro-library" in d for d in report["discrepancies"]))

    def test_generated_case_cannot_borrow_a_known_functions_final_name(self):
        captures, file = self.seed_discovery()
        data = captures["discovery"]["workspace"]["data"]
        suite = next(iter(data["rust-suites"].values()))
        suite["testcases"]["aaa_macro::ns_checkout_is_readonly"] = {"ignored": False}
        data["test-count"] += 1
        file.write_text(json.dumps(captures))
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("unclassified discovered case" in d for d in report["discrepancies"]))
        self.assertTrue(any(c.get("case") == "ns_checkout_is_readonly" for c in report["cases"]))

    def test_cfg_alternative_helpers_do_not_make_real_tests_ambiguous(self):
        captures, file = self.seed_discovery()
        source = self.scratch / "source"
        shutil.copytree(ROOT, source, ignore=shutil.ignore_patterns(".git", ".serena", "target", "__pycache__"))
        path = source / "crates/kaban-index/tests/namespaces.rs"
        with path.open("a") as output:
            output.write('\n#[cfg(target_os = "linux")]\nfn conditional_helper() -> u8 { 1 }\n'
                         '#[cfg(not(target_os = "linux"))]\nfn conditional_helper() -> u8 { 2 }\n')
        for relative in captures["source_inputs"]:
            captures["source_inputs"][relative] = hashlib.sha256((source / relative).read_bytes()).hexdigest()
        file.write_text(json.dumps(captures))
        output = self.scratch / "inventory.json"
        result = self.invoke("inventory", "--root", source, "--captures", file,
                             "--candidate", CANDIDATE, "--output", output)
        self.assertEqual(result.returncode, 0, json.loads(output.read_text())["discrepancies"])

    def test_blank_line_before_markers_does_not_change_branch_registrations(self):
        captures, file = self.seed_discovery()
        source = self.scratch / "source"
        shutil.copytree(ROOT, source, ignore=shutil.ignore_patterns(".git", ".serena", "target", "__pycache__"))
        path = source / "crates/kaban-index/tests/cli.rs"
        path.write_text("\n" + path.read_text())
        for relative in captures["source_inputs"]:
            captures["source_inputs"][relative] = hashlib.sha256((source / relative).read_bytes()).hexdigest()
        file.write_text(json.dumps(captures))
        output = self.scratch / "inventory.json"
        result = self.invoke("inventory", "--root", source, "--captures", file,
                             "--candidate", CANDIDATE, "--output", output)
        self.assertEqual(result.returncode, 0, json.loads(output.read_text())["discrepancies"])

    def test_helper_marker_without_case_context_cannot_borrow_one_callers_proof(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text(
            "NOT RUN: set KABAN_TEST_RUST_PRODUCER to the installed kaban-rust-scip\n")
        self.add_namespace_counterpart(subcheck="subcheck.rust_real_producer_cli_acceptance.inputs-producer")
        self.assertEqual(self.aggregate().returncode, 3)

    def test_helper_proof_discharges_only_the_case_named_by_raw_log_context(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text(
            "NOT RUN: set KABAN_TEST_RUST_PRODUCER to the installed kaban-rust-scip\n")
        context = {"workspace": "root", "package": "kaban-index", "binary": "cli",
                   "case": "rust_real_producer_cli_acceptance", "source": "crates/kaban-index/tests/cli.rs"}
        self.modify_result("functional", lambda v: v.update(log_cases={"commands.log": context}))
        self.add_namespace_counterpart(subcheck="subcheck.rust_real_producer_cli_acceptance.inputs-producer")
        self.assertEqual(self.aggregate().returncode, 0)
        context["case"] = "rust_real_producer_workspace_on_selected_tier"
        self.modify_result("functional", lambda v: v.update(log_cases={"commands.log": context}))
        self.assertEqual(self.aggregate().returncode, 3)

    def test_new_dynamic_namespace_caller_requires_its_own_branch_mapping(self):
        captures, file = self.seed_discovery()
        source = self.scratch / "source"
        shutil.copytree(ROOT, source, ignore=shutil.ignore_patterns(".git", ".serena", "target", "__pycache__"))
        path = source / "crates/kaban-index/tests/namespaces.rs"
        with path.open("a") as output:
            output.write('\n#[test]\nfn ns_new_case() { if !available("ns_new_case") { return; } }\n')
        data = captures["discovery"]["workspace"]["data"]
        data["test-count"] += 1
        next(iter(data["rust-suites"].values()))["testcases"]["ns_new_case"] = {"ignored": False}
        for relative in captures["source_inputs"]:
            captures["source_inputs"][relative] = hashlib.sha256((source / relative).read_bytes()).hexdigest()
        file.write_text(json.dumps(captures))
        output = self.scratch / "inventory.json"
        result = self.invoke("inventory", "--root", source, "--captures", file,
                             "--candidate", CANDIDATE, "--output", output)
        report = json.loads(output.read_text())
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("dynamic helper" in d for d in report["discrepancies"]))

    def test_changed_unmarked_early_return_is_an_inventory_discrepancy(self):
        captures, file = self.seed_discovery()
        source = self.scratch / "source"
        shutil.copytree(ROOT, source, ignore=shutil.ignore_patterns(".git", ".serena", "target", "__pycache__"))
        path = source / "crates/kaban-index/tests/cli.rs"
        path.write_text(path.read_text().replace("the tier comparison did not run", "the comparison was omitted"))
        for relative in captures["source_inputs"]:
            captures["source_inputs"][relative] = hashlib.sha256((source / relative).read_bytes()).hexdigest()
        file.write_text(json.dumps(captures))
        output = self.scratch / "inventory.json"
        result = self.invoke("inventory", "--root", source, "--captures", file,
                             "--candidate", CANDIDATE, "--output", output)
        report = json.loads(output.read_text())
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("unmarked" in d for d in report["discrepancies"]))

    def test_counterpart_capabilities_must_satisfy_its_execution_predicate(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text("NOT RUN: ns_checkout_is_readonly: unavailable\n")
        self.add_namespace_counterpart()
        path = directory / "counterpart/result.json"
        value = json.loads(path.read_text())
        value["capabilities"].update(isolation_tier="none", namespaces_available=False)
        path.write_text(json.dumps(value))
        self.assertEqual(self.aggregate().returncode, 3)

    def test_focused_counterpart_can_prove_the_same_assertion_closure(self):
        directory = self.seed_results()
        (directory / "functional/commands.log").write_text("NOT RUN: ns_checkout_is_readonly: unavailable\n")
        self.add_namespace_counterpart()
        path = directory / "counterpart/result.json"
        value = json.loads(path.read_text())
        value["configuration"].update(packages=["kaban-index"], features=["explicit-default"])
        value["configuration_id"] = hashlib.sha256(json.dumps(value["configuration"], sort_keys=True,
            separators=(",", ":")).encode()).hexdigest()
        path.write_text(json.dumps(value))
        self.assertEqual(self.aggregate().returncode, 0)
        value["configuration"]["resolved_features"]["kaban-core"] = ["different-effective-feature"]
        value["configuration_id"] = hashlib.sha256(json.dumps(value["configuration"], sort_keys=True,
            separators=(",", ":")).encode()).hexdigest()
        path.write_text(json.dumps(value))
        self.assertEqual(self.aggregate().returncode, 3)

    def test_unselected_optional_table_can_be_reported_not_applicable(self):
        self.seed_results()
        self.modify_result("gnu", lambda value: value["checks"].append({"id": "gnu.optional-table",
            "status": "not_applicable", "child_exit": 0, "logs": ["commands.log"], "executed": False,
            "predicate": {"optional": "size_table", "selected": False}, "counterpart_ids": [],
            "reason": "optional size table not selected"}))
        result = self.aggregate()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(json.loads(result.stdout)["optional_outcomes"][0]["status"], "not_applicable")

    def test_selected_conditional_obligation_can_use_its_qualified_counterpart(self):
        directory = self.seed_results(changes=[{"status": "M", "path": "Cargo.lock"}])
        file = directory / "qualification/result.json"
        value = json.loads(file.read_text())
        row = next(c for c in value["checks"] if c["id"] == "qualification.available-isolation")
        predicate = {"os": "linux", "namespaces_available": True, "isolation_tier": "namespaces"}
        row.update(status="not_applicable", predicate=predicate, executed=False,
            counterpart_ids=[row["id"]], reason="namespace capability unavailable here",
            capabilities={"os": "linux", "namespaces_available": False, "isolation_tier": "none"})
        file.write_text(json.dumps(value))
        self.assertEqual(self.aggregate().returncode, 3)
        counterpart = directory / "available-host"
        counterpart.mkdir()
        (counterpart / "commands.log").write_text("selected namespace profile actually executed\n")
        proof = json.loads(json.dumps(value))
        proof["checks"] = [{**row, "status": "pass", "executed": True, "child_exit": 0,
                            "capabilities": predicate}]
        (counterpart / "result.json").write_text(json.dumps(proof))
        result = self.aggregate()
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_emitter_cannot_move_to_an_unregistered_function(self):
        captures, file = self.seed_discovery()
        source = self.scratch / "source"
        shutil.copytree(ROOT, source, ignore=shutil.ignore_patterns(".git", ".serena", "target", "__pycache__"))
        policy = json.loads((ROOT / "scripts/ci/obligations.json").read_text())
        emitter = next(e for e in policy["emitters"] if e["parent"] == "index_go_drivers_real")
        path = source / emitter["source"]
        text = path.read_text()
        match = re.search(r'eprintln!\(\s*"' + re.escape(emitter["text"]) + r'"\s*\);', text)
        self.assertIsNotNone(match)
        statement = match[0]
        text = text[:match.start()] + 'eprintln!("ordinary output");' + text[match.end():]
        path.write_text(text + '\nfn new_unregistered_emitter() { ' + statement + ' }\n')
        captures["source_inputs"][emitter["source"]] = hashlib.sha256(path.read_bytes()).hexdigest()
        file.write_text(json.dumps(captures))
        output = self.scratch / "inventory.json"
        result = self.invoke("inventory", "--root", source, "--candidate", CANDIDATE, "--captures", file,
                             "--output", output)
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertIn("emitter parent", output.read_text())

    def test_discovery_cannot_use_an_unadmitted_cargo_executable(self):
        captures, file = self.seed_discovery()
        captures["discovery"]["workspace"]["command"][0] = "/untrusted/fake-cargo"
        file.write_text(json.dumps(captures))
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 3)
        self.assertTrue(any("Cargo executable" in d for d in report["discrepancies"]))

    def test_case_classification_keeps_branch_capabilities_and_real_generator(self):
        captures, file = self.seed_discovery()
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 0, report["discrepancies"])
        case = next(c for c in report["cases"] if c.get("case") == "ns_checkout_is_readonly")
        self.assertEqual(case["purpose"], "ns checkout is readonly")
        self.assertIn("example", case["technique"])
        branch = next(c for c in case["capabilities"]["branches"] if c["id"] == "subcheck.ns_checkout_is_readonly")
        self.assertTrue(branch["predicate"]["namespaces_available"])
        suite = captures["discovery"]["workspace"]["data"]
        suite["rust-suites"] = {"kaban-codegraph::grammar": {"package-name": "kaban-codegraph",
            "binary-name": "grammar", "kind": "test", "testcases": {
                "roundtrip::symbol_grammar_roundtrip": {"ignored": False}}}}
        file.write_text(json.dumps(captures))
        result, report = self.inventory(file)
        self.assertEqual(result.returncode, 0, report["discrepancies"])
        case = next(c for c in report["cases"] if c.get("case") == "roundtrip::symbol_grammar_roundtrip")
        self.assertIn("property", case["technique"])
        self.assertNotIn("example", case["technique"])
        self.assertEqual(case["scope"], "unit")

    def test_counterpart_rejects_changed_compiler_target_flags_or_emulation(self):
        self.seed_results()
        (self.scratch / "results/functional/commands.log").write_text(
            "NOT RUN: ns_checkout_is_readonly: unavailable\n")
        self.add_namespace_counterpart()
        proof = self.scratch / "results/counterpart/result.json"
        # The helper names its default folder after the branch.
        if not proof.exists():
            proof = next(p for p in (self.scratch / "results").rglob("result.json")
                         if "subcheck.ns_checkout_is_readonly" in p.read_text())
        original = proof.read_text()
        for field, changed in (("target", "another-linux-target"), ("compiler", "different-rustc"),
                               ("flags", ["-Ctarget-cpu=native"]), ("native_execution", False)):
            with self.subTest(field=field):
                value = json.loads(original)
                value["configuration"][field] = changed
                if field == "compiler":
                    value["tool_identities"]["compiler"] = changed
                value["configuration_id"] = hashlib.sha256(json.dumps(value["configuration"], sort_keys=True,
                    separators=(",", ":")).encode()).hexdigest()
                proof.write_text(json.dumps(value))
                self.assertEqual(self.aggregate().returncode, 3)
        proof.write_text(original)
        self.assertEqual(self.aggregate().returncode, 0)

    def test_cargo_capture_requires_trusted_matching_effective_object(self):
        captures, file = self.seed_discovery()
        original = json.dumps(captures)
        for key, changed in (("mode", 0o777), ("uid", 2001), ("regular", False),
                             ("effective_sha256", "0" * 64)):
            with self.subTest(field=key):
                captures = json.loads(original)
                captures["discovery"]["workspace"]["tool_identities"]["cargo_launch"][key] = changed
                file.write_text(json.dumps(captures))
                result, report = self.inventory(file)
                self.assertEqual(result.returncode, 3, report)

    def test_malformed_captured_tool_identity_stays_an_inventory_discrepancy(self):
        captures, file = self.seed_discovery()
        captures["discovery"]["workspace"]["tool_identities"] = ["not-an-object"]
        file.write_text(json.dumps(captures))
        output = self.scratch / "inventory.json"
        result = self.invoke("inventory", "--captures", file, "--candidate", CANDIDATE, "--output", output)
        self.assertEqual(result.returncode, 3, result.stderr)
        report = json.loads(output.read_text())
        self.assertTrue(any("tool identities" in d for d in report["discrepancies"]))


if __name__ == "__main__":
    unittest.main()
