// Fills the page's download placeholders from the latest release: each
// platform's link and wording, the QR code for the Android section, and the
// links main.js uses to offer the visitor's own platform.
import { qrSvg } from "./qr.js";
import { downloadLinks } from "./release.js";

export const SITE = "https://cagents.carsonburke.com/";

const escapeHtml = (text) =>
  text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

// The words for each platform, depending on whether its file is released yet.
const WORDING = {
  linux: {
    direct: {
      button: "Download for Linux",
      action: "Download the AppImage",
      detail: "One AppImage for x86_64. It installs cagents the first time it runs.",
    },
    guide: {
      button: "Install on Linux",
      action: "Install steps",
      detail: "For x86_64, with a verified installer and Debian packages.",
    },
  },
  mac: {
    direct: {
      button: "Download for macOS",
      action: "Download the disk image",
      detail: "For Apple Silicon Macs with macOS 11 or newer.",
    },
    guide: {
      button: "Install on macOS",
      action: "Install steps",
      detail: "For Apple Silicon Macs with macOS 11 or newer.",
    },
  },
  android: {
    direct: {
      button: "Get the Android app",
      action: "Download the APK",
      detail: "Follow your computer's agents and answer them from your phone.",
    },
    guide: {
      button: "Get the Android app",
      action: "About the app",
      detail: "Follow your computer's agents and answer them from your phone.",
    },
  },
};

export function downloadOffers(release) {
  const links = downloadLinks(release);
  return Object.fromEntries(
    Object.entries(links).map(([platform, link]) => [
      platform,
      { ...link, ...WORDING[platform][link.direct ? "direct" : "guide"] },
    ]),
  );
}

// Replaces `%%PLATFORM.FIELD%%`, `<!--download-offers-->` and
// `<!--qr:android-->` in the page.
export function renderDownloads(html, { release }) {
  const offers = downloadOffers(release);
  const json = JSON.stringify(
    Object.fromEntries(Object.entries(offers).map(([platform, { href, button }]) => [platform, { href, button }])),
  ).replace(/</g, "\\u003c");
  return html
    .replace(/%%(linux|mac|android)\.(href|button|action|detail)%%/g, (_, platform, field) =>
      escapeHtml(String(offers[platform][field])),
    )
    .replace("<!--download-offers-->", `<script type="application/json" id="download-offers">${json}</script>`)
    .replace(
      "<!--qr:android-->",
      qrSvg(`${SITE}#android`, { label: "QR code that opens the Android app on cagents.carsonburke.com" }),
    )
    .replace(/%%[^%]*%%|<!--(?:download-offers|qr:[a-z]+)-->/, (left) => {
      throw new Error(`index.html has a download placeholder the build doesn't know: ${left}`);
    });
}
