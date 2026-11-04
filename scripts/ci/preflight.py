#!/usr/bin/env python3
"""Cheap prerequisites for ci.sh; no compilation or capability qualification."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


def incomplete(message):
    print(f"not-run: {message}\nRESULT: incomplete (prerequisite); exit 3", file=sys.stderr)
    raise SystemExit(3)


def executable(command):
    path = shutil.which(command)
    if not path:
        incomplete(f"{command!r} absent or not executable; see docs/developer/CI.md")
    print(f"tool: {command} => {path}", flush=True)
    return path


def probe(command, expected=None):
    # Suppress colour only for discovery; leave the gates' environment intact.
    result = subprocess.run(command, capture_output=True, text=True,
                            env={**os.environ, "CARGO_TERM_COLOR": "never", "RUSTUP_AUTO_INSTALL": "0"})
    if result.returncode:
        print(result.stdout, end="")
        print(result.stderr, end="", file=sys.stderr)
        print(f"FAILED (exit {result.returncode}): {' '.join(command)}", file=sys.stderr)
        raise SystemExit(result.returncode if result.returncode > 0 else 128 - result.returncode)
    output = re.sub(r"\x1b\[[0-9;]*m", "", result.stdout).strip()
    if expected and not (output == expected or output.startswith(expected + " ")):
        incomplete(f"{' '.join(command)} returned {output!r}; expected {expected}; use the pinned setup")
    return output


def main():
    if sys.version_info < (3, 11):
        incomplete("Python 3.11+ is needed to read TOML pins; install the Python version in mise.toml")
    import tomllib

    root = Path(__file__).resolve().parents[2]
    os.chdir(root)
    with (root / "mise.toml").open("rb") as stream:
        pins = tomllib.load(stream)["tools"]
    with (root / "rust-toolchain.toml").open("rb") as stream:
        rust = tomllib.load(stream)["toolchain"]["channel"]
    cargo = executable(os.environ.get("CARGO", "cargo"))
    probe([cargo, "--version"], f"cargo {rust}")
    probe([cargo, "nextest", "--version"], f"cargo-nextest {pins['aqua:nextest-rs/nextest/cargo-nextest']}")
    probe([cargo, "deny", "--version"], f"cargo-deny {pins['aqua:EmbarkStudios/cargo-deny']}")
    probe([cargo, "fmt", "--version"])
    probe([cargo, "clippy", "--version"])
    rustc = executable(os.environ.get("RUSTC", "rustc"))
    probe([rustc, "--version"], f"rustc {rust}")
    probe([sys.executable, "--version"], f"Python {pins['python']}")
    probe([executable("node"), "--version"], f"v{pins['node']}")
    for tool in ("git", "jq", "cc", "go", os.environ.get("SCIP_GO", "scip-go")):
        executable(tool)
    if probe(["uname", "-s"]) != "Linux":
        incomplete("comprehensive gates require Linux (GNU tools and native musl); use focused Cargo checks here")
    machine = probe(["uname", "-m"])
    target = f"{machine}-unknown-linux-musl"
    with (root / ".cargo/config.toml").open("rb") as stream:
        config = tomllib.load(stream)
    linker = os.environ.get(f"CARGO_TARGET_{target.upper().replace('-', '_')}_LINKER",
                            config.get("target", {}).get(target, {}).get("linker"))
    if not linker:
        incomplete(f"no configured musl linker for {target}")
    executable(linker)
    libdir = probe([rustc, "--print", "target-libdir", "--target", target])
    if not libdir or not Path(libdir).is_dir():
        incomplete(f"musl target absent ({target}); run rustup target add {target} for the pinned toolchain")
    if os.environ.get("CI_SKIP_SMOKE") != "1":
        executable("strace")
    print("prerequisites: available; runtime capability checks still apply")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError) as error:
        incomplete(str(error))
