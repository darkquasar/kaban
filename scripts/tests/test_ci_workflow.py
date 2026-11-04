"""Run the workflow's public shell steps with invented events and native results.

Only extract literal run blocks by step name; GitHub parses the YAML/expressions.
These fixtures need no YAML library, network, Rust tools or GitHub credentials.
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[2]


def run_block(name):
    source = (ROOT / '.github/workflows/ci.yml').read_text()
    lines = source.splitlines()
    for index, line in enumerate(lines):
        if line.strip() == '- name: ' + name:
            for offset in range(index + 1, len(lines)):
                match = re.match(r'^(\s*)run: (.*)$', lines[offset])
                if match:
                    indent, value = match.groups()
                    if value != '|':
                        return value + '\n'
                    end = offset + 1
                    while end < len(lines) and (not lines[end].strip() or
                                               len(lines[end]) - len(lines[end].lstrip()) > len(indent)):
                        end += 1
                    return textwrap.dedent('\n'.join(lines[offset + 1:end])) + '\n'
    raise AssertionError('No literal run step named ' + name)


class WorkflowResults(unittest.TestCase):
    def execute(self, name, *, needs=None, event=None, event_name='pull_request', ref='refs/pull/7/merge', default='main', changed_path=None, base_present=True):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            event = dict(event or {})
            if changed_path is not None:
                def git(*args):
                    return subprocess.check_output(['git', *args], cwd=root, text=True, stderr=subprocess.DEVNULL).strip()
                git('init', '-q')
                git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                    '-c', 'core.hooksPath=/dev/null', 'commit', '--allow-empty', '-qm', 'base')
                base = git('rev-parse', 'HEAD')
                changed = root / changed_path
                changed.parent.mkdir(parents=True, exist_ok=True)
                changed.write_text('invented diff\n')
                git('add', '--', changed_path)
                git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                    '-c', 'core.hooksPath=/dev/null', 'commit', '-qm', 'change')
                base = base if base_present else 'f' * 40
                if event_name == 'pull_request': event['pull_request'] = {'base': {'sha': base}}
                elif event_name == 'push': event['before'] = base
            (root / 'event.json').write_text(json.dumps(event))
            (root / 'scripts/ci').mkdir(parents=True)
            (root / 'scripts/ci/ci.sh').write_text('exit 0\n')
            output = root / 'output'
            summary = root / 'summary'
            output.touch(); summary.touch()
            env = {**os.environ, 'NEEDS_JSON': json.dumps(needs or {}),
                   'GITHUB_EVENT_PATH': str(root / 'event.json'), 'GITHUB_EVENT_NAME': event_name,
                   'GITHUB_REF': ref, 'GITHUB_RUN_ID': '123', 'DEFAULT_BRANCH': default,
                   'GITHUB_OUTPUT': str(output), 'GITHUB_STEP_SUMMARY': str(summary)}
            result = subprocess.run([shutil.which('bash'), '-e', '-o', 'pipefail', '-c', run_block(name)],
                                    cwd=root, env=env, text=True, capture_output=True)
            return result, dict(line.split('=', 1) for line in output.read_text().splitlines()), summary.read_text()

    def required(self, selected='true'):
        needs = {job: {'result': 'success'} for job in ('selection', 'quality', 'functional', 'pdf', 'gnu', 'musl', 'script-tests', 'producer')}
        needs['selection']['outputs'] = {'producer': selected}
        return needs

    def test_pdf_failure_is_required_for_readiness(self):
        needs = self.required()
        needs['pdf'] = {'result': 'failure'}
        result, _, summary = self.execute('Run gates', needs=needs)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr + summary)
        self.assertIn('pdf: failure', summary)

    def test_failed_required_job_cannot_be_replaced_by_a_successful_ci_child(self):
        needs = self.required()
        needs['functional']['result'] = 'failure'
        result, _, summary = self.execute('Run gates', needs=needs)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr + summary)
        self.assertIn('functional: failure', summary)

    def test_readiness_requires_every_result_and_only_explicit_producer_skip(self):
        for job in self.required():
            for result in ('failure', 'cancelled', 'skipped', 'missing'):
                with self.subTest(job=job, result=result):
                    needs = self.required()
                    if result == 'missing': del needs[job]
                    else: needs[job]['result'] = result
                    run, _, _ = self.execute('Run gates', needs=needs)
                    self.assertEqual(run.returncode, 1, run.stderr)
        for selected in ('true', 'false', 'unknown'):
            for producer in ('success', 'skipped', 'failure', 'cancelled'):
                with self.subTest(selected=selected, producer=producer):
                    needs = self.required(selected)
                    needs['producer']['result'] = producer
                    run, _, _ = self.execute('Run gates', needs=needs)
                    accepted = (selected == 'true' and producer == 'success') or (selected == 'false' and producer == 'skipped')
                    self.assertEqual(run.returncode, 0 if accepted else 1, run.stderr)

    def test_producer_selection_uses_real_changed_paths_and_unknowns_select(self):
        for event in ('pull_request', 'push'):
            for path, expected in (('docs/guide.md', 'false'), ('README.md', 'false'),
                    ('tools/kaban-rust-scip/src/lib.rs', 'true'), ('crates/kaban-core/Cargo.toml', 'true'),
                    ('fixtures/protocol.json', 'true'), ('scripts/driver.sh', 'true'),
                    ('mise.lock', 'true'), ('unknown.txt', 'true')):
                with self.subTest(event=event, path=path):
                    run, output, _ = self.execute('Select producer and cache scope',
                        event={'number': 7}, event_name=event, changed_path=path)
                    self.assertEqual(run.returncode, 0, run.stderr)
                    self.assertEqual(output['producer'], expected)
        run, output, _ = self.execute('Select producer and cache scope',
            changed_path='docs/guide.md', base_present=False)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(output['producer'], 'true')

    def test_pr_can_restore_compatible_default_branch_dependency_keys(self):
        _, main, _ = self.execute('Select producer and cache scope', event_name='push', ref='refs/heads/main')
        _, pr, _ = self.execute('Select producer and cache scope', event={'number': 7})
        source = (ROOT / '.github/workflows/ci.yml').read_text()
        keys = []
        for group in ('quality', 'functional', 'pdf', 'gnu', 'musl', 'producer'):
            # Scope the public action input to this job (not a cache-service emulator).
            body = source.split('\n  ' + group + ':\n', 1)[1]
            body = re.split(r'\n  [a-z][a-z-]*:\n', body, maxsplit=1)[0]
            prefix = re.search(r'^          prefix-key: (.+)$', body, re.MULTILINE).group(1)
            def resolve(outputs):
                return prefix.replace('${{ needs.selection.outputs.cache-prefix }}', outputs.get('cache-prefix', ''))
            self.assertEqual(resolve(main), resolve(pr), 'A new PR must be able to reuse a compatible default-branch key')
            keys.append(resolve(main))
        self.assertEqual(len(set(keys)), 6)  # incompatible group targets still stay distinct

    def test_cache_writes_follow_event_and_default_branch_trust(self):
        for event, ref, payload, save in (
            ('pull_request', 'refs/pull/7/merge', {'number': 7}, 'true'),
            ('push', 'refs/heads/main', {}, 'true'),
            ('workflow_dispatch', 'refs/heads/main', {}, 'true'),
            ('workflow_dispatch', 'refs/heads/topic', {}, 'false'),
            ('push', 'refs/heads/topic', {}, 'false'),
            ('merge_group', 'refs/heads/gh-readonly-queue/main/pr-7', {}, 'false'),
        ):
            with self.subTest(event=event, ref=ref):
                run, output, _ = self.execute('Select producer and cache scope', event=payload, event_name=event, ref=ref)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertEqual(output['cache-save'], save)
                self.assertEqual(output['producer'], 'true')  # unknown event diff fails toward selection


if __name__ == '__main__':
    unittest.main()
