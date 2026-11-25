import { defineConfig } from "vite";
import { loadScreens, renderPage } from "./lib/screens.js";

const root = new URL("./", import.meta.url);

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
        handler: (html) => renderPage(html, { screens: loadScreens(root) }),
      },
    },
  ],
});
