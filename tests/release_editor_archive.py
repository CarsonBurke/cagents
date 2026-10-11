#!/usr/bin/env python3
"""Exercise the publisher's actual editor archive validator without publication."""
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
VERSION = "1.2.3"


class EditorArchive(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir="/var/tmp", prefix="ceditor-archive-contract-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        source = (ROOT / "scripts/publish-release").read_text()
        # Run the production function unchanged, with only its caller inputs.
        function = source.split("check_archive() {", 1)[1].split(
            '\n}\n\ncheck_asset_set "$DIST_DIR"', 1
        )[0]
        self.validator = self.work / "validate"
        self.validator.write_text(
            '#!/usr/bin/env bash\nset -euo pipefail\nexport LC_ALL=C\n'
            'VERSION=$1\nWORK_DIR=$2\nfail() { printf "%s\\n" "$*" >&2; exit 1; }\n'
            'check_archive() {' + function + '\n}\n'
            'check_archive "$3" "$4" ceditor io.github.cagents.ceditor\n'
        )
        self.root = self.work / f"ceditor-{VERSION}"
        self.binaries = self.root / f"lib/ceditor/releases/{VERSION}/bin"
        self.binaries.mkdir(parents=True)
        (self.root / "bin").mkdir()
        for name in ("ceditor", "cagents", "cagentsd"):
            path = self.binaries / name
            path.write_text("fixture executable; never run\n")
            path.chmod(0o755)
        marker = self.binaries / ".ceditor-companions"
        marker.write_text("ceditor companions v1\n")
        marker.chmod(0o644)
        (self.root / "bin/ceditor").symlink_to("../lib/ceditor/current/bin/ceditor")
        (self.root / "lib/ceditor/current").symlink_to(f"releases/{VERSION}")
        for relative in (
            "share/applications/io.github.cagents.ceditor.desktop",
            "share/icons/hicolor/scalable/apps/io.github.cagents.ceditor.svg",
            "share/doc/ceditor/LICENSE",
        ):
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("fixture metadata\n")
            path.chmod(0o644)

    def validate(self, duplicate=None):
        archive = self.work / "editor.tar.gz"
        with tarfile.open(archive, "w:gz", format=tarfile.GNU_FORMAT) as output:
            output.add(self.root, arcname=self.root.name)
            if duplicate is not None:
                output.add(self.root / duplicate, arcname=f"{self.root.name}/{duplicate}")
        return subprocess.run(
            ["bash", str(self.validator), VERSION, str(self.work), str(self.work), archive.name],
            capture_output=True, text=True, timeout=10,
        )

    def test_exact_private_companion_layout_is_accepted(self):
        result = self.validate()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_public_and_current_links_cannot_redirect(self):
        for relative, destination in (
            ("bin/ceditor", "/usr/bin/ceditor"),
            ("bin/ceditor", "../../outside/ceditor"),
            ("lib/ceditor/current", "releases/other"),
            ("lib/ceditor/current", "/outside/release"),
        ):
            with self.subTest(relative=relative):
                path = self.root / relative
                original = os.readlink(path)
                path.unlink()
                path.symlink_to(destination)
                self.assertNotEqual(self.validate().returncode, 0)
                path.unlink()
                path.symlink_to(original)

    def test_companions_must_be_present_regular_and_executable(self):
        path = self.binaries / "cagentsd"
        original = path.read_bytes()
        path.unlink()
        self.assertNotEqual(self.validate().returncode, 0)
        path.symlink_to("cagents")
        self.assertNotEqual(self.validate().returncode, 0)
        path.unlink()
        path.write_bytes(original)
        path.chmod(0o644)
        self.assertNotEqual(self.validate().returncode, 0)

    def test_unexpected_members_and_link_markers_are_rejected(self):
        extra = self.binaries / "unexpected"
        extra.write_text("not shipped\n")
        self.assertNotEqual(self.validate().returncode, 0)
        extra.unlink()
        marker = self.binaries / ".ceditor-companions"
        marker.unlink()
        marker.symlink_to("cagents")
        self.assertNotEqual(self.validate().returncode, 0)

    def test_marker_requires_exact_bytes_and_mode(self):
        marker = self.binaries / ".ceditor-companions"
        for content in (b"wrong marker", b"ceditor companions v1\0", b"ceditor companions v1\n\n"):
            with self.subTest(content=content):
                marker.write_bytes(content)
                self.assertNotEqual(self.validate().returncode, 0)
        marker.write_bytes(b"ceditor companions v1\n")
        marker.chmod(0o666)
        self.assertNotEqual(self.validate().returncode, 0)

    def test_duplicate_archive_members_are_rejected(self):
        self.assertNotEqual(
            self.validate(f"lib/ceditor/releases/{VERSION}/bin/cagents").returncode, 0
        )


if __name__ == "__main__":
    unittest.main()
