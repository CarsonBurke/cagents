// Fills the page's download placeholders from the latest release: each
// platform's link and wording, the QR code for the Android section, and the
// inline script that tells the page which platform the visitor is on.
import { detectPlatform } from "../src/platform.js";
import { escapeHtml } from "./html.js";
import { qrSvg } from "./qr.js";
import { downloadLinks } from "./release.js";

export const SITE = "https://cagents.carsonburke.com/";

// The platforms main.js and the head script can name; "other" is everything
// cagents has no build for.
export const VISITOR_PLATFORMS = ["linux", "mac", "android", "ios", "other"];

const macGuide = "https://github.com/CarsonBurke/cagents/blob/main/docs/installation.md#install-on-apple-silicon-macos";

// The words for each platform, depending on whether its file is released yet.
// `note` is trusted HTML shown under the hero button; empty hides it.
const WORDING = {
  linux: {
    direct: {
      button: "Download for Linux",
      action: "Download the AppImage",
      detail: "One AppImage for x86_64. It installs cagents the first time it runs.",
      note: "",
    },
    guide: {
      button: "Install on Linux",
      action: "Install steps",
      detail: "For x86_64, with a verified installer and Debian packages.",
      note: "",
    },
  },
  mac: {
    direct: {
      button: "Download for macOS",
      action: "Download the disk image",
      detail: "For Apple Silicon Macs with macOS 11 or newer and Git 2.36 or newer. On first launch, choose Open Anyway in Privacy & Security.",
      note: `Apple Silicon preview. On first launch, choose Open Anyway in Privacy &amp; Security. <a href="${macGuide}">Install steps</a>`,
    },
    guide: {
      button: "Install on macOS",
      action: "Install steps",
      detail: "For Apple Silicon Macs with macOS 11 or newer and Git 2.36 or newer.",
      note: "A preview for Apple Silicon Macs, installed from Terminal.",
    },
  },
  android: {
    direct: {
      button: "Get the Android app",
      heroHref: null,
      section: "Download for Android",
      action: "Download the APK",
      detail: "Follow your computer's agents and answer them from your phone.",
      note: "Android asks you to allow installs from your browser the first time.",
    },
    guide: {
      button: "About the Android app",
      heroHref: "#android",
      section: "Read about the app",
      action: "About the app",
      detail: "Follow your computer's agents and answer them from your phone.",
      note: "",
    },
  },
};

export function downloadOffers(release) {
  const links = downloadLinks(release);
  return Object.fromEntries(
    Object.entries(links).map(([platform, link]) => {
      const wording = WORDING[platform][link.direct ? "direct" : "guide"];
      return [platform, { ...link, ...wording, heroHref: wording.heroHref ?? link.href }];
    }),
  );
}

// Runs before first paint, so the page shows the visitor's own download
// without swapping labels after load. main.js refines it with the processor
// architecture where the browser reports one.
export function platformScript() {
  return `<script>(() => {
  const detectPlatform = ${detectPlatform.toString()};
  try {
    const nav = navigator;
    document.documentElement.dataset.platform = detectPlatform({
      uaPlatform: nav.userAgentData?.platform ?? "",
      platform: nav.platform ?? "",
      userAgent: nav.userAgent ?? "",
      maxTouchPoints: nav.maxTouchPoints ?? 0,
      coarsePointer: matchMedia("(pointer: coarse)").matches,
    });
  } catch {}
})();</script>`;
}

const FIELDS = /%%(linux|mac|android)\.(href|heroHref|button|section|action|detail|note)%%/g;

// Replaces `%%PLATFORM.FIELD%%`, `<!--platform-script-->` and
// `<!--qr:android-->` in the page.
export function renderDownloads(html, { release }) {
  const offers = downloadOffers(release);
  return html
    .replace(FIELDS, (_, platform, field) => {
      const value = offers[platform][field];
      return field === "note" ? value : escapeHtml(value);
    })
    .replace("<!--platform-script-->", platformScript())
    .replace(
      "<!--qr:android-->",
      qrSvg(`${SITE}#android`, { label: "QR code for cagents.carsonburke.com/#android, the Android app's section" }),
    )
    .replace(/%%[^%\s]*%%|<!--(?:platform-script|qr:[a-z]+)-->/, (left) => {
      throw new Error(`index.html has a download placeholder the build doesn't know: ${left}`);
    });
}
