import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

const command = readFileSync(new URL("../src/install-command.sh", import.meta.url), "utf8");

test("the website and installation guide publish the same command", () => {
  const guide = readFileSync(new URL("../../docs/installation.md", import.meta.url), "utf8");
  const example = guide.match(/## Install with the verified installer[\s\S]*?```sh\n([\s\S]*?)\n```/);
  assert.ok(example);
  assert.equal(example[1], command.trimEnd());
});

for (const scenario of ["success", "download-failure", "bad-checksum", "missing-installer-checksum", "duplicate-archive", "duplicate-installer"]) {
  test(`installer: ${scenario}`, (t) => {
    const root = mkdtempSync(join(tmpdir(), "cagents-install-test-"));
    t.after(() => rmSync(root, { recursive: true, force: true }));
    const bin = join(root, "bin");
    const fixture = join(root, "release");
    mkdirSync(bin);
    mkdirSync(fixture);
    const installer = `printf '%s\\n' "$*" "$CAGENTS_ARCHIVE" "$CAGENTS_CHECKSUMS" > "$INSTALL_LOG"\n`;
    const archive = "cagents-9.8.7-x86_64-unknown-linux-gnu.tar.gz";
    writeFileSync(join(fixture, "install.sh"), installer);
    writeFileSync(join(fixture, archive), "fixture archive");
    const checksum = (name, bytes) => `${createHash("sha256").update(bytes).digest("hex")}  ${name}\n`;
    const installerChecksum = checksum("install.sh", installer);
    const archiveChecksum = checksum(archive, scenario === "bad-checksum" ? "wrong archive" : "fixture archive");
    const manifest = scenario === "duplicate-archive" ? archiveChecksum.repeat(2)
      : scenario === "duplicate-installer" ? installerChecksum.repeat(2)
      : (scenario === "missing-installer-checksum" ? "" : installerChecksum) + archiveChecksum;
    writeFileSync(join(fixture, "SHA256SUMS"), manifest);
    // Plays github.com: /releases/latest redirects to the newest tag, and
    // /releases/download/TAG/NAME serves that release's file.
    writeFileSync(join(bin, "curl"), `#!/bin/sh
set -eu
printf '%s\\n' "$*" >> "$CALL_LOG"
[ "$SCENARIO" != download-failure ] || exit 22
releases=https://github.com/CarsonBurke/cagents/releases
case "$*" in
  *" $releases/latest")
    printf '%s' "$releases/tag/v9.8.7"
    ;;
  *--remote-name-all*)
    for url do
      case "$url" in
        "$releases/download/v9.8.7/"*) cp "$FIXTURE/\${url##*/}" . ;;
        https://*) exit 22 ;;
      esac
    done
    ;;
  *) exit 2 ;;
esac
`, { mode: 0o700 });
    const installLog = join(root, "installed");
    const callLog = join(root, "calls");
    const result = spawnSync("/bin/sh", [], {
      input: command,
      encoding: "utf8",
      timeout: 5000,
      env: {
        ...process.env,
        PATH: `${bin}:${process.env.PATH}`,
        SCENARIO: scenario,
        FIXTURE: fixture,
        CALL_LOG: callLog,
        INSTALL_LOG: installLog,
      },
    });
    assert.ifError(result.error);
    const calls = readFileSync(callLog, "utf8").trim().split("\n");
    assert.match(calls[0], /--proto =https .*https:\/\/github\.com\/CarsonBurke\/cagents\/releases\/latest$/);
    if (scenario === "success") {
      assert.equal(result.status, 0, result.stderr);
      assert.equal(calls.length, 2);
      assert.match(calls[1], /--proto =https --remote-name-all /);
      for (const name of ["install.sh", "SHA256SUMS", archive]) {
        assert.ok(calls[1].includes(`/releases/download/v9.8.7/${name}`), name);
      }
      assert.match(readFileSync(installLog, "utf8"), /^--version 9\.8\.7\n.*\/cagents-9\.8\.7-.*\n.*\/SHA256SUMS\n$/);
    } else {
      assert.notEqual(result.status, 0);
      assert.throws(() => readFileSync(installLog), { code: "ENOENT" });
      if (scenario === "download-failure") assert.equal(calls.length, 1);
    }
  });
}
