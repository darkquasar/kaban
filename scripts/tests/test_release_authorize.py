"""Release authorization through its CLI, using real Git and a fake API boundary."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts/release/authorize.py'


class ReleaseAuthorization(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git('init', '-q', '-b', 'main')
        (self.root / 'Cargo.toml').write_text('[workspace.package]\nversion="3.0.0-alpha.1"\n')
        self.git('add', 'Cargo.toml')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                 '-c', 'core.hooksPath=/dev/null', 'commit', '-qm', 'base')
        self.sha = self.git('rev-parse', 'HEAD')
        self.git('update-ref', 'refs/remotes/origin/main', self.sha)
        self.git('tag', 'v3.0.0-alpha.1')
        fake = self.root / 'bin'
        fake.mkdir()
        gh = fake / 'gh'
        gh.write_text(f'#!{sys.executable}\nimport os\nprint(os.environ["API_RESPONSE"])\n')
        gh.chmod(0o755)
        self.env = {**os.environ, 'PATH': str(fake) + os.pathsep + os.environ['PATH'],
                    'GITHUB_REPOSITORY': 'darkquasar/kaban', 'GITHUB_EVENT_NAME': 'push',
                    'GITHUB_REF': 'refs/tags/v3.0.0-alpha.1', 'GITHUB_ACTOR': 'darkquasar',
                    'GITHUB_ACTOR_ID': '28435534', 'GITHUB_TRIGGERING_ACTOR': 'darkquasar',
                    'GITHUB_SHA': self.sha}
        self.run_record = dict(head_sha=self.sha, head_branch='main', event='push',
                               status='completed', conclusion='success', path='.github/workflows/ci.yml')

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.root, text=True,
                                       stderr=subprocess.DEVNULL).strip()

    def run_auth(self, *, records=None, **env):
        response = json.dumps({'workflow_runs': [self.run_record] if records is None else records})
        return subprocess.run([sys.executable, str(SCRIPT)], cwd=self.root,
            env={**self.env, 'API_RESPONSE': response, **env}, capture_output=True, text=True)

    def test_owner_tag_with_exact_successful_main_push_is_authorized(self):
        result = self.run_auth()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(self.sha, result.stdout)

    def test_wrong_actor_repository_event_and_ref_are_refused(self):
        for key, value in [('GITHUB_ACTOR', 'someone'), ('GITHUB_ACTOR_ID', '1'),
                           ('GITHUB_TRIGGERING_ACTOR', 'someone'), ('GITHUB_REPOSITORY', 'other/kaban'),
                           ('GITHUB_EVENT_NAME', 'pull_request'), ('GITHUB_REF', 'refs/heads/main')]:
            with self.subTest(key=key):
                result = self.run_auth(**{key: value})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('release refused:', result.stderr)

    def test_tag_must_match_cargo_version_and_checked_out_commit(self):
        result = self.run_auth(GITHUB_REF='refs/tags/v9.0.0')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Cargo', result.stderr)
        result = self.run_auth(GITHUB_SHA='a' * 40)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('commit', result.stderr)

    def test_commit_outside_main_history_is_refused(self):
        self.git('checkout', '--orphan', 'unrelated')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                 '-c', 'core.hooksPath=/dev/null', 'commit', '-qm', 'unrelated')
        sha = self.git('rev-parse', 'HEAD')
        self.git('tag', '-f', 'v3.0.0-alpha.1')
        result = self.run_auth(GITHUB_SHA=sha)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('main', result.stderr)

    def test_ci_must_be_completed_success_for_exact_main_push(self):
        cases = [[]] + [[{**self.run_record, field: value}] for field, value in (
            ('head_sha', 'a' * 40), ('head_branch', 'topic'), ('event', 'pull_request'),
            ('status', 'in_progress'), ('conclusion', 'failure'), ('path', '.github/workflows/other.yml'))]
        for records in cases:
            with self.subTest(records=records):
                result = self.run_auth(records=records)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('successful main push CI', result.stderr)


if __name__ == '__main__':
    unittest.main()
