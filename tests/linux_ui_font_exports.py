#!/usr/bin/env python3
"""Real build-script/ELF interposition contracts; requires rustc, cc, BFD and LLD.

These small compiler/linker fixtures do not build Cargo or launch cagents. They
are separate from the full-package and actual integrated-browser qualification.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class LinuxUiFontExports(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        retained = os.environ.get("FONT_EXPORT_EVIDENCE_ROOT")
        if retained is None:
            cls.temporary = tempfile.TemporaryDirectory(prefix="linux-ui-font-exports-")
            cls.addClassCleanup(cls.temporary.cleanup)
            cls.directory = Path(cls.temporary.name)
        else:
            cls.directory = Path(retained)
            if not cls.directory.is_absolute() or cls.directory.exists():
                raise AssertionError("Evidence root must be a new absolute task-owned directory")
            cls.directory.mkdir(mode=0o700, parents=True)
        cls.commands = []
        cls.script = cls.directory / "actual-build-script"
        cls.command(["rustc", "--edition=2024", str(ROOT / "build.rs"), "-o", str(cls.script)], timeout=30)

    @classmethod
    def command(cls, arguments, *, timeout, env=None):
        directory = cls.directory / ("command-" + str(len(cls.commands)))
        directory.mkdir(mode=0o700)
        record = {"argv": arguments, "timeout": timeout, "exit": None}
        cls.commands.append(record)
        try:
            result = subprocess.run(arguments, env=env, capture_output=True, text=True, timeout=timeout)
        except BaseException as error:
            record.update(error_type=type(error).__name__, error=str(error))
            (directory / "command.json").write_text(json.dumps(record, indent=2) + "\n")
            raise
        record["exit"] = result.returncode
        for name, value in (("stdout", result.stdout), ("stderr", result.stderr)):
            (directory / (name + ".log")).write_text(value)
            record[name + "_sha256"] = hashlib.sha256(value.encode()).hexdigest()
        (directory / "command.json").write_text(json.dumps(record, indent=2) + "\n")
        if result.returncode:
            raise AssertionError(f"{arguments!r} exited {result.returncode}\n{result.stdout}\n{result.stderr}")
        return result

    def build_script(self, label, target, features):
        output = self.directory / (label + ' output with spaces "quote"')
        output.mkdir()
        environment = os.environ.copy()
        # These are Cargo's synthetic per-build-script inputs, never an agent
        # authority or runtime provenance claim. Keep opaque inherited context.
        for key in list(environment):
            if key.startswith("CARGO_FEATURE_"):
                del environment[key]
        environment.update(CARGO_CFG_TARGET_OS=target, CARGO_PKG_VERSION="9.9.9",
                           CARGO_MANIFEST_DIR=str(output), OUT_DIR=str(output))
        environment.update({"CARGO_FEATURE_" + feature: "1" for feature in features})
        result = self.command([str(self.script)], env=environment, timeout=10)
        instructions = [line for line in result.stdout.splitlines()
                        if line.startswith("cargo:rustc-link-")]
        return output, instructions

    def test_actual_cargo_instructions_are_only_for_linux_integrated_ui_binary(self):
        for target, features in (
            ("linux", ["FULL"]),
            ("linux", ["UI"]),
            ("linux", ["HOST"]),
            ("macos", ["UI", "FULL", "INTEGRATED_BROWSER"]),
            ("windows", ["UI", "FULL", "INTEGRATED_BROWSER"]),
        ):
            with self.subTest(target=target, features=features):
                output, instructions = self.build_script(target + "-" + features[0], target, features)
                self.assertEqual(instructions, [])
                self.assertFalse((output / "linux-ui-font-symbols.map").exists())
        output, instructions = self.build_script(
            "linux-integrated", "linux", ["UI", "FULL", "INTEGRATED_BROWSER"])
        path = output / "linux-ui-font-symbols.map"
        self.assertEqual(path.read_text(), "{ local: hb_*; FT_*; };\n")
        self.assertEqual(instructions, [
            "cargo:rustc-link-arg-bin=cagents-ui=-Wl,--version-script=" + str(path),
        ])

    def test_real_interposition_is_removed_without_losing_runtime_export_or_import(self):
        output, instructions = self.build_script(
            "real-link", "linux", ["UI", "FULL", "INTEGRATED_BROWSER"])
        self.assertEqual(len(instructions), 1)
        prefix = "cargo:rustc-link-arg-bin=cagents-ui="
        self.assertTrue(instructions[0].startswith(prefix))
        flag = instructions[0][len(prefix):]
        system = output / "system.c"
        system.write_text("""
int hb_fixture_value(void) { return 73; }
int FT_fixture_value(void) { return 79; }
int FT_imported_api(void) { return 89; }
""")
        consumer = output / "consumer.c"
        consumer.write_text("""
extern int hb_fixture_value(void);
extern int FT_fixture_value(void);
extern int unrelated_runtime_api(void);
int system_hb_state(void) { return hb_fixture_value(); }
int system_ft_state(void) { return FT_fixture_value(); }
int required_callback(void) { return unrelated_runtime_api(); }
""")
        application = output / "application.c"
        application.write_text("""
#include <stdio.h>
int hb_fixture_value(void) { return 17; }
int FT_fixture_value(void) { return 19; }
int unrelated_runtime_api(void) { return 97; }
extern int system_hb_state(void);
extern int system_ft_state(void);
extern int required_callback(void);
extern int FT_imported_api(void);
int main(void) {
    printf("direct=%d,%d shared=%d,%d runtime=%d import=%d\\n",
        hb_fixture_value(), FT_fixture_value(), system_hb_state(),
        system_ft_state(), required_callback(), FT_imported_api());
    return 0;
}
""")
        for linker in ("bfd", "lld"):
            with self.subTest(linker=linker):
                self.assertIsNotNone(shutil.which("ld." + linker), "required fixture linker: " + linker)
                compiler = ["cc", "-fuse-ld=" + linker]
                shared_system = output / ("libfont-export-system-" + linker + ".so")
                shared_consumer = output / ("libfont-export-consumer-" + linker + ".so")
                commands = [
                    compiler + ["-shared", "-fPIC", str(system), "-o", str(shared_system)],
                    compiler + ["-shared", "-fPIC", str(consumer), "-L" + str(output),
                                "-l:" + shared_system.name, "-Wl,-rpath,$ORIGIN", "-o", str(shared_consumer)],
                ]
                for command in commands:
                    self.command(command, timeout=20)
                for variant, extra, expected in (
                    ("before", [], "direct=17,19 shared=17,19 runtime=97 import=89\n"),
                    ("after", [flag], "direct=17,19 shared=73,79 runtime=97 import=89\n"),
                ):
                    program = output / (linker + "-" + variant)
                    # Later --export-dynamic exercises the actual restriction;
                    # unrelated_runtime_api is a required named DSO callback.
                    command = compiler + [str(application), "-L" + str(output),
                        "-l:" + shared_consumer.name, "-l:" + shared_system.name,
                        "-Wl,-rpath,$ORIGIN", *extra, "-Wl,--export-dynamic", "-o", str(program)]
                    self.command(command, timeout=20)
                    actual = self.command([str(program)], timeout=5)
                    self.assertEqual(actual.stdout, expected)
                    dynamic = self.command(["readelf", "--wide", "--dyn-syms", str(program)], timeout=5)
                    self.assertEqual(dynamic.stderr, "")
                    rows = [line.split() for line in dynamic.stdout.splitlines()
                            if line.split() and line.split()[0].rstrip(":").isdigit()]
                    defined = {row[7] for row in rows if len(row) >= 8 and row[6] != "UND"}
                    imported = {row[7] for row in rows if len(row) >= 8 and row[6] == "UND"}
                    self.assertIn("unrelated_runtime_api", defined)
                    self.assertIn("FT_imported_api", imported)
                    fonts = {name for name in defined if name.startswith(("hb_", "FT_"))}
                    self.assertEqual(fonts, {"hb_fixture_value", "FT_fixture_value"} if variant == "before" else set())


if __name__ == "__main__":
    unittest.main()
