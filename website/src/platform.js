// Which download fits the visitor: "linux", "mac", "android", "ios", or
// "other" for anything cagents has no build for (Windows, Intel Macs, Linux on
// Arm, ChromeOS, or a system the browser doesn't name).
//
// `architecture` is the client hint ("x86" or "arm") when the browser offers
// one. Safari and Firefox don't, and report every Mac as "MacIntel", so a Mac
// of unknown architecture is offered the Apple Silicon build: Apple has sold
// no Intel Mac since 2023, and the page says the build is for Apple Silicon.
//
// The page also runs this function inline in <head>, before first paint, so
// it must stay self-contained.
export function detectPlatform({
  uaPlatform = "",
  platform = "",
  userAgent = "",
  architecture = "",
  maxTouchPoints = 0,
  coarsePointer = false,
} = {}) {
  const name = (uaPlatform || platform).toLowerCase();
  if (name === "android" || /\bandroid\b/i.test(userAgent)) return "android";
  // Android's "desktop site" mode reports an X11 Linux browser on a touch
  // screen with a coarse pointer or an Arm processor.
  if (name.startsWith("linux") && maxTouchPoints > 0 && (coarsePointer || /arm|aarch64/i.test(platform))) {
    return "android";
  }
  if (name === "ios" || /^(iphone|ipad|ipod)/.test(name) || /\b(iphone|ipad|ipod)\b/i.test(userAgent)) return "ios";
  // iPadOS asks for desktop sites as a touch-screen "MacIntel".
  if (name.startsWith("mac") && maxTouchPoints > 1) return "ios";
  if (name.startsWith("mac")) return architecture === "x86" ? "other" : "mac";
  if (name.startsWith("linux") && !/cros/i.test(userAgent)) {
    if (architecture) return architecture === "x86" ? "linux" : "other";
    // navigator.platform names the machine, as in "Linux x86_64" or "Linux aarch64".
    return /arm|aarch64/i.test(platform) ? "other" : "linux";
  }
  return "other";
}

// The same answer from the running browser, asking for the architecture hint
// where the browser supports it.
export async function currentPlatform(nav = navigator) {
  let architecture = "";
  try {
    const hints = await nav.userAgentData?.getHighEntropyValues?.(["architecture"]);
    architecture = hints?.architecture ?? "";
  } catch {
    // Without the hint, the platform string decides.
  }
  return detectPlatform({
    uaPlatform: nav.userAgentData?.platform ?? "",
    platform: nav.platform ?? "",
    userAgent: nav.userAgent ?? "",
    architecture,
    maxTouchPoints: nav.maxTouchPoints ?? 0,
    coarsePointer: globalThis.matchMedia?.("(pointer: coarse)").matches ?? false,
  });
}
