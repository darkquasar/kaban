#!/usr/bin/env python3
"""Render requirements only when this run contains all four full-profile archives."""
import json
import os
from pathlib import Path
import sys
from package import BINARIES, FEATURES, TARGETS

root = Path(sys.argv[1])
print('Full-profile binaries: `kaban` and `kaban-index`, including PDF and Markdown splitting, with licence/notice texts.\n')
print('| Target | Runtime requirement |\n|---|---|')
expected_archives = set()
for target in TARGETS:
    metadata = json.loads((root / (target + '.json')).read_text())
    if metadata['commit'] != os.environ['GITHUB_SHA'] or 'v' + metadata['version'] != os.environ['GITHUB_REF_NAME']:
        raise ValueError('archive source/version differs from authorized tag')
    if (metadata['target'], metadata['features'], metadata['profile'], metadata['packages'],
            metadata['pdf'], metadata['markdown_splitting']) != (target, FEATURES, 'release', list(BINARIES), True, True):
        raise ValueError('archive does not provide the canonical full profile')
    archive = f"kaban-v{metadata['version']}-{target}.tar.gz"
    expected_archives.add(archive)
    if not (root/archive).is_file():
        raise ValueError('missing archive: ' + archive)
    if target == 'aarch64-apple-darwin':
        if metadata['notarized'] or metadata['publisher_identity']:
            raise ValueError('unsigned release metadata must not claim publisher identity/notarization')
        requirement = ('Apple Silicon macOS; load-command minimum ' + metadata['minimum_macos'] +
                       ', package tested on ' + metadata['tested_macos'] +
                       '; unsigned/ad-hoc, no Developer ID or notarization')
    elif metadata['static']:
        requirement = 'Static musl; no glibc dependency; executed natively on Linux x86-64'
    else:
        requirement = 'glibc >= ' + metadata['minimum_glibc']
    print(f'| `{target}` | {requirement} |')
if {p.name for p in root.glob('*.tar.gz')} != expected_archives:
    raise ValueError('release must contain exactly the four selected archives')
print('''
Download SHA256SUMS and the archive for your target. Verify its exact filename with
`shasum -a 256 -c` (macOS) or `sha256sum -c` (Linux) before extracting.
Install both executables together on PATH; see docs/developer/RELEASE.md for complete commands.
Each archive includes BUILD.json and RELEASE.json with exact features, source, binary hashes,
sizes, native runtime dependencies and signing status. No Cargo or PDF renderer is needed.

External Go/TypeScript/Rust indexing producers and their runtimes are separate explicit installs.
Rust producer qualification covers Linux ARM64 GNU and native macOS ARM64 with root-owned CLT.
Mac trusted-none is not Linux namespace isolation; explicit repository trust is required.
Linux x86-64 GNU runs generic functional CI; ARM64 GNU and x86-64 musl run native package smoke.
macOS runs focused native CI plus extracted package smoke, not the complete Linux feature matrix.

macOS downloads have no verified publisher identity/notarization. Ad-hoc signatures supply no
publisher trust. Browser quarantine may require explicit approval for these exact verified files;
see the narrow per-file guidance. Updates replace both binaries and preserve private data/config/keys;
older executables do not authorize a database schema downgrade.
''')
