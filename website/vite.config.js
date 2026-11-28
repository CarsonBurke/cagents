import { defineConfig } from "vite";
import { renderDownloads } from "./lib/downloads.js";
import { fetchLatestRelease } from "./lib/release.js";
import { loadScreens, renderPage } from "./lib/screens.js";

const root = new URL("./", import.meta.url);

// One request per build or dev server; CAGENTS_SITE_OFFLINE=1 skips it and
// links every download to its guide.
let release;
const latestRelease = () => (release ??= process.env.CAGENTS_SITE_OFFLINE ? Promise.resolve(null) : fetchLatestRelease());

export default defineConfig({
  base: "./",
  build: { assetsInlineLimit: 0 },
  html: {
    additionalAssetSources: {
      a: {
        srcAttributes: ["href"],
        filter: ({ attributes }) => attributes.class === "shot-expand",
      },
    },
  },
  plugins: [
    {
      name: "cagents-page",
      transformIndexHtml: {
        order: "pre",
        handler: async (html) =>
          renderDownloads(renderPage(html, { screens: loadScreens(root) }), { release: await latestRelease() }),
      },
    },
  ],
});
