#!/usr/bin/env python3
"""Inspect generated Linux packages using disposable executable fixtures."""
import io
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import subprocess
import tarfile
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

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(
            dir="/var/tmp", prefix="cagents-package-deps-",
        )
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)
        self.tools = self.work / "tools"
        self.binaries = self.work / "target/release"
        self.tools.mkdir()
        self.binaries.mkdir(parents=True)
        # These are deliberately shell fixtures, never application ELFs. Only
        # the fake CLI protocol query executes; UI/daemon execution fails.
        fixtures = {
            self.tools / "cargo": '#!/bin/sh\n[ "$1" = tree ] || exit 91\nprintf "fixture v1\\n"\n',
            self.tools / "strip": "#!/bin/sh\nexit 0\n",
            self.binaries / "cagents": '#!/bin/sh\n[ "$*" = "daemon protocol-version" ] || exit 92\nprintf "1\\n"\n',
            self.binaries / "cagents-ui": "#!/bin/sh\nexit 93\n",
            self.binaries / "cagentsd": "#!/bin/sh\nexit 94\n",
        }
        for path, contents in fixtures.items():
            path.write_text(contents)
            path.chmod(0o755)
        self.environment = os.environ.copy()
        self.environment.pop("CARGO_BUILD_TARGET", None)
        self.environment.pop("CAGENTS_SKIP_STRIP", None)
        self.environment.update(
            PATH=f"{self.tools}:{os.environ['PATH']}",
            CARGO_TARGET_DIR=str(self.work / "target"),
            CAGENTS_SKIP_BUILD="1",
            CAGENTS_RELEASE_TARGET="x86_64-unknown-linux-gnu",
            VERSION=VERSION,
            SOURCE_DATE_EPOCH="0",
        )
        self.invocations = self.work / "readelf-calls"
        self.packages = 0
        self.readelf({})

    @staticmethod
    def version_info(*versions):
        # A deliberately higher exported definition and symbol-table label
        # must NOT determine the executable's required glibc minimum.
        names = "\n".join(
            f"  0x{16 * (index + 1):04x}: Name: {version} Flags: none Version: {index + 2}"
            for index, version in enumerate(versions)
        )
        return (
            "Version symbols section '.gnu.version' contains 2 entries:\n"
            " Addr: 0x0000000000000400 Offset: 0x400 Link: 3 (.dynsym)\n"
            "  000: 0 (*local*) 2 (GLIBC_999.0)\n\n"
            "Version definition section '.gnu.version_d' contains 1 entry:\n"
            " Addr: 0x0000000000000500 Offset: 0x500 Link: 4 (.dynstr)\n"
            "  0x0000: Rev: 1 Flags: none Index: 2 Cnt: 1 Name: GLIBC_999.0\n\n"
            "Version needs section '.gnu.version_r' contains 1 entry:\n"
            " Addr: 0x0000000000000600 Offset: 0x600 Link: 4 (.dynstr)\n"
            f"  0x0000: Version: 1 File: libc.so.6 Cnt: {len(versions)}\n"
            f"{names}\n"
        )

    def readelf(self, outputs, failures=None, warnings=None):
        failures = failures or {}
        warnings = warnings or {}
        source = (
            '#!/bin/sh\n'
            # Full UI font visibility is a separate pre-publication inspection.
            # Keep this log/count dedicated to the existing GLIBC version needs.
            'if [ "$#" = 3 ] && [ "$1" = --wide ] && [ "$2" = --dyn-syms ]; then\n'
            '  [ "${3##*/}" = cagents-ui ] || exit 95\n'
            '  cat <<EOF\n'
            "Symbol table '.dynsym' contains 2 entries:\n"
            ' Num: Value Size Type Bind Vis Ndx Name\n'
            ' 0: 0000000000000000 0 NOTYPE LOCAL DEFAULT UND\n'
            ' 1: 0000000000000000 0 FUNC GLOBAL DEFAULT UND FT_Init_FreeType\n'
            'EOF\n'
            '  exit 0\n'
            'fi\n'
            '[ "$#" = 3 ] && [ "$1" = --wide ] '
            '&& [ "$2" = --version-info ] || exit 95\n'
            f'printf "%s\\n" "${{3##*/}}" >> {shlex.quote(str(self.invocations))}\n'
            'case ${3##*/} in\n'
        )
        for binary in ("cagents", "cagents-ui", "cagentsd"):
            output = outputs.get(binary, self.version_info("GLIBC_2.2.5", "GLIBC_2.35"))
            source += (
                f"{binary})\n"
                f"  printf '%s' {shlex.quote(output)}\n"
                f"  printf '%s' {shlex.quote(warnings.get(binary, ''))} >&2\n"
                f"  exit {failures.get(binary, 0)} ;;\n"
            )
        source += "*) exit 96 ;;\nesac\n"
        path = self.tools / "readelf"
        path.write_text(source)
        path.chmod(0o755)

    def package(self, variant):
        self.packages += 1
        self.invocations.unlink(missing_ok=True)
        dist = self.work / f"{variant}-{self.packages}"
        result = subprocess.run(
            ["/bin/sh", str(ROOT / "scripts/package-linux.sh"), "--variant", variant],
            env=self.environment | {"CAGENTS_DIST_DIR": str(dist)},
            capture_output=True, text=True, timeout=30,
        )
        return result, dist

    def dependencies(self, variant):
        result, dist = self.package(variant)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            self.invocations.read_text().splitlines(),
            ["cagents", "cagents-ui", "cagentsd"],
        )
        name = "cagents" if variant == "full" else "cagents-browserless"
        metadata = subprocess.check_output(
            ["dpkg-deb", "--field", str(dist / f"{name}_{VERSION}_amd64.deb"), "Depends"],
            text=True,
        )
        return {dependency.strip() for dependency in metadata.split(",")}

    def refused(self, variant, diagnostic):
        result, dist = self.package(variant)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(diagnostic, result.stderr)
        self.assertFalse(dist.exists() and any(dist.iterdir()), "no partial package may be published")

    def test_only_integrated_package_requires_egl(self):
        runtime_packages = provisioned_runtime_packages()
        for variant in ("full", "browserless"):
            with self.subTest(variant=variant):
                dependencies = self.dependencies(variant)
                self.assertIn("libc6 (>= 2.35)", dependencies)
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

    def test_explicit_strip_opt_out_preserves_all_executable_payloads(self):
        originals = {
            binary: (self.binaries / binary).read_bytes()
            for binary in ("cagents", "cagents-ui", "cagentsd")
        }
        strip_calls = self.work / "strip-calls"
        marker = b"# fixture strip mutation\n"
        # A real byte change makes both an ignored opt-out and accidentally
        # disabled default stripping observable in the generated artifacts.
        (self.tools / "strip").write_text(
            '#!/bin/sh\n[ "$#" = 4 ] && [ "$1" = --strip-unneeded ] || exit 97\n'
            'shift\nfor binary in "$@"; do\n'
            f'  printf "%s\\n" "${{binary##*/}}" >> {shlex.quote(str(strip_calls))}\n'
            '  printf "# fixture strip mutation\\n" >> "$binary"\ndone\n'
        )
        for variant in ("full", "browserless"):
            for preserve in (False, True):
                with self.subTest(variant=variant, preserve=preserve):
                    strip_calls.unlink(missing_ok=True)
                    self.environment.pop("CAGENTS_SKIP_STRIP", None)
                    if preserve:
                        self.environment["CAGENTS_SKIP_STRIP"] = "1"
                    result, dist = self.package(variant)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertEqual(
                        strip_calls.read_text().splitlines() if strip_calls.exists() else [],
                        [] if preserve else list(originals),
                    )
                    self.assertEqual(self.invocations.read_text().splitlines(), list(originals))
                    name = "cagents" if variant == "full" else "cagents-browserless"
                    archive = dist / f"{name}-{VERSION}-x86_64-unknown-linux-gnu.tar.gz"
                    deb = dist / f"{name}_{VERSION}_amd64.deb"
                    deb_payload = subprocess.check_output(
                        ["dpkg-deb", "--fsys-tarfile", str(deb)], timeout=10,
                    )
                    with tarfile.open(archive, "r:gz") as portable, \
                            tarfile.open(fileobj=io.BytesIO(deb_payload), mode="r:") as debian:
                        for binary, original in originals.items():
                            expected = original if preserve else original + marker
                            for payload, member in (
                                (portable, f"{name}-{VERSION}/bin/{binary}"),
                                (debian, f"./usr/libexec/{name}/{binary}"),
                            ):
                                with self.subTest(binary=binary, member=member):
                                    self.assertEqual(payload.extractfile(member).read(), expected)
                            self.assertEqual((self.binaries / binary).read_bytes(), original)

    def test_newer_requirement_from_each_packaged_elf_raises_both_variants(self):
        for newer in ("cagents", "cagents-ui", "cagentsd"):
            self.readelf({newer: self.version_info("GLIBC_2.2.5", "GLIBC_2.39")})
            for variant in ("full", "browserless"):
                with self.subTest(binary=newer, variant=variant):
                    dependencies = self.dependencies(variant)
                    self.assertIn("libc6 (>= 2.39)", dependencies)
                    self.assertNotIn("libc6 (>= 2.35)", dependencies)

    def test_numeric_maximum_preserves_baseline_and_orders_double_and_patch_versions(self):
        for versions, expected in [
            (("2.9", "2.2.5", "2.34"), "2.35"),
            (("2.39", "2.9", "2.100"), "2.100"),
            (("2.35.1", "2.9", "2.35"), "2.35.1"),
        ]:
            self.readelf({
                binary: self.version_info(f"GLIBC_{version}")
                for binary, version in zip(("cagents", "cagents-ui", "cagentsd"), versions)
            })
            for variant in ("full", "browserless"):
                with self.subTest(versions=versions, variant=variant):
                    self.assertIn(f"libc6 (>= {expected})", self.dependencies(variant))

    def test_failed_inspection_is_not_hidden_by_valid_stdout_or_later_binaries(self):
        for binary in ("cagents", "cagents-ui", "cagentsd"):
            self.readelf({}, failures={binary: 7})
            for variant in ("full", "browserless"):
                with self.subTest(binary=binary, variant=variant):
                    self.refused(variant, f"cannot inspect glibc requirements for {binary}")

    def test_missing_malformed_or_unsupported_version_needs_refuse_packaging(self):
        valid = self.version_info("GLIBC_2.35")
        for name, output in [
            ("empty", ""),
            ("no version information", "No version information found in this file.\n"),
            ("no glibc needs despite exported glibc", self.version_info("CXXABI_1.3")),
            ("malformed numeric version", self.version_info("GLIBC_2.bad")),
            ("private ABI", self.version_info("GLIBC_PRIVATE")),
            ("unmapped named ABI", self.version_info("GLIBC_ABI_DT_RELR")),
            ("mixed valid and malformed", self.version_info("GLIBC_2.35", "GLIBC_2.39_suffix")),
            ("malformed name record", valid.replace("Name: GLIBC_2.35", "Name:GLIBC_2.35")),
            ("duplicate needs sections", valid + valid),
        ]:
            self.readelf({"cagents-ui": output})
            for variant in ("full", "browserless"):
                with self.subTest(output=name, variant=variant):
                    self.refused(variant, "missing or malformed required glibc versions for cagents-ui")

    def test_readelf_warning_and_missing_tool_refuse_before_package_output(self):
        self.readelf({}, warnings={"cagentsd": "readelf: Warning: invalid version data\n"})
        for variant in ("full", "browserless"):
            with self.subTest(reason="warning", variant=variant):
                self.refused(variant, "readelf reported invalid version information for cagentsd")
        # Leave only explicit ordinary commands and fixtures on PATH, so a real
        # host readelf cannot accidentally satisfy the missing-tool control.
        for command in (
            "awk", "date", "dpkg-deb", "gzip", "install", "mktemp", "sed",
            "sha256sum", "sort", "tar", "touch", "wc", "dirname", "uname", "getconf",
        ):
            executable = shutil.which(command)
            self.assertIsNotNone(executable, command)
            (self.tools / command).symlink_to(executable)
        (self.tools / "readelf").unlink()
        self.environment["PATH"] = str(self.tools)
        for variant in ("full", "browserless"):
            with self.subTest(reason="missing tool", variant=variant):
                self.refused(variant, "required command not found: readelf")


if __name__ == "__main__":
    unittest.main()
