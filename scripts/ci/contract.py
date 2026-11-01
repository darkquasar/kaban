"""Version 1 selection, inventory and result contracts (standard library only)."""

import hashlib
import ast
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
from collections import Counter


ROOT = Path(__file__).resolve().parents[2]
POLICY = Path(__file__).with_name("obligations.json")
EVENTS = ("pull_request", "push", "workflow_dispatch", "merge_group", "schedule")
STATUSES = ("pass", "assertion_failure", "infrastructure_failure", "incomplete",
            "cancelled", "not_applicable")


class InvalidEvidence(ValueError):
    """Invalid input cannot establish a verification claim."""


def digest(data):
    return hashlib.sha256(data).hexdigest()


def identity(value):
    return digest(json.dumps(value, sort_keys=True, ensure_ascii=True, allow_nan=False,
                             separators=(",", ":")).encode("utf-8"))


def read_json(path):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise InvalidEvidence("duplicate JSON key: " + key)
            result[key] = value
        return result
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"), object_pairs_hook=unique,
                          parse_constant=lambda value: invalid("invalid JSON constant: " + value))
    except (OSError, UnicodeError, ValueError) as error:
        raise InvalidEvidence(f"{path}: {error}") from error


def invalid(message):
    raise InvalidEvidence(message)


def require(condition, message):
    if not condition:
        invalid(message)


def commit(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value) and value != "0" * 40


def safe_path(value):
    return (isinstance(value, str) and bool(value) and "\\" not in value
            and not PurePosixPath(value).is_absolute()
            and all(part not in ("", ".", "..") for part in value.split("/")))


def load_policy(path=POLICY):
    policy = read_json(path)
    require(isinstance(policy, dict) and policy.get("version") == 1, "unsupported policy version")
    obligations = policy.get("obligations")
    require(isinstance(obligations, list) and obligations, "missing obligation map")
    names = [o.get("id") for o in obligations if isinstance(o, dict)]
    require(len(names) == len(obligations) and len(set(names)) == len(names), "duplicate/invalid obligations")
    rows = marker_rows(policy)
    by_id = {s["id"]: s for s in rows}
    for emitter in policy["emitters"]:
        if emitter["informational"]:
            continue
        variants = emitter.get("variants", [{"text": emitter["text"], "subcheck_ids": emitter["subcheck_ids"]}])
        for variant in variants:
            for subcheck_id in variant["subcheck_ids"]:
                message = variant["text"].replace("{test}", by_id[subcheck_id]["parent"])
                matched = [s["id"] for s in rows if re.search(s["marker_pattern"], message)]
                expected = [s["id"] for s in rows if s["id"] in variant["subcheck_ids"]]
                if "{test}" in variant["text"]:
                    expected = [subcheck_id]
                require(matched == expected, "ambiguous/unmapped registered emitter: " + emitter["source"])
                if len(matched) > 1:
                    require(all(by_id[name].get("context_required") for name in matched)
                            and len({by_id[name].get("marker_family") for name in matched}) == 1,
                            "undeclared ambiguous marker family")
    return policy, digest(Path(path).read_bytes())


def marker_rows(policy):
    unmarked = {s["id"] for s in policy.get("unmarked_subchecks", [])}
    return [s for s in policy["subchecks"] if s["id"] not in unmarked]


def marker_message(text, marker):
    start = text.rfind("\n", 0, marker.start()) + 1
    end = text.find("\n", marker.end())
    if end < 0:
        end = len(text)
    # An empty marker header may put its label on the next nonempty line.
    # Other output lines (including independent unmarked branches) are separate.
    if not text[marker.end():end].strip():
        following = end + 1
        while following < len(text):
            end = text.find("\n", following)
            if end < 0:
                end = len(text)
            if text[following:end].strip():
                break
            following = end + 1
    return text[start:end]


def changed_paths(root, event, candidate, base, supplied):
    if event in ("schedule", "workflow_dispatch"):
        return [], None, "broad: independent scheduled/manual selection"
    if not commit(base):
        return [], None, "broad: missing or invalid event base"
    if supplied:
        try:
            raw = Path(supplied).read_bytes()
            value = read_json(supplied)
        except (OSError, InvalidEvidence) as error:
            return [], None, "broad: unreadable/malformed change input: " + str(error)
        if (not isinstance(value, dict) or value.get("candidate") != candidate
                or value.get("base") != base or value.get("trusted") is not True):
            return [], digest(raw), "broad: untrusted or mismatched change metadata"
        changes = value.get("changes")
        if not isinstance(changes, list):
            return [], digest(raw), "broad: invalid change list"
        for change in changes:
            if (not isinstance(change, dict) or change.get("status") not in ("A", "M", "D", "R", "C", "T")
                    or not safe_path(change.get("path"))
                    or (change.get("status") in ("R", "C") and not safe_path(change.get("old_path")))):
                return [], digest(raw), "broad: invalid path/change kind"
        return changes, digest(raw), "event-bound explicit changes"
    command = ["git", "diff", "--name-status", "-z", "--find-renames", base, candidate, "--"]
    run = subprocess.run(command, cwd=root, capture_output=True)
    if run.returncode:
        return [], digest(run.stdout + run.stderr), "broad: comparison objects unavailable"
    fields = run.stdout.decode("utf-8", "replace").rstrip("\0").split("\0") if run.stdout else []
    changes = []
    while fields:
        status = fields.pop(0)[0]
        require(bool(fields), "truncated git diff")
        path = fields.pop(0)
        change = {"status": status, "path": path}
        if status in ("R", "C"):
            require(bool(fields), "truncated rename")
            change = {"status": status, "old_path": path, "path": fields.pop(0)}
        changes.append(change)
    return changes, digest(run.stdout), "event-derived git comparison"


def select(root, policy, policy_hash, event, candidate, base=None, supplied=None, size_table=False):
    require(commit(candidate), "candidate must be a nonzero full commit SHA")
    require(event in EVENTS, "unsupported event")
    changes, impact_hash, reason = changed_paths(root, event, candidate, base, supplied)
    paths = [p for c in changes for p in (c.get("old_path"), c["path"]) if p]
    broad = reason.startswith("broad:")
    sensitive = tuple("crates/" + name + "/" for name in policy["impact"]["sensitive_crates"])
    producer = broad or any(p.startswith(("tools/", "fixtures/", "scripts/", ".github/"))
                           or p in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "mise.toml", "mise.lock")
                           or p.endswith(("/Cargo.toml", "/Cargo.lock", "/rust-toolchain.toml"))
                           or p.startswith(sensitive) for p in paths)
    isolation = broad or producer
    known = ("docs/", "crates/", "examples/", "hooks/", "LICENSE", "NOTICE", "README.md", "CHANGELOG.md", ".gitignore")
    unknown_crate = any(p.startswith("crates/") and p.split("/")[1] not in policy["impact"]["known_crates"]
                        for p in paths)
    if unknown_crate or any(not p.startswith(known) and not producer for p in paths):
        producer = isolation = True
        reason += "; broad: unknown/shared impact"
    if event == "schedule":
        profiles = list(policy["profiles"])
    else:
        profiles = ["quality", "functional", "gnu", "musl"]
        if producer:
            profiles.append("producer")
        if isolation:
            profiles.append("qualification")
    chosen = []
    for obligation in policy["obligations"]:
        if obligation["group"] not in profiles:
            continue
        if "events" in obligation and event not in obligation["events"]:
            continue
        if obligation.get("optional") == "size_table" and not size_table:
            continue
        chosen.append({**obligation, "required": True,
                       "reason": "preserved base gate" if obligation["group"] in profiles[:4]
                       else "declared impact or broad fallback"})
    return {"version": 1, "candidate": candidate, "base": base if commit(base) else None,
            "impact_input_sha256": impact_hash, "impact_reason": reason, "changes": changes,
            "policy_sha256": policy_hash, "event": event, "profiles": profiles,
            "options": {"size_table": size_table},
            "prerequisites": {p: policy["profiles"][p] for p in profiles},
            "obligations": chosen, "exclusions": [p for p in policy["profiles"] if p not in profiles]}


def validate_status(value):
    require(isinstance(value, dict) and value.get("status") in STATUSES, "invalid check status")
    code = value.get("child_exit")
    require(type(code) is int and 0 <= code <= 255, "invalid child_exit")
    require(value["status"] != "pass" or code == 0, "pass with nonzero child_exit")
    require(value["status"] not in ("assertion_failure", "infrastructure_failure") or code != 0,
            "failure with zero child_exit")


def validate_logs(logs, path):
    require(isinstance(logs, list) and bool(logs), "missing raw log references")
    for log in logs:
        require(safe_path(log), "unsafe log path")
        file = path.parent / log
        require(file.resolve().is_relative_to(path.parent.resolve()), "log escapes result bundle")
        require(file.is_file(), "missing log: " + log)


_BOOLEAN_CAPABILITIES = frozenset(("namespaces_available", "go_repo_directory", "go_home_directory",
    "rust_producer_executable", "rust_sysroot_rustc_executable", "index_binary_executable",
    "scip_go_reachable", "go_version_succeeds", "go_version_parseable", "usr_include_directory",
    "system_cc_resolvable", "usr_lib_os_release_file", "crt1_parent_one_level_below_usr_lib"))


def predicate_state(predicate, capabilities):
    """True/False for observed conditions; None when required facts are absent."""
    require(isinstance(predicate, dict) and bool(predicate), "missing typed execution predicate")
    require(isinstance(capabilities, dict), "invalid observed capabilities")
    missing, mismatch = False, False
    for key, expected in predicate.items():
        if key in _BOOLEAN_CAPABILITIES:
            require(type(expected) is bool, "invalid boolean capability requirement")
            actual = capabilities.get(key)
            require(key not in capabilities or type(actual) is bool, "invalid observed boolean capability")
        elif key == "tmp_free_bytes_min":
            require(type(expected) is int and expected >= 0, "invalid /tmp minimum")
            actual = capabilities.get("tmp_free_bytes")
            require(actual is None or (type(actual) is int and actual >= 0), "invalid /tmp observation")
            if actual is None:
                missing = True
            else:
                mismatch |= actual < expected
            continue
        else:
            allowed = {"os": ("linux", "darwin"), "isolation_tier": ("none", "namespaces")}
            require(key in allowed and expected in allowed[key], "unknown execution predicate")
            actual = capabilities.get(key)
            require(key not in capabilities or actual in allowed[key], "invalid observed capability value")
        missing |= key not in capabilities
        mismatch |= key in capabilities and actual != expected
    return False if mismatch else None if missing else True


def assertion_identity(configuration, row):
    """Compare the same assertion closure while retaining the full run identity."""
    require(configuration.get("native_execution") is True, "assertion requires native execution")
    for field in ("source_inputs", "resolved_features", "dependency_graph", "package_roots", "tool_objects"):
        require(isinstance(configuration.get(field), dict) and configuration[field], "missing assertion " + field)
    require(row["source"].rsplit(":", 1)[0] in configuration["source_inputs"], "missing assertion source identity")
    root = configuration["package_roots"].get(row["package"])
    require(isinstance(root, str), "missing assertion package root")
    closure, pending = set(), [root]
    while pending:
        package = pending.pop()
        if package in closure:
            continue
        closure.add(package)
        require(package in configuration["dependency_graph"] and package in configuration["resolved_features"],
                "incomplete assertion dependency closure")
        edges = configuration["dependency_graph"][package]
        features = configuration["resolved_features"][package]
        require(isinstance(edges, list) and all(isinstance(e, str) for e in edges), "invalid dependency closure")
        require(isinstance(features, list) and all(isinstance(f, str) for f in features), "invalid resolved features")
        pending.extend(edges)
    tools = {}
    for tool in row["required_tools"]:
        value = configuration["tool_objects"].get(tool)
        require(isinstance(value, str) and bool(re.fullmatch(r"[0-9a-f]{64}", value)), "missing assertion tool: " + tool)
        tools[tool] = value
    fixed = {key: configuration[key] for key in ("source", "manifests", "locks", "source_inputs", "target",
        "runtime", "compiler", "native_toolchain", "profile", "flags", "environment", "native_execution")}
    return identity({**fixed, "branch": row["id"], "parent_case": row["parent"], "package": row["package"],
        "binary": row["binary"], "tools": tools,
        "features": {p: sorted(configuration["resolved_features"][p]) for p in sorted(closure)},
        "dependencies": {p: sorted(configuration["dependency_graph"][p]) for p in sorted(closure)}})


def validate_result(value, selection, policy, policy_hash, path):
    require(isinstance(value, dict) and value.get("version") == 1, "unsupported result version")
    require(value.get("candidate") == selection["candidate"], "candidate identity mismatch")
    require(value.get("policy_sha256") == policy_hash, "policy identity mismatch")
    require(value.get("group") in selection["profiles"] or value.get("group") == "qualification",
            "unexpected result group")
    validate_status(value)
    for field in ("tool_identities", "capabilities", "timings", "resources"):
        require(isinstance(value.get(field), dict), "missing/invalid " + field)
    require(bool(value["tool_identities"]), "missing effective tool identities")
    require(isinstance(value.get("artifacts"), list), "missing artifact list")
    validate_logs(value.get("logs"), path)
    log_cases = value.get("log_cases", {})
    require(isinstance(log_cases, dict), "invalid per-log case contexts")
    declared_logs = set(value["logs"])
    for check in value.get("checks", []):
        if isinstance(check, dict) and isinstance(check.get("logs"), list):
            declared_logs.update(check["logs"])
    for log, context in log_cases.items():
        require(log in declared_logs and isinstance(context, dict), "unbound log case context")
        for field in ("workspace", "package", "binary", "case", "source"):
            require(isinstance(context.get(field), str) and bool(context[field]), "incomplete log case identity")
    configuration = value.get("configuration")
    require(isinstance(configuration, dict), "missing configuration identity")
    require(configuration.get("source") == selection["candidate"], "configuration source mismatch")
    for field in ("manifests", "locks", "environment"):
        require(isinstance(configuration.get(field), dict), "missing configuration " + field)
    for field in ("packages", "features", "flags"):
        require(isinstance(configuration.get(field), list)
                and all(isinstance(x, str) for x in configuration[field]), "invalid configuration " + field)
    for field in ("target", "runtime", "compiler", "native_toolchain", "profile"):
        require(isinstance(configuration.get(field), str) and bool(configuration[field]),
                "missing configuration " + field)
    require(value.get("configuration_id") == identity(configuration), "configuration identity mismatch")
    require(value["tool_identities"].get("compiler") == configuration["compiler"], "compiler identity mismatch")
    require(isinstance(value.get("checks"), list) and bool(value["checks"]), "missing checks")
    expected = {o["id"]: o for o in selection["obligations"]}
    unselected_optionals = {o["id"]: o for o in policy["obligations"]
        if o.get("optional") and o["group"] in selection["profiles"] and o["id"] not in expected}
    expected.update(unselected_optionals)
    subchecks = {s["id"]: s for s in policy["subchecks"]}
    expected.update(subchecks)
    seen = set()
    for check in value["checks"]:
        validate_status(check)
        name = check.get("id")
        require(name in expected and name not in seen, "unknown/duplicate check")
        require(expected[name]["group"] == value["group"], "check group mismatch")
        if name in unselected_optionals:
            require(check["status"] == "not_applicable" and check["child_exit"] == 0
                    and check.get("executed") is False and check.get("counterpart_ids") == []
                    and check.get("predicate") == {"optional": expected[name]["optional"], "selected": False}
                    and isinstance(check.get("reason"), str) and bool(check["reason"]),
                    "unselected optional check requires an explicit not-applicable outcome")
        if isinstance(expected[name]["predicate"], dict) and name not in unselected_optionals:
            require(check.get("predicate") == expected[name]["predicate"], "execution predicate mismatch")
            state = predicate_state(expected[name]["predicate"], check.get("capabilities", value["capabilities"]))
            if check["status"] == "pass":
                require(check.get("executed") is True and state is True, "execution capabilities do not satisfy predicate")
            if check["status"] == "not_applicable":
                require(state is False, "not-applicable predicate was not observed false")
        if name in subchecks:
            require(check.get("predicate") == subchecks[name]["predicate"]
                    and check.get("parent_case") == subchecks[name]["parent"]
                    and (check.get("executed") is True or check["status"] == "not_applicable"),
                    "counterpart lacks exact branch execution identity")
            state = predicate_state(subchecks[name]["predicate"], check.get("capabilities", value["capabilities"]))
            if check["status"] == "pass":
                require(state is True, "counterpart execution capabilities do not satisfy predicate")
            if check["status"] == "not_applicable":
                require(state is False, "not-applicable predicate was not observed false")
            assertion_identity(configuration, subchecks[name])
        if check["status"] == "not_applicable" and name not in unselected_optionals:
            require(bool(expected[name]["counterpart_ids"])
                    and check.get("counterpart_ids") == expected[name]["counterpart_ids"]
                    and check.get("predicate") == expected[name]["predicate"]
                    and isinstance(check.get("reason"), str) and bool(check["reason"])
                    and check.get("executed") is False, "unjustified not-applicable result")
        seen.add(name)
        validate_logs(check.get("logs"), path)


def aggregate(selection, results, policy, policy_hash):
    require(isinstance(selection, dict) and selection.get("version") == 1, "invalid selection version")
    require(commit(selection.get("candidate")), "invalid selection candidate")
    require(selection.get("event") in EVENTS, "invalid selection event")
    require(selection.get("policy_sha256") == policy_hash, "selection policy identity mismatch")
    profiles = selection.get("profiles")
    require(isinstance(profiles, list) and {"quality", "functional", "gnu", "musl"} <= set(profiles)
            and all(p in policy["profiles"] for p in profiles), "missing/unknown selected profile")
    if selection.get("event") == "schedule":
        require(set(profiles) == set(policy["profiles"]), "schedule missing qualification profiles")
    selected = selection.get("obligations")
    require(isinstance(selected, list) and bool(selected), "missing required obligations")
    known = {o["id"]: o for o in policy["obligations"] if o["group"] in profiles
             and ("events" not in o or selection["event"] in o["events"])}
    names = [o.get("id") for o in selected if isinstance(o, dict)]
    require(len(names) == len(selected) and len(set(names)) == len(names), "duplicate/invalid selected obligation")
    require({o["id"] for o in known.values() if not o.get("optional")} <= set(names),
            "selection dropped required obligations")
    for obligation in selected:
        require(obligation.get("id") in known and obligation.get("required") is True,
                "invalid required obligation")
        for field in ("group", "predicate", "counterpart_ids"):
            require(obligation.get(field) == known[obligation["id"]][field], "selection contract drift: " + field)
    options = selection.get("options", {"size_table": False})
    require(isinstance(options, dict) and type(options.get("size_table")) is bool, "invalid optional selection")
    for obligation in known.values():
        if obligation.get("optional"):
            require((obligation["id"] in names) == options.get(obligation["optional"], False),
                    "optional selection identity mismatch")
    checks = {}
    subchecks = {s["id"]: s for s in policy["subchecks"]}
    discrepancies = []
    group_failures = []
    cancellations = []
    for path in sorted(Path(results).rglob("result.json")):
        try:
            result = read_json(path)
            validate_result(result, selection, policy, policy_hash, path)
        except (InvalidEvidence, OSError, UnicodeError, ValueError) as error:
            discrepancies.append(f"{path}: {error}")
            continue
        if result["status"] not in ("pass", "not_applicable"):
            if result["status"] in ("assertion_failure", "infrastructure_failure"):
                group_failures.append(result)
            elif result["status"] == "cancelled":
                cancellations.append(result)
            else:
                discrepancies.append(f"{path}: group {result['status']}, child_exit {result['child_exit']}")
        for check in result["checks"]:
            if check["status"] in ("assertion_failure", "infrastructure_failure"):
                group_failures.append(check)
            if check["status"] == "cancelled":
                cancellations.append(check)
            attempt = dict(check)
            attempt["configuration_id"] = result["configuration_id"]
            attempt["missing_subchecks"] = []
            attempt["raw_logs"] = sorted(set(check["logs"] + result["logs"]))
            for log in attempt["raw_logs"]:
                text = (path.parent / log).read_text(encoding="utf-8", errors="replace")
                markers = list(re.finditer(r"NOT RUN:|SKIPPED\(mandatory\)", text))
                for marker in markers:
                    fragment = marker_message(text, marker)
                    mapped = [s["id"] for s in marker_rows(policy)
                              if re.search(s["marker_pattern"], fragment)]
                    if len(mapped) > 1:
                        context = result.get("log_cases", {}).get(log, {})
                        mapped = [s["id"] for s in marker_rows(policy) if s["id"] in mapped
                                  and context.get("case") == s["parent"]
                                  and context.get("source") == s["source"].rsplit(":", 1)[0]
                                  and context.get("package") == s["source"].split("/")[1]
                                  and context.get("binary") == Path(s["source"].rsplit(":", 1)[0]).stem
                                  and context.get("workspace") == "root"]
                    if len(mapped) != 1:
                        discrepancies.append(f"{path}: unclassified/ambiguous marker: {fragment[:200]}")
                    else:
                        attempt["missing_subchecks"].extend(mapped)
                for subcheck in policy.get("unmarked_subchecks", []):
                    if re.search(subcheck["marker_pattern"], text):
                        attempt["missing_subchecks"].append(subcheck["id"])
            attempt["missing_subchecks"] = sorted(set(attempt["missing_subchecks"]))
            attempt["assertion_identities"] = {}
            if check["id"] not in subchecks:
                attempt["assertion_identities"][check["id"]] = result["configuration_id"]
            for name in set(attempt["missing_subchecks"] + ([check["id"]] if check["id"] in subchecks else [])):
                try:
                    attempt["assertion_identities"][name] = assertion_identity(result["configuration"], subchecks[name])
                except InvalidEvidence as error:
                    discrepancies.append(f"{path}: {name}: {error}")
            checks.setdefault(check["id"], []).append(attempt)
    for name, attempts in checks.items():
        for attempt in attempts:
            if attempt["status"] == "not_applicable":
                attempt["counterparts_satisfied"] = True
                for counterpart in attempt["counterpart_ids"]:
                    if not any(c["status"] == "pass" and not c["missing_subchecks"]
                               and c["assertion_identities"].get(counterpart) is not None
                               and c["assertion_identities"].get(counterpart) == attempt["assertion_identities"].get(counterpart)
                               for c in checks.get(counterpart, [])):
                        attempt["counterparts_satisfied"] = False
                        discrepancies.append(f"{name}: missing successful not-applicable counterpart {counterpart}")
    obligations = []
    failure_exit = 0
    failure_status = None
    for obligation in selection["obligations"]:
        if not obligation["required"]:
            continue
        attempts = checks.get(obligation["id"], [])
        for attempt in attempts:
            attempt["unresolved_subchecks"] = [name for name in attempt["missing_subchecks"]
                if not any(c["status"] == "pass" and not c["missing_subchecks"]
                           and c["assertion_identities"].get(name) is not None
                           and c["assertion_identities"].get(name) == attempt["assertion_identities"].get(name)
                           for c in checks.get(name, []))]
        failures = [c for c in attempts if c["status"] in ("assertion_failure", "infrastructure_failure")]
        status = (failures[0]["status"] if failures else
                  "cancelled" if any(c["status"] == "cancelled" for c in attempts) else
                  "pass" if attempts and all((c["status"] == "pass" or
                                               (c["status"] == "not_applicable" and c.get("counterparts_satisfied")))
                                               and not c["unresolved_subchecks"]
                                               for c in attempts) else "incomplete")
        if failures and not failure_exit:
            failure_exit = 1 if failures[0]["child_exit"] == 3 else failures[0]["child_exit"]
            failure_status = status
        obligations.append({"id": obligation["id"], "status": status, "attempts": attempts})
    code = 0 if not discrepancies and all(o["status"] == "pass" for o in obligations) else 3
    code = failure_exit or code
    if group_failures:
        code = 1 if group_failures[0]["child_exit"] == 3 else group_failures[0]["child_exit"]
        failure_status = group_failures[0]["status"]
    elif cancellations:
        code = cancellations[0]["child_exit"] or 130
        failure_status = "cancelled"
    optional_names = {o["id"] for o in policy["obligations"] if o.get("optional") and o["id"] not in names}
    optional_outcomes = [{"id": name, **attempt} for name in sorted(optional_names)
                         for attempt in checks.get(name, [])]
    return {"version": 1, "candidate": selection["candidate"],
            "status": failure_status or ("pass" if code == 0 else "incomplete"), "obligations": obligations,
            "optional_outcomes": optional_outcomes, "discrepancies": discrepancies}, code


def source_inputs(root):
    root = Path(root)
    inputs = [root / p for p in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")]
    for directory in (".cargo", "crates", "tools/kaban-rust-scip", "fixtures", "examples"):
        inputs.extend(p for p in (root / directory).rglob("*") if p.is_file()
                      and "target" not in p.relative_to(root).parts)
    return {str(p.relative_to(root)): digest(p.read_bytes()) for p in sorted(inputs) if p.is_file()}


def cargo_arguments(command, tools):
    require(isinstance(tools, dict) and bool(tools), "invalid Cargo tool identities")
    require(isinstance(command, list) and all(isinstance(x, str) for x in command), "invalid command arguments")
    require(len(command) > 1, "missing Cargo command")
    # Captures retain exact argv separately; this contract compares Cargo's actual
    # arguments, allowing the shared slot launcher and an absolute admitted Cargo.
    if Path(command[0]).name == "cargo-slot.sh":
        command = command[1:]
    require(len(command) > 1, "missing Cargo executable")
    launch = tools.get("cargo_launch", {})
    require(isinstance(launch, dict) and launch.get("command") == command[0], "unadmitted Cargo executable")
    for field in ("path", "resolved_path", "effective_path"):
        require(isinstance(launch.get(field), str) and Path(launch[field]).is_absolute(),
                "missing Cargo executable " + field)
    require(command[0] == "cargo" or command[0] == launch["path"], "Cargo executable override mismatch")
    require(launch.get("regular") is True and type(launch.get("mode")) is int
            and launch["mode"] & 0o111 and not launch["mode"] & 0o022
            and type(launch.get("euid")) is int and launch.get("uid") in (0, launch["euid"]),
            "untrusted Cargo executable ownership/mode")
    for field in ("sha256", "effective_sha256"):
        require(isinstance(launch.get(field), str) and re.fullmatch(r"[0-9a-f]{64}", launch[field]),
                "missing Cargo executable hash")
    effective = tools.get("cargo_binary", {})
    require(isinstance(effective, dict) and effective.get("path") == launch["effective_path"]
            and effective.get("sha256") == launch["effective_sha256"], "effective Cargo executable mismatch")
    return command[1:]


_RUST_RAW = re.compile(r'(?:b|c)?r(#{0,255})"')
_RUST_LITERAL = re.compile(r'''(?:b|c)?"(?:\\.|[^"\\])*"|(?:b)?'(?:\\(?:u\{[0-9A-Fa-f]+\}|.)|[^'\\])' ''', re.S | re.X)
_RUST_NAME = re.compile(r"(?:r#)?[A-Za-z_][A-Za-z_0-9]*")


def rust_tokens(text):
    """Bounded source inventory lexer, preserving offsets and literal tokens."""
    result = []
    at = 0
    while at < len(text):
        if text[at].isspace():
            at += 1
            continue
        if text.startswith("//", at):
            end = text.find("\n", at)
            at = len(text) if end < 0 else end
            continue
        if text.startswith("/*", at):
            depth = 1
            at += 2
            while at < len(text) and depth:
                if text.startswith("/*", at):
                    depth += 1
                    at += 2
                elif text.startswith("*/", at):
                    depth -= 1
                    at += 2
                else:
                    at += 1
            require(depth == 0, "unterminated Rust source comment")
            continue
        raw = _RUST_RAW.match(text, at)
        if raw:
            end = text.find('"' + raw[1], raw.end())
            require(end >= 0, "unterminated Rust raw literal")
            end += 1 + len(raw[1])
        else:
            match = _RUST_LITERAL.match(text, at) or _RUST_NAME.match(text, at)
            end = match.end() if match else at + 1
        result.append((text[at:end], at))
        at = end
    return result


class RustCatalog:
    """Resolve complete module/test names from an admitted Cargo target root."""

    def __init__(self, root):
        self.root = Path(root).resolve()
        self.cache = {}

    def scan(self, path, prefix=(), *, target_root=False, inherited=(), generator=None, trail=()):
        path = Path(path).resolve()
        require(path.is_relative_to(self.root), "Rust module escapes source root")
        require(path not in trail, "cyclic Rust module declaration")
        key = (path, prefix, target_root, tuple(inherited), generator)
        if key in self.cache:
            return self.cache[key]
        text = path.read_text(encoding="utf-8")
        tokens = rust_tokens(text)
        pairs, stack = {}, []
        closing = {"}": "{", "]": "[", ")": "("}
        for i, (value, _) in enumerate(tokens):
            if value in ("{", "[", "("):
                stack.append((value, i))
            elif value in closing:
                require(bool(stack) and stack[-1][0] == closing[value], "unbalanced Rust delimiters")
                _, opened = stack.pop()
                pairs[opened] = i
        require(not stack, "unclosed Rust delimiters")
        functions = {}

        def merge(found):
            for name, fact in found.items():
                if name in functions and functions[name] != fact:
                    require(not functions[name]["is_test"] and not fact["is_test"],
                            "ambiguous Rust test identity: " + name)
                    variants = functions[name].get("conditional_variants", [])
                    fact = {**fact, "conditional_variants": [*variants, functions[name]["attributes"]]}
                functions[name] = fact

        def walk(start, stop, module, directory, inherited_attrs, generated):
            at, attrs = start, []
            while at < stop:
                value, offset = tokens[at]
                if value == "#" and at + 1 < stop:
                    inner = tokens[at + 1][0] == "!"
                    opening = at + 2 if inner else at + 1
                    if opening < stop and tokens[opening][0] == "[" and opening in pairs:
                        end = pairs[opening]
                        attribute = "".join(t[0] for t in tokens[opening + 1:end])
                        if inner:
                            inherited_attrs = (*inherited_attrs, attribute)
                        else:
                            attrs.append(attribute)
                        at = end + 1
                        continue
                if value == "mod" and at + 2 < stop:
                    name, next_at = tokens[at + 1][0].removeprefix("r#"), at + 2
                    attributes = (*inherited_attrs, *attrs)
                    if tokens[next_at][0] == "{":
                        walk(next_at + 1, pairs[next_at], (*module, name), directory / name, attributes, generated)
                        at, attrs = pairs[next_at] + 1, []
                        continue
                    if tokens[next_at][0] == ";":
                        override = re.search(r'path="([^"\\]+)"', " ".join(attrs))
                        candidates = ([path.parent / override[1]] if override else
                                      [directory / (name + ".rs"), directory / name / "mod.rs"])
                        existing = [p for p in candidates if p.is_file()]
                        require(len(existing) <= 1, "ambiguous Rust module source: " + name)
                        if existing:
                            merge(self.scan(existing[0], (*module, name), inherited=attributes,
                                            generator=generated, trail=(*trail, path)))
                        at, attrs = next_at + 1, []
                        continue
                if value == "fn" and at + 1 < stop:
                    name = tokens[at + 1][0].removeprefix("r#")
                    end = at + 2
                    while end < stop and tokens[end][0] not in ("{", ";"):
                        end = pairs[end] + 1 if end in pairs else end + 1
                    body_end = pairs[end] if end < stop and tokens[end][0] == "{" else end
                    attributes = [*inherited_attrs, *attrs]
                    is_test = any(a == "test" or a == "tokio::test" or a.startswith("tokio::test(") for a in attrs)
                    case = "::".join((*module, name))
                    merge({case: {"case": case, "function": name, "module_path": list(module),
                        "source": str(path.relative_to(self.root)), "line": text.count("\n", 0, offset) + 1,
                        "source_sha256": digest(text.encode("utf-8")), "attributes": attributes,
                        "generator": generated, "is_test": is_test,
                        "body": text[offset:tokens[body_end][1] + 1] if body_end < stop else ""}})
                    at, attrs = body_end + 1, []
                    continue
                if value == "!" and at + 1 < stop and tokens[at + 1][0] in ("{", "(", "["):
                    opening = at + 1
                    if at > start and tokens[at - 1][0] == "proptest":
                        walk(opening + 1, pairs[opening], module, directory, inherited_attrs, "proptest")
                    at, attrs = pairs[opening] + 1, []
                    continue
                if value in ("{", "[", "(") and at in pairs:
                    at = pairs[at] + 1
                    continue
                if value == ";":
                    attrs = []
                at += 1

        directory = path.parent if target_root or path.name == "mod.rs" else path.parent / path.stem
        walk(0, len(tokens), prefix, directory, inherited, generator)
        self.cache[key] = functions
        return functions

    def target(self, metadata, workspace, package, kind, name):
        packages = [p for p in metadata.get("packages", []) if p.get("name") == package]
        require(len(packages) == 1, "missing/ambiguous metadata package: " + package)
        targets = [t for t in packages[0].get("targets", []) if t.get("name") == name and kind in t.get("kind", [])]
        require(len(targets) == 1, "missing/ambiguous metadata target: " + name)
        captured_root = Path(metadata["workspace_root"])
        remote = Path(targets[0]["src_path"])
        require(remote.is_relative_to(captured_root), "target source escapes captured workspace")
        local_root = self.root / "tools/kaban-rust-scip" if workspace == "producer" else self.root
        return self.scan(local_root / remote.relative_to(captured_root), target_root=True)


def literal_calls(fact, callee):
    tokens = [value for value, _ in rust_tokens(fact["body"])]
    calls = []
    local_binding = False
    for at, value in enumerate(tokens):
        if value == "let":
            end = at + 1
            while end < len(tokens) and tokens[end] not in ("=", ";"):
                local_binding |= tokens[end] == callee
                end += 1
    for at, value in enumerate(tokens):
        if value != callee or (at and tokens[at - 1] in ("fn", ".")):
            continue
        if at + 1 < len(tokens) and tokens[at + 1] != "(" and (local_binding or tokens[at + 1] == ":"):
            continue
        require(at + 1 < len(tokens) and tokens[at + 1] == "(", "unsupported dynamic helper reference")
        args, at_arg = [], at + 2
        while at_arg < len(tokens) and tokens[at_arg] != ")":
            token = tokens[at_arg]
            if token == ",":
                at_arg += 1
                continue
            if token in ("true", "false"):
                args.append(token == "true")
            else:
                require(token.startswith('"'), "nonliteral dynamic helper argument")
                args.append(json.loads(token))
            at_arg += 1
        require(at_arg < len(tokens), "unclosed dynamic helper call")
        calls.append(args)
    return calls


def reconcile_branches(root, catalog, policy, discrepancies):
    for emitter in policy["emitters"]:
        try:
            functions = catalog.scan(root / emitter["source"], target_root=True)
            parents = [fact for name, fact in functions.items()
                       if name == emitter["parent"] or name.endswith("::" + emitter["parent"])]
            require(len(parents) == 1, "emitter parent missing/ambiguous")
            messages = re.findall(r'(?:eprintln|println)!\(\s*"((?:[^"\\]|\\.)*)"', parents[0]["body"])
            require(emitter["text"] in messages, "emitter parent/message binding changed")
        except (InvalidEvidence, OSError) as error:
            discrepancies.append(f"{emitter['source']}: emitter parent: {error}")
    for descriptor in policy.get("dynamic_helpers", []):
        try:
            functions = catalog.scan(root / descriptor["source"], target_root=True)
            helper = functions.get(descriptor["helper"])
            require(helper is not None and not helper["is_test"], "missing dynamic helper source")
            require(identity([t for t, _ in rust_tokens(helper["body"])]) == descriptor["body_sha256"],
                    "dynamic helper body changed")
            calls = [{"parent": name, "arguments": args} for name, fact in functions.items() if fact["is_test"]
                     for args in literal_calls(fact, descriptor["helper"])]
            require(sorted(calls, key=identity) == sorted(descriptor["calls"], key=identity),
                    "dynamic helper callers changed")
            if "internal_probes" in descriptor:
                probes = descriptor["internal_probes"]
                require(literal_calls(helper, probes["callee"]) == probes["arguments"], "dynamic helper probes changed")
        except (InvalidEvidence, OSError, ValueError) as error:
            discrepancies.append(f"dynamic helper {descriptor['source']}/{descriptor['helper']}: {error}")
    for row in policy["subchecks"]:
        try:
            source = row["source"].rsplit(":", 1)[0]
            functions = catalog.scan(root / source, target_root=True)
            require(row["parent"] in functions and functions[row["parent"]]["is_test"], "subcheck parent is not a source test")
        except (InvalidEvidence, OSError) as error:
            discrepancies.append(f"{row['id']}: {error}")
    for row in policy.get("unmarked_subchecks", []):
        source = row["source"].rsplit(":", 1)[0]
        try:
            text = (root / source).read_text(encoding="utf-8")
            require(sum(token == row["literal"] for token, _ in rust_tokens(text)) == 1,
                    "unmarked source message changed/missing/duplicated")
            fact = catalog.scan(root / source, target_root=True).get(row["parent"], {})
            require(row["literal"] in fact.get("body", ""), "unmarked source parent changed")
        except (InvalidEvidence, OSError) as error:
            discrepancies.append(f"unmarked {row['id']}: {error}")


def classify_case(fact, functions, workspace, package, binary, policy):
    """Keep independent dimensions and source-bound conditional branches."""
    closure, pending = {}, [fact["case"]]
    while pending:
        name = pending.pop()
        if name in closure:
            continue
        current = functions[name]
        closure[name] = current
        tokens = [value for value, _ in rust_tokens(current["body"])]
        for at in range(len(tokens) - 1):
            if tokens[at + 1] != "(" or (at and tokens[at - 1] in ("fn", ".")):
                continue
            # Resolve lexical free functions only. An associated method or an
            # unrelated imported name cannot borrow a same-named local helper.
            if at >= 2 and tokens[at - 2:at] == [":", ":"]:
                continue
            called = "::".join((*current["module_path"], tokens[at]))
            if called in functions and not functions[called]["is_test"]:
                pending.append(called)
    body = "\n".join(f["body"] for f in closure.values())
    tokens = " ".join(value for value, _ in rust_tokens(body))
    effects = []
    def effect(name, observed):
        if observed:
            effects.append(name)
    effect("product-cli", bool(re.search(r'CARGO_BIN_EXE_(?:kaban|kaban-index|kaban-rust-scip)', body)))
    effect("mcp-transport", bool(re.search(r'(?:Server|McpClient)\s*::\s*start\w*\s*\(', body)))
    effect("sqlite", bool(re.search(r'(?:SqliteStore|Connection)\s*::\s*(?:open\w*|new)\s*\(', body)))
    effect("api-composition", bool(re.search(r'KabanApi\s*::\s*(?:new|with_config)\s*\(', body)))
    effect("process", bool(re.search(r'(?:Command\s*::\s*new|run_indexer)\s*\(', body)))
    effect("filesystem", bool(re.search(r'(?:fs|std\s*::\s*fs)\s*::\s*(?:write|read\w*|create\w*|symlink|rename|remove\w*)\s*\(', body)))
    techniques = ["property" if fact["generator"] == "proptest" else "example"]
    if any(a.startswith("tokio::test") for a in fact["attributes"]):
        techniques.append("async")
    if re.search(r'assert_\w*snapshot\s*!', body):
        techniques.append("snapshot")
    if "Instant : : now (" in tokens and any(x in tokens for x in ("elapsed (", "Duration : :")):
        techniques.append("bounded-performance")
    source = fact["source"]
    scope = ("end-to-end" if {"product-cli", "mcp-transport"}.intersection(effects) else
             "integration" if "process" in effects or {"sqlite", "api-composition"} <= set(effects) else
             "component" if effects else policy["scope_rules"][package])
    basis = ("resolved test/free-function source effects" if effects else
             "component ownership rule; no resolved external effect in bounded scan")
    for rule in policy.get("case_scope_rules", []):
        if source != rule["source"] or ("cases" in rule and fact["case"] not in rule["cases"]):
            continue
        scope, basis = rule["scope"], rule["reason"]
        techniques.extend(rule.get("techniques", []))
    branches = [{"id": row["id"], "predicate": row["predicate"], "required_tools": row["required_tools"]}
                for row in policy["subchecks"] if row["workspace"] == workspace and row["package"] == package
                and row["binary"] == binary and row["parent"] == fact["case"]
                and row["source"].rsplit(":", 1)[0] == source]
    return {"scope": scope, "scope_basis": basis, "boundary_effects": effects,
        "classification_sources": sorted({f["source"] for f in closure.values()}),
        "purpose": fact["function"].replace("_", " "), "technique": sorted(set(techniques)),
        "capabilities": {"source_attributes": [a for a in fact["attributes"] if a.startswith("cfg")],
                         "branches": branches}}


def inventory(root, captures, candidate, policy, policy_hash):
    require(commit(candidate), "inventory requires full candidate SHA")
    root = Path(root)
    bundle = read_json(captures) if captures else {}
    require(isinstance(bundle, dict), "invalid discovery bundle")
    discrepancies = []
    cases = []
    catalog = RustCatalog(root)
    if bundle and (bundle.get("version") != 1 or bundle.get("candidate") != candidate
                   or bundle.get("source_inputs") != source_inputs(root)):
        discrepancies.append("discovery bundle version/candidate/source inputs mismatch")
    registered = Counter((e["source"], e["text_sha256"]) for e in policy["emitters"])
    observed = Counter()
    occurrences = Counter((e["source"], e["sha256"]) for e in policy["marker_occurrences"])
    found_occurrences = Counter()
    for source in sorted((root / "crates").rglob("*.rs")):
        text = source.read_text()
        for line, content in enumerate(text.splitlines(), 1):
            if "NOT RUN" in content or "SKIPPED(mandatory)" in content:
                key = (str(source.relative_to(root)), digest(content.encode()))
                found_occurrences[key] += 1
                if found_occurrences[key] > occurrences[key]:
                    discrepancies.append(f"unclassified marker occurrence: {key[0]}:{line}")
        for match in re.finditer(r'(?:eprintln|println)!\(\s*"((?:[^"\\]|\\.)*)"', text):
            message = match.group(1)
            if "NOT RUN" not in message and "SKIPPED(mandatory)" not in message:
                continue
            key = (str(source.relative_to(root)), digest(message.encode()))
            observed[key] += 1
            if observed[key] > registered[key]:
                discrepancies.append(f"unclassified emitter: {key[0]}")
    for source, _ in sorted(registered - observed):
        discrepancies.append(f"registered emitter missing/changed: {source}")
    for source, _ in sorted(occurrences - found_occurrences):
        discrepancies.append(f"registered marker occurrence missing/changed: {source}")
    reconcile_branches(root, catalog, policy, discrepancies)
    discovery = bundle.get("discovery", {})
    for name, expected in policy["discovery"].items():
        capture = discovery.get(name)
        if capture is None:
            discrepancies.append("missing discovery capture: " + name)
            continue
        try:
            require(capture.get("candidate") == candidate, "discovery candidate mismatch")
            require(capture.get("child_exit") == 0, "discovery child failed")
            require(capture.get("configuration") == expected, "discovery configuration mismatch")
            require(bool(capture.get("tool_identities")), "missing discovery tool identities")
            arguments = cargo_arguments(capture.get("command"), capture["tool_identities"])
            require(arguments == [*expected["command"], "--locked"], "discovery command/configuration mismatch")
            suites = capture["data"]["rust-suites"]
            require(isinstance(suites, dict) and bool(suites), "empty/invalid test suites")
            count = 0
            for suite_id, suite in sorted(suites.items()):
                package = suite["package-name"]
                require(package in expected["packages"], "unclassified package: " + package)
                known = catalog.target(bundle.get("metadata", {}).get(expected["workspace"], {}),
                                       expected["workspace"], package, suite["kind"], suite["binary-name"])
                for case, details in sorted(suite["testcases"].items()):
                    count += 1
                    function = case.rsplit("::", 1)[-1]
                    if case not in known or not known[case]["is_test"]:
                        discrepancies.append(f"{name}: unclassified discovered case: {suite_id}/{case}")
                        continue
                    require(type(details.get("ignored")) is bool, "missing ignored classification")
                    cases.append({"id": f"{expected['workspace']}/{package}/{suite_id}/{case}/{expected['configuration']}",
                        "workspace": expected["workspace"], "package": package, "binary": suite_id,
                        "case": case, "configuration": expected["configuration"],
                        **classify_case(known[case], known, expected["workspace"], package, suite["binary-name"], policy),
                        "owner": package, "features": expected["features"], "target": expected["target"],
                        "profiles": ["producer" if expected["workspace"] == "producer" else "functional"],
                        "origin": "nextest", "sources": [known[case]["source"]], "source_fact":
                        {k: v for k, v in known[case].items() if k != "body"}, "ignored": details["ignored"],
                        "skip_explanation": "explicit ignored inventory; requires declared qualification selection"
                        if details["ignored"] else None})
                    if details["ignored"]:
                        rule = policy["ignored_rules"].get(function)
                        require(rule is not None, "unclassified ignored case: " + case)
                        cases[-1].update(rule)
            require(count == capture["data"]["test-count"], "discovery count mismatch")
        except (InvalidEvidence, KeyError, TypeError, OSError) as error:
            discrepancies.append(f"{name}: {error}")
    applicable_docs = set(policy["doctests"])
    for workspace in ("root", "producer"):
        metadata = bundle.get("metadata", {}).get(workspace)
        if metadata is None:
            discrepancies.append("missing Cargo metadata capture: " + workspace)
            continue
        for package in metadata.get("packages", []):
            for target in package.get("targets", []):
                if {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}.intersection(target.get("kind", [])) and target.get("doctest") is True:
                    applicable_docs.add(package["name"])
    for package in sorted(applicable_docs):
        capture = bundle.get("doctests", {}).get(package)
        if capture is None:
            discrepancies.append("missing doctest capture: " + package)
        elif (capture.get("candidate") != candidate or capture.get("child_exit") != 0
              or not capture.get("tool_identities") or "test result: ok." not in capture.get("stdout", "")):
            discrepancies.append("incompatible/failed doctest capture: " + package)
        else:
            try:
                expected = ["test", "--doc", "--locked", "-p", package.split("+")[0]]
                if package.endswith("+pdf"):
                    expected.extend(["--features", "pdf"])
                require(cargo_arguments(capture.get("command"), capture["tool_identities"]) == expected,
                        "doctest command/configuration mismatch")
                cases.append({"id": "doctest/" + package, "origin": "cargo-doctest", "package": package,
                              "scope": "documentation", "purpose": "executable doc examples",
                              "technique": "cargo rustdoc", "owner": package, "capture": capture})
            except InvalidEvidence as error:
                discrepancies.append(f"{package}: {error}")
    for directory in ("scripts/tests", "scripts/mcp"):
        for source in sorted((root / directory).glob("test*.py")):
            tree = ast.parse(source.read_text(), filename=str(source))
            for node in ast.walk(tree):
                if isinstance(node, ast.ClassDef):
                    for method in node.body:
                        if isinstance(method, (ast.FunctionDef, ast.AsyncFunctionDef)) and method.name.startswith("test"):
                            cases.append({"id": f"python/{source.relative_to(root)}/{node.name}.{method.name}",
                                "origin": "python-unittest", "scope": "component", "purpose": method.name.removeprefix("test_").replace("_", " "),
                                "technique": ["example", "unittest"], "owner": "quality", "profiles": ["quality"],
                                "source": str(source.relative_to(root)), "line": method.lineno})
    return {"version": 1, "candidate": candidate, "policy_sha256": policy_hash,
            "cases": cases, "subchecks": policy["subchecks"], "emitters": policy["emitters"],
            "feature_matrix": policy["feature_matrix"], "manual_helpers": policy["manual_helpers"],
            "prerequisites": policy["profiles"],
            "discrepancies": discrepancies}, 3 if discrepancies else 0
