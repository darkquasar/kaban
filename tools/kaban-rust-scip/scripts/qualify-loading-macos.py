#!/usr/bin/env python3
"""Trusted native counterpart of qualify-loading.sh; this provides no isolation tier."""
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tree_hashes(root):
    return {str(p.relative_to(root)): digest(p) for p in sorted(root.rglob("*")) if p.is_file()}


def clone_tree(source, destination):
    # APFS cloning is an optimization, never a prerequisite. Retain these private copies.
    copied = subprocess.run(["/bin/cp", "-cR", str(source), str(destination)],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if copied.returncode:
        shutil.copytree(source, destination, symlinks=True, dirs_exist_ok=True)


def main():
    producer, toolchain, source, fixture, scratch, expectation, checkout_name = sys.argv[1:]
    producer, toolchain, source, scratch = map(lambda p: Path(p).resolve(),
                                              (producer, toolchain, source, scratch))
    assert not any(scratch.iterdir())
    assert shutil.disk_usage(scratch).free >= 2_000_000_000
    os.umask(0o077)
    original_source = tree_hashes(source)
    original_tools = {str(p): digest(p) for p in (producer, toolchain / "bin/cargo",
        toolchain / "bin/rustc", toolchain / "libexec/rust-analyzer-proc-macro-srv")}
    for name in ("home", "tmp", "out", "cargo-home", "target"):
        (scratch / name).mkdir(mode=0o700)
    (scratch / "cargo-home/config.toml").write_text("[net]\noffline = true\n")
    checkout = scratch / checkout_name.lstrip("/")
    shutil.copytree(source, checkout)
    masked = fixture in ("missing-server", "missing-sysroot", "sysroot-metadata-failure",
                         "sysroot-manifest-failure", "shadow-backend")
    if masked:
        original_toolchain = toolchain
        toolchain = scratch / "toolchain"
        clone_tree(original_toolchain, toolchain)
        assert toolchain.resolve().is_relative_to(scratch)
    if fixture == "missing-server":
        (toolchain / "libexec/rust-analyzer-proc-macro-srv").unlink()
    if fixture == "missing-sysroot":
        shutil.rmtree(toolchain / "lib/rustlib/src")
    if fixture in ("sysroot-metadata-failure", "sysroot-manifest-failure"):
        library = toolchain / "lib/rustlib/src/rust/library"
        replacements = [("sysroot.toml" if fixture == "sysroot-metadata-failure" else "malformed.toml", "Cargo.toml")]
        if fixture == "sysroot-metadata-failure":
            replacements.append(("core.toml", "core/Cargo.toml"))
        for src, dst in replacements:
            destination = library / dst
            assert destination.resolve().is_relative_to(toolchain)
            destination.unlink()
            shutil.copyfile(source / src, destination)
    if fixture == "block-derive":
        shutil.copyfile(source / "derive.rs", checkout / "src/lib.rs")
    cc_query = subprocess.run(["/usr/bin/xcrun", "--find", "clang"], env={}, cwd="/",
                              check=True, capture_output=True, text=True)
    cc = Path(cc_query.stdout.strip()).resolve(strict=True)
    shadow_marker = scratch / "tmp/shadow-backend-ran"
    if fixture == "shadow-backend":
        # A disposable toolchain places shadows on the clean child's PATH. The admitted
        # Clang backend directory must precede them, including its real LTO linker.
        for name in ("ld", "as", "collect2", "lto-wrapper"):
            shadow = toolchain / "bin" / name
            assert not shadow.exists()
            real = cc.parent / name
            action = "exec " + shlex.quote(str(real)) + ' "$@"' if real.is_file() else "exit 91"
            shadow.write_text("#!/bin/sh\necho " + name + " >> " + shlex.quote(str(shadow_marker)) + "\n" + action + "\n")
            shadow.chmod(0o700)
    config = {"protocol_version": 2, "target": "aarch64-apple-darwin", "cargo": str(toolchain / "bin/cargo"),
              "rustc": str(toolchain / "bin/rustc"), "sysroot": str(toolchain),
              "proc_macro_server": str(toolchain / "libexec/rust-analyzer-proc-macro-srv"), "linker": str(cc)}
    (scratch / "prepared.json").write_text(json.dumps(config) + "\n")
    environment = {"HOME": str(scratch / "home"), "TMPDIR": str(scratch / "tmp"),
        "CARGO_HOME": str(scratch / "cargo-home"), "CARGO_TARGET_DIR": str(scratch / "target"),
        "CARGO": "/invented-host-cargo-proxy", "RUSTC_WRAPPER": "/invented-host-wrapper",
        "CARGO_REGISTRY_TOKEN": "invented-must-not-reach-child", "SDKROOT": "/invented-host-sdk",
        "DEVELOPER_DIR": "/invented-host-developer"}
    command = [str(producer), "index", "--workspace", str(checkout), "--output", str(scratch / "out/index.scip"),
               "--config", str(scratch / "prepared.json"), "--threads", "4"]
    assert expectation != "--cargo-proof", "native qualification uses the actual producer"
    source_before = tree_hashes(checkout)
    (scratch / "inputs.json").write_text(json.dumps({"command": command, "environment": environment,
        "original_source": original_source, "original_tools": original_tools, "source": source_before}, indent=2) + "\n")
    with (scratch / "stdout.log").open("wb") as out, (scratch / "stderr.log").open("wb") as err:
        child = subprocess.Popen(command, cwd=checkout, env=environment, stdin=subprocess.DEVNULL,
                                 stdout=out, stderr=err, start_new_session=True)
        try:
            rc = child.wait(timeout=120)
        except subprocess.TimeoutExpired:
            rc = 124
        finally:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
    (scratch / "producer.exit").write_text(str(rc) + "\n")
    assert original_source == tree_hashes(source), "original fixture changed"
    assert source_before == tree_hashes(checkout), "disposable checkout changed"
    assert all(digest(Path(p)) == value for p, value in original_tools.items()), "host tool changed"
    assert not shadow_marker.exists(), "shadow backend executed"
    output = scratch / "out/index.scip"
    assert not (scratch / "stdout.log").stat().st_size, "producer stdout must stay empty"
    if expectation == "--expect-scip":
        assert rc == 0 and output.is_file() and output.stat().st_size, (rc, (scratch / "stderr.log").read_text())
    else:
        expected = {"metadata-failure": "semantic metadata loading failed", "build-failure": "semantic build loading failed",
            "shared-root": "unsupported shared-source Cargo target root", "missing-server": "proc-macro server resource is unavailable",
            "missing-sysroot": "matching rust-src resources are unavailable",
            "sysroot-metadata-failure": "semantic sysroot metadata loading failed",
            "sysroot-manifest-failure": "semantic sysroot metadata loading failed"}[fixture]
        assert rc == 1 and not output.exists() and expected in (scratch / "stderr.log").read_text(), (rc, expected)
    if fixture in ("workspace", "shadow-backend"):
        assert any((scratch / "target").rglob("build-script-ran.txt"))
        assert (scratch / "tmp/proc-macro-ran.txt").is_file()
    if fixture == "shadow-backend":
        assert any((scratch / "target").rglob("c-compiled.txt")), "real LTO C compile did not run"
    assert shutil.disk_usage(scratch).free >= 2_000_000_000
    print("qualification passed: " + expectation)


if __name__ == "__main__":
    main()
