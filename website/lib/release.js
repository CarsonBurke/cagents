// Download links, decided when the site builds. Each platform links straight to
// its release asset only when the latest release has it; otherwise it falls
// back to the guide for that platform, so the page never ships a dead link.
const repository = "https://github.com/CarsonBurke/cagents";
const guide = `${repository}/blob/main/docs/installation.md`;

export const RELEASE_API = "https://api.github.com/repos/CarsonBurke/cagents/releases/latest";

export const PLATFORMS = {
  linux: {
    asset: "cagents-x86_64.AppImage",
    fallback: `${guide}#install-with-the-verified-installer`,
  },
  mac: {
    asset: "Cagents-arm64.dmg",
    fallback: `${guide}#install-on-apple-silicon-macos`,
  },
  android: {
    asset: "cagents-android.apk",
    fallback: `${repository}/blob/main/mobile/README.md`,
  },
};

export const INSTALL_GUIDE = guide;

// { linux: { href, direct }, mac: …, android: … } for a release from the
// GitHub API, or for null when the release could not be read.
export function downloadLinks(release) {
  const names = new Set((release?.assets ?? []).map((asset) => asset?.name));
  return Object.fromEntries(
    Object.entries(PLATFORMS).map(([platform, { asset, fallback }]) => {
      const direct = names.has(asset);
      const href = direct ? `${repository}/releases/latest/download/${asset}` : fallback;
      return [platform, { href, direct }];
    }),
  );
}

// The latest release, or null if GitHub doesn't answer in time. One
// unauthenticated request; a failure never fails the build.
export async function fetchLatestRelease({ fetch = globalThis.fetch, timeout = 8000, log = console.warn } = {}) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(new Error(`no answer within ${timeout} ms`)), timeout);
  try {
    const response = await fetch(RELEASE_API, {
      headers: { Accept: "application/vnd.github+json", "User-Agent": "cagents-website-build" },
      signal: controller.signal,
    });
    if (!response.ok) throw new Error(`GitHub answered ${response.status}`);
    const release = await response.json();
    if (!Array.isArray(release?.assets)) throw new Error("the release lists no assets");
    return release;
  } catch (error) {
    log(`cagents website: linking downloads to the install guide (${error.message})`);
    return null;
  } finally {
    clearTimeout(timer);
  }
}
