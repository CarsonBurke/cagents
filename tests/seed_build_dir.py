#!/usr/bin/env python3
"""Exercise scripts/seed-build-dir on private directories."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import time
import unittest


CARGO_TAG = (
    "Signature: 8a477f597d28d172789f06886806bc55\n"
    "# This file is a cache directory tag created by cargo.\n"
    "# For information about cache directory tags see https://bford.info/cachedir/\n"
)
SEED = Path(__file__).resolve().parents[1] / "scripts/seed-build-dir"


class CopiedFingerprints(unittest.TestCase):
    """Real Cargo: a copied build must not pass for this checkout's sources.

    Only the copying differs from seeding, so this runs where reflinks do not.
    """

    def setUp(self):
        if shutil.which("cargo") is None:
            self.skipTest("no cargo")
        temporary = tempfile.TemporaryDirectory(prefix="cagents-seed-", dir="/var/tmp")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.main = self.root / "main"
        files = {
            "Cargo.toml": """
                [package]
                name = "probe"
                version = "0.1.0"
                edition = "2021"
                build = "build.rs"
                [dependencies]
                vendored = { path = "vendor/vendored" }
                [workspace]
            """,
            "build.rs": """
                fn main() {
                    println!("cargo:rerun-if-changed=generated.txt");
                    let text = std::fs::read_to_string("generated.txt").unwrap();
                    println!("cargo:rustc-env=PROBE_GENERATED={}", text.trim());
                }
            """,
            "generated.txt": "main",
            "src/main.rs": """
                fn main() {
                    println!("own=main vendored={} generated={}",
                             vendored::NAME, env!("PROBE_GENERATED"));
                }
            """,
            "vendor/vendored/Cargo.toml": """
                [package]
                name = "vendored"
                version = "0.1.0"
                edition = "2021"
            """,
            "vendor/vendored/src/lib.rs": 'pub const NAME: &str = "main";\n',
        }
        for name, text in files.items():
            path = self.main / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(textwrap.dedent(text))
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("CAGENTS_BUILD_", "CARGO_"))}
        for name in ("CARGO_HOME", "RUSTUP_HOME"):
            if name in os.environ:
                self.env[name] = os.environ[name]
        self.cargo(self.main)

    def cargo(self, checkout):
        return subprocess.run(["cargo", "build"], cwd=checkout, env=self.env, capture_output=True,
                              text=True, timeout=240, check=True).stderr

    def worktree(self, name):
        """A checkout with different sources, all older than main's build."""
        checkout = self.root / name
        shutil.copytree(self.main, checkout, ignore=shutil.ignore_patterns("target"))
        for edited, old, new in (
            ("src/main.rs", "own=main", "own=worktree"),
            ("vendor/vendored/src/lib.rs", '"main"', '"worktree"'),
            ("generated.txt", "main", "worktree"),
        ):
            path = checkout / edited
            path.write_text(path.read_text().replace(old, new))
        long_ago = time.time() - 86400
        for path in checkout.rglob("*"):
            os.utime(path, (long_ago, long_ago), follow_symlinks=False)
        os.utime(checkout, (long_ago, long_ago))
        return checkout

    def output(self, checkout):
        return subprocess.run([checkout / "target/debug/probe"], capture_output=True,
                              text=True, check=True).stdout.strip()

    def test_a_copy_is_stale_until_its_checkouts_crates_are_forgotten(self):
        stale = self.worktree("stale")
        subprocess.run(["cp", "-a", "--reflink=auto", self.main / "target", stale / "target"], check=True)
        self.assertIn("Finished", self.cargo(stale))
        # What the fix is for: Cargo itself sees nothing to do here.
        self.assertEqual(self.output(stale), "own=main vendored=main generated=main")

        worktree = self.worktree("worktree")
        subprocess.run(["cp", "-a", "--reflink=auto", self.main / "target", worktree / "target"], check=True)
        subprocess.run([SEED, "--forget-local", worktree / "target"], cwd=worktree, env=self.env, check=True)
        built = self.cargo(worktree)
        self.assertIn("Compiling vendored", built)
        self.assertIn("Compiling probe", built)
        self.assertEqual(self.output(worktree), "own=worktree vendored=worktree generated=worktree")

    def test_seeding_builds_this_checkouts_sources(self):
        if not reflinks(self.root):
            self.skipTest("this file system cannot reflink")
        worktree = self.worktree("worktree")
        subprocess.run([SEED, "--from", self.main / "target", "--to", worktree / "target"],
                       cwd=worktree, env=self.env, check=True, capture_output=True)
        self.assertTrue((worktree / "target/debug/.fingerprint").is_dir())
        self.cargo(worktree)
        self.assertEqual(self.output(worktree), "own=worktree vendored=worktree generated=worktree")

    def test_forgetting_reaches_a_target_triples_build(self):
        triple = subprocess.run(["rustc", "--print", "host-tuple"], cwd=self.main, env=self.env,
                                capture_output=True, text=True)
        if triple.returncode != 0:
            self.skipTest("this rustc cannot name its host")
        def build(checkout):
            subprocess.run(["cargo", "build", "--target", triple.stdout.strip(),
                            "--target-dir", "target/browserless"], cwd=checkout, env=self.env,
                           capture_output=True, text=True, timeout=240, check=True)
        build(self.main)
        worktree = self.worktree("worktree")
        subprocess.run(["cp", "-a", "--reflink=auto", self.main / "target", worktree / "target"], check=True)
        subprocess.run([SEED, "--forget-local", "target"], cwd=worktree, env=self.env, check=True)
        build(worktree)
        program = worktree / "target/browserless" / triple.stdout.strip() / "debug/probe"
        self.assertEqual(subprocess.run([program], capture_output=True, text=True, check=True).stdout.strip(),
                         "own=worktree vendored=worktree generated=worktree")

    def test_forgetting_follows_a_link_to_the_build(self):
        worktree = self.worktree("worktree")
        subprocess.run(["cp", "-a", "--reflink=auto", self.main / "target", worktree / "copied"], check=True)
        (worktree / "target").symlink_to("copied")
        subprocess.run([SEED, "--forget-local", "target"], cwd=worktree, env=self.env, check=True)
        self.assertEqual(list((worktree / "copied/debug/.fingerprint").glob("probe-*")), [])

    def test_forgetting_refuses_what_it_cannot_do(self):
        worktree = self.worktree("worktree")
        result = subprocess.run([SEED, "--forget-local", "missing"], cwd=worktree, env=self.env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("could not forget", result.stderr)
        (worktree / "Cargo.lock").write_text("")
        result = subprocess.run([SEED, "--forget-local", self.main / "target"], cwd=worktree, env=self.env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("names no crate", result.stderr)
        self.assertNotEqual(list((self.main / "target/debug/.fingerprint").glob("probe-*")), [])

    def test_a_directory_without_fingerprints_has_nothing_to_forget(self):
        worktree = self.worktree("worktree")
        (worktree / "executables/debug").mkdir(parents=True)
        (worktree / "executables/debug/probe").write_text("program")
        result = subprocess.run([SEED, "--forget-local", "executables"], cwd=worktree, env=self.env,
                                check=True, capture_output=True, text=True)
        # It says so, as this may be the wrong path.
        self.assertIn("nothing to forget", result.stderr)
        self.assertTrue((worktree / "executables/debug/probe").is_file())

    def test_forgetting_refuses_an_empty_path(self):
        # `find "/"` would otherwise search the whole host for fingerprints to remove.
        # A stand-in, so that a regression here reports itself rather than doing that.
        worktree = self.worktree("worktree")
        tools = worktree / "tools"
        tools.mkdir()
        searched = worktree / "searched"
        (tools / "find").write_text(f'#!/bin/sh\ntouch "{searched}"\n')
        (tools / "find").chmod(0o755)
        result = subprocess.run([SEED, "--forget-local", ""], cwd=worktree,
                                env={**self.env, "PATH": f"{tools}:{os.environ['PATH']}"},
                                capture_output=True, text=True, timeout=30)
        self.assertFalse(searched.exists())
        self.assertEqual(result.returncode, 2)

    def test_a_lock_file_awk_could_not_finish_is_not_believed(self):
        worktree = self.worktree("worktree")
        subprocess.run(["cp", "-a", "--reflink=auto", self.main / "target", worktree / "target"], check=True)
        tools = self.root / "tools"
        tools.mkdir()
        (tools / "awk").write_text("#!/bin/sh\necho vendored\nexit 1\n")
        (tools / "awk").chmod(0o755)
        result = subprocess.run([SEED, "--forget-local", "target"], cwd=worktree,
                                env={**self.env, "PATH": f"{tools}:{self.env['PATH']}"},
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertNotEqual(list((worktree / "target/debug/.fingerprint").glob("vendored-*")), [])

    def test_forgetting_reaches_a_profile_of_any_name_at_any_depth(self):
        worktree = self.worktree("worktree")
        fingerprints = self.main / "target/debug/.fingerprint"
        for profile in ("deps", "examples", "incremental", "a/b/c/d/release"):
            copy = worktree / "target" / profile / ".fingerprint"
            shutil.copytree(fingerprints, copy, symlinks=True)
        subprocess.run([SEED, "--forget-local", "target"], cwd=worktree, env=self.env, check=True)
        self.assertEqual(list((worktree / "target").rglob(".fingerprint/probe-*")), [])

    def test_a_fingerprint_that_could_not_be_removed_fails_in_any_profile(self):
        worktree = self.worktree("worktree")
        for profile in ("debug", "release"):
            shutil.copytree(self.main / "target/debug/.fingerprint",
                            worktree / "target" / profile / ".fingerprint", symlinks=True)
        tools = self.root / "tools"
        tools.mkdir()
        rm = tools / "rm"
        for failing in ("debug", "release"):
            with self.subTest(failing=failing):
                rm.write_text(f'#!/bin/sh\ncase "$*" in *"/{failing}/"*) exit 1;; esac\nexit 0\n')
                rm.chmod(0o755)
                env = {**self.env, "PATH": f"{tools}:{self.env['PATH']}"}
                result = subprocess.run([SEED, "--forget-local", "target"], cwd=worktree, env=env,
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("could not forget", result.stderr)

    def test_a_directory_that_could_not_be_searched_fails(self):
        if os.geteuid() == 0:
            self.skipTest("root reads the unreadable directory")
        worktree = self.worktree("worktree")
        shutil.copytree(self.main / "target/debug/.fingerprint",
                        worktree / "target/debug/.fingerprint", symlinks=True)
        hidden = worktree / "target/release"
        hidden.mkdir()
        hidden.chmod(0)
        self.addCleanup(hidden.chmod, 0o700)
        result = subprocess.run([SEED, "--forget-local", "target"], cwd=worktree, env=self.env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("could not forget", result.stderr)

    def test_a_stray_lock_file_is_not_taken_for_the_workspaces(self):
        worktree = self.worktree("worktree")
        (worktree / "vendor/vendored/Cargo.lock").write_text("")
        (worktree / "Cargo.toml").write_text((worktree / "Cargo.toml").read_text().replace(
            "[workspace]", '[workspace]\nmembers = ["vendor/vendored"]'))
        subprocess.run(["cp", "-a", "--reflink=auto", self.main / "target", worktree / "target"], check=True)
        subprocess.run([SEED, "--forget-local", worktree / "target"], cwd=worktree / "vendor/vendored",
                       env=self.env, check=True)
        self.assertEqual(list((worktree / "target/debug/.fingerprint").glob("vendored-*")), [])

    def test_forgetting_needs_a_lock_file(self):
        empty = self.root / "empty"
        empty.mkdir()
        result = subprocess.run([SEED, "--forget-local", empty], cwd=empty, env=self.env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("no Cargo.lock", result.stderr)


def reflinks(directory: Path) -> bool:
    source = directory / "probe-source"
    source.write_bytes(b"probe")
    result = subprocess.run(
        ["cp", "--reflink=always", source, directory / "probe-copy"],
        capture_output=True,
    )
    for path in (source, directory / "probe-copy"):
        path.unlink(missing_ok=True)
    return result.returncode == 0


class SeedBuildDir(unittest.TestCase):
    def setUp(self):
        # Reflinks need a file system that has them, which a RAM-backed /tmp
        # usually does not. /var/tmp is on disk.
        self.temporary = tempfile.TemporaryDirectory(prefix="cagents-seed-", dir="/var/tmp")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "main/target"
        for profile in ("debug", "dev-runtime"):
            for part in ("deps", "build/zstd-sys-1/out", ".fingerprint/zstd-sys-1"):
                (self.source / profile / part).mkdir(parents=True)
            (self.source / profile / "deps/libzstd-1.rlib").write_bytes(b"rlib")
            (self.source / profile / ".fingerprint/zstd-sys-1/lib-zstd").write_text("fingerprint")
            (self.source / profile / "incremental/cagents-1").mkdir(parents=True)
        # Fingerprints compare times, so the copy must keep them.
        self.stamp = 1_700_000_000
        os.utime(self.source / "debug/deps/libzstd-1.rlib", (self.stamp, self.stamp))
        self.checkout = self.root / "worktree"
        self.checkout.mkdir()
        (self.checkout / "Cargo.toml").write_text("[workspace]\n")
        (self.checkout / "Cargo.lock").write_text(textwrap.dedent('''
            version = 4

            [[package]]
            name = "cagents"
            version = "0.1.0"
            dependencies = ["zstd-sys"]

            [[package]]
            name = "vendored-crate"
            version = "1.0.0"

            [[package]]
            name = "zstd-sys"
            version = "2.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        '''))
        self.destination = self.checkout / "target"
        self.env = dict(os.environ)
        self.env.pop("CAGENTS_BUILD_SEED", None)

    def seed(self, *arguments, **changes):
        return subprocess.run(
            [str(SEED), "--from", str(self.source), "--to", str(self.destination), *arguments],
            cwd=self.checkout, env={**self.env, **changes},
            capture_output=True, text=True, timeout=60,
        )

    def seed_to_cargos_choice(self, tools):
        """Seed with no --to, under a `cargo` from `tools` that says where to."""
        return subprocess.run(
            [str(SEED), "--from", str(self.source)], cwd=self.checkout,
            env={**self.env, "PATH": f"{tools}:{os.environ['PATH']}"},
            capture_output=True, text=True, timeout=60,
        )

    def fingerprints(self, *names):
        for name in names:
            (self.source / "debug/.fingerprint" / name).mkdir(parents=True)

    def test_this_checkouts_own_crates_keep_no_fingerprint(self):
        if not reflinks(self.root):
            self.skipTest("this file system cannot reflink")
        self.fingerprints("cagents-0a1b", "cagents-2c3d", "vendored-crate-4e5f",
                          "cagents-browser-0a1b", "cagents-browser-import-0a1b")
        result = self.seed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            sorted(path.name for path in (self.destination / "debug/.fingerprint").iterdir()),
            ["cagents-browser-0a1b", "cagents-browser-import-0a1b", "zstd-sys-1"],
        )
        # The source keeps its own, and its artifacts remain for reuse.
        self.assertTrue((self.source / "debug/.fingerprint/cagents-0a1b").is_dir())
        self.assertTrue((self.destination / "debug/deps/libzstd-1.rlib").is_file())

    def test_every_way_of_writing_a_lock_file_names_its_own_crates(self):
        if not reflinks(self.root):
            self.skipTest("this file system cannot reflink")
        lock = self.checkout / "Cargo.lock"
        # A checkout's own crate sorted last, which what follows must not hide.
        written = lock.read_text() + '\n[[package]]\nname = "zzz-local"\nversion = "1.0.0"\n'
        for name, text in (
            ("CRLF", written.replace("\n", "\r\n")),
            ("unused patch", written + '\n[[patch.unused]]\nname = "unused"\nversion = "1.0.0"\n'),
            ("metadata", written + '\n[metadata]\nname = "not-a-package"\n'),
        ):
            with self.subTest(name):
                lock.write_text(text)
                self.fingerprints("cagents-0a1b", "vendored-crate-4e5f", "zzz-local-6a7b")
                self.assertEqual(self.seed().returncode, 0)
                self.assertEqual(
                    sorted(path.name for path in (self.destination / "debug/.fingerprint").iterdir()),
                    ["zstd-sys-1"])
                shutil.rmtree(self.source / "debug/.fingerprint/cagents-0a1b")
                shutil.rmtree(self.source / "debug/.fingerprint/vendored-crate-4e5f")
                shutil.rmtree(self.source / "debug/.fingerprint/zzz-local-6a7b")
                shutil.rmtree(self.destination)

    def test_a_lock_file_naming_no_crate_of_its_own_is_not_seeded(self):
        if not reflinks(self.root):
            self.skipTest("this file system cannot reflink")
        (self.checkout / "Cargo.lock").write_text("")
        result = self.seed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("not seeding", result.stderr)
        self.assertEqual(list(self.destination.glob("*/.fingerprint")), [])

    def test_a_checkout_without_a_lock_file_is_not_seeded(self):
        if not reflinks(self.root):
            self.skipTest("this file system cannot reflink")
        (self.checkout / "Cargo.lock").unlink()
        result = self.seed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("no Cargo.lock", result.stderr)
        self.assertEqual(list(self.destination.glob("*/.fingerprint")), [])

    def test_a_new_build_directory_gets_the_compiled_dependencies(self):
        if not reflinks(self.root):
            self.skipTest("this file system cannot reflink")
        # A seeder killed outright leaves its staging directory behind.
        (self.destination / ".seeding.killed/debug").mkdir(parents=True)
        result = self.seed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("seeded", result.stderr)
        self.assertEqual(
            (self.destination / "debug/deps/libzstd-1.rlib").stat().st_mtime, self.stamp)
        for profile in ("debug", "dev-runtime"):
            self.assertEqual((self.destination / profile / "deps/libzstd-1.rlib").read_bytes(), b"rlib")
            self.assertTrue((self.destination / profile / "build/zstd-sys-1/out").is_dir())
            self.assertTrue((self.destination / profile / ".fingerprint/zstd-sys-1/lib-zstd").is_file())
            # Incremental caches belong to the checkout's own crates.
            self.assertFalse((self.destination / profile / "incremental").exists())
        # Cargo tags only directories it creates; the pruner needs the tag.
        self.assertEqual((self.destination / "CACHEDIR.TAG").read_text(), CARGO_TAG)
        leftovers = [path.name for path in self.destination.iterdir() if path.name.startswith(".seeding")]
        self.assertEqual(leftovers, [])
        # Seeded once, it is a build directory like any other.
        (self.source / "debug/deps/libzstd-1.rlib").write_bytes(b"newer")
        result = self.seed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.destination / "debug/deps/libzstd-1.rlib").read_bytes(), b"rlib")

    def test_the_destination_is_the_build_directory_else_the_target(self):
        if not reflinks(self.root):
            self.skipTest("this file system cannot reflink")
        tools = self.root / "tools"
        tools.mkdir()
        cargo = tools / "cargo"
        wrong = self.root / "not-this-one"
        for metadata in (f'"target_directory":"{self.destination}"',
                         f'"build_directory":"{self.destination}","target_directory":"{wrong}"',
                         f'"target_directory":"{wrong}","build_directory":"{self.destination}"'):
            with self.subTest(metadata=metadata):
                shutil.rmtree(self.destination, ignore_errors=True)
                cargo.write_text(f"""#!/bin/sh
printf '%s\\n' '{{"workspace_root":"{self.checkout}",{metadata}}}'
""")
                cargo.chmod(0o755)
                result = self.seed_to_cargos_choice(tools)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((self.destination / "debug/deps/libzstd-1.rlib").read_bytes(), b"rlib")
                self.assertFalse(wrong.exists())

    def test_cargo_failing_to_describe_the_workspace_leaves_seeding_undone(self):
        tools = self.root / "tools"
        tools.mkdir()
        # What it printed before failing is not believed.
        (tools / "cargo").write_text(
            f"""#!/bin/sh
printf '%s\\n' '{{"workspace_root":"{self.checkout}","target_directory":"{self.destination}"}}'
exit 37
""")
        (tools / "cargo").chmod(0o755)
        result = self.seed_to_cargos_choice(tools)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.destination.exists())

    def test_a_build_holding_either_cargo_lock_delays_the_copy(self):
        if not reflinks(self.root):
            self.skipTest("this file system cannot reflink")
        for lock in (".cargo-lock", ".cargo-build-lock"):
            with self.subTest(lock=lock):
                for profile in ("debug", "dev-runtime"):
                    subprocess.run(["rm", "-rf", "--", self.destination / profile], check=True)
                holder = subprocess.Popen(
                    ["flock", "-x", self.source / "debug" / lock, "sleep", "2"])
                self.addCleanup(holder.kill)
                time.sleep(0.3)
                started = time.monotonic()
                result = self.seed()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("seeded", result.stderr)
                self.assertGreaterEqual(time.monotonic() - started, 1.5)
                holder.wait(timeout=10)

    def test_a_built_directory_is_left_alone(self):
        (self.destination / "debug/.fingerprint").mkdir(parents=True)
        (self.destination / ".seed-lock").touch()
        result = self.seed()
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        self.assertFalse((self.destination / "debug/deps").exists())
        self.assertFalse((self.destination / "dev-runtime").exists())
        # Seeded before seeding tagged directories, it still gets the tag.
        self.assertEqual((self.destination / "CACHEDIR.TAG").read_text(), CARGO_TAG)
        # A directory Cargo built itself, such as the main checkout's, is not
        # made prunable.
        (self.destination / "CACHEDIR.TAG").unlink()
        (self.destination / ".seed-lock").unlink()
        result = self.seed()
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        self.assertFalse((self.destination / "CACHEDIR.TAG").exists())

    def test_nothing_to_copy_or_opted_out_does_nothing(self):
        for name, run in (
            ("missing source", lambda: subprocess.run(
                [str(SEED), "--from", str(self.root / "absent"), "--to", str(self.destination)],
                env=self.env, capture_output=True, text=True, timeout=60)),
            ("opted out", lambda: self.seed(CAGENTS_BUILD_SEED="0")),
            ("same directory", lambda: subprocess.run(
                [str(SEED), "--from", str(self.source), "--to", str(self.source)],
                env=self.env, capture_output=True, text=True, timeout=60)),
        ):
            with self.subTest(case=name):
                result = run()
                self.assertEqual((result.returncode, result.stderr), (0, ""))
                self.assertFalse((self.destination / "debug").exists())

    def test_a_file_system_without_reflinks_leaves_the_destination_unbuilt(self):
        shm = Path("/dev/shm")
        if not shm.is_dir():
            self.skipTest("no tmpfs to stand in for a file system without reflinks")
        with tempfile.TemporaryDirectory(prefix="cagents-seed-", dir=shm) as other:
            if reflinks(Path(other)):
                self.skipTest("/dev/shm can reflink here")
            self.destination = Path(other) / "target"
            result = self.seed()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("cannot reflink", result.stderr)
            # Unbuilt, but tagged: Cargo will not tag a directory it did not create.
            self.assertEqual(sorted(path.name for path in self.destination.iterdir()),
                             [".seed-lock", "CACHEDIR.TAG"])


if __name__ == "__main__":
    unittest.main()
