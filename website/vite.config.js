import { readFileSync } from "node:fs";
import { defineConfig } from "vite";
import { loadScreens, renderPage } from "./lib/screens.js";

const root = new URL("./", import.meta.url);

export default defineConfig({
  base: "/cagents/",
  build: { assetsInlineLimit: 0 },
  plugins: [
    {
      name: "cagents-page",
      transformIndexHtml: {
        order: "pre",
        handler: (html) =>
          renderPage(html, {
            screens: loadScreens(root),
            installCommand: readFileSync(new URL("src/install-command.sh", root), "utf8"),
          }),
      },
    },
  ],
});
