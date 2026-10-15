#!/usr/bin/env python3
"""Inspect real generated Debian metadata using disposable executable fixtures."""
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
VERSION = "9.9.9"


@unittest.skipUnless(platform.system() == "Linux" and shutil.which("dpkg-deb"),
                     "Linux and dpkg-deb are required")
class PackageDependencies(unittest.TestCase):
    def test_only_integrated_package_requires_egl(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp", prefix="cagents-package-deps-") as temporary:
            work = Path(temporary)
            tools = work / "tools"
            binaries = work / "target/release"
            tools.mkdir()
            binaries.mkdir(parents=True)

            # Exercise the production package generator without Cargo builds,
            # stripping fixture shell scripts, or executing application code.
            fixtures = {
                tools / "cargo": '#!/bin/sh\n[ "$1" = tree ] || exit 91\nprintf "fixture v1\\n"\n',
                tools / "strip": "#!/bin/sh\nexit 0\n",
                binaries / "cagents": '#!/bin/sh\n[ "$*" = "daemon protocol-version" ] || exit 92\nprintf "1\\n"\n',
                binaries / "cagents-ui": "#!/bin/sh\nexit 93\n",
                binaries / "cagentsd": "#!/bin/sh\nexit 94\n",
            }
            for path, contents in fixtures.items():
                path.write_text(contents)
                path.chmod(0o755)

            environment = os.environ.copy()
            environment.pop("CARGO_BUILD_TARGET", None)
            environment.update(
                PATH=f"{tools}:{os.environ['PATH']}",
                CARGO_TARGET_DIR=str(work / "target"),
                CAGENTS_SKIP_BUILD="1",
                CAGENTS_RELEASE_TARGET="x86_64-unknown-linux-gnu",
                VERSION=VERSION,
                SOURCE_DATE_EPOCH="0",
            )
            for variant in ("full", "browserless"):
                with self.subTest(variant=variant):
                    dist = work / variant
                    result = subprocess.run(
                        ["sh", str(ROOT / "scripts/package-linux.sh"), "--variant", variant],
                        env=environment | {"CAGENTS_DIST_DIR": str(dist)},
                        capture_output=True, text=True, timeout=30,
                    )
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    name = "cagents" if variant == "full" else "cagents-browserless"
                    metadata = subprocess.check_output(
                        ["dpkg-deb", "--field", str(dist / f"{name}_{VERSION}_amd64.deb"), "Depends"],
                        text=True,
                    )
                    dependencies = {dependency.strip() for dependency in metadata.split(",")}
                    self.assertEqual("libegl1" in dependencies, variant == "full")
                    self.assertEqual("xdg-utils" in dependencies, variant == "browserless")


if __name__ == "__main__":
    unittest.main()
