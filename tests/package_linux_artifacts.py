#!/usr/bin/env python3
"""Real shell/TAR checks of artifact selection, with a private Debian boundary fixture."""
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
NATIVE = "x86_64-unknown-linux-gnu"
PRODUCTS = ("cagents", "cagentsd", "cagents-ui")
VERSION = "9.9.9"


class PackageLinuxArtifacts(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="package-linux-artifacts-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.checkout = self.directory / "checkout"
        for leaf in ("scripts", "packaging"):
            (self.checkout / leaf).mkdir(parents=True)
        for name in ("package-linux.sh", "build-ceditor-product"):
            shutil.copy2(ROOT / "scripts" / name, self.checkout / "scripts" / name)
        for name in ("dev.cagents.Cagents.desktop", "dev.cagents.Cagents.Daemon.desktop",
                     "dev.cagents.Cagents.svg", "dev.cagents.Cagents.png",
                     "dev.cagents.Cagents.metainfo.xml", "cagents-package-lifecycle",
                     "cagents-uninstall"):
            shutil.copy2(ROOT / "packaging" / name, self.checkout / "packaging" / name)
        for name in ("LICENSE", "install.sh"):
            shutil.copy2(ROOT / name, self.checkout / name)
        (self.checkout / "Cargo.toml").write_text('[package]\nversion = "9.9.9"\n')
        (self.directory / ".cargo").mkdir()
        self.configuration = self.directory / ".cargo/config.toml"
        self.configuration.write_text(f'[build]\ntarget="{NATIVE}"\n')
        self.target = self.directory / 'target with spaces "quote" \\backslash'
        self.reported = self.directory / 'reported products "quote" \\path'
        self.dist = self.directory / "dist"
        self.tools = self.directory / "tools"
        self.tools.mkdir()
        self.calls = self.directory / "calls.jsonl"
        self.protocol = self.directory / "protocol-calls"
        self.decoys = {}
        for layout in ("release", f"{NATIVE}/release"):
            for name in PRODUCTS:
                path = self.target / layout / name
                self.executable(path, f"#!/bin/sh\n# stale {layout} {name}\nexit 88\n")
                self.decoys[path] = path.read_bytes()
        self.tool("cargo", r'''
import tomllib
args = sys.argv[1:]
if args[0] == 'tree':
    print('fixture v1')
    sys.exit(0)
assert args[0] == 'build', args
name = args[args.index('--bin') + 1]
if os.environ.get('MOCK_BUILD_FAILURE') == name:
    print('fixture compiler failure: ' + name, file=sys.stderr)
    sys.exit(101)
config = tomllib.loads((Path.cwd() / '.cargo/config.toml').read_text())
triple = args[args.index('--target') + 1] if '--target' in args else config['build']['target']
product = Path(os.environ['MOCK_PRODUCTS']) / name / triple / name
product.parent.mkdir(parents=True, exist_ok=True)
body = '#!/bin/sh\n# fresh:' + name + ':' + triple + '\n'
if name == 'cagents':
    body += '[ "$*" = "daemon protocol-version" ] || exit 92\n'
    body += 'printf "fresh\\n" >> "$MOCK_PROTOCOL"\nprintf "127\\n"\n'
else:
    body += 'exit 93\n'
product.write_text(body)
product.chmod(0o755)
message = {'reason': 'compiler-artifact', 'target': {'name': name, 'kind': ['bin']},
           'executable': str(product)}
if os.environ.get('MOCK_MISSING_ARTIFACT') != name:
    print(json.dumps(message))
if os.environ.get('MOCK_AMBIGUOUS_ARTIFACT') == name:
    other = product.with_name('other-' + name)
    other.write_bytes(product.read_bytes())
    other.chmod(0o755)
    print(json.dumps(dict(message, executable=str(other))))
''')
        self.tool("zig", "print(json.dumps({'zig_exe': str(Path(sys.argv[0]).resolve())}, indent=2))\n")
        self.tool("strip", "pass\n")
        self.tool("uname", "print('Linux' if sys.argv[1] == '-s' else 'x86_64')\n")
        self.tool("getconf", "print('glibc 2.99')\n")
        # Inspect the exact staged Debian tree/control bytes without requiring
        # host dpkg. This fixture archive is not a Debian-format qualification.
        self.tool("dpkg-deb", r'''
import tarfile
args = sys.argv[1:]
if args[:2] == ['--root-owner-group', '--build']:
    assert len(args) == 4, args
    with tarfile.open(args[3], 'w') as archive:
        archive.add(args[2], arcname='.')
elif args[0] == '--fsys-tarfile':
    assert len(args) == 2, args
    sys.stdout.buffer.write(Path(args[1]).read_bytes())
elif args[0] == '--field':
    assert len(args) == 3, args
    with tarfile.open(args[1], 'r:') as archive:
        control = archive.extractfile('./DEBIAN/control').read().decode()
    print(next(line.split(': ', 1)[1] for line in control.splitlines()
               if line.startswith(args[2] + ': ')))
else:
    raise AssertionError(args)
''')
        self.tool("readelf", r'''
assert sys.argv[1:3] == ['--wide', '--version-info'], sys.argv
name = Path(sys.argv[3]).name
version = {'cagents': '2.2.5', 'cagentsd': '2.36', 'cagents-ui': '2.39'}[name]
print("Version definition section '.gnu.version_d' contains 1 entry:")
print('  0x0000: Rev: 1 Flags: none Index: 2 Cnt: 1 Name: GLIBC_999.0')
print("Version needs section '.gnu.version_r' contains 1 entry:")
print('  0x0000: Version: 1 File: libc.so.6 Cnt: 1')
print('  0x0010: Name: GLIBC_' + version + ' Flags: none Version: 2')
''')
        self.environment = os.environ.copy()
        for key in ("CARGO_BUILD_TARGET", "CAGENTS_SKIP_BUILD", "CAGENTS_SKIP_STRIP"):
            self.environment.pop(key, None)
        self.environment.update(
            PATH=str(self.tools) + os.pathsep + os.defpath,
            HOME=str(self.directory / "home"), CARGO_TARGET_DIR=str(self.target),
            CAGENTS_DIST_DIR=str(self.dist), CAGENTS_RELEASE_TARGET=NATIVE,
            VERSION=VERSION, SOURCE_DATE_EPOCH="0",
            MOCK_CALLS=str(self.calls), MOCK_PRODUCTS=str(self.reported),
            MOCK_PROTOCOL=str(self.protocol),
        )

    @staticmethod
    def executable(path, text):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        path.chmod(0o755)

    def tool(self, name, body):
        self.executable(self.tools / name, f"#!{sys.executable}\n" + """
import json,os,sys
from pathlib import Path
with Path(os.environ['MOCK_CALLS']).open('a') as output:
    output.write(json.dumps([Path(sys.argv[0]).name, sys.argv[1:]]) + '\\n')
""" + body)

    def reset_outputs(self):
        self.calls.unlink(missing_ok=True)
        self.protocol.unlink(missing_ok=True)
        shutil.rmtree(self.dist, ignore_errors=True)

    def package(self, variant, **overrides):
        return subprocess.run(
            ["/bin/sh", str(self.checkout / "scripts/package-linux.sh"), "--variant", variant],
            cwd=self.directory, env=self.environment | overrides,
            capture_output=True, text=True, timeout=30,
        )

    def events(self, tool):
        return [args for name, args in map(json.loads, self.calls.read_text().splitlines())
                if name == tool]

    def builds(self):
        return [args for args in self.events("cargo") if args[0] == "build"]

    def assert_build_prefix(self, count):
        self.assertEqual([args[args.index("--bin") + 1] for args in self.builds()],
                         list(PRODUCTS[:count]))

    def assert_packages(self, variant, fresh=True):
        name = "cagents" if variant == "full" else "cagents-browserless"
        archive = self.dist / f"{name}-{VERSION}-{NATIVE}.tar.gz"
        deb = self.dist / f"{name}_{VERSION}_amd64.deb"
        deb_payload = subprocess.check_output(["dpkg-deb", "--fsys-tarfile", str(deb)],
                                              env=self.environment, timeout=10)
        with tarfile.open(archive, "r:gz") as portable, \
                tarfile.open(fileobj=io.BytesIO(deb_payload), mode="r:") as debian:
            for product in PRODUCTS:
                expected = ((self.reported / product / NATIVE / product).read_bytes() if fresh
                            else (self.target / "release" / product).read_bytes())
                for payload, prefix in ((portable, f"{name}-{VERSION}/bin/"),
                                        (debian, f"./usr/libexec/{name}/")):
                    with self.subTest(product=product, prefix=prefix):
                        member = payload.getmember(prefix + product)
                        self.assertEqual(member.mode, 0o755)
                        self.assertEqual(payload.extractfile(member).read(), expected)
            daemon = ((self.reported / "cagentsd" / NATIVE / "cagentsd").read_bytes() if fresh
                      else (self.target / "release/cagentsd").read_bytes())
            marker = debian.extractfile(f"./usr/libexec/{name}/.cagents-daemon-protocol").read()
            self.assertEqual(marker, f"127 {hashlib.sha256(daemon).hexdigest()}\n".encode())
        depends = subprocess.check_output(["dpkg-deb", "--field", str(deb), "Depends"],
                                          env=self.environment, text=True, timeout=10)
        self.assertIn("libc6 (>= 2.39)", depends)
        self.assertEqual("libegl1" in depends, variant == "full")
        self.assertEqual("xdg-utils" in depends, variant == "browserless")
        checksum = "SHA256SUMS" if variant == "full" else "SHA256SUMS-browserless"
        entries = (self.dist / checksum).read_text().splitlines()
        expected = {archive.name, deb.name, f"{name}-{VERSION}-sizes.txt"}
        if variant == "full":
            expected.add("install.sh")
        self.assertEqual({line.split("  ", 1)[1] for line in entries}, expected)
        self.assertEqual(len(entries), len(expected))
        self.assertEqual({path.name for path in self.dist.iterdir()}, expected | {checksum})
        for line in entries:
            digest, filename = line.split("  ", 1)
            self.assertEqual(hashlib.sha256((self.dist / filename).read_bytes()).hexdigest(), digest)
        self.assertEqual(self.protocol.read_text().splitlines(), ["fresh"])
        self.assertEqual([Path(args[2]).name for args in self.events("readelf")],
                         ["cagents", "cagents-ui", "cagentsd"])

    def test_native_build_uses_exact_artifacts_for_both_variants_and_configured_targets(self):
        for variant in ("full", "browserless"):
            for configured in (NATIVE, "aarch64-unknown-linux-gnu"):
                with self.subTest(variant=variant, configured=configured):
                    self.reset_outputs()
                    self.configuration.write_text(f'[build]\ntarget="{configured}"\n')
                    result = self.package(variant)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assert_packages(variant)
                    common = ["build", "--manifest-path", str(self.checkout / "Cargo.toml"),
                              "--locked", "--release", "--target-dir", str(self.target),
                              "--target", NATIVE]
                    defaults = ["--no-default-features"] if variant == "browserless" else []
                    ui = "ui" if variant == "browserless" else "integrated-browser"
                    message = ["--message-format=json-render-diagnostics"]
                    self.assertEqual(self.builds(), [
                        common + defaults + ["--bin", "cagents"] + message,
                        common + defaults + ["--features", "host", "--bin", "cagentsd"] + message,
                        common + defaults + ["--features", ui, "--bin", "cagents-ui"] + message,
                    ])
                    for path, original in self.decoys.items():
                        self.assertEqual(path.read_bytes(), original)

    def test_each_build_failure_preserves_101_before_protocol_or_package_output(self):
        for variant in ("full", "browserless"):
            for count, product in enumerate(PRODUCTS, 1):
                with self.subTest(variant=variant, product=product):
                    self.reset_outputs()
                    result = self.package(variant, MOCK_BUILD_FAILURE=product)
                    self.assertEqual(result.returncode, 101, result.stdout + result.stderr)
                    self.assertIn("fixture compiler failure: " + product, result.stderr)
                    self.assert_build_prefix(count)
                    self.assertFalse(self.protocol.exists())
                    self.assertEqual(self.events("readelf"), [])
                    self.assertFalse(self.dist.exists())

    def test_missing_or_ambiguous_artifact_refuses_before_protocol_or_package_output(self):
        for variant in ("full", "browserless"):
            for variable in ("MOCK_MISSING_ARTIFACT", "MOCK_AMBIGUOUS_ARTIFACT"):
                for count, product in enumerate(PRODUCTS, 1):
                    with self.subTest(variant=variant, variable=variable, product=product):
                        self.reset_outputs()
                        result = self.package(variant, **{variable: product})
                        self.assertNotEqual(result.returncode, 0)
                        found = 0 if variable == "MOCK_MISSING_ARTIFACT" else 2
                        self.assertIn(f"exactly one executable for {product} (found {found})",
                                      result.stderr)
                        self.assert_build_prefix(count)
                        self.assertFalse(self.protocol.exists())
                        self.assertEqual(self.events("readelf"), [])
                        self.assertFalse(self.dist.exists())

    def test_prebuilt_layout_does_not_require_python(self):
        for command in ("awk", "date", "gzip", "install", "mktemp", "sed",
                        "sha256sum", "sort", "tar", "touch", "wc", "dirname", "rm",
                        "chmod", "ln", "find", "cat"):
            (self.tools / command).symlink_to(shutil.which(command))
        self.assertIsNone(shutil.which("python3", path=str(self.tools)))
        for product in PRODUCTS:
            body = (self.reported / product / NATIVE / product)
            body.parent.mkdir(parents=True, exist_ok=True)
            self.executable(body, "#!/bin/sh\n# prebuilt\n" +
                            ('printf "fresh\\n" >> "$MOCK_PROTOCOL"\nprintf "127\\n"\n'
                             if product == "cagents" else "exit 93\n"))
            shutil.copy2(body, self.target / "release" / product)
        for variant in ("full", "browserless"):
            with self.subTest(variant=variant):
                self.reset_outputs()
                result = self.package(variant, CAGENTS_SKIP_BUILD="1", PATH=str(self.tools))
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.builds(), [])
                self.assert_packages(variant, fresh=False)


if __name__ == "__main__":
    unittest.main()
