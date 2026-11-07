#!/usr/bin/env python3
"""Archive already checked bytes and record actual ELF/runtime requirements."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tarfile


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def package(args):
    stage, output = args.stage, args.output
    binaries, versions = {}, set()
    musl = args.target.endswith('-musl')
    for name in ('kaban', 'kaban-index'):
        path = stage / name
        size = path.stat().st_size
        if (not musl and name == 'kaban' and size >= 40 * 1024 * 1024) or (musl and name == 'kaban-index' and size >= 12_000_000):
            raise ValueError(f'{name} exceeds its release size budget: {size} bytes')
        dynamic = subprocess.check_output(['readelf', '-d', str(path)], text=True)
        segments = subprocess.check_output(['readelf', '-l', str(path)], text=True)
        symbols = subprocess.check_output(['readelf', '--version-info', str(path)], text=True)
        needed = re.findall(r'\(NEEDED\).*?\[([^]]+)\]', dynamic)
        required = set(re.findall(r'GLIBC_(\d+(?:\.\d+)+)', symbols))
        if musl and (needed or 'INTERP' in segments or required):
            raise ValueError(f'{name}: musl archive must contain static executables')
        if not musl and not required:
            raise ValueError(f'{name}: cannot determine GNU binary glibc requirement')
        versions.update(required)
        binaries[name] = {'bytes': size, 'sha256': sha256(path), 'needed': needed}
    floor = max(versions, key=lambda v: tuple(map(int, v.split('.')))) if versions else None
    metadata = {'version': args.version, 'commit': args.commit, 'target': args.target,
                'profile': 'release', 'packages': ['kaban', 'kaban-index'], 'features': 'default',
                'pdf': False, 'external_producers_bundled': False,
                'qualification': 'native build and package smoke; full functional CI runs on x86_64 GNU',
                'minimum_glibc': floor, 'static': musl, 'binaries': binaries}
    (stage / 'RELEASE.json').write_text(json.dumps(metadata, indent=2, sort_keys=True) + '\n')
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f'kaban-v{args.version}-{args.target}.tar.gz'
    with tarfile.open(archive, 'w:gz') as stream:
        stream.add(stage, arcname=stage.name)
    (output / (archive.name + '.sha256')).write_text(sha256(archive) + '  ' + archive.name + '\n')
    (output / (args.target + '.json')).write_text(json.dumps(metadata, indent=2, sort_keys=True) + '\n')
    print(f'{archive}: minimum glibc {floor or "none (static musl)"}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--stage', type=Path, required=True)
    parser.add_argument('--target', required=True, choices=('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-musl'))
    parser.add_argument('--version', required=True)
    parser.add_argument('--commit', required=True)
    parser.add_argument('--output', type=Path, required=True)
    package(parser.parse_args())
