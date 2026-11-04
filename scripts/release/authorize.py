#!/usr/bin/env python3
"""Authorize the owner-created tag against main history and exact-commit push CI."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def authorize():
    expected = {'GITHUB_REPOSITORY': 'darkquasar/kaban', 'GITHUB_EVENT_NAME': 'push',
                'GITHUB_ACTOR': 'darkquasar', 'GITHUB_ACTOR_ID': '28435534',
                'GITHUB_TRIGGERING_ACTOR': 'darkquasar'}
    for key, value in expected.items():
        if os.environ.get(key) != value:
            raise ValueError(f'unauthorized {key}')
    with open('Cargo.toml', 'rb') as stream:
        version = tomllib.load(stream)['workspace']['package']['version']
    if not re.fullmatch(r'\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?', version):
        raise ValueError('Cargo version is not a release version')
    tag = 'v' + version
    if os.environ.get('GITHUB_REF') != 'refs/tags/' + tag:
        raise ValueError('tag must agree with Cargo.toml version')
    sha = command('git', 'rev-parse', 'HEAD')
    if os.environ.get('GITHUB_SHA') != sha or command('git', 'rev-parse', tag + '^{commit}') != sha:
        raise ValueError('tag, checkout and event must identify the same commit')
    result = subprocess.run(['git', 'merge-base', '--is-ancestor', sha, 'refs/remotes/origin/main'])
    if result.returncode != 0:
        raise ValueError('tag commit must belong to origin/main history')
    query = (f'repos/darkquasar/kaban/actions/workflows/ci.yml/runs?head_sha={sha}'
             '&event=push&branch=main&status=success&per_page=100')
    runs = json.loads(command('gh', 'api', query))['workflow_runs']
    if not any(run.get('head_sha') == sha and run.get('head_branch') == 'main' and
               run.get('event') == 'push' and run.get('status') == 'completed' and
               run.get('conclusion') == 'success' and run.get('path') == '.github/workflows/ci.yml'
               for run in runs):
        raise ValueError('no successful main push CI for this exact commit; tag a main tip after CI finishes, then rerun')
    print(f'Authorized {tag} at {sha}')


if __name__ == '__main__':
    try:
        authorize()
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f'release refused: {error}', file=sys.stderr)
        sys.exit(1)
