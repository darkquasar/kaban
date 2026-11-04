"""Package contract over invented binaries and ELF metadata (no Rust compiler)."""
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


class ReleasePackage(unittest.TestCase):
    def test_archive_preserves_exact_executables_licenses_and_measured_glibc(self):
        self.check_package('x86_64-unknown-linux-gnu')

    def test_musl_with_dynamic_dependency_is_refused(self):
        self.check_package('x86_64-unknown-linux-musl', dynamic=True)

    def check_package(self, target, dynamic=False):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            stage = root / 'kaban-v1.0.0-test'
            stage.mkdir()
            for name in ('kaban', 'kaban-index'):
                file = stage / name
                file.write_bytes(b'invented executable: ' + name.encode())
                file.chmod(0o755)
            (stage / 'LICENSE').write_text('project license')
            (stage / 'NOTICE').write_text('project notice')
            (stage / 'third-party').mkdir()
            (stage / 'third-party/THIRD_PARTY_LICENSES.txt').write_text('upstream terms')
            readelf = root / 'readelf'
            readelf.write_text(f'#!{sys.executable}\nimport sys\n'
                "print('Name: GLIBC_2.9 Name: GLIBC_2.39' if '--version-info' in sys.argv else "
                "'(NEEDED) Shared library: [libc.so.6]')\n")
            readelf.chmod(0o755)
            output = root / 'dist'
            result = subprocess.run([sys.executable, str(SCRIPT), '--stage', str(stage),
                '--target', target, '--version', '1.0.0-test', '--commit', 'a' * 40,
                '--output', str(output)], capture_output=True, text=True,
                env={**os.environ, 'PATH': str(root) + os.pathsep + os.environ['PATH']})
            if dynamic:
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('static', result.stderr)
                self.assertFalse(list(output.glob('*.tar.gz')))
                return
            self.assertEqual(result.returncode, 0, result.stderr)
            archive = next(output.glob('*.tar.gz'))
            with tarfile.open(archive) as tar:
                self.assertEqual(tar.extractfile(stage.name + '/kaban').read(), (stage / 'kaban').read_bytes())
                self.assertEqual(tar.getmember(stage.name + '/kaban').mode & 0o111, 0o111)
                self.assertEqual(tar.extractfile(stage.name + '/third-party/THIRD_PARTY_LICENSES.txt').read(), b'upstream terms')
                metadata = json.load(tar.extractfile(stage.name + '/RELEASE.json'))
            self.assertEqual(metadata['minimum_glibc'], '2.39')
            self.assertEqual(metadata['binaries']['kaban']['sha256'], hashlib.sha256((stage / 'kaban').read_bytes()).hexdigest())
            self.assertEqual((output / (archive.name + '.sha256')).read_text(),
                hashlib.sha256(archive.read_bytes()).hexdigest() + '  ' + archive.name + '\n')


if __name__ == '__main__':
    unittest.main()
