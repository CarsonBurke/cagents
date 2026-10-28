#!/usr/bin/env python3
"""Test the qualifier's first-build paths with fake Cargo, never a compiler.

Each positive case deliberately stops at the second build. It exercises actual
fixture setup, owned executable stubs and the first iteration's existing
oracles; it does not qualify Cargo's cache behavior.
"""

from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest import mock


sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "checkout_cache_qualifier", ROOT / "scripts/qualify-checkout-cache.py"
)
QUALIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(QUALIFIER)
NATIVE = "x86_64-unknown-linux-gnu"
BOUNDARY = "focused first-build boundary complete; no cache qualification"


class CargoArtifacts(unittest.TestCase):
    @contextmanager
    def fixture(self, *, mode="normal", layout="split"):
        scratch = ROOT / ".worktrees"
        scratch.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="cache artifacts ", dir=scratch) as name:
            root = Path(name)
            tools = root / "tools"
            tools.mkdir()
            calls = root / "cargo-calls.jsonl"
            executed = root / "executed.jsonl"
            cargo = tools / "cargo"
            cargo.write_text(f"#!{sys.executable}\n" + r'''
import json,os,sys
from pathlib import Path
arguments=sys.argv[1:]
assert arguments[:4]==['build','--offline','--workspace','--verbose'], arguments
assert arguments[4:] in ([],['--message-format=json-render-diagnostics']), arguments
assert os.environ['CARGO_BUILD_TARGET']=='x86_64-unknown-linux-gnu'
calls=Path(os.environ['FIXTURE_CALLS'])
prior=calls.read_text().splitlines() if calls.exists() else []
with calls.open('a') as out:out.write(json.dumps(arguments)+'\n')
mode=os.environ['FIXTURE_MODE']
if prior and mode!='changed-directives':
    print('focused first-build boundary complete; no cache qualification',file=sys.stderr)
    sys.exit(97)
target=Path(os.environ['CARGO_TARGET_DIR'])
build=Path(os.environ['CARGO_BUILD_BUILD_DIR'])
products=target/os.environ['CARGO_BUILD_TARGET']/'debug'
products.mkdir(parents=True,exist_ok=True)
cwd=Path.cwd()
workspace=(cwd/'Cargo.toml').read_text()
names=['cagents','ceditor','cache-probe'] if 'name="cagents"' in workspace else ['qualification-path-observer']
for name in names:
    product=products/name
    value=(str(cwd)+'|plain-A|generated-source-A|generated-output-A' if name=='cache-probe'
           else str(cwd)+'|input-A|'+str(cwd/'vendor/path-source') if name=='qualification-path-observer'
           else 'owned '+name+' stub')
    body=('import json\nfrom pathlib import Path\n'
          'with Path('+repr(os.environ['FIXTURE_EXECUTED'])+").open('a') as out:out.write("+repr(json.dumps(name)+'\n')+')\n'
          'print('+repr(value)+')\n')
    product.write_text('#!'+sys.executable+'\n'+body)
    product.chmod(0o755)
    message={'reason':'compiler-artifact','package_id':'fixture-'+name,
             'target':{'name':name,'kind':['bin']},'executable':str(product)}
    if mode!='missing' or name!=names[0]:print(json.dumps(message))
    if mode=='ambiguous' and name==names[0]:
        other=product.with_name('second-'+name)
        other.write_text(product.read_text());other.chmod(0o755)
        print(json.dumps(dict(message,executable=str(other))))
if names[0]=='cagents':
    # The package ID intentionally does not encode the crate name: bind the
    # library artifact's ID to build-script-executed rather than guessing it.
    print(json.dumps({'reason':'compiler-artifact','package_id':'generated-id',
                      'target':{'name':'qualification_generated','kind':['lib']},'executable':None}))
    base=build/os.environ['CARGO_BUILD_TARGET']/'debug/build'
    execution=(base/'qualification-generated-hash' if os.environ['FIXTURE_LAYOUT']=='flat'
               else base/'qualification_generated/hash')
    out_dir=execution/'out';out_dir.mkdir(parents=True,exist_ok=True)
    output=(execution/'output' if os.environ['FIXTURE_LAYOUT']=='flat' else execution/'run/stdout')
    output.parent.mkdir(parents=True,exist_ok=True)
    if mode!='missing-output':output.write_text('cargo:rerun-if-changed='+('changed' if prior else 'input.txt')+'\n')
    print(json.dumps({'reason':'build-script-executed','package_id':'generated-id','out_dir':str(out_dir)}))
    # An unrelated script and old unsuffixed directory must not replace the
    # selected generator's output, even when the real reported file is absent.
    decoy=target/'debug/build/qualification-generated-decoy/output'
    decoy.parent.mkdir(parents=True,exist_ok=True);decoy.write_text('wrong decoy directives\n')
    print(json.dumps({'reason':'build-script-executed','package_id':'unrelated-id','out_dir':str(decoy.parent/'out')}))
dependency=build/os.environ['CARGO_BUILD_TARGET']/'debug/deps'/(
    'libqualification_dependency-fixture.rlib' if names[0]=='cagents'
    else 'libqualification_independent-fixture.rlib')
dependency.parent.mkdir(parents=True,exist_ok=True)
if not dependency.exists():dependency.write_text('fixture dependency, not compiled\n')
print('Fresh qualification-dependency\nFresh qualification-independent',file=sys.stderr)
if mode=='failed':
    print('fixture Cargo failure 101 after valid artifact records',file=sys.stderr)
    sys.exit(101)
''')
            cargo.chmod(0o755)
            environment = {
                key: value for key, value in os.environ.items() if not key.startswith("GIT_")
            }
            home = root / "home"
            home.mkdir()
            environment.update(
                PATH=str(tools) + os.pathsep + os.environ["PATH"], HOME=str(home),
                XDG_CONFIG_HOME=str(home / "config"), GIT_CONFIG_NOSYSTEM="1",
                GIT_CONFIG_SYSTEM=os.devnull, GIT_CONFIG_GLOBAL=os.devnull,
                GIT_TERMINAL_PROMPT="0", CARGO_BUILD_TARGET=NATIVE,
                FIXTURE_CALLS=str(calls), FIXTURE_EXECUTED=str(executed),
                FIXTURE_MODE=mode, FIXTURE_LAYOUT=layout,
            )
            with mock.patch.dict(os.environ, environment, clear=True):
                self.assertEqual(shutil.which("cargo"), str(cargo))
                yield root, calls, executed

    def invoke(self, root, family):
        if family == "workspace":
            QUALIFIER.qualify(root, False)
        else:
            QUALIFIER.qualify_manifest_paths(root, False)

    def assert_boundary(self, family, layout="split"):
        with self.fixture(layout=layout) as (root, calls, executed):
            try:
                self.invoke(root, family)
            except FileNotFoundError as error:
                self.assertTrue(calls.exists(), "failure occurred before fake Cargo")
                self.assertEqual(len(calls.read_text().splitlines()), 1)
                self.fail(f"successful fake Cargo followed by missing guessed executable: {error.filename}")
            except RuntimeError as error:
                self.assertIn(BOUNDARY, str(error))
            else:
                self.fail("focused fixture must stop before claiming cache qualification")
            self.assertEqual(len(calls.read_text().splitlines()), 2)
            names = [json.loads(line) for line in executed.read_text().splitlines()]
            self.assertEqual(names, ["cagents", "ceditor", "cache-probe"]
                             if family == "workspace" else ["qualification-path-observer"])

    def test_workspace_paths_and_generator_output_follow_reported_native_layout(self):
        for layout in ("flat", "split"):
            with self.subTest(layout=layout):
                self.assert_boundary("workspace", layout)

    def test_manifest_observer_follows_reported_native_layout(self):
        self.assert_boundary("manifest")

    def test_failed_build_never_executes_reported_products(self):
        for family in ("workspace", "manifest"):
            with self.subTest(family=family), self.fixture(mode="failed") as (root, calls, executed):
                with self.assertRaisesRegex(RuntimeError, "fixture Cargo failure 101"):
                    self.invoke(root, family)
                self.assertEqual(len(calls.read_text().splitlines()), 1)
                self.assertFalse(executed.exists())

    def test_missing_or_ambiguous_artifact_rejects_before_execution(self):
        for family in ("workspace", "manifest"):
            for mode in ("missing", "ambiguous"):
                with self.subTest(family=family, mode=mode), self.fixture(mode=mode) as (root, calls, executed):
                    with self.assertRaisesRegex(RuntimeError, "exactly one Cargo executable"):
                        self.invoke(root, family)
                    self.assertEqual(len(calls.read_text().splitlines()), 1)
                    self.assertFalse(executed.exists())

    def test_missing_reported_generator_output_cannot_use_old_unsuffixed_decoy(self):
        with self.fixture(mode="missing-output") as (root, calls, executed):
            with self.assertRaisesRegex(AssertionError, "missing generated build-script output"):
                self.invoke(root, "workspace")
            self.assertEqual(len(calls.read_text().splitlines()), 1)
            self.assertEqual(len(executed.read_text().splitlines()), 3)

    def test_changed_generator_directives_still_fail_original_oracle(self):
        with self.fixture(mode="changed-directives") as (root, calls, executed):
            with self.assertRaisesRegex(AssertionError, "generator directives changed"):
                self.invoke(root, "workspace")
            self.assertEqual(len(calls.read_text().splitlines()), 2)
            self.assertEqual(len(executed.read_text().splitlines()), 6)


if __name__ == "__main__":
    unittest.main()
