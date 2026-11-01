#!/usr/bin/env python3
"""Admit selected verification tools before any build or test command."""
import argparse
import hashlib
import json
import os
import platform
from pathlib import Path
import re
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import time

import contract


class Refused(ValueError):
    pass


class ProbeFailed(Refused):
    def __init__(self, result):
        self.result = result
        super().__init__("tool probe failed: " + " ".join(result["command"]))


def require(condition, message):
    if not condition:
        raise Refused(message)


def validate_policy(policy, selected):
    require(isinstance(policy, dict) and policy.get("version") == 1, "unsupported tool policy version/object")
    require(isinstance(policy.get("profiles"), dict) and selected in policy["profiles"], "unknown preflight profile")
    require(isinstance(policy.get("tools"), dict), "invalid tool descriptors")
    profile = policy["profiles"][selected]
    require(isinstance(profile, dict) and isinstance(profile.get("tools"), list) and bool(profile["tools"])
            and all(isinstance(name, str) and re.fullmatch(r"[a-z][a-z0-9_-]*", name)
                    and name in policy["tools"] for name in profile["tools"])
            and len(profile["tools"]) == len(set(profile["tools"])), "invalid selected tool list")
    for key in ("rust", "native_linux", "tmp_quota", "native_musl", "python_sqlite", "namespace_view_required"):
        require(key not in profile or type(profile[key]) is bool, "invalid profile flag: " + key)
    require(profile.get("workspace", "root") in ("root", "producer"), "unknown working-directory selection")
    require(isinstance(policy.get("project_cargo_configuration", {}), dict), "invalid Cargo configuration policy")
    environment = profile.get("required_environment", {})
    require(isinstance(environment, dict) and all(isinstance(k, str) and isinstance(v, str) for k, v in environment.items()),
            "invalid required environment")
    for name in profile["tools"]:
        descriptor = policy["tools"][name]
        require(isinstance(descriptor, dict) and isinstance(descriptor.get("command"), str) and bool(descriptor["command"])
                and descriptor.get("kind") in ("native", "executable") and isinstance(descriptor.get("args"), list)
                and all(isinstance(a, str) for a in descriptor["args"]), "invalid tool descriptor: " + name)
        require(any(isinstance(descriptor.get(k), str) and bool(descriptor[k])
                    for k in ("version", "version_from", "version_regex")), "missing version policy: " + name)
        if "version_regex" in descriptor:
            try:
                re.compile(descriptor["version_regex"])
            except (TypeError, re.error) as error:
                raise Refused("invalid version pattern: " + name) from error


def sha256(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def trusted_path(path, forbidden, executable=True):
    launch = Path(os.path.abspath(path))
    resolved = launch.resolve(strict=True)
    for candidate in (launch, resolved):
        require(not any(candidate.is_relative_to(root) for root in forbidden),
                "tool is inside a forbidden source/target/scratch/data root: " + str(candidate))
        info = candidate.stat()
        require(stat.S_ISREG(info.st_mode) and (not executable or info.st_mode & 0o111),
                "tool is not a regular executable/input")
        require(info.st_uid in (0, os.geteuid()) and not info.st_mode & 0o022,
                "untrusted tool owner or writable mode: " + str(candidate))
        for parent in candidate.parents:
            info = parent.stat()
            sticky_system = info.st_uid == 0 and bool(info.st_mode & stat.S_ISVTX)
            require(info.st_uid in (0, os.geteuid()) and (not info.st_mode & 0o022 or sticky_system),
                    "untrusted tool ancestor: " + str(parent))
    info = resolved.stat()
    return {"launch_path": str(launch), "resolved_path": str(resolved), "sha256": sha256(resolved),
            "uid": info.st_uid, "mode": stat.S_IMODE(info.st_mode), "device": info.st_dev,
            "inode": info.st_ino, "bytes": info.st_size}


def probe(path, arguments, cwd, directory, label):
    command = [path, *arguments]
    started = time.monotonic()
    environment_overrides = {"RUSTUP_AUTO_INSTALL": "0"}
    timed_out = False
    with subprocess.Popen(command, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, close_fds=True,
                          start_new_session=True, env={**os.environ, **environment_overrides}) as child:
        try:
            stdout, stderr = child.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            timed_out = True
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            stdout, stderr = child.communicate()
        completed = subprocess.CompletedProcess(command, None if timed_out else child.returncode, stdout, stderr)
        termination_exit = child.returncode if timed_out else None
    # Keep native streams, including non-UTF8 bytes, available to the caller.
    sys.stdout.buffer.write(completed.stdout)
    sys.stdout.buffer.flush()
    sys.stderr.buffer.write(completed.stderr)
    sys.stderr.buffer.flush()
    (directory / (label + ".stdout")).write_bytes(completed.stdout)
    (directory / (label + ".stderr")).write_bytes(completed.stderr)
    child_exit = 124 if timed_out else completed.returncode
    if child_exit < 0:
        child_exit = 128 - child_exit
    result = {"command": command, "cwd": str(cwd), "raw_child_exit": completed.returncode,
            "child_exit": child_exit, "timed_out": timed_out, "termination_exit": termination_exit,
            "environment_overrides": environment_overrides,
            "stdout": completed.stdout.decode("utf-8", errors="replace"),
            "stderr": completed.stderr.decode("utf-8", errors="replace"),
            "elapsed_seconds": time.monotonic() - started}
    if timed_out:
        raise ProbeFailed(result)
    return result


def native_architecture(path):
    with Path(path).open("rb") as stream:
        header = stream.read(4096)
    machine = {"arm64": "aarch64", "amd64": "x86_64"}.get(platform.machine().lower(), platform.machine().lower())
    architectures = []
    if header[:4] == b"\x7fELF" and len(header) >= 20:
        order = "little" if header[5] == 1 else "big"
        architectures = [{62: "x86_64", 183: "aarch64"}.get(int.from_bytes(header[18:20], order))]
    elif header[:4] in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"):
        order = "little" if header[:4] == b"\xcf\xfa\xed\xfe" else "big"
        architectures = [{0x1000007: "x86_64", 0x100000c: "aarch64"}.get(int.from_bytes(header[4:8], order))]
    elif header[:4] in (b"\xca\xfe\xba\xbe", b"\xca\xfe\xba\xbf"):
        count = int.from_bytes(header[4:8], "big")
        stride = 20 if header[:4] == b"\xca\xfe\xba\xbe" else 32
        require(0 < count <= 32 and 8 + count * stride <= len(header), "invalid native universal object")
        architectures = [{0x1000007: "x86_64", 0x100000c: "aarch64"}.get(
            int.from_bytes(header[8 + n * stride:12 + n * stride], "big")) for n in range(count)]
    require(machine in architectures, "tool is not an admitted native executable for " + machine)
    return machine


def distribution_identity(root, paths=("bin", "lib", "include", "share")):
    entries = []
    bases = set()
    for name in paths:
        require(contract.safe_path(name), "invalid distribution identity path")
        bases.add(root / name)
    # Root files include Go's go.env/VERSION and flat tool distributions.
    bases.update(path for path in root.iterdir() if path.is_file() or path.is_symlink())
    for base in sorted(bases):
        if not base.exists():
            continue
        for path in [base, *sorted(base.rglob("*"))]:
            relative = str(path.relative_to(root))
            info = path.lstat()
            if path.is_symlink():
                require(path.resolve(strict=True).is_relative_to(root), "distribution symlink escapes admitted root")
                entries.append({"path": relative, "link": os.readlink(path)})
                continue
            require(info.st_uid in (0, os.geteuid()) and not info.st_mode & 0o022,
                    "untrusted distribution entry: " + relative)
            require(stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode), "unsupported distribution entry")
            entry = {"path": relative, "mode": stat.S_IMODE(info.st_mode), "directory": stat.S_ISDIR(info.st_mode)}
            if stat.S_ISREG(info.st_mode):
                entry["sha256"] = sha256(path)
            entries.append(entry)
    require(bool(entries), "empty distribution identity")
    return contract.identity(entries)


def rust_channel(root):
    text = (root / "rust-toolchain.toml").read_text(encoding="utf-8")
    matches = re.findall(r'(?m)^channel\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*(?:#.*)?$', text)
    require(len(matches) == 1, "missing/ambiguous exact Rust channel")
    return matches[0]


def cargo_configuration_override(name):
    return (name.startswith(("CARGO_PROFILE_", "CARGO_BUILD_", "CARGO_ALIAS_", "CARGO_UNSTABLE_", "CARGO_HOST_"))
            or re.fullmatch(r"CARGO_TARGET_.+_(?:LINKER|RUSTFLAGS|RUSTDOCFLAGS|RUNNER)", name) is not None)


def admit_rust_environment(root, profile):
    channel = rust_channel(root)
    override = os.environ.get("RUSTUP_TOOLCHAIN")
    system, machine = platform.system(), platform.machine().lower()
    architecture = {"arm64": "aarch64", "amd64": "x86_64"}.get(machine, machine)
    host = architecture + ("-unknown-linux-gnu" if system == "Linux" else "-apple-darwin")
    require(override in (None, "", channel, channel + "-" + host), "RUSTUP_TOOLCHAIN disagrees with pinned channel")
    for name in ("RUSTC", "RUSTDOC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
                 "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTDOCFLAGS", "RUSTC_BOOTSTRAP", "RUSTC_FORCE_UNSTABLE",
                 "LD_PRELOAD", "LD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES", "DYLD_LIBRARY_PATH",
                 "CC", "CXX", "AR", "RANLIB", "CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "CXXSTDLIB", "REALGCC"):
        require(not os.environ.get(name), "unadmitted outcome-affecting override: " + name)
    admitted = profile.get("required_environment", {})
    for name, value in os.environ.items():
        if not value:
            require(not cargo_configuration_override(name) or admitted.get(name) == value,
                    "unadmitted empty Cargo override: " + name)
            continue
        cargo_override = cargo_configuration_override(name)
        native_override = name.startswith(("CC_", "CXX_", "AR_", "RANLIB_", "HOST_CC", "TARGET_CC",
                                           "HOST_CXX", "TARGET_CXX", "HOST_AR", "TARGET_AR",
                                           "HOST_CFLAGS", "TARGET_CFLAGS"))
        require(not native_override and (not cargo_override or admitted.get(name) == value),
                "unadmitted native/profile override: " + name)
    return channel, host


def admit_cargo_configuration(repository, root, policy, report):
    allowed = policy.get("project_cargo_configuration", {})
    home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    require(home.is_absolute(), "CARGO_HOME must be absolute for Cargo configuration admission")
    directories = [root, *root.parents]
    candidates = []
    for parent in directories:
        candidates.extend(parent / ".cargo" / name for name in ("config", "config.toml"))
    candidates.extend(home / name for name in ("config", "config.toml"))
    observed = {}
    for candidate in sorted(set(candidates)):
        if not candidate.exists():
            continue
        require(candidate.is_file(), "invalid Cargo configuration file")
        require(candidate.is_relative_to(repository), "unadmitted ambient Cargo configuration: " + str(candidate))
        relative = str(candidate.relative_to(repository))
        value = sha256(candidate)
        require(allowed.get(relative) == value, "unadmitted project Cargo configuration: " + relative)
        observed[relative] = value
    for relative, expected in allowed.items():
        require(contract.safe_path(relative) and (repository / relative).is_file()
                and sha256(repository / relative) == expected, "missing/changed admitted Cargo configuration: " + relative)
    report["cargo_configuration"] = observed


def admit_rust_context(root, profile, policy, report, forbidden, logs):
    channel, host = report["rust"]["channel"], report["rust"]["expected_host"]
    require("rustup" in report["tool_identities"] and "rustc" in report["tool_identities"],
            "missing Rust tool admission")
    rustup = report["tool_identities"]["rustup"]
    def run(arguments, label):
        result = probe(rustup["launch_path"], arguments, root, logs, "rustup-" + label)
        rustup["probes"].append(result)
        if result["child_exit"]:
            raise ProbeFailed(result)
        return result["stdout"].strip()
    active = run(["show", "active-toolchain"], "active")
    require(bool(active) and active.split()[0] == channel + "-" + host, "active toolchain disagrees with pinned native Rust")
    compiler = report["tool_identities"]["rustc"]["probes"][0]["stdout"]
    require(re.search(r"(?m)^release: " + re.escape(channel) + r"$", compiler)
            and re.search(r"(?m)^host: " + re.escape(host) + r"$", compiler), "wrong effective Rust release/host")
    text = (root / "rust-toolchain.toml").read_text(encoding="utf-8")
    def declared_list(name):
        found = re.findall(r"(?m)^" + name + r"\s*=\s*(\[[^\]]*\])", text)
        require(len(found) <= 1, "ambiguous Rust " + name)
        return json.loads(found[0]) if found else []
    components = declared_list("components")
    if report["profile"] in ("quality", "producer"):
        components = sorted(set([*components, "rustfmt", "clippy"]))
    installed = run(["component", "list", "--installed"], "components").splitlines()
    installed = {line.split()[0].removesuffix("-" + host) for line in installed if line.strip()}
    require(set(components) <= installed, "missing pinned Rust components")
    targets = run(["target", "list", "--installed"], "targets").splitlines()
    require(set(declared_list("targets")) <= set(targets), "missing declared Rust target standard libraries")
    objects = {}
    for tool in ["cargo", "rustc", "rustdoc", *(["rustfmt"] if "rustfmt" in components else []),
                 *(["cargo-clippy"] if "clippy" in components else [])]:
        path = run(["which", tool], "which-" + tool)
        obj = trusted_path(path, forbidden)
        if policy["tools"]["rustup"].get("kind") == "native":
            obj["architecture"] = native_architecture(obj["resolved_path"])
        objects[tool] = obj
        if tool in report["tool_identities"]:
            selected = report["tool_identities"][tool]
            # Keep argv0 of rustup proxies. Never execute their canonical rustup
            # target as if it were cargo/rustc, or replace an explicit CARGO.
            selected["effective"] = (obj if Path(selected["resolved_path"]).name == "rustup" else
                                      {key: value for key, value in selected.items() if key != "probes"})
    report["rust"].update(active_toolchain=active, components=sorted(installed), targets=targets, objects=objects)


def admit_musl_inputs(root, report, forbidden, logs):
    linker = report["tool_identities"]["musl-gcc"]
    raw = Path(linker["resolved_path"]).read_bytes()
    require(raw.startswith(b"#!/bin/sh\n"), "unsupported musl driver; expected the declared shell wrapper")
    wrapper = re.fullmatch(r'#!/bin/sh\nexec "\$\{REALGCC:-([^\s"{}]+)\}" "\$@" -specs "(/[^"\n]+)"\n?',
                           raw.decode("utf-8"))
    require(wrapper is not None, "unsupported musl wrapper structure")
    backend = shutil.which(wrapper[1])
    require(backend is not None, "missing musl native compiler backend")
    objects = {"backend": trusted_path(backend, forbidden), "shell": trusted_path("/bin/sh", forbidden),
               "specs": trusted_path(wrapper[2], forbidden, executable=False)}
    for name in ("backend", "shell"):
        objects[name]["architecture"] = native_architecture(objects[name]["resolved_path"])
    specs = Path(objects["specs"]["resolved_path"]).read_text()
    library = re.search(r"(?m)^\*link_libgcc:\n-L(/[^\s]+)(?:\s|$)", specs)
    require(library is not None, "musl specs omit an absolute primary library root")
    library = Path(library[1]).resolve(strict=True)
    objects["libc"] = trusted_path(library / "libc.a", forbidden, executable=False)
    with Path(objects["libc"]["resolved_path"]).open("rb") as stream:
        require(stream.read(8) == b"!<arch>\n", "musl libc is not a static archive")

    # GCC -### prints the expanded pipeline without executing compiler/linker
    # commands. Metadata such as -dumpmachine/-print-file-name can remain GNU.
    result = probe(linker["launch_path"], ["-###", "-static", "-x", "c", "/dev/null"],
                   root, logs, "musl-static-recipe")
    linker["probes"].append(result)
    if result["child_exit"]:
        raise ProbeFailed(result)
    commands = [shlex.split(line) for line in result["stderr"].splitlines() if line.startswith(" /")]
    links = [command for command in commands if "-lc" in command]
    require(len(links) == 1, "missing/ambiguous musl static link recipe")
    link = links[0]
    search = []
    for number, argument in enumerate(link):
        if argument == "-L":
            require(number + 1 < len(link), "incomplete library search option")
            search.append(link[number + 1])
        elif argument.startswith("-L"):
            search.append(argument[2:])
    require(bool(search) and Path(search[0]).resolve() == library and "-static" in link and "-nostdlib" in link,
            "musl library is not selected first by the static recipe")
    architecture = report["rust"]["expected_host"].split("-", 1)[0]
    require(any(argument == "-dynamic-linker" and number + 1 < len(link) and
                Path(link[number + 1]).name == "ld-musl-" + architecture + ".so.1"
                for number, argument in enumerate(link)),
            "static recipe does not select the expected musl toolchain")
    selected_crt = {Path(argument).name for argument in link
                    if Path(argument).is_absolute() and Path(argument).parent.resolve() == library}
    require({"crti.o", "crtn.o"} <= selected_crt and
            bool(selected_crt & {"crt1.o", "Scrt1.o", "rcrt1.o"}), "missing musl startup inputs")
    for name in selected_crt & {"crt1.o", "Scrt1.o", "rcrt1.o", "crti.o", "crtn.o"}:
        objects["musl-crt:" + name] = trusted_path(library / name, forbidden, executable=False)
    for number, command in enumerate(commands):
        objects["command:" + str(number)] = trusted_path(command[0], forbidden)
        native_architecture(objects["command:" + str(number)]["resolved_path"])
    for number, argument in enumerate(link):
        path = Path(argument)
        if path.is_absolute() and (path.suffix in (".a", ".so") or
                path.name in ("crtbegin.o", "crtend.o", "crtbeginS.o", "crtendS.o") or
                path.is_file() and path.suffix == ".o"):
            objects["link-input:" + str(number)] = trusted_path(path, forbidden, executable=False)
    ld = probe(objects["backend"]["launch_path"], ["-print-prog-name=ld"], root, logs, "musl-backend-linker")
    linker["probes"].append(ld)
    if ld["child_exit"]:
        raise ProbeFailed(ld)
    ld_path = shutil.which(ld["stdout"].strip())
    require(ld_path is not None, "missing musl backend linker")
    objects["linker"] = trusted_path(ld_path, forbidden)
    native_architecture(objects["linker"]["resolved_path"])
    report["musl"].update(library_root=str(library), objects=objects,
        proof="trusted wrapper/specs and printed static inputs; actual artifact/runtime qualification required")


def resource_observations(target, scratch):
    def filesystem(path):
        path = path.resolve()
        parent = path
        while not parent.exists():
            parent = parent.parent
        usage = shutil.disk_usage(parent)
        return {"path": str(path), "observed_at": str(parent), "device": parent.stat().st_dev,
                "free_bytes": usage.free, "total_bytes": usage.total}
    available = None
    meminfo = Path("/proc/meminfo")
    if platform.system() == "Linux" and meminfo.is_file():
        match = re.search(r"(?m)^MemAvailable:\s+(\d+) kB$", meminfo.read_text())
        if match:
            available = int(match[1]) * 1024
    return {"system": platform.system(), "machine": platform.machine(), "kernel": platform.release(),
            "cpu_count": os.cpu_count(), "available_memory_bytes": available,
            "target": filesystem(target), "scratch": filesystem(scratch), "tmp": filesystem(Path("/tmp")),
            "full_job_qualification": "unmeasured; phase observations and derived margins required during composed checks",
            "build_environment": {name: os.environ.get(name) for name in ("CARGO_BUILD_JOBS", "CARGO_INCREMENTAL",
                "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_DEV_STRIP", "CARGO_TARGET_DIR")}}


def admit_resources(args, profile, report):
    require(args.target_dir.is_absolute() and args.scratch_dir.is_absolute(), "target/scratch paths must be absolute")
    require(args.target_dir.resolve() != args.scratch_dir.resolve(), "target and scratch must be separate")
    observations = resource_observations(args.target_dir, args.scratch_dir)
    report["resources"] = observations
    if profile.get("rust"):
        selected_target = os.environ.get("CARGO_TARGET_DIR")
        require(bool(selected_target) and Path(selected_target).is_absolute()
                and Path(selected_target).resolve() == args.target_dir.resolve(),
                "CARGO_TARGET_DIR must match the admitted absolute target directory")
    if profile.get("native_linux"):
        require(platform.system() == "Linux" and platform.machine().lower() in ("aarch64", "x86_64"),
                "profile requires supported native Linux")
    if profile.get("native_musl"):
        require(platform.system() == "Linux" and platform.machine().lower() in ("aarch64", "x86_64"),
                "musl profile requires supported native Linux")
    for name, expected in profile.get("required_environment", {}).items():
        require(os.environ.get(name) == expected, "required build environment mismatch: " + name)
    if profile.get("tmp_quota"):
        require(observations["tmp"]["free_bytes"] >= 2_500_000_000,
                "insufficient /tmp disk for existing quota assertion; recheck before suite")
    if args.resource_envelope:
        envelope = contract.read_json(args.resource_envelope)
        require(isinstance(envelope, dict) and isinstance(envelope.get("runner_class"), str)
                and bool(envelope["runner_class"]) and isinstance(envelope.get("basis"), str)
                and bool(envelope["basis"]), "resource envelope requires a named measured runner and basis")
        for key in ("min_disk_bytes", "min_memory_bytes"):
            require(type(envelope.get(key)) is int and envelope[key] >= 0, "invalid resource minimum: " + key)
        report["resource_envelope"] = {**envelope, "sha256": sha256(args.resource_envelope)}
        require(min(observations["target"]["free_bytes"], observations["scratch"]["free_bytes"])
                >= envelope["min_disk_bytes"], "insufficient disk for measured runner envelope")
        require(envelope["min_memory_bytes"] == 0 or (observations["available_memory_bytes"] is not None
                and observations["available_memory_bytes"] >= envelope["min_memory_bytes"]),
                "insufficient/unknown available memory for measured runner envelope")


def admit_tool(name, descriptor, root, forbidden, logs, expected=None):
    override = descriptor.get("override")
    requested = os.environ[override] if override and override in os.environ else descriptor["command"]
    require(isinstance(requested, str) and bool(requested), "empty executable override for " + name)
    selected = (requested if os.path.isabs(requested) else shutil.which(requested))
    require(selected is not None, "missing required tool: " + name)
    record = trusted_path(selected, forbidden)
    require(Path(record["resolved_path"]).name not in ("mise", "mise-shim", "mise.exe"),
            "mise shim/launcher is not a direct tool object: " + name)
    if descriptor.get("system_path"):
        require(Path(descriptor["system_path"]).is_absolute(), "system tool anchor must be absolute")
        anchor = trusted_path(descriptor["system_path"], forbidden)
        require(record["sha256"] == anchor["sha256"] and
                (record["resolved_path"] == anchor["resolved_path"] or
                 (record["device"], record["inode"]) == (anchor["device"], anchor["inode"])),
                "tool differs from fixed system object: " + name)
        record["system_path"] = anchor["resolved_path"]
    acquisition = expected.get(name) if expected is not None else None
    if expected is not None:
        expected_sha = acquisition.get("sha256") if isinstance(acquisition, dict) else acquisition
        require(isinstance(expected_sha, str) and re.fullmatch(r"[0-9a-f]{64}", expected_sha)
                and record["sha256"] == expected_sha, "tool does not match trusted acquisition manifest: " + name)
    if descriptor.get("kind") == "native":
        record["architecture"] = native_architecture(record["resolved_path"])
    if descriptor.get("root_env") or descriptor.get("root_from_binary"):
        supplied = (os.environ.get(descriptor["root_env"]) if descriptor.get("root_env") else
                    str(Path(record["resolved_path"]).parent.parent))
        require(bool(supplied), "missing admitted distribution root for " + name)
        distribution = Path(supplied).resolve(strict=True)
        require(distribution not in (Path.home().resolve(), Path("/"), Path("/usr"), Path("/usr/local"), Path("/tmp").resolve()),
                "distribution root is too broad")
        require(not Path.home().resolve().is_relative_to(distribution), "distribution root is too broad for HOME")
        relative = descriptor.get("executable_relative")
        if relative is not None:
            require(contract.safe_path(relative), "invalid distribution executable path")
            expected_path = (distribution / relative).resolve(strict=True)
            require(expected_path.is_relative_to(distribution) and
                    Path(record["resolved_path"]) == expected_path,
                    "tool differs from independently supplied distribution object")
        else:
            require(Path(record["resolved_path"]).is_relative_to(distribution / "bin"), "tool escapes distribution bin root")
        require(not any(distribution.is_relative_to(p) or p.is_relative_to(distribution) for p in forbidden),
                "distribution root overlaps source/target/scratch")
        info = distribution.stat()
        require(stat.S_ISDIR(info.st_mode) and info.st_uid in (0, os.geteuid()) and not info.st_mode & 0o022,
                "untrusted distribution root")
        record["distribution_root"] = str(distribution)
        scope = descriptor.get("distribution_paths", ["bin", "lib", "include", "share"])
        record["distribution_sha256"] = distribution_identity(distribution, scope)
        record["distribution_identity_scope"] = scope
        if expected is not None:
            require(isinstance(acquisition, dict)
                    and acquisition.get("distribution_sha256") == record["distribution_sha256"],
                    "distribution does not match trusted acquisition manifest: " + name)
    record["requested"] = requested
    record["probes"] = [probe(record["launch_path"], descriptor["args"], root, logs, name + "-version")]
    result = record["probes"][0]
    if result["child_exit"]:
        return record, {"tool": name, "status": "infrastructure_failure", "child_exit": result["child_exit"],
                        "reason": "version probe failed"}
    pattern = descriptor.get("version_regex")
    if pattern is None:
        pattern = r"^" + re.escape(descriptor["version"]) + r"(?:\s|$)"
    plain = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", result["stdout"])
    require(re.search(pattern, plain.strip()), "wrong version for " + name + ": expected " + pattern)
    return record, None


def tool_descriptor(policy, name, repository, channel):
    descriptor = dict(policy["tools"][name])
    origin = descriptor.get("version_from", "")
    if origin == "rust-channel":
        descriptor["version"] = descriptor["command"] + " " + channel
    elif origin.startswith("mise:"):
        key = origin.removeprefix("mise:")
        text = (repository / "mise.toml").read_text(encoding="utf-8")
        section = re.search(r"(?ms)^\[tools\]\s*\n(.*?)(?=^\[|\Z)", text)
        require(section is not None, "missing mise tool declarations")
        found = re.findall(r'(?m)^\s*(?:"' + re.escape(key) + r'"|' + re.escape(key) +
                           r')\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$', section[1])
        require(len(found) == 1, "missing/ambiguous exact mise pin: " + key)
        descriptor["version"] = descriptor.get("version_prefix", "") + found[0]
    return descriptor


def admit_fixture_execution(profile, root, forbidden, logs, report):
    declaration = profile.get("fixture_execution")
    if declaration is None:
        return
    require(isinstance(declaration, dict), "invalid fixture execution declaration")
    minimal = declaration.get("minimal_path")
    require(isinstance(minimal, list) and bool(minimal) and all(
        isinstance(path, str) and Path(path).is_absolute() and Path(path).is_dir() for path in minimal),
        "missing/invalid minimal PATH directories")
    result = {"minimal_path": minimal, "same_primary": {}, "fixed_helpers": {}, "read_only_resources": []}
    report["fixture_execution"] = result
    resources = declaration.get("read_only_resources", [])
    require(isinstance(resources, list), "invalid fixture resources")
    def add_resource(resource):
        optional = isinstance(resource, dict) and resource.get("if_present") is True
        resource = resource.get("path") if isinstance(resource, dict) else resource
        require(isinstance(resource, str) and Path(resource).is_absolute(), "fixture resources must be absolute")
        if optional and not Path(resource).exists():
            return
        path = Path(resource).resolve(strict=True)
        require(not any(path.is_relative_to(p) or p.is_relative_to(path) for p in forbidden),
                "fixture resource overlaps source/target/scratch/data")
        for candidate in (path, *path.parents):
            info = candidate.stat()
            sticky_system = info.st_uid == 0 and bool(info.st_mode & stat.S_ISVTX)
            require(info.st_uid in (0, os.geteuid()) and (not info.st_mode & 0o022 or sticky_system),
                    "untrusted fixture resource: " + str(candidate))
        result["read_only_resources"].append(str(path))
    for resource in resources:
        add_resource(resource)

    def select(role, descriptor):
        require(re.fullmatch(r"[a-z][a-z0-9_-]*", role) and isinstance(descriptor, dict),
                "invalid fixture tool role")
        if "absolute" in descriptor:
            path = descriptor["absolute"]
            require(isinstance(path, str) and Path(path).is_absolute(), "fixture path must be absolute")
        else:
            require(descriptor.get("lookup") == "minimal_path", "invalid fixture lookup")
            command = descriptor.get("command", role)
            require(isinstance(command, str) and bool(command) and Path(command).name == command,
                    "invalid minimal PATH command")
            path = shutil.which(command, path=os.pathsep.join(minimal))
            require(path is not None, "missing minimal PATH tool: " + command)
        obj = trusted_path(path, forbidden)
        obj["exposure"] = descriptor
        return obj

    same = declaration.get("same_primary", {})
    helpers = declaration.get("fixed_helpers", {})
    require(isinstance(same, dict) and isinstance(helpers, dict), "invalid fixture roles")
    for role, descriptor in same.items():
        obj = select(role, descriptor)
        primary = report["tool_identities"].get(descriptor.get("primary"))
        require(primary is not None, "missing admitted primary for fixture role: " + role)
        primary = primary.get("effective", primary)
        require(obj["sha256"] == primary["sha256"] and
                (obj["resolved_path"] == primary["resolved_path"] or
                 (obj["device"], obj["inode"]) == (primary["device"], primary["inode"])),
                "fixture tool must be the same admitted primary object: " + role)
        result["same_primary"][role] = obj
    for role, descriptor in helpers.items():
        obj = select(role, descriptor)
        obj["architecture"] = native_architecture(obj["resolved_path"])
        arguments = descriptor.get("args")
        obj["probes"] = []
        if arguments is not None:
            require(isinstance(arguments, list) and all(isinstance(a, str) for a in arguments),
                    "invalid fixture probe arguments")
            probe_result = probe(obj["launch_path"], arguments, root, logs, "fixture-" + role)
            obj["probes"].append(probe_result)
            if probe_result["child_exit"]:
                raise ProbeFailed(probe_result)
            require(re.search(descriptor["version_regex"], probe_result["stdout"].strip()),
                    "wrong fixture helper version: " + role)
        else:
            obj["version_policy"] = "identity only; no portable version interface"
        if descriptor.get("python_runtime"):
            script = ("import sysconfig,ctypes,_ctypes,socket,_socket,json; "
                      "print(json.dumps(dict(stdlib=sysconfig.get_path('stdlib'), "
                      "ctypes=getattr(ctypes,'__file__',None), ctypes_native=getattr(_ctypes,'__file__',None), "
                      "socket=getattr(socket,'__file__',None), socket_native=getattr(_socket,'__file__',None))))")
            probe_result = probe(obj["launch_path"], ["-I", "-c", script], root, logs, "fixture-" + role + "-runtime")
            obj["probes"].append(probe_result)
            if probe_result["child_exit"]:
                raise ProbeFailed(probe_result)
            paths = json.loads(probe_result["stdout"])
            require(isinstance(paths, dict) and bool(paths) and all(
                isinstance(name, str) and (path is None or isinstance(path, str) and Path(path).is_absolute()
                and Path(path).exists()) for name, path in paths.items()), "invalid fixture Python runtime paths")
            obj["builtin_modules"] = sorted(name for name, path in paths.items() if path is None)
            obj["runtime_paths"] = {name: str(Path(path).resolve(strict=True))
                                    for name, path in paths.items() if path is not None}
            for path in obj["runtime_paths"].values():
                add_resource(path)
        result["fixed_helpers"][role] = obj


def configuration_inputs(repository, root, args, report, logs):
    inputs = contract.source_inputs(repository)
    verification = {str(path.relative_to(repository)): sha256(path)
        for directory in (repository / "scripts", repository / ".github/workflows")
        for path in sorted(directory.rglob("*")) if path.is_file() and "__pycache__" not in path.parts
        and path.suffix in (".py", ".sh", ".json", ".toml", ".yml", ".yaml", ".js", ".mjs", ".ts")}
    tools = {name: (contract.identity({"executable": value["sha256"], "distribution": value["distribution_sha256"]})
                   if "distribution_sha256" in value else value.get("effective", value)["sha256"])
             for name, value in report["tool_identities"].items()}
    for name, value in report.get("rust", {}).get("objects", {}).items():
        tools["rust-component:" + name] = value["sha256"]
    for name, value in report.get("musl", {}).get("objects", {}).items():
        tools["musl-component:" + name] = value["sha256"]
    for group in ("same_primary", "fixed_helpers"):
        for name, value in report.get("fixture_execution", {}).get(group, {}).items():
            tools["fixture:" + name] = value["sha256"]
    revision = None
    if "git" in report["tool_identities"]:
        git = report["tool_identities"]["git"]
        result = probe(git["launch_path"], ["rev-parse", "HEAD"], repository, logs, "git-source")
        git["probes"].append(result)
        if result["child_exit"]:
            raise ProbeFailed(result)
        revision = result["stdout"].strip()
        require(contract.commit(revision), "invalid checked source commit")
    environment_names = {"RUSTUP_TOOLCHAIN", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "CARGO_PROFILE_DEV_DEBUG",
        "CARGO_PROFILE_DEV_STRIP", "CGO_ENABLED", "GOFLAGS", "GOTOOLCHAIN", "GOPROXY", "GOSUMDB"}
    environment_names.update(name for name in os.environ if cargo_configuration_override(name))
    value = {"source": revision, "source_inputs": inputs, "pins_sha256": report["pins_sha256"],
        "verification_inputs": verification,
        "profile": report["profile"], "tool_objects": tools, "working_directory": str(root.relative_to(repository)),
        "manifests": {p: sha for p, sha in inputs.items() if p.endswith("Cargo.toml")},
        "locks": {p: sha for p, sha in inputs.items() if p.endswith("Cargo.lock")},
        "tool_configuration": {p: sha256(repository / p) for p in ("mise.toml", "mise.lock") if (repository / p).is_file()},
        "namespace_view_sha256": report.get("namespace_view", {}).get("sha256"),
        "runtime": {"system": platform.system(), "machine": platform.machine(), "libc": platform.libc_ver()},
        "environment": {name: os.environ.get(name) for name in sorted(environment_names)}}
    report["configuration_inputs"] = value
    report["configuration_inputs_id"] = contract.identity(value)


def namespace_inputs(args, report):
    roots = {}
    tools = report["tool_identities"]
    if "node" in tools:
        roots["node"] = tools["node"]["distribution_root"]
    if "go" in tools:
        roots["go"] = str(Path(tools["go"]["resolved_path"]).parent.parent)
    if "rustc" in report.get("rust", {}).get("objects", {}):
        roots["rust"] = str(Path(report["rust"]["objects"]["rustc"]["resolved_path"]).parent.parent)
    for group in ("same_primary", "fixed_helpers"):
        for name, value in report.get("fixture_execution", {}).get(group, {}).items():
            roots["fixture:" + name] = value["resolved_path"]
    for number, path in enumerate(report.get("fixture_execution", {}).get("read_only_resources", [])):
        roots["fixture-resource:" + str(number)] = path
    for number, path in enumerate(report.get("fixture_execution", {}).get("minimal_path", [])):
        roots["fixture-path:" + str(number)] = str(Path(path).resolve(strict=True))
    report["required_namespace_roots"] = roots
    if not args.namespace_view:
        return
    view = contract.read_json(args.namespace_view)
    require(isinstance(view, dict) and view.get("version") == 1 and isinstance(view.get("basis"), str) and bool(view["basis"])
            and isinstance(view.get("read_only_roots"), list), "invalid namespace view declaration")
    exposed = []
    for name in view["read_only_roots"]:
        require(isinstance(name, str) and Path(name).is_absolute(), "namespace roots must be absolute")
        path = Path(name).resolve(strict=True)
        require(path not in (Path("/"), Path.home().resolve(), Path("/usr"), Path("/tmp").resolve()),
                "namespace tool exposure root is too broad")
        require(not Path.home().resolve().is_relative_to(path), "namespace tool exposure is too broad for HOME")
        info = path.stat()
        require(info.st_uid in (0, os.geteuid()) and not info.st_mode & 0o022, "untrusted namespace exposure root")
        exposed.append(path)
    missing = [name for name, root in roots.items() if not any(Path(root).is_relative_to(p) for p in exposed)]
    report["namespace_view"] = {**view, "sha256": sha256(args.namespace_view), "missing": missing,
        "execution_proof": "declaration only; actual public namespace assertions remain required in T4"}
    require(not missing, "namespace view omits required distribution roots: " + ", ".join(missing))


def revalidate_objects(report, forbidden):
    objects = list(report["tool_identities"].values()) + list(report.get("rust", {}).get("objects", {}).values())
    objects.extend(report.get("musl", {}).get("objects", {}).values())
    for group in ("same_primary", "fixed_helpers"):
        objects.extend(report.get("fixture_execution", {}).get(group, {}).values())
    for previous in objects:
        current = trusted_path(previous["launch_path"], forbidden, executable=bool(previous["mode"] & 0o111))
        require(all(current[key] == previous[key] for key in ("resolved_path", "sha256", "uid", "mode", "device", "inode")),
                "tool identity changed during admission: " + previous["launch_path"])
        if "distribution_sha256" in previous:
            require(distribution_identity(Path(previous["distribution_root"]), previous["distribution_identity_scope"])
                    == previous["distribution_sha256"],
                    "distribution identity changed during admission")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--root", type=Path, default=contract.ROOT)
    parser.add_argument("--pins", type=Path, default=Path(__file__).with_name("tool-pins.json"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--scratch-dir", type=Path, required=True)
    parser.add_argument("--resource-envelope", type=Path, help="measured runner minima; no invented default margins")
    parser.add_argument("--expected-tools", type=Path, help="trusted acquisition manifest for restored executable bytes")
    parser.add_argument("--namespace-view", type=Path, help="job-local read-only tool exposures; not kernel qualification")
    parser.add_argument("--data-root", type=Path, action="append", default=[], help="forbidden product/fixture data root")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    logs = args.output / "probes"
    logs.mkdir()
    report = {"version": 1, "profile": args.profile, "status": "incomplete", "child_exit": 3,
              "tool_identities": {}, "discrepancies": [], "logs": []}
    try:
        policy = contract.read_json(args.pins)
        validate_policy(policy, args.profile)
        report["pins_sha256"] = sha256(args.pins)
        expected = None
        if args.expected_tools:
            acquisition = contract.read_json(args.expected_tools)
            require(isinstance(acquisition, dict) and acquisition.get("pins_sha256") == report["pins_sha256"]
                    and isinstance(acquisition.get("tools"), dict), "incompatible acquisition manifest")
            expected = acquisition["tools"]
            report["acquisition_manifest_sha256"] = sha256(args.expected_tools)
        repository = args.root.resolve(strict=True)
        profile = policy["profiles"][args.profile]
        if profile.get("rust") or "musl-gcc" in profile["tools"]:
            require("REALGCC" not in os.environ, "unadmitted musl backend override: REALGCC")
        require(not profile.get("namespace_view_required") or args.namespace_view is not None,
                "profile requires an explicit namespace view declaration")
        root = repository / "tools/kaban-rust-scip" if profile.get("workspace") == "producer" else repository
        require(all(path.is_absolute() for path in args.data_root), "data roots must be absolute")
        forbidden = [repository, args.target_dir.resolve(), args.scratch_dir.resolve(),
                     *(path.resolve() for path in args.data_root)]
        report["forbidden_data_roots"] = [str(path.resolve()) for path in args.data_root]
        admit_resources(args, profile, report)
        if profile.get("rust"):
            channel, host = admit_rust_environment(root, profile)
            admit_cargo_configuration(repository, root, policy, report)
            report["rust"] = {"channel": channel, "expected_host": host,
                              "toolchain_sha256": sha256(root / "rust-toolchain.toml")}
        for name in profile["tools"]:
            try:
                descriptor = tool_descriptor(policy, name, repository, report.get("rust", {}).get("channel"))
                record, failure = admit_tool(name, descriptor, root, forbidden, logs, expected)
                report["tool_identities"][name] = record
                if failure:
                    report["discrepancies"].append(failure)
            except ProbeFailed:
                raise
            except (Refused, OSError, subprocess.TimeoutExpired) as error:
                report["discrepancies"].append({"tool": name, "status": "incomplete", "reason": str(error)})
        if policy.get("admit_interpreter") and not report["discrepancies"]:
            selected = report["tool_identities"].get("python3", {})
            require(Path(sys.executable).resolve() == Path(selected.get("resolved_path", "")),
                    "preflight interpreter differs from selected pinned Python object")
            report["interpreter"] = {"path": str(Path(sys.executable).resolve()), "version": sys.version,
                                     "sha256": sha256(sys.executable)}
        if profile.get("python_sqlite") and not report["discrepancies"]:
            python = report["tool_identities"].get("python3")
            require(python is not None, "missing Python SQLite admission")
            result = probe(python["launch_path"], ["-c", "import sqlite3; print(sqlite3.sqlite_version)"], root,
                           logs, "python-sqlite")
            python["probes"].append(result)
            if result["child_exit"]:
                raise ProbeFailed(result)
            require(bool(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", result["stdout"].strip())), "invalid SQLite identity")
            report["sqlite_version"] = result["stdout"].strip()
        if profile.get("rust") and not report["discrepancies"]:
            admit_rust_context(root, profile, policy, report, forbidden, logs)
        if profile.get("native_musl") and not report["discrepancies"]:
            architecture = report["rust"]["expected_host"].split("-", 1)[0]
            target = architecture + "-unknown-linux-musl"
            require(target in report["rust"]["targets"], "missing native musl standard library target")
            linker = report["tool_identities"].get("musl-gcc")
            require(linker is not None, "missing native musl linker admission")
            result = probe(linker["launch_path"], ["-dumpmachine"], root, logs, "musl-compiler-target")
            linker["probes"].append(result)
            if result["child_exit"]:
                raise ProbeFailed(result)
            require(result["stdout"].strip().startswith(architecture + "-") and "linux" in result["stdout"],
                    "musl linker backend architecture mismatch")
            report["musl"] = {"target": target, "compiler_target": result["stdout"].strip(),
                              "runtime_qualification": "requires actual native artifact build/consumers"}
            admit_musl_inputs(root, report, forbidden, logs)
        if not report["discrepancies"]:
            admit_fixture_execution(profile, root, forbidden, logs, report)
            namespace_inputs(args, report)
            configuration_inputs(repository, root, args, report, logs)
            revalidate_objects(report, forbidden)
        failures = [d for d in report["discrepancies"] if d["status"] == "infrastructure_failure"]
        if failures:
            report.update(status="infrastructure_failure", child_exit=failures[0]["child_exit"])
        elif not report["discrepancies"]:
            report.update(status="pass", child_exit=0)
    except ProbeFailed as error:
        report.update(status="infrastructure_failure", child_exit=error.result["child_exit"])
        report["discrepancies"].append({"status": "infrastructure_failure", "reason": str(error),
                                       "probe": error.result})
    except (Refused, contract.InvalidEvidence, KeyError, TypeError, OSError, ValueError) as error:
        report["discrepancies"].append({"status": "incomplete", "reason": str(error)})
    report["logs"] = sorted(str(p.relative_to(args.output)) for p in logs.iterdir())
    (args.output / "preflight.json").write_text(json.dumps(report, indent=2) + "\n")
    return report["child_exit"] if report["child_exit"] >= 0 else 128 - report["child_exit"]


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        print("preflight incomplete: " + str(error), file=sys.stderr)
        raise SystemExit(3)
