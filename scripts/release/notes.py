#!/usr/bin/env python3
"""Render runtime requirements recorded from this run's three actual archives."""
import json
import os
from pathlib import Path
import sys

root = Path(sys.argv[1])
targets = ('x86_64-unknown-linux-gnu', 'x86_64-unknown-linux-musl', 'aarch64-unknown-linux-gnu')
print('Default-feature Linux binaries: `kaban` and `kaban-index`, with licence and notice texts.\n')
print('| Target | Runtime requirement |\n|---|---|')
for target in targets:
    metadata = json.loads((root / (target + '.json')).read_text())
    assert metadata['commit'] == os.environ['GITHUB_SHA'], 'archive commit differs from authorized tag'
    assert 'v' + metadata['version'] == os.environ['GITHUB_REF_NAME'], 'archive version differs from tag'
    assert metadata['target'] == target
    assert (root / f"kaban-v{metadata['version']}-{target}.tar.gz").is_file()
    requirement = ('Static musl; no glibc dependency' if metadata['static'] else
                   'glibc >= ' + metadata['minimum_glibc'])
    print(f'| `{target}` | {requirement} |')
print('''
Download SHA256SUMS alongside your chosen archive and verify with `sha256sum --ignore-missing -c SHA256SUMS`.
Extract the archive and put both executables on PATH.
Each archive includes `RELEASE.json` with binary hashes, sizes, runtime dependencies and source commit.

PDF is opt-in and absent from these downloads. External indexing producers are installed separately.
The Rust semantic producer currently advertises only Linux ARM64 GNU; these downloads do not include it.
GNU x86-64 has full main CI coverage. ARM64 and musl packages have native build and executable/protocol
smoke coverage; the full functional suite does not run on those configurations.
''')
