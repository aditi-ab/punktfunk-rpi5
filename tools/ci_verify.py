"""Verify all immutable snapshots without checking out application sources."""
import json
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='punktfunk-replay-') as tmp:
    subprocess.run(['git', 'init', '-q', tmp], check=True)
    subprocess.run(['git', '-C', tmp, 'remote', 'add', 'origin',
                    'https://github.com/aditi-ab/punktfunk-rpi5.git'], check=True)
    fetched = set()
    for path in sorted((root / 'series').glob('*/manifest.json')):
        manifest = json.loads(path.read_text())
        for commit in (manifest['upstream_commit'], manifest['source_commit']):
            if not re.fullmatch('[0-9a-f]{40}', commit):
                raise ValueError('invalid commit')
            if commit not in fetched:
                # This short-lived repository needs no background maintenance.
                # A detached GC can prune .git/shallow while the next fetch reads it.
                subprocess.run(['git', '-C', tmp, '-c', 'maintenance.auto=false',
                                '-c', 'gc.auto=0', 'fetch', '-q', '--depth=1',
                                'origin', commit], check=True)
                fetched.add(commit)
        subprocess.run(['python', str(root / 'tools/patchset.py'), 'verify',
                        '--source', tmp, '--snapshot', path.parent.name], check=True)
