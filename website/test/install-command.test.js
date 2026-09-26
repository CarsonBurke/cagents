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

for (const scenario of ["success", "auth-failure", "bad-checksum", "missing-installer-checksum", "duplicate-archive", "duplicate-installer"]) {
  test(`authenticated installer: ${scenario}`, (t) => {
    const root = mkdtempSync(join(tmpdir(), "cagents-install-test-"));
    t.after(() => rmSync(root, { recursive: true, force: true }));
    const bin = join(root, "bin");
    const fixture = join(root, "release");
    mkdirSync(bin);
    mkdirSync(fixture);
    const installer = `printf '%s\\n' "$*" "$CAGENTS_REPOSITORY" "$CAGENTS_ARCHIVE" "$CAGENTS_CHECKSUMS" > "$INSTALL_LOG"\n`;
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
    writeFileSync(join(bin, "gh"), `#!/bin/sh
set -eu
printf '%s\\n' "$*" >> "$CALL_LOG"
case "$1 $2" in
  'auth status') [ "$SCENARIO" != auth-failure ] ;;
  'release view') printf '%s\\n' v9.8.7 ;;
  'release download')
    [ "$3" = v9.8.7 ]
    while [ "$1" != --dir ]; do shift; done
    cp "$FIXTURE/install.sh" "$FIXTURE/SHA256SUMS" "$FIXTURE/${archive}" "$2/"
    ;;
  *) exit 1 ;;
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
    assert.equal(calls[0], "auth status --hostname github.com");
    if (scenario === "success") {
      assert.equal(result.status, 0, result.stderr);
      assert.equal(calls.filter((line) => line.startsWith("release view")).length, 1);
      assert.match(calls[2], /^release download v9\.8\.7 --repo github\.com\/CarsonBurke\/cagents /);
      assert.match(calls[2], /--pattern install\.sh --pattern SHA256SUMS --pattern cagents-9\.8\.7-/);
      assert.match(readFileSync(installLog, "utf8"), /^--version 9\.8\.7\nCarsonBurke\/cagents\n.*\/cagents-9\.8\.7-.*\n.*\/SHA256SUMS\n$/);
    } else {
      assert.notEqual(result.status, 0);
      assert.throws(() => readFileSync(installLog), { code: "ENOENT" });
      if (scenario === "auth-failure") assert.equal(calls.length, 1);
    }
  });
}
