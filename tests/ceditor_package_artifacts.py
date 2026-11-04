#!/usr/bin/env python3
"""Real shell/archive checks with inert Cargo products; no compilation or launch."""
import hashlib
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
PRODUCTS = ("cagents", "cagentsd", "ceditor")


class CeditorPackageArtifacts(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="ceditor-package-artifacts-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.checkout = self.directory / "checkout"
        (self.checkout / "scripts").mkdir(parents=True)
        (self.checkout / "packaging").mkdir()
        for name in ("package-ceditor-linux.sh", "build-ceditor-product"):
            shutil.copy2(ROOT / "scripts" / name, self.checkout / "scripts" / name)
        for name in ("io.github.cagents.ceditor.svg", "io.github.cagents.ceditor.desktop"):
            shutil.copy2(ROOT / "packaging" / name, self.checkout / "packaging" / name)
        shutil.copy2(ROOT / "LICENSE", self.checkout / "LICENSE")
        (self.checkout / "Cargo.toml").write_text('[package]\nname="cagents"\nversion="0.1.3"\n')
        # Cargo searches configuration from the caller cwd, independently of
        # the package script's absolute --manifest-path.
        (self.directory / ".cargo").mkdir()
        self.configuration = self.directory / ".cargo/config.toml"
        self.configuration.write_text(f'[build]\ntarget="{NATIVE}"\n')
        self.target = self.directory / 'target with spaces "quote" \\backslash'
        self.dist = self.directory / "dist"
        self.tools = self.directory / "tools"
        self.tools.mkdir()
        self.calls = self.directory / "calls.jsonl"
        for name in PRODUCTS:
            self.executable(self.target / "release" / name, f"stale:{name}\n")
            self.executable(self.target / NATIVE / "release" / name, f"stale-native:{name}\n")
        self.tool("cargo", r'''
import tomllib
args = sys.argv[1:]
if args[0] == 'metadata':
    print(json.dumps({'packages': [{'name': name, 'version': '0.1.3'}
        for name in ['cagents', 'ceditor']], 'target_directory': os.environ['CARGO_TARGET_DIR']}))
    sys.exit(0)
assert args[0] == 'build', args
name = args[args.index('--bin') + 1] if '--bin' in args else 'ceditor'
if os.environ.get('MOCK_FAIL_BUILD') == name:
    print('fixture compiler failure', file=sys.stderr)
    sys.exit(101)
config = tomllib.loads((Path.cwd() / '.cargo/config.toml').read_text())
triple = args[args.index('--target') + 1] if '--target' in args else config['build']['target']
# Opaque reported paths reject implementations that guess either release layout.
product = Path(os.environ['MOCK_PRODUCTS']) / name / triple / name
product.parent.mkdir(parents=True, exist_ok=True)
product.write_text('fresh:' + name + ':' + triple + '\n')
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
        self.tool("jq", """
metadata = json.load(sys.stdin)
name = 'ceditor' if 'ceditor' in sys.argv[-1] else 'cagents'
print(next(package['version'] for package in metadata['packages'] if package['name'] == name))
""")
        self.tool("strip", "pass\n")  # Preserve inert fixture bytes, never read an ELF.
        self.tool("uname", "print('Linux' if sys.argv[1] == '-s' else 'x86_64')\n")
        self.tool("getconf", "print('glibc 2.41')\n")
        # A minimal environment avoids inherited Cargo/package switches.
        self.environment = {
            "PATH": str(self.tools) + os.pathsep + os.defpath,
            "HOME": str(self.directory / "home"),
            "CARGO_TARGET_DIR": str(self.target), "CAGENTS_DIST_DIR": str(self.dist),
            "SOURCE_DATE_EPOCH": "0",
            "MOCK_CALLS": str(self.calls),
            "MOCK_PRODUCTS": str(self.directory / 'reported products "quote" \\path'),
        }

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

    def package(self, **overrides):
        return subprocess.run(
            ["/bin/sh", str(self.checkout / "scripts/package-ceditor-linux.sh")],
            cwd=self.directory, env=self.environment | overrides,
            capture_output=True, text=True, timeout=20,
        )

    def reset_outputs(self):
        self.calls.unlink(missing_ok=True)
        shutil.rmtree(self.dist, ignore_errors=True)

    def builds(self):
        return [args for tool, args in map(json.loads, self.calls.read_text().splitlines())
                if tool == "cargo" and args[0] == "build"]

    def assert_build_prefix(self, count):
        products = [args[args.index("--bin") + 1] if "--bin" in args else "ceditor"
                    for args in self.builds()]
        self.assertEqual(products, list(PRODUCTS[:count]))

    def assert_archive(self, fresh):
        name = f"ceditor-0.1.3-{NATIVE}.tar.gz"
        self.assertEqual(sorted(p.name for p in self.dist.iterdir()), ["SHA256SUMS-ceditor", name])
        archive = self.dist / name
        self.assertEqual((self.dist / "SHA256SUMS-ceditor").read_text(),
                         hashlib.sha256(archive.read_bytes()).hexdigest() + "  " + name + "\n")
        prefix = "ceditor-0.1.3/"
        private = prefix + "lib/ceditor/releases/0.1.3/bin/"
        with tarfile.open(archive, "r:gz") as package:
            for product in PRODUCTS:
                member = package.getmember(private + product)
                self.assertTrue(member.isfile())
                self.assertEqual(member.mode, 0o755)
                expected = f"fresh:{product}:{NATIVE}\n" if fresh else f"stale:{product}\n"
                self.assertEqual(package.extractfile(member).read(), expected.encode())
            self.assertEqual(package.extractfile(private + ".ceditor-companions").read(),
                             b"ceditor companions v1\n")
            self.assertEqual(package.getmember(prefix + "lib/ceditor/current").linkname,
                             "releases/0.1.3")
            self.assertEqual(package.getmember(prefix + "bin/ceditor").linkname,
                             "../lib/ceditor/current/bin/ceditor")

    def test_native_build_overrides_same_and_foreign_config_and_packages_exact_artifacts(self):
        for configured in (NATIVE, "aarch64-unknown-linux-gnu"):
            with self.subTest(configured=configured):
                self.reset_outputs()
                self.configuration.write_text(f'[build]\ntarget="{configured}"\n')
                result = self.package()
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assert_archive(fresh=True)
                common = ["build", "--manifest-path", str(self.checkout / "Cargo.toml"),
                          "--locked", "--release", "--target-dir", str(self.target),
                          "--target", NATIVE]
                message = ["--message-format=json-render-diagnostics"]
                self.assertEqual(self.builds(), [
                    common + ["--no-default-features", "--bin", "cagents"] + message,
                    common + ["--no-default-features", "--features", "host", "--bin", "cagentsd"] + message,
                    common + ["--package", "ceditor"] + message,
                ])

    def test_each_build_failure_preserves_101_and_leaves_no_package_output(self):
        for count, product in enumerate(PRODUCTS, 1):
            with self.subTest(product=product):
                self.reset_outputs()
                result = self.package(MOCK_FAIL_BUILD=product)
                self.assertEqual(result.returncode, 101, result.stdout + result.stderr)
                self.assertIn("fixture compiler failure", result.stderr)
                self.assert_build_prefix(count)
                self.assertFalse(self.dist.exists())

    def test_missing_or_ambiguous_artifact_stops_before_package_output(self):
        for variable in ("MOCK_MISSING_ARTIFACT", "MOCK_AMBIGUOUS_ARTIFACT"):
            for count, product in enumerate(PRODUCTS, 1):
                with self.subTest(variable=variable, product=product):
                    self.reset_outputs()
                    result = self.package(**{variable: product})
                    self.assertNotEqual(result.returncode, 0)
                    found = 0 if variable == "MOCK_MISSING_ARTIFACT" else 2
                    self.assertIn(f"exactly one executable for {product} (found {found})", result.stderr)
                    self.assert_build_prefix(count)
                    self.assertFalse(self.dist.exists())

    def test_skip_build_retains_prebuilt_layout_without_python_on_path(self):
        for name in ("dirname", "gzip", "install", "mktemp", "sha256sum", "tar", "rm", "ln", "chmod"):
            (self.tools / name).symlink_to(shutil.which(name))
        self.assertIsNone(shutil.which("python3", path=str(self.tools)))
        self.configuration.write_text('[build]\ntarget="aarch64-unknown-linux-gnu"\n')
        result = self.package(CAGENTS_SKIP_BUILD="1", PATH=str(self.tools))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.builds(), [])
        self.assert_archive(fresh=False)

    def test_existing_native_target_override_guards_remain(self):
        for override in ({"CARGO_BUILD_TARGET": NATIVE},
                         {"CAGENTS_RELEASE_TARGET": "aarch64-unknown-linux-gnu"}):
            with self.subTest(override=override):
                result = self.package(**override)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.calls.exists() and self.builds())
                self.assertFalse(self.dist.exists())


if __name__ == "__main__":
    unittest.main()
