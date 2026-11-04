#!/usr/bin/env python3
"""Stage authoritative Kaban inputs for an independently built Tart image."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="A new, absent build directory")
    args = parser.parse_args()
    runner = Path(__file__).resolve().parent
    repo = runner.parents[1]
    paths = (
        "mise.toml", "mise.lock", "rust-toolchain.toml",
        "tools/kaban-rust-scip/rust-toolchain.toml", ".cargo/config.toml",
        "scripts/ci/install-go.sh", "scripts/ci/install-scip-go.sh",
        "scripts/ci/preflight.py",
    )
    missing = [name for name in paths if not (repo / name).is_file()]
    if missing:
        parser.error("Missing checkout inputs: " + ", ".join(missing))
    # mkdir without exist_ok refuses existing files/directories, including symlinks.
    args.output.mkdir(parents=True)
    for name in ("kaban-tools.pkr.hcl", "provision-tools.sh"):
        shutil.copy2(runner / name, args.output / name)
    inputs = args.output / "inputs"
    inputs.mkdir()
    shutil.copy2(runner / "bootstrap-inputs.json", inputs / "bootstrap-inputs.json")
    hashes = {}
    for name in paths:
        target = inputs / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(repo / name, target)
        hashes[name] = hashlib.sha256(target.read_bytes()).hexdigest()
    (inputs / "source-hashes.json").write_text(json.dumps(hashes, indent=2) + "\n")
    print(args.output.resolve())


if __name__ == "__main__":
    main()
