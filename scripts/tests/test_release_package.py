"""Package contract over invented binaries and native tool output (no compiler)."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts/release/package.py'
FEATURES = ['default', 'kaban-ingest/pdf', 'kaban-ingest/md-splitter']


class ReleasePackage(unittest.TestCase):
    def test_archive_preserves_exact_executables_licenses_and_measured_glibc(self):
        self.check_package('x86_64-unknown-linux-gnu')

    def test_darwin_full_profile_records_actual_runtime_and_ad_hoc_signatures(self):
        self.check_package('aarch64-apple-darwin')

    def test_unsupported_targets_are_refused_before_build_or_archive(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for target in ('x86_64-apple-darwin', 'x86_64-pc-windows-msvc'):
                package = subprocess.run([sys.executable, str(SCRIPT), '--stage', str(root),
                    '--target', target, '--version', '1.0.0', '--commit', 'a'*40,
                    '--output', str(root/'dist')], capture_output=True, text=True)
                build = subprocess.run(['bash', str(ROOT/'scripts/release/build.sh'), target],
                    capture_output=True, text=True)
                self.assertEqual(package.returncode, 2, package.stderr)
                self.assertEqual(build.returncode, 2, build.stderr)
                self.assertFalse((root/'dist').exists())

    def test_musl_with_dynamic_dependency_is_refused(self):
        self.check_package('x86_64-unknown-linux-musl', dynamic=True)

    def test_static_musl_is_packaged_independently(self):
        self.check_package('x86_64-unknown-linux-musl')

    def test_gnu_kaban_size_budget_accepts_below_40_mib_and_refuses_at_boundary(self):
        for size in (40 * 1024 * 1024 - 1, 40 * 1024 * 1024, 40 * 1024 * 1024 + 1):
            with self.subTest(size=size):
                self.check_package('x86_64-unknown-linux-gnu', kaban_size=size,
                                   refused='size budget' if size >= 40 * 1024 * 1024 else '')

    def test_package_refuses_changed_missing_extra_or_nonexecutable_inputs(self):
        for scenario, reason in (('changed', 'changed'), ('missing', 'members'),
                                 ('extra', 'members'), ('mode', 'executable'),
                                 ('features', 'profile'), ('notices', 'notice')):
            with self.subTest(scenario=scenario):
                self.check_package('x86_64-unknown-linux-gnu', scenario=scenario, refused=reason)

    def test_release_notes_require_all_four_full_profile_packages(self):
        targets = ('x86_64-unknown-linux-gnu', 'x86_64-unknown-linux-musl',
                   'aarch64-unknown-linux-gnu', 'aarch64-apple-darwin')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for target in targets:
                metadata = {'commit': 'a'*40, 'version': '1.0.0-test', 'target': target,
                            'features': FEATURES, 'pdf': True, 'markdown_splitting': True,
                            'packages': ['kaban', 'kaban-index'], 'profile': 'release',
                            'static': target.endswith('musl'), 'minimum_glibc': '2.39',
                            'minimum_macos': '15.0', 'tested_macos': '15.8.2',
                            'notarized': False, 'publisher_identity': False}
                (root/(target+'.json')).write_text(json.dumps(metadata))
                (root/f'kaban-v1.0.0-test-{target}.tar.gz').write_bytes(b'invented archive')
            def notes():
                return subprocess.run([sys.executable, str(ROOT/'scripts/release/notes.py'), str(root)],
                    env={**os.environ, 'GITHUB_SHA': 'a'*40, 'GITHUB_REF_NAME': 'v1.0.0-test'},
                    capture_output=True, text=True)
            result = notes()
            self.assertEqual(result.returncode, 0, result.stderr)
            with self.subTest('complete four-target notes'):
                self.assertIn('aarch64-apple-darwin', result.stdout)
            self.assertIn('PDF', result.stdout)
            (root/'aarch64-apple-darwin.json').unlink()
            self.assertNotEqual(notes().returncode, 0)

    def check_package(self, target, dynamic=False, kaban_size=None, refused='', scenario=''):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            stage = root / ('kaban-v1.0.0-test-' + target)
            stage.mkdir()
            for name in ('kaban', 'kaban-index'):
                file = stage / name
                file.write_bytes(b'invented executable: ' + name.encode())
                file.chmod(0o755)
                if name == 'kaban' and kaban_size is not None:
                    with file.open('r+b') as stream:
                        stream.truncate(kaban_size)
            (stage / 'LICENSE').write_text('project license')
            (stage / 'NOTICE').write_text('project notice')
            third = stage / 'third-party'
            third.mkdir()
            (third / 'THIRD_PARTY_LICENSES.txt').write_text('upstream terms')
            (third / 'RUST_STANDARD_LIBRARY_NOTICES.html').write_text('runtime terms')
            manifest = {'schema': 1, 'target': target, 'packages': ['kaban', 'kaban-index'],
                        'features': FEATURES, 'dependency_edges': ['normal', 'build'],
                        'files': {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in third.iterdir()}}
            (third / 'manifest.json').write_text(json.dumps(manifest))
            cargo_args = ['build', '--release', '--locked', '-p', 'kaban', '-p', 'kaban-index',
                          '--features', ','.join(FEATURES[1:])]
            if target.endswith('-musl'):
                cargo_args += ['--target', target]
            build = {'target': target, 'version': '1.0.0-test', 'commit': 'a' * 40,
                     'features': FEATURES, 'cargo_args': cargo_args,
                     'binaries': {n: {'sha256': hashlib.sha256((stage/n).read_bytes()).hexdigest(),
                                      'mode': 0o755} for n in ('kaban', 'kaban-index')}}
            (stage / 'BUILD.json').write_text(json.dumps(build))
            if scenario == 'changed':
                (stage/'kaban').write_bytes(b'changed after staging')
            elif scenario == 'missing':
                (stage/'kaban-index').unlink()
            elif scenario == 'extra':
                (stage/'producer').write_bytes(b'not an application')
            elif scenario == 'mode':
                (stage/'kaban').chmod(0o644)
            elif scenario == 'features':
                build['features'] = ['default']
                (stage/'BUILD.json').write_text(json.dumps(build))
            elif scenario == 'notices':
                (third/'THIRD_PARTY_LICENSES.txt').write_text('damaged')
            tool = root/'native-tool'
            tool.write_text(f'#!{sys.executable}\nimport os,sys\nfrom pathlib import Path\n'
                'name=Path(sys.argv[0]).name\n'
                'target=os.environ["PACKAGE_TARGET"]\n'
                'if name == "readelf":\n'
                ' print("" if target.endswith("musl") and os.environ["DYNAMIC"] == "0" else '
                '"Name: GLIBC_2.9 Name: GLIBC_2.39" if "--version-info" in sys.argv else "(NEEDED) Shared library: [libc.so.6]")\n'
                'elif name == "file": print("Mach-O 64-bit executable arm64" if "apple" in target else "ELF 64-bit LSB executable, " + ("ARM aarch64" if target.startswith("aarch64") else "x86-64"))\n'
                'elif name == "otool": print("fixture:\\n\\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1351.0.0)" if "-L" in sys.argv else "cmd LC_BUILD_VERSION\\n cmdsize 32\\n platform 1\\n minos 15.0\\n sdk 15.5")\n'
                'elif name == "sw_vers": print("15.8.2")\n'
                'elif name == "codesign": print("Signature=adhoc\\nTeamIdentifier=not set", file=sys.stderr)\n')
            tool.chmod(0o755)
            for name in ('readelf', 'file', 'otool', 'codesign', 'sw_vers'):
                (root/name).symlink_to(tool)
            output = root / 'dist'
            result = subprocess.run([sys.executable, str(SCRIPT), '--stage', str(stage),
                '--target', target, '--version', '1.0.0-test', '--commit', 'a' * 40,
                '--output', str(output)], capture_output=True, text=True,
                env={**os.environ, 'PATH': str(root) + os.pathsep + os.environ['PATH'],
                     'PACKAGE_TARGET': target, 'DYNAMIC': str(int(dynamic))})
            if refused or dynamic:
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(refused or 'static', result.stderr)
                self.assertFalse(list(output.glob('*.tar.gz')))
                return
            self.assertEqual(result.returncode, 0, result.stderr)
            archive = next(output.glob('*.tar.gz'))
            with tarfile.open(archive) as tar:
                self.assertEqual(tar.extractfile(stage.name + '/kaban').read(), (stage / 'kaban').read_bytes())
                self.assertEqual(tar.getmember(stage.name + '/kaban').mode & 0o777, 0o755)
                self.assertEqual(tar.extractfile(stage.name + '/third-party/THIRD_PARTY_LICENSES.txt').read(), b'upstream terms')
                metadata = json.load(tar.extractfile(stage.name + '/RELEASE.json'))
            self.assertEqual(metadata['features'], FEATURES)
            self.assertTrue(metadata['pdf'])
            self.assertTrue(metadata['markdown_splitting'])
            if 'apple' in target:
                self.assertEqual(metadata['minimum_macos'], '15.0')
                self.assertEqual(metadata['tested_macos'], '15.8.2')
                self.assertEqual(metadata['binaries']['kaban']['signature'], 'ad-hoc')
                self.assertFalse(metadata['notarized'])
                self.assertEqual(metadata['binaries']['kaban']['needed'], ['/usr/lib/libSystem.B.dylib'])
            else:
                self.assertEqual(metadata['minimum_glibc'], None if target.endswith('musl') else '2.39')
            self.assertEqual(metadata['binaries']['kaban']['sha256'], hashlib.sha256((stage / 'kaban').read_bytes()).hexdigest())
            self.assertEqual((output / (archive.name + '.sha256')).read_text(),
                hashlib.sha256(archive.read_bytes()).hexdigest() + '  ' + archive.name + '\n')


if __name__ == '__main__':
    unittest.main()
