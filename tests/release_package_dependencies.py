#!/usr/bin/env python3
"""Inspect real generated Debian metadata using disposable executable fixtures."""
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
VERSION = "9.9.9"


def provisioned_runtime_packages():
    source = (ROOT / "scripts/prepare-release-smoke-rootfs").read_text()
    parts = source.split("<<'PROVISION'\n")
    if len(parts) != 2 or "\nPROVISION\n" not in parts[1]:
        raise ValueError("expected one literal provisioning heredoc")
    lines = parts[1].split("\nPROVISION\n", 1)[0].splitlines()
    installs = []
    for index, line in enumerate(lines):
        if not line.startswith("apt-get "):
            continue
        command = line
        while command.endswith("\\"):
            index += 1
            if index >= len(lines):
                raise ValueError("unterminated provisioning apt command")
            command = command[:-1] + lines[index]
        arguments = shlex.split(command)
        if "install" in arguments:
            installs.append(arguments)
    if len(installs) != 1:
        raise ValueError("expected one provisioning apt install command")
    arguments = installs[0]
    if arguments[:4] != ["apt-get", "-o", "Dir::Bin::dpkg=/usr/local/sbin/cagents-dpkg", "install"]:
        raise ValueError("unsupported provisioning apt install prefix")
    if arguments[4:6] != ["--yes", "--no-install-recommends"]:
        raise ValueError("unsupported provisioning apt install options")
    packages = arguments[6:]
    if not packages or any(not re.fullmatch(r"[a-z0-9][a-z0-9+.-]*", name) for name in packages):
        raise ValueError("expected literal provisioning package names")
    return set(packages)


def native_package_host_supported():
    if (platform.system() != "Linux" or platform.machine() != "x86_64"
            or not shutil.which("dpkg-deb")):
        return False
    try:
        result = subprocess.run(
            ["getconf", "GNU_LIBC_VERSION"], stdin=subprocess.DEVNULL,
            capture_output=True, text=True, check=False, timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    # Match package-linux.sh's `getconf ... || :`: stdout decides support,
    # including when getconf returns nonzero. Do not impose a glibc version.
    return result.stdout.startswith("glibc ")


class HostRequirements(unittest.TestCase):
    def test_host_requirements_match_native_packager(self):
        query = ["getconf", "GNU_LIBC_VERSION"]

        def output(version, returncode=0):
            return subprocess.CompletedProcess(query, returncode, version, "")

        cases = (
            ("glibc", "Linux", "x86_64", "/fixture/dpkg-deb", output("glibc 2.35\n"), True),
            ("newer glibc", "Linux", "x86_64", "/fixture/dpkg-deb", output("glibc 2.39\n"), True),
            ("non-Linux", "Darwin", "x86_64", "/fixture/dpkg-deb", None, False),
            ("aarch64", "Linux", "aarch64", "/fixture/dpkg-deb", None, False),
            ("no dpkg-deb", "Linux", "x86_64", None, None, False),
            ("musl", "Linux", "x86_64", "/fixture/dpkg-deb", output("musl 1.2.5\n"), False),
            ("empty failed query", "Linux", "x86_64", "/fixture/dpkg-deb", output("", 1), False),
            ("failed query with glibc stdout", "Linux", "x86_64", "/fixture/dpkg-deb",
             output("glibc 2.35\n", 1), True),
            ("unavailable query", "Linux", "x86_64", "/fixture/dpkg-deb", FileNotFoundError(), False),
            ("timed-out query", "Linux", "x86_64", "/fixture/dpkg-deb",
             subprocess.TimeoutExpired(query, 5), False),
        )
        for name, system, machine, dpkg, result, expected in cases:
            with self.subTest(host=name), \
                    mock.patch.object(platform, "system", return_value=system), \
                    mock.patch.object(platform, "machine", return_value=machine), \
                    mock.patch.object(shutil, "which", return_value=dpkg), \
                    mock.patch.object(subprocess, "run") as run:
                if isinstance(result, Exception):
                    run.side_effect = result
                else:
                    run.return_value = result
                self.assertEqual(native_package_host_supported(), expected)
                if result is None:
                    run.assert_not_called()
                else:
                    run.assert_called_once_with(
                        query, stdin=subprocess.DEVNULL, capture_output=True,
                        text=True, check=False, timeout=5,
                    )

        with mock.patch(f"{__name__}.native_package_host_supported", return_value=False):
            with self.assertRaisesRegex(unittest.SkipTest, "Linux x86_64 glibc and dpkg-deb"):
                PackageDependencies.setUpClass()
        with mock.patch(f"{__name__}.native_package_host_supported", return_value=True):
            PackageDependencies.setUpClass()


class PackageDependencies(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not native_package_host_supported():
            raise unittest.SkipTest("Linux x86_64 glibc and dpkg-deb are required")

    def test_only_integrated_package_requires_egl(self):
        runtime_packages = provisioned_runtime_packages()
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
                    # Keep preparation self-contained for every declared runtime
                    # package instead of relying on incidental apt transitivity.
                    required_packages = set()
                    for dependency in dependencies:
                        match = re.fullmatch(
                            r"([a-z0-9][a-z0-9+.-]*)(?: \((?:<<|<=|=|>=|>>) [^()\s]+\))?",
                            dependency,
                        )
                        self.assertIsNotNone(match, f"unsupported Debian dependency: {dependency}")
                        required_packages.add(match.group(1))
                    self.assertEqual(
                        required_packages - runtime_packages, set(),
                        f"{variant} runtime dependencies missing from rootfs provisioning",
                    )


if __name__ == "__main__":
    unittest.main()
