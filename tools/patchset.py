#!/usr/bin/env python3
"""Export, verify and apply the complete downstream tree delta."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
GROUPS = (
    'decoder-and-recovery', 'wayland-and-presentation',
    'console-and-rendering', 'bluetooth-and-system-services',
    'packaging-documentation-and-integration',
)


def git(source, *args, env=None):
    return subprocess.check_output(['git', '-C', str(source), *args], env=env, stderr=subprocess.PIPE)


def revision(source, ref):
    return git(source, 'rev-parse', '--verify', '--end-of-options', ref + '^{commit}').decode().strip()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def group(path):
    if path.startswith('crates/client/pf-presenter/'):
        return 1
    if path.startswith('crates/client/pf-console-ui/') or path == 'clients/session/src/console.rs':
        return 2
    if path.startswith('crates/client/pf-client-core/') and not path.endswith(('Cargo.toml', '/session.rs')):
        return 3
    if path.startswith(('crates/client/pf-client-video/', 'crates/core/')) or path in (
        'Cargo.toml', 'Cargo.lock', 'clients/session/Cargo.toml',
        'crates/client/pf-client-core/Cargo.toml', 'crates/client/pf-client-core/src/session.rs',
    ):
        return 0
    return 4


def snapshot_path(commit):
    if not re.fullmatch('[0-9a-f]{40}', commit):
        raise ValueError('snapshot must be a full commit hash')
    return ROOT / 'series' / commit


def load(commit):
    folder = snapshot_path(commit)
    manifest = json.loads((folder / 'manifest.json').read_text())
    if manifest.get('format') != 1:
        raise ValueError('unsupported snapshot format')
    for key in ('upstream_commit', 'source_tree'):
        if not re.fullmatch('[0-9a-f]{40}', manifest[key]):
            raise ValueError('invalid snapshot hash: ' + key)
    if manifest['source_commit'] != commit:
        raise ValueError('snapshot identity mismatch')
    expected = [f'{i+1:04d}-{name}.patch' for i, name in enumerate(GROUPS)]
    if [p['file'] for p in manifest['patches']] != expected:
        raise ValueError('unexpected patch order or filename')
    if (folder / 'series').read_text().splitlines() != expected:
        raise ValueError('series order mismatch')
    for entry in manifest['patches']:
        if digest((folder / entry['file']).read_bytes()) != entry['sha256']:
            raise ValueError('patch checksum mismatch: ' + entry['file'])
    return folder, manifest


def verify(source, commit):
    folder, manifest = load(commit)
    patches = [str(folder / p['file']) for p in manifest['patches'] if p['paths']]
    with tempfile.TemporaryDirectory(prefix='punktfunk-patch-index-') as tmp:
        env = dict(os.environ, GIT_INDEX_FILE=str(Path(tmp) / 'index'))
        git(source, 'read-tree', manifest['upstream_commit'], env=env)
        if patches:
            git(source, 'apply', '--cached', '--check', *patches, env=env)
            git(source, 'apply', '--cached', *patches, env=env)
        actual = git(source, 'write-tree', env=env).decode().strip()
    target = git(source, 'rev-parse', manifest['source_commit'] + '^{tree}').decode().strip()
    if actual != manifest['source_tree'] or actual != target:
        raise ValueError('replayed tree differs from the source commit')
    print('Verified exact tree:', actual)


def export(source, base_ref, target_ref):
    base, target = revision(source, base_ref), revision(source, target_ref)
    if git(source, 'status', '--porcelain', '--untracked-files=all').strip():
        raise ValueError('commit source changes before exporting')
    folder = snapshot_path(target)
    if folder.exists():
        verify(source, target)
        return
    paths = git(source, 'diff', '--no-renames', '--name-only', '-z', base, target).decode().split('\0')
    paths = [p for p in paths if p]
    patches = []
    contents = {}
    for i, name in enumerate(GROUPS):
        selected = [p for p in paths if group(p) == i]
        filename = f'{i+1:04d}-{name}.patch'
        data = git(source, 'diff', '--binary', '--full-index', '--no-renames', '--no-ext-diff', '--no-textconv', base, target, '--', *selected) if selected else b''
        contents[filename] = data
        patches.append(dict(file=filename, sha256=digest(data), paths=selected))
    manifest = dict(
        format=1, upstream_repository='https://git.unom.io/unom/punktfunk.git',
        upstream_ref=base_ref, upstream_commit=base,
        source_repository='https://github.com/aditi-ab/punktfunk-rpi5.git',
        source_commit=target,
        source_tree=git(source, 'rev-parse', target + '^{tree}').decode().strip(),
        changed_paths=len(paths), patches=patches,
    )
    folder.mkdir(parents=True)
    for filename, data in contents.items():
        (folder / filename).write_bytes(data)
    (folder / 'series').write_text(''.join(p['file'] + '\n' for p in patches), encoding='utf-8')
    (folder / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
    verify(source, target)
    (ROOT / 'CURRENT').write_text(target + '\n', encoding='ascii')
    print('Exported', len(paths), 'paths to', folder)


def apply(source, commit, allow_new_base):
    folder, manifest = load(commit)
    if git(source, 'status', '--porcelain', '--untracked-files=all').strip():
        raise ValueError('target checkout must be clean, including untracked files')
    if not allow_new_base and revision(source, 'HEAD') != manifest['upstream_commit']:
        raise ValueError('wrong upstream base; use --allow-new-base only when porting')
    patches = [str(folder / p['file']) for p in manifest['patches'] if p['paths']]
    if patches:
        # Preflight the whole series before changing any working-tree file.
        git(source, 'apply', '--index', '--check', *patches)
        git(source, 'apply', '--index', *patches)
    print('Applied and staged; review the diff and run the target-version tests.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['export', 'verify', 'apply'])
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--base')
    parser.add_argument('--ref', default='HEAD')
    parser.add_argument('--snapshot')
    parser.add_argument('--expect-head', action='store_true')
    parser.add_argument('--allow-new-base', action='store_true')
    args = parser.parse_args()
    if args.action == 'export':
        if not args.base:
            parser.error('export requires an explicit --base upstream release')
        export(args.source, args.base, args.ref)
        return
    commit = revision(args.source, 'HEAD') if args.expect_head else (
        args.snapshot or (ROOT / 'CURRENT').read_text().strip())
    if args.action == 'verify':
        verify(args.source, commit)
    else:
        apply(args.source, commit, args.allow_new_base)


if __name__ == '__main__':
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.stderr.decode(errors='replace') if error.stderr else str(error))
    except (ValueError, OSError) as error:
        raise SystemExit(str(error))
