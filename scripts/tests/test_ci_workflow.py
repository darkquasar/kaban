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
    def execute(self, name, *, needs=None, event=None, event_name='pull_request', ref='refs/pull/7/merge', default='main', changed_path=None, base_present=True, file_contents=None, initial_files=None):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            event = dict(event or {})
            if changed_path is not None:
                def git(*args):
                    return subprocess.check_output(['git', *args], cwd=root, text=True, stderr=subprocess.DEVNULL).strip()
                git('init', '-q')
                for filename, content in (initial_files or {}).items():
                    initial = root / filename
                    initial.parent.mkdir(parents=True, exist_ok=True)
                    initial.write_text(content)
                    git('add', '--', filename)
                git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                    '-c', 'core.hooksPath=/dev/null', 'commit', '--allow-empty', '-qm', 'base')
                base = git('rev-parse', 'HEAD')
                paths = (changed_path,) if isinstance(changed_path, str) else changed_path
                for filename in paths:
                    changed = root / filename
                    changed.parent.mkdir(parents=True, exist_ok=True)
                    content = file_contents if file_contents is not None else 'invented diff\n'
                    changed.write_bytes(content if isinstance(content, bytes) else content.encode())
                    git('add', '--', filename)
                git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                    '-c', 'core.hooksPath=/dev/null', 'commit', '--allow-empty', '-qm', 'change')
                base = base if base_present else 'f' * 40
                if event_name == 'pull_request': event['pull_request'] = {'base': {'sha': base}}
                elif event_name == 'push': event['before'] = base
                elif event_name == 'merge_group': event['merge_group'] = {'base_sha': base}
            (root / 'event.json').write_text(json.dumps(event))
            (root / 'scripts/ci').mkdir(parents=True, exist_ok=True)
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
        needs = {job: {'result': 'success'} for job in ('selection', 'quality', 'functional', 'pdf', 'gnu', 'musl', 'script-tests', 'producer', 'macos')}
        needs['selection']['outputs'] = {'producer': selected, 'rust': 'true'}
        return needs

    def test_macos_producer_scope_is_narrow_and_unknowns_are_conservative(self):
        for path, expected in (
            ('docs/guide.md', 'false'), ('scripts/tests/test_ci_workflow.py', 'false'),
            ('crates/kaban-ingest/src/chunk.rs', 'false'),
            ('crates/kaban-store-sqlite/src/lib.rs', 'false'),
            ('crates/kaban-index-tantivy/src/lib.rs', 'false'),
            ('crates/kaban/src/cli/remember.rs', 'false'),
            ('hooks/codex/stop.sh', 'false'), ('fixtures/ingest/doc.md', 'false'),
            ('examples/policy.yaml', 'false'),
            ('crates/kaban-ingest/build.rs', 'true'),
            ('crates/kaban-ingest/Cargo.toml', 'true'),
            ('tools/kaban-rust-scip/src/apple.rs', 'true'),
            ('crates/kaban-index/src/language/rust.rs', 'true'),
            ('crates/kaban-index/src/tools.rs', 'true'),
            ('crates/kaban-core/src/process.rs', 'true'),
            ('crates/kaban/src/mcp/code.rs', 'true'),
            ('fixtures/codegraph/VERSIONS', 'true'), ('Cargo.lock', 'true'),
            ('mise.toml', 'true'), ('scripts/ci/ci.sh', 'true'),
            ('unknown.txt', 'true'),
        ):
            for event in ('pull_request', 'push'):
                with self.subTest(path=path, event=event):
                    result, output, summary = self.execute('Select producer and cache scope',
                        event={'number': 7}, event_name=event, changed_path=path)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(output.get('macos-producer'), expected, summary)
        for name in ('workflow_dispatch', 'merge_group'):
            result, output, summary = self.execute('Select producer and cache scope',
                event_name=name, changed_path='docs/guide.md', base_present=False)
            self.assertEqual(output.get('macos-producer'), 'true', summary)
        result, output, summary = self.execute('Select producer and cache scope',
            changed_path='docs/guide.md', base_present=False)
        self.assertEqual(output.get('macos-producer'), 'true', summary)

    def test_macos_producer_scope_handles_mixed_changes_and_merge_base(self):
        for paths, expected in (
            (('hooks/codex/stop.sh', 'fixtures/ingest/doc.md'), 'false'),
            (('crates/kaban/src/cli/remember.rs', 'crates/kaban-core/src/process/env.rs'), 'true'),
            (('docs/guide.md', 'tools/kaban-rust-scip/src/lib.rs'), 'true'),
            (('examples/policy.yaml', 'unknown.txt'), 'true'),
        ):
            for event in ('pull_request', 'push', 'merge_group'):
                result, output, summary = self.execute('Select producer and cache scope',
                    event={'number': 7}, event_name=event, changed_path=paths)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output.get('macos-producer'), expected, summary)
                if event == 'merge_group':
                    self.assertEqual(output['rust'], 'true', summary)  # existing Linux scope
                    self.assertEqual(output['producer'], 'true', summary)

    def test_linguist_only_attributes_skip_rust_for_pr_and_main_push(self):
        for event in ('pull_request', 'push'):
            with self.subTest(event=event):
                result, outputs, summary = self.execute('Select producer and cache scope',
                    event={'number': 7}, event_name=event, changed_path='.gitattributes',
                    file_contents='# Upstream notices\nlicenses/release/**/*.html linguist-documentation\n')
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(outputs.get('rust'), 'false', summary)
                self.assertEqual(outputs['producer'], 'false', summary)

    def test_readiness_accepts_only_explicit_metadata_skips(self):
        needs = self.required('false')
        needs['selection']['outputs']['rust'] = 'false'
        for job in ('quality', 'functional', 'pdf', 'gnu', 'musl', 'producer', 'macos'):
            needs[job]['result'] = 'skipped'
        result, _, summary = self.execute('Run gates', needs=needs)
        self.assertEqual(result.returncode, 0, summary)
        self.assertIn('Readiness: PASS', summary)
        needs['script-tests']['result'] = 'failure'
        result, _, summary = self.execute('Run gates', needs=needs)
        self.assertNotEqual(result.returncode, 0, summary)

    def test_rust_selection_omits_only_known_documentation_and_metadata(self):
        for path, expected in (
            ('README.md', 'false'), ('CODEBASE.md', 'false'), ('docs/guide.mdx', 'false'),
            ('docs/image.svg', 'false'), ('NOTICE', 'false'),
            ('licenses/release/upstream/notice.html', 'false'),
            ('crates/kaban/src/main.rs', 'true'), ('Cargo.lock', 'true'),
            ('Cargo.toml', 'true'), ('.cargo/config.toml', 'true'),
            ('.github/workflows/ci.yml', 'true'), ('mise.toml', 'true'),
            ('scripts/tests/test_ci_workflow.py', 'false'), ('fixtures/example.md', 'true'),
            ('docs/executable.rs', 'true'), ('unknown.txt', 'true'),
        ):
            with self.subTest(path=path):
                result, output, summary = self.execute('Select producer and cache scope',
                    event={'number': 7}, changed_path=path)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output['rust'], expected, summary)
        for paths in (('README.md', 'crates/kaban/src/main.rs'), ()):
            result, output, summary = self.execute('Select producer and cache scope',
                event={'number': 7}, changed_path=paths)
            self.assertEqual(output['rust'], 'true', summary)
        result, output, summary = self.execute('Select producer and cache scope',
            event={'number': 7}, changed_path='README.md', base_present=False)
        self.assertEqual(output['rust'], 'true', summary)

    def test_helper_only_changes_skip_rust_but_shared_scripts_and_fixtures_do_not(self):
        cases = (
            ('scripts/ci/clean-room.sh', 'false'),
            ('scripts/dev/cargo-slot.sh', 'false'),
            ('scripts/dev/lane-gate.sh', 'false'),
            ('scripts/tests/test_ci_gates.py', 'false'),
            ('scripts/mcp/test_mcp_catalog.py', 'false'),
            ('scripts/release/build.sh', 'false'),
            ('scripts/release/authorize.py', 'false'),
            ('scripts/release/licenses.py', 'false'),
            ('scripts/release/notes.py', 'false'),
            ('scripts/release/package.py', 'false'),
            ('scripts/README.md', 'false'),
            ('hooks/HOST-EVENTS.md', 'false'),
            ('hooks/pi/README.md', 'false'),
            ('tools/kaban-rust-scip/README.md', 'false'),
            ('scripts/ci/ci.sh', 'true'),
            ('scripts/ci/preflight.py', 'true'),
            ('scripts/ci/install-go.sh', 'true'),
            ('scripts/ci/deps_allowlist.sh', 'true'),
            ('scripts/mcp/mcp_smoke.py', 'true'),
            ('hooks/pi/kaban-hooks.ts', 'true'),
            ('fixtures/judged/README.md', 'true'),
            ('fixtures/codegraph/VERSIONS', 'true'),
            ('scripts/new-helper.sh', 'true'),
            ('scripts/release/new-script.py', 'true'),
            ('scripts/tests/fixtures/example.rs', 'true'),
            ('.github/workflows/ci.yml', 'true'),
        )
        for event in ('pull_request', 'push'):
            for path, expected in cases:
                with self.subTest(event=event, path=path):
                    result, output, summary = self.execute('Select producer and cache scope',
                        event={'number': 7}, event_name=event, changed_path=path)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(output['rust'], expected, summary)
                    self.assertEqual(output['producer'], expected, summary)
        for paths, expected in (
            (('README.md', 'scripts/ci/clean-room.sh'), 'false'),
            (('scripts/tests/test_ci_gates.py', 'crates/kaban/src/main.rs'), 'true'),
            (('scripts/ci/clean-room.sh', 'scripts/ci/ci.sh'), 'true'),
        ):
            result, output, summary = self.execute('Select producer and cache scope',
                event={'number': 7}, changed_path=paths)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(output['rust'], expected, summary)

    def test_helper_only_scope_preserves_fallbacks_and_release_package_validation(self):
        for name in ('workflow_dispatch', 'merge_group'):
            result, output, summary = self.execute('Select producer and cache scope',
                event_name=name, changed_path='scripts/ci/clean-room.sh')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(output['rust'], 'true', summary)
        result, output, summary = self.execute('Select producer and cache scope',
            changed_path='scripts/ci/clean-room.sh', base_present=False)
        self.assertEqual(output['rust'], 'true', summary)
        # Root Rust skips rely on the separate workflow still executing the
        # release scripts against the packaged binaries when those scripts change.
        release = (ROOT / '.github/workflows/release.yml').read_text()
        paths = release.split('    paths:\n', 1)[1].split('\nconcurrency:', 1)[0]
        self.assertIn("      - 'scripts/release/**'", paths)
        self.assertIn("      - 'scripts/mcp/mcp_smoke.py'", paths)
        self.assertIn("      - 'scripts/mcp/mcp_catalog.py'", paths)
        self.assertNotIn("'scripts/mcp/**'", paths)
        self.assertNotIn('scripts/tests/', paths)
        self.assertIn('run: bash scripts/release/build.sh', release)

    def test_agent_instructions_and_local_instruction_ignore_do_not_build_rust(self):
        original = '/target/\n# Existing project rules\n/fixtures/generated/\n'
        for event in ('pull_request', 'push'):
            result, output, summary = self.execute('Select producer and cache scope',
                event={'number': 7}, event_name=event, changed_path='AGENTS.md')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(output['rust'], 'false', summary)
            for content, expected in (
                (original + '/REPOLOCAL.md\n', 'false'),
                (original + '# Machine-local instructions\n/REPOLOCAL.md\n', 'false'),
                (original + '/REPOLOCAL.md\r/fixtures/\n', 'true'),
                (original + '/REPOLOCAL.md\x00\n', 'true'),
                (original + 'REPOLOCAL.md\n', 'true'),
                (original + '/fixtures/\n', 'true'),
                (original + '!/fixtures/generated/\n', 'true'),
            ):
                with self.subTest(event=event, content=content):
                    result, output, summary = self.execute('Select producer and cache scope',
                        event={'number': 7}, event_name=event, changed_path='.gitignore',
                        initial_files={'.gitignore': original}, file_contents=content)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(output['rust'], expected, summary)
                    self.assertEqual(output['producer'], expected, summary)
        result, output, summary = self.execute('Select producer and cache scope',
            changed_path='.gitignore', initial_files={'.gitignore': original + '/REPOLOCAL.md\n'},
            file_contents=original)
        self.assertEqual(output['rust'], 'false', summary)
        for paths, expected in (
            (('AGENTS.md', '.gitignore'), 'false'),
            (('AGENTS.md', '.gitignore', 'Cargo.lock'), 'true'),
        ):
            result, output, summary = self.execute('Select producer and cache scope',
                changed_path=paths, initial_files={'.gitignore': original},
                file_contents=original + '/REPOLOCAL.md\n')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(output['rust'], expected, summary)

    def test_attributes_affecting_checkout_or_build_inputs_keep_rust_checks(self):
        for content, original in (
            ('*.rs text eol=lf\n', None),
            ('*.rs filter=custom linguist-documentation\n', None),
            ('[attr]binary linguist-documentation\n', None),
            ('*.html linguist-documentation\n', '*.rs text eol=lf\n'),
            ('* linguist-documentation\reol=crlf\n', None),
            ('*.rs linguist-documentation\rworking-tree-encoding=UTF-16LE\n', None),
            (b'# caf\xe9\n*.html linguist-documentation\n', None),
        ):
            with self.subTest(content=content, original=original):
                result, output, summary = self.execute('Select producer and cache scope',
                    event={'number': 7}, changed_path='.gitattributes', file_contents=content,
                    initial_files={'.gitattributes': original} if original else None)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output['rust'], 'true', summary)

    def test_unchanged_attribute_macro_cannot_hide_checkout_changes(self):
        for pattern in ('[attr]linguist-documentation', '"[attr]linguist-documentation"'):
            with self.subTest(pattern=pattern):
                macro = pattern + ' working-tree-encoding=UTF-16LE\n'
                result, output, summary = self.execute('Select producer and cache scope',
                    event={'number': 7}, changed_path='.gitattributes',
                    initial_files={'.gitattributes': macro},
                    file_contents=macro + '*.rs linguist-documentation\n')
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output['rust'], 'true', summary)

    def test_readiness_requires_valid_scope_and_all_selected_results(self):
        for flag in ('rust', 'producer'):
            for value in (None, 'unknown', True):
                with self.subTest(flag=flag, value=value):
                    needs = self.required()
                    if value is None:
                        del needs['selection']['outputs'][flag]
                    else:
                        needs['selection']['outputs'][flag] = value
                    result, _, summary = self.execute('Run gates', needs=needs)
                    self.assertEqual(result.returncode, 1, summary)
        for job in ('selection', 'quality', 'functional', 'pdf', 'gnu', 'musl', 'script-tests', 'producer', 'macos'):
            for state in ('missing', 'cancelled', 'failure', 'success'):
                if job in ('selection', 'script-tests') and state == 'success':
                    continue
                with self.subTest(job=job, state=state):
                    needs = self.required('false')
                    needs['selection']['outputs']['rust'] = 'false'
                    for name in ('quality', 'functional', 'pdf', 'gnu', 'musl', 'producer', 'macos'):
                        needs[name]['result'] = 'skipped'
                    if state == 'missing':
                        del needs[job]
                    else:
                        needs[job]['result'] = state
                    result, _, summary = self.execute('Run gates', needs=needs)
                    self.assertEqual(result.returncode, 1, summary)

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
        for group in ('quality', 'functional', 'pdf', 'gnu', 'musl', 'producer', 'macos'):
            # Scope the public action input to this job (not a cache-service emulator).
            body = source.split('\n  ' + group + ':\n', 1)[1]
            body = re.split(r'\n  [a-z][a-z-]*:\n', body, maxsplit=1)[0]
            prefix = re.search(r'^          prefix-key: (.+)$', body, re.MULTILINE).group(1)
            def resolve(outputs):
                return prefix.replace('${{ needs.selection.outputs.cache-prefix }}', outputs.get('cache-prefix', ''))
            self.assertEqual(resolve(main), resolve(pr), 'A new PR must be able to reuse a compatible default-branch key')
            keys.append(resolve(main))
        self.assertEqual(len(set(keys)), 7)  # incompatible group targets still stay distinct

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
