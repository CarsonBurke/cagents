"""Exercise the Android packager with an isolated checkout and tool fixtures."""

import hashlib
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/package-android"
VERSION = "1.2.3"
APK = f"cagents-android-{VERSION}.apk"
ALIAS = "cagents-android.apk"
CHECKSUMS = "SHA256SUMS-android"
PIN = hashlib.sha256(b"fixture certificate").hexdigest()

GRADLE = r"""#!/bin/bash
set -eu
[[ $* == '-- :androidApp:assembleRelease -Pcagents.releaseVersion=1.2.3 -Pcagents.releaseVersionCode=1002003' ]]
[[ -f crates/mobile-api/fixtures/v1.jsonl ]]
[[ ! -e mobile/local.properties && ! -v CAGENTS_GRADLE_LOCAL ]]
echo gradle >> "$FIXTURE/log"
out=mobile/androidApp/build/outputs/apk/release
mkdir -p "$out"
printf 'fixture-unsigned' > "$out/androidApp-release-unsigned.apk"
"""

KEYTOOL = r"""#!/bin/bash
set -eu
[[ $1 == -exportcert && $7 == */store.pass && $9 == cagents-android ]]
if [[ "$(cat "$7")" != fixture-password ]]; then
    cat "$7" >&2
    exit 1
fi
printf 'fixture certificate'
"""

APKSIGNER = r"""#!/bin/bash
set -eu
if [[ $1 == sign ]]; then
    [[ $3 == */release.p12 && $9 == file:*/store.pass && ${11} == false ]]
    [[ "$(cat "${9#file:}")" == fixture-password && "$(cat "${14}")" == fixture-unsigned ]]
    echo apksigner >> "$FIXTURE/log"
    printf 'fixture-signed' > "${13}"
else
    [[ $* == "verify -Werr --print-certs "* && "$(cat "$4")" == fixture-signed ]]
    printf 'Signer #1 certificate DN: %s\n' "${FIXTURE_DN:-CN=Fixture}"
    printf 'Signer #1 certificate SHA-256 digest: %s\n' "$FIXTURE_PIN"
fi
"""

AAPT = r"""#!/bin/bash
set -eu
[[ $1 == dump && $2 == badging && -s $3 ]]
printf "package: name='%s' versionCode='%s' versionName='%s'\n" \
    "${FIXTURE_APP_ID:-dev.cagents.mobile}" "${FIXTURE_CODE:-1002003}" "${FIXTURE_VERSION:-1.2.3}"
if [[ -v FIXTURE_DEBUGGABLE ]]; then echo application-debuggable; fi
"""


class PackageAndroid(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.checkout = self.root / "checkout"
        self.checkout.mkdir()
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.signing = self.root / "signing"
        self.signing.mkdir(mode=0o700)
        self.output = self.root / "output"
        self.home = self.root / "home"
        self.home.mkdir()
        self.log = self.root / "log"
        self.env = {
            "HOME": str(self.home), "PATH": f"{self.tools}:{os.defpath}",
            "CAGENTS_BUILD_REMOTE": socket.gethostname().split(".")[0],
            "CAGENTS_ANDROID_BUILD_TOOLS": str(self.tools),
            "CAGENTS_ANDROID_SIGNER_SHA256": PIN,
            "CAGENTS_GRADLE_LOCAL": "1",
            "FIXTURE": str(self.root), "FIXTURE_PIN": PIN,
        }
        self.script = self.write(self.checkout / "scripts/package-android", SCRIPT.read_text(), 0o755)
        self.write(self.checkout / "scripts/gradle-remote", GRADLE, 0o755)
        self.write(self.checkout / "crates/mobile-api/fixtures/v1.jsonl", "{}\n")
        self.write(self.checkout / "mobile/gradlew", "fixture")
        self.write(self.checkout / "Cargo.toml", f'[package]\nversion = "{VERSION}"\n')
        for name, body in (("aapt", AAPT), ("apksigner", APKSIGNER), ("keytool", KEYTOOL)):
            self.write(self.tools / name, body, 0o755)
        self.write(self.signing / "release.p12", "fixture private key", 0o600)
        self.write(self.signing / "store.pass", "fixture-password\n", 0o600)
        self.write(self.signing / "signer.sha256", PIN + "\n", 0o600)
        self.git("init", "-q")
        self.git("add", ".")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "commit", "-qm", "fixture release source")
        self.commit = self.git("rev-parse", "HEAD")
        self.write(self.checkout / "mobile/local.properties", "untracked")
        self.write(self.checkout / "scripts/gradle-remote", "#!/bin/bash\nexit 99\n", 0o755)

    @staticmethod
    def write(path, content, mode=0o644):
        path.parent.mkdir(exist_ok=True, parents=True)
        path.write_text(content)
        path.chmod(mode)
        return path

    def git(self, *args):
        return subprocess.check_output(["git", "-C", self.checkout, *args], env=self.env, text=True).strip()

    def run_package(self, *args):
        return subprocess.run([self.script, *args], env=self.env, check=False, text=True,
                              capture_output=True)

    def build(self, signing=None):
        return self.run_package("build", "--ref", self.commit, "--output", str(self.output),
                                "--signing-dir", str(signing or self.signing))

    def verify(self):
        return self.run_package("verify", "--dir", str(self.output), "--version", VERSION)

    def assert_refused(self, result, reason):
        self.assertEqual(result.returncode, 1, result)
        self.assertIn(reason, result.stderr)
        self.assertNotIn("fixture-password", result.stderr)

    def test_build_exports_exact_source_and_verifies_assets(self):
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text().splitlines(), ["gradle", "apksigner"])
        self.assertEqual(set(path.name for path in self.output.iterdir()),
                         {APK, ALIAS, CHECKSUMS, "android-build-receipt"})
        self.assertEqual((self.output / APK).read_bytes(), (self.output / ALIAS).read_bytes())
        manifest_digest = hashlib.sha256((self.output / CHECKSUMS).read_bytes()).hexdigest()
        self.assertEqual((self.output / "android-build-receipt").read_text(),
                         f"{self.commit}\n{manifest_digest}\n")
        shutil.rmtree(self.signing)
        self.assertEqual(self.verify().returncode, 0)
        self.assert_refused(self.build(), "--output must be a new directory")

    def test_refuses_missing_insecure_and_wrong_signing_material_before_gradle(self):
        self.assert_refused(self.build(self.root / "absent"), "No such file")
        password = self.signing / "store.pass"
        password.chmod(0o644)
        self.assert_refused(self.build(), "no group or other access")
        password.unlink()
        password.symlink_to(self.signing / "release.p12")
        self.assert_refused(self.build(), "no group or other access")
        password.unlink()
        self.write(password, "wrong-password\n", 0o600)
        self.assert_refused(self.build(), "check the keystore, alias and password")
        self.write(password, "fixture-password\n", 0o600)
        self.write(self.signing / "signer.sha256", "0" * 64, 0o600)
        self.assert_refused(self.build(), "keystore certificate differs")
        self.assertFalse(self.log.exists())
        self.assertFalse(self.output.exists())

    def test_refuses_signing_material_in_checkout_before_gradle(self):
        inside = self.checkout / "signing"
        shutil.copytree(self.signing, inside)
        self.assert_refused(self.build(inside), "outside a Git checkout")
        self.assertFalse(self.log.exists())

    def test_refuses_an_unexpected_output_signer(self):
        self.env["FIXTURE_PIN"] = "0" * 64
        self.assert_refused(self.build(), "APK signer differs")
        self.assertFalse(self.output.exists())

    def test_refuses_an_unexpected_unsigned_package_without_signing_it(self):
        self.env["FIXTURE_APP_ID"] = "dev.cagents.mobile.debug"
        self.assert_refused(self.build(), "APK must be dev.cagents.mobile")
        self.assertEqual(self.log.read_text().splitlines(), ["gradle"])
        self.assertFalse(self.output.exists())

    def test_verification_rejects_identity_and_signature_failures(self):
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        for environment, value, reason in (
            ("FIXTURE_APP_ID", "dev.cagents.mobile.debug", "APK must be dev.cagents.mobile"),
            ("FIXTURE_CODE", "1002002", "version code 1002003"),
            ("FIXTURE_VERSION", "1.2.2", "version 1.2.3"),
            ("FIXTURE_DEBUGGABLE", "1", "APK is debuggable"),
            ("FIXTURE_DN", "CN=Android Debug,O=Android,C=US", "Android debug key"),
            ("FIXTURE_PIN", "0" * 64, "APK signer differs"),
        ):
            with self.subTest(environment=environment):
                previous = self.env.get(environment)
                self.env[environment] = value
                self.assert_refused(self.verify(), reason)
                if previous is None:
                    del self.env[environment]
                else:
                    self.env[environment] = previous
        self.env["CAGENTS_ANDROID_SIGNER_SHA256"] = ""
        self.assert_refused(self.verify(), "set CAGENTS_ANDROID_SIGNER_SHA256")
        self.env["CAGENTS_ANDROID_SIGNER_SHA256"] = PIN
        for name in (APK, ALIAS):
            (self.output / name).write_text("fixture-unsigned")
        digest = hashlib.sha256(b"fixture-unsigned").hexdigest()
        (self.output / CHECKSUMS).write_text(f"{digest}  {APK}\n{digest}  {ALIAS}\n")
        self.assert_refused(self.verify(), "apksigner failed")

    def test_verification_rejects_mismatched_alias_and_checksum_records(self):
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        (self.output / ALIAS).write_text("mismatched")
        self.assert_refused(self.verify(), "differs from")
        shutil.copyfile(self.output / APK, self.output / ALIAS)
        with (self.output / CHECKSUMS).open("a") as manifest:
            manifest.write("0" * 64 + "  ../unexpected.apk\n")
        self.assert_refused(self.verify(), "does not match the two APKs")

    def test_staging_requires_matching_commit_and_omits_private_receipt(self):
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        shutil.rmtree(self.signing)
        bundle = self.root / "bundle"
        bundle.mkdir()
        arguments = ("stage", "--dir", str(self.output), "--output", str(bundle),
                     "--version", VERSION, "--source-commit")
        self.assert_refused(self.run_package(*arguments, "0" * 40), "receipt does not match")
        self.assertEqual(list(bundle.iterdir()), [])
        result = self.run_package(*arguments, self.commit)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual({path.name for path in bundle.iterdir()}, {APK, ALIAS, CHECKSUMS})
        self.assert_refused(self.run_package(*arguments, self.commit), "already contains Android assets")


if __name__ == "__main__":
    unittest.main()
