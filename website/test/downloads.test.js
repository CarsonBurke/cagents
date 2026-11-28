import assert from "node:assert/strict";
import test from "node:test";
import { decodeQr } from "./qr-decode.js";
import { qrSvg } from "../lib/qr.js";
import { downloadLinks, fetchLatestRelease, PLATFORMS } from "../lib/release.js";
import { detectPlatform } from "../src/platform.js";

const latest = "https://github.com/CarsonBurke/cagents/releases/latest/download/";
// The shape of GitHub's latest-release response, trimmed to what the build reads.
const releaseWith = (...names) => ({
  tag_name: "v0.2.5",
  assets: names.map((name) => ({ name, browser_download_url: `https://example.test/${name}` })),
});

test("a platform links to its asset only when the latest release has it", () => {
  const links = downloadLinks(releaseWith("cagents-x86_64.AppImage", "cagents-linux-x86_64.tar.gz"));
  assert.deepEqual(links.linux, { href: `${latest}cagents-x86_64.AppImage`, direct: true });
  assert.deepEqual(links.mac, { href: PLATFORMS.mac.fallback, direct: false });
  assert.deepEqual(links.android, { href: PLATFORMS.android.fallback, direct: false });
});

test("every platform falls back to its guide when the release can't be read", () => {
  for (const release of [null, undefined, {}, { assets: null }, releaseWith()]) {
    for (const [platform, link] of Object.entries(downloadLinks(release))) {
      assert.deepEqual(link, { href: PLATFORMS[platform].fallback, direct: false });
    }
  }
  assert.match(PLATFORMS.linux.fallback, /installation\.md#install-with-the-verified-installer$/);
  assert.match(PLATFORMS.mac.fallback, /installation\.md#install-on-apple-silicon-macos$/);
  assert.match(PLATFORMS.android.fallback, /mobile\/README\.md$/);
});

test("an asset still uploading is not linked", () => {
  const release = { assets: [{ name: "cagents-x86_64.AppImage", state: "new" }] };
  assert.equal(downloadLinks(release).linux.direct, false);
});

test("the build sends a token only when it has one", async () => {
  const seen = [];
  const fetch = async (_, { headers }) => {
    seen.push(headers.Authorization);
    return { ok: true, status: 200, json: async () => releaseWith() };
  };
  await fetchLatestRelease({ fetch, token: "secret" });
  await fetchLatestRelease({ fetch, token: "" });
  assert.deepEqual(seen, ["Bearer secret", undefined]);
});

test("all three assets link directly once released", () => {
  const links = downloadLinks(releaseWith(...Object.values(PLATFORMS).map(({ asset }) => asset)));
  for (const [platform, { asset }] of Object.entries(PLATFORMS)) {
    assert.deepEqual(links[platform], { href: `${latest}${asset}`, direct: true });
  }
});

test("reading the release never throws", async () => {
  const quiet = () => {};
  const answers = [
    async () => ({ ok: false, status: 403, json: async () => ({ message: "rate limited" }) }),
    async () => {
      throw new TypeError("fetch failed");
    },
    async () => ({ ok: true, status: 200, json: async () => ({ message: "Not Found" }) }),
    async () => ({ ok: true, status: 200, json: async () => JSON.parse("not json") }),
  ];
  for (const fetch of answers) assert.equal(await fetchLatestRelease({ fetch, log: quiet }), null);

  const release = releaseWith("Cagents-arm64.dmg");
  const fetch = async () => ({ ok: true, status: 200, json: async () => release });
  assert.equal(await fetchLatestRelease({ fetch, log: quiet }), release);
});

test("a slow API gives up at the timeout", async () => {
  const fetch = (_, { signal }) =>
    new Promise((_, reject) => signal.addEventListener("abort", () => reject(signal.reason)));
  const started = Date.now();
  assert.equal(await fetchLatestRelease({ fetch, timeout: 50, log: () => {} }), null);
  assert.ok(Date.now() - started < 2000);
});

test("the QR code encodes the Android section's address", () => {
  const url = "https://cagents.carsonburke.com/#android";
  const svg = qrSvg(url, { label: "QR code" });
  assert.equal(decodeQr(svg), url);
  assert.match(svg, /viewBox="0 0 (\d+) \1"/);
});

test("each system is offered the build it can run", () => {
  const cases = [
    [{ uaPlatform: "Linux", platform: "Linux x86_64", architecture: "x86" }, "linux"],
    [{ platform: "Linux x86_64", userAgent: "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0" }, "linux"],
    [{ platform: "Linux aarch64" }, "other"],
    [{ uaPlatform: "Linux", architecture: "arm" }, "other"],
    [{ uaPlatform: "macOS", platform: "MacIntel", architecture: "arm" }, "mac"],
    [{ uaPlatform: "macOS", platform: "MacIntel", architecture: "x86" }, "other"],
    [{ platform: "MacIntel", userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Safari/605.1.15" }, "mac"],
    [{ platform: "MacIntel", maxTouchPoints: 5 }, "ios"],
    [{ platform: "iPhone", userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X)" }, "ios"],
    [{ uaPlatform: "Android", platform: "Linux armv8l", userAgent: "Mozilla/5.0 (Linux; Android 15)" }, "android"],
    [{ platform: "Linux armv8l", userAgent: "Mozilla/5.0 (Linux; Android 14; Pixel 8)" }, "android"],
    [{ uaPlatform: "Windows", platform: "Win32", architecture: "x86" }, "other"],
    [{ uaPlatform: "Chrome OS", platform: "Linux x86_64", userAgent: "Mozilla/5.0 (X11; CrOS x86_64 14541.0.0)" }, "other"],
    [{ platform: "Linux armv8l", userAgent: "Mozilla/5.0 (X11; Linux x86_64)", maxTouchPoints: 5 }, "android"],
    [{ platform: "Linux x86_64", userAgent: "Mozilla/5.0 (X11; Linux x86_64)", maxTouchPoints: 5, coarsePointer: true }, "android"],
    [{ platform: "Linux x86_64", userAgent: "Mozilla/5.0 (X11; Linux x86_64)", maxTouchPoints: 10 }, "linux"],
    [{}, "other"],
  ];
  for (const [visitor, expected] of cases) assert.equal(detectPlatform(visitor), expected, JSON.stringify(visitor));
});
