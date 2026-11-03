import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('patchset', Path(__file__).parents[1] / 'tools/patchset.py')
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)


class PatchsetTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / 'source'
        self.source.mkdir()
        p.git(self.source, 'init', '-q')
        p.git(self.source, 'config', 'user.email', 'test@example.invalid')
        p.git(self.source, 'config', 'user.name', 'Patch Test')
        (self.source / '.gitattributes').write_text('* -text\n')
        self.video = 'crates/client/pf-client-video/src/video.rs'
        (self.source / self.video).parent.mkdir(parents=True)
        (self.source / self.video).write_text('old decoder\n')
        (self.source / 'a.txt').write_text('old packaging\n')
        (self.source / 'delete.txt').write_text('remove me\n')
        self.commit('base')
        self.base = p.revision(self.source, 'HEAD')
        (self.source / self.video).write_text('new decoder\n')
        (self.source / 'a.txt').write_text('new packaging\n')
        (self.source / 'delete.txt').unlink()
        (self.source / 'backend.so').write_bytes(bytes(range(256)))
        (self.source / 'new.sh').write_text('#!/bin/sh\nexit 0\n')
        p.git(self.source, 'add', '.')
        p.git(self.source, 'update-index', '--chmod=+x', 'new.sh')
        self.commit('fork')
        self.target = p.revision(self.source, 'HEAD')
        old_root = p.ROOT
        p.ROOT = self.root / 'archive'
        self.addCleanup(setattr, p, 'ROOT', old_root)
        p.export(self.source, self.base, self.target)
        self.checkout = self.root / 'checkout'
        subprocess.check_call(['git', 'clone', '-q', '--shared', str(self.source), str(self.checkout)])
        p.git(self.checkout, 'checkout', '-q', '--detach', self.base)
        p.git(self.checkout, 'config', 'user.email', 'test@example.invalid')
        p.git(self.checkout, 'config', 'user.name', 'Patch Test')

    def commit(self, message, source=None):
        source = source or self.source
        p.git(source, 'add', '-A')
        p.git(source, 'commit', '-qm', message)

    def test_exact_replay_and_apply_preserve_binary_deletion_and_mode(self):
        before = p.git(self.source, 'write-tree')
        p.verify(self.source, self.target)
        self.assertEqual(before, p.git(self.source, 'write-tree'))
        p.apply(self.checkout, self.target, False)
        self.assertEqual(p.git(self.checkout, 'write-tree'), before)
        self.assertFalse((self.checkout / 'delete.txt').exists())
        self.assertEqual((self.checkout / 'backend.so').read_bytes(), bytes(range(256)))

    def test_tampered_patch_is_rejected(self):
        patch = p.snapshot_path(self.target) / '0001-decoder-and-recovery.patch'
        patch.write_bytes(patch.read_bytes() + b'altered')
        with self.assertRaises(ValueError):
            p.apply(self.checkout, self.target, False)
        self.assertEqual(p.git(self.checkout, 'status', '--porcelain'), b'')

    def test_dirty_checkout_is_preserved(self):
        (self.checkout / 'notes.txt').write_text('user work')
        with self.assertRaises(ValueError):
            p.apply(self.checkout, self.target, False)
        self.assertEqual((self.checkout / 'notes.txt').read_text(), 'user work')

    def test_wrong_base_requires_explicit_port(self):
        (self.checkout / 'upstream.txt').write_text('new upstream feature')
        self.commit('upstream', self.checkout)
        with self.assertRaises(ValueError):
            p.apply(self.checkout, self.target, False)
        p.apply(self.checkout, self.target, True)
        self.assertEqual((self.checkout / 'upstream.txt').read_text(), 'new upstream feature')

    def test_later_patch_conflict_does_not_partially_apply_earlier_patch(self):
        (self.checkout / 'a.txt').write_text('conflicting upstream change\n')
        self.commit('conflict', self.checkout)
        with self.assertRaises(subprocess.CalledProcessError):
            p.apply(self.checkout, self.target, True)
        self.assertEqual((self.checkout / self.video).read_text(), 'old decoder\n')
        self.assertEqual(p.git(self.checkout, 'status', '--porcelain'), b'')


if __name__ == '__main__':
    unittest.main()
