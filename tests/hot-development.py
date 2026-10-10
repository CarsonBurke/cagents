#!/usr/bin/env python3
"""No Cargo/provider calls: exercise installed identity and actual Linux lock mount."""
import importlib.machinery
import importlib.util
import errno
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
source = Path(__file__).resolve().parents[1] / "scripts/hot-development"
loader = importlib.machinery.SourceFileLoader("hot_development", str(source))
spec = importlib.util.spec_from_loader(loader.name, loader)
hot = importlib.util.module_from_spec(spec)
loader.exec_module(hot)


class HotRunner(unittest.TestCase):
    def test_staging_preserves_reader_of_previous_executable_inode(self):
        fixtures = source.parents[1] / ".worktrees"
        fixtures.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="hot-stage-test-", dir=fixtures) as directory:
            root = Path(directory)
            original = root / "cagentsd"
            original.write_bytes(b"previous executable")
            product = root / "built-cagentsd"
            product.write_bytes(b"replacement executable")
            product.chmod(0o755)
            with original.open("rb") as reader:
                previous_inode = os.fstat(reader.fileno()).st_ino
                hot.stage_product(product, original)
                self.assertEqual(reader.read(), b"previous executable")
                self.assertNotEqual(previous_inode, original.stat().st_ino)
                self.assertEqual(original.read_bytes(), b"replacement executable")
                self.assertEqual(original.stat().st_mode & 0o777, 0o755)
            self.assertEqual(list(root.glob(".hot-product-*")), [])

    def test_exact_recorded_tool_source_required(self):
        package = f"cargo-hot {hot.HOT_VERSION} (git+https://github.com/hecrj/cargo-hot.git?rev={hot.HOT_REVISION}#{hot.HOT_REVISION})"
        self.assertTrue(hot.matches_install(package, {"bins": ["cargo-hot"]}))
        self.assertFalse(hot.matches_install(package.replace(hot.HOT_REVISION, "a" * 40), {"bins": ["cargo-hot"]}))
        self.assertFalse(hot.matches_install(package.replace("0.1.1", "0.2.0"), {"bins": ["cargo-hot"]}))
        self.assertFalse(hot.matches_install(package.replace("/cargo-hot.git", "/cargo-hot-evil.git"), {"bins": ["cargo-hot"]}))
        self.assertFalse(hot.matches_install(package, {"bins": ["unrelated"]}))

    @unittest.skipUnless(shutil.which("bwrap"), "bubblewrap not installed")
    def test_real_mount_refuses_inplace_and_atomic_lock_changes_but_edits_source(self):
        # Disk-backed temporary fixture, never /tmp RAM or real repository state.
        fixtures = source.parents[1] / ".worktrees"
        fixtures.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="hot-lock-test-", dir=fixtures) as directory:
            root = Path(directory)
            lock = root / "Cargo.lock"
            lock.write_text("immutable lock\n")
            script = root / "probe.py"
            script.write_text("""
import errno,json,os
from pathlib import Path
lock=Path('Cargo.lock')
results={}
for name,operation in [('write',lambda:lock.write_text('bad')),('unlink',lock.unlink),('rename',lambda:Path('replacement').replace(lock))]:
    Path('replacement').write_text('bad')
    try: operation()
    except OSError as error: results[name]=error.errno
    else: raise RuntimeError(name+' unexpectedly changed lock')
Path('source.rs').write_text('editable source')
assert lock.read_text()=='immutable lock\\n'
print(json.dumps({'uid':os.getuid(),'errors':results,'offline':os.environ.get('CARGO_NET_OFFLINE')}))
""")
            original = hot.ROOT
            hot.ROOT = root
            try:
                result = subprocess.run(hot.sandbox(["python3", str(script)], offline=True), capture_output=True, text=True, timeout=10)
            finally:
                hot.ROOT = original
            self.assertEqual(result.returncode, 0, result.stderr)
            values = json.loads(result.stdout)
            self.assertEqual(values["uid"], os.getuid())
            self.assertEqual(values["offline"], "true")
            self.assertEqual(set(values["errors"]), {"write", "unlink", "rename"})
            self.assertEqual(values["errors"]["write"], errno.EROFS)
            self.assertEqual(values["errors"]["unlink"], errno.EBUSY)
            self.assertEqual(values["errors"]["rename"], errno.EBUSY)
            self.assertEqual(lock.read_text(), "immutable lock\n")
            self.assertEqual((root / "source.rs").read_text(), "editable source")


if __name__ == "__main__":
    unittest.main()
