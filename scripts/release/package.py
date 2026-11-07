#!/usr/bin/env python3
"""Archive the canonical full-profile pair and record native runtime requirements."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tarfile
import stat
import tempfile

TARGETS = ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu',
           'x86_64-unknown-linux-musl', 'aarch64-apple-darwin')
FEATURES = ['default', 'kaban-ingest/pdf', 'kaban-ingest/md-splitter']
BINARIES = ('kaban', 'kaban-index')


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def native_output(*args):
    return subprocess.check_output(args, text=True)


def darwin_runtime(path):
    description = native_output('file', '-b', str(path))
    if 'Mach-O 64-bit executable arm64' not in description:
        raise ValueError(f'{path.name}: expected native ARM64 Mach-O executable')
    needed = re.findall(r'^\s+(\S+) \(compatibility version', native_output('otool', '-L', str(path)), re.M)
    if not needed or any(not p.startswith(('/usr/lib/', '/System/Library/')) for p in needed):
        raise ValueError(f'{path.name}: unexpected non-system dylib requirements: {needed}')
    commands = native_output('otool', '-l', str(path))
    build = re.search(r'cmd LC_BUILD_VERSION\s+cmdsize \d+\s+platform 1\s+minos ([\d.]+)\s+sdk ([\d.]+)', commands)
    if build is None:
        raise ValueError(f'{path.name}: cannot determine macOS deployment minimum')
    signature = subprocess.run(['codesign', '-dvv', str(path)], capture_output=True, text=True)
    if signature.returncode == 0 and 'Signature=adhoc' in signature.stderr:
        subprocess.run(['codesign', '--verify', '--strict', str(path)], check=True)
        status = 'ad-hoc'
    elif signature.returncode != 0 and 'not signed at all' in signature.stderr:
        status = 'unsigned'
    else:
        raise ValueError(f'{path.name}: unexpected or unreadable signing identity')
    return {'needed': needed, 'minimum_macos': build[1], 'sdk': build[2], 'signature': status}


def checked_stage(stage, args):
    expected = {'kaban', 'kaban-index', 'LICENSE', 'NOTICE', 'third-party', 'BUILD.json'}
    if {p.name for p in stage.iterdir()} != expected:
        raise ValueError('package members must be the application pair and required support files')
    for path in stage.rglob('*'):
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise ValueError(f'package members must be regular files/directories: {path.name}')
    build = json.loads((stage/'BUILD.json').read_text())
    cargo_args = ['build', '--release', '--locked', '-p', 'kaban', '-p', 'kaban-index',
                  '--features', ','.join(FEATURES[1:])]
    if args.target.endswith('-musl'):
        cargo_args += ['--target', args.target]
    if (build.get('target'), build.get('version'), build.get('commit'), build.get('features'), build.get('cargo_args')) != (
            args.target, args.version, args.commit, FEATURES, cargo_args):
        raise ValueError('staged build profile/source does not match canonical release configuration')
    if set(build.get('binaries', {})) != set(BINARIES):
        raise ValueError('staged executable inventory must contain exactly the application pair')
    for name in BINARIES:
        path = stage/name
        if not path.is_file() or stat.S_IMODE(path.stat().st_mode) != 0o755:
            raise ValueError(f'{name}: expected a regular executable with mode 0755')
        if build['binaries'][name] != {'sha256': sha256(path), 'mode': 0o755}:
            raise ValueError(f'{name}: executable changed after staging')
    third = stage/'third-party'
    manifest = json.loads((third/'manifest.json').read_text())
    if (manifest.get('schema'), manifest.get('target'), manifest.get('packages'),
            manifest.get('features'), manifest.get('dependency_edges')) != (
                1, args.target, list(BINARIES), FEATURES, ['normal', 'build']):
        raise ValueError('staged notice profile does not match the release')
    if set(p.name for p in third.iterdir()) != {'manifest.json', 'THIRD_PARTY_LICENSES.txt', 'RUST_STANDARD_LIBRARY_NOTICES.html'}:
        raise ValueError('unexpected or missing notice members')
    if {p.name: sha256(p) for p in third.iterdir() if p.name != 'manifest.json'} != manifest['files']:
        raise ValueError('staged notice hashes changed')


def verify_extracted(stage, extracted):
    expected = {p.relative_to(stage): (sha256(p), stat.S_IMODE(p.stat().st_mode))
                for p in stage.rglob('*') if p.is_file()}
    actual = {p.relative_to(extracted): (sha256(p), stat.S_IMODE(p.stat().st_mode))
              for p in extracted.rglob('*') if p.is_file()}
    if actual != expected:
        raise ValueError('extracted archive differs from checked files/modes')


def package(args):
    stage, output = args.stage, args.output
    checked_stage(stage, args)
    binaries, versions = {}, set()
    musl, darwin = args.target.endswith('-musl'), args.target == 'aarch64-apple-darwin'
    for name in BINARIES:
        path = stage / name
        size = path.stat().st_size
        if (not musl and name == 'kaban' and size >= 40 * 1024 * 1024) or (musl and name == 'kaban-index' and size >= 12_000_000):
            raise ValueError(f'{name} exceeds its release size budget: {size} bytes')
        if darwin:
            runtime = darwin_runtime(path)
        else:
            description = native_output('file', '-b', str(path))
            architecture = 'ARM aarch64' if args.target.startswith('aarch64') else 'x86-64'
            if 'ELF 64-bit' not in description or 'executable' not in description or architecture not in description:
                raise ValueError(f'{name}: expected native {args.target} ELF executable')
            dynamic = native_output('readelf', '-d', str(path))
            segments = native_output('readelf', '-l', str(path))
            symbols = native_output('readelf', '--version-info', str(path))
            needed = re.findall(r'\(NEEDED\).*?\[([^]]+)\]', dynamic)
            required = set(re.findall(r'GLIBC_(\d+(?:\.\d+)+)', symbols))
            if musl and (needed or 'INTERP' in segments or required):
                raise ValueError(f'{name}: musl archive must contain static executables')
            if not musl and not required:
                raise ValueError(f'{name}: cannot determine GNU binary glibc requirement')
            versions.update(required)
            runtime = {'needed': needed}
        binaries[name] = {'bytes': size, 'sha256': sha256(path), **runtime}
    floor = max(versions, key=lambda v: tuple(map(int, v.split('.')))) if versions else None
    if floor and tuple(map(int, floor.split('.'))) > (2, 39):
        raise ValueError(f'GNU release exceeds the glibc 2.39 runtime baseline: {floor}')
    metadata = {'version': args.version, 'commit': args.commit, 'target': args.target,
                'profile': 'release', 'packages': list(BINARIES), 'features': FEATURES,
                'pdf': True, 'markdown_splitting': True, 'external_producers_bundled': False,
                'qualification': 'native packaged CLI/MCP and Markdown/PDF smoke; generic functional CI is Linux x86_64 GNU',
                'minimum_glibc': floor, 'static': musl, 'binaries': binaries,
                'notarized': False, 'publisher_identity': False}
    if darwin:
        metadata.update(minimum_macos=max((b['minimum_macos'] for b in binaries.values()), key=lambda v: tuple(map(int, v.split('.')))),
                        tested_macos=native_output('sw_vers', '-productVersion').strip())
    (stage / 'RELEASE.json').write_text(json.dumps(metadata, indent=2, sort_keys=True) + '\n')
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f'kaban-v{args.version}-{args.target}.tar.gz'
    with tarfile.open(archive, 'w:gz') as stream:
        stream.add(stage, arcname=stage.name)
    with tempfile.TemporaryDirectory() as temporary:
        extracted_root = args.extract or Path(temporary)
        with tarfile.open(archive) as stream:
            stream.extractall(extracted_root, filter='data')
        verify_extracted(stage, extracted_root/stage.name)
    (output / (archive.name + '.sha256')).write_text(sha256(archive) + '  ' + archive.name + '\n')
    (output / (args.target + '.json')).write_text(json.dumps(metadata, indent=2, sort_keys=True) + '\n')
    print(f'{archive}: runtime metadata recorded for {args.target}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--stage', type=Path, required=True)
    parser.add_argument('--target', required=True, choices=TARGETS)
    parser.add_argument('--version', required=True)
    parser.add_argument('--commit', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--extract', type=Path, help='retain the verified extracted archive for native smoke')
    package(parser.parse_args())
