// Turns `<!--screenshot:NAME-->` markers in index.html into <picture>
// elements, and `<!--install-command-->` into the published install command.
// Every screenshot is a real capture of the native app; screens.json names
// each one's alt text and the themes it was captured in.
import { readFileSync } from "node:fs";

const escapeHtml = (text) =>
  text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

// The pixel size of a WebP, from its VP8, VP8L or VP8X header.
export function webpSize(bytes) {
  const tag = (offset) => bytes.toString("latin1", offset, offset + 4);
  if (tag(0) !== "RIFF" || tag(8) !== "WEBP") throw new Error("not a WebP file");
  const chunk = tag(12);
  if (chunk === "VP8X") {
    return { width: 1 + bytes.readUIntLE(24, 3), height: 1 + bytes.readUIntLE(27, 3) };
  }
  if (chunk === "VP8L") {
    const bits = bytes.readUInt32LE(21);
    return { width: 1 + (bits & 0x3fff), height: 1 + ((bits >> 14) & 0x3fff) };
  }
  if (chunk === "VP8 ") {
    return { width: bytes.readUInt16LE(26) & 0x3fff, height: bytes.readUInt16LE(28) & 0x3fff };
  }
  throw new Error(`unknown WebP chunk ${chunk}`);
}

export function loadScreens(root) {
  const manifest = JSON.parse(readFileSync(new URL("screens.json", root), "utf8"));
  const screens = {};
  for (const [name, screen] of Object.entries(manifest)) {
    const themes = screen.themes ?? ["dark", "light"];
    const files = Object.fromEntries(
      themes.map((theme) => [theme, `src/screens/${name}-${theme}`]),
    );
    const size = webpSize(readFileSync(new URL(`${files[themes[0]]}.webp`, root)));
    for (const theme of themes.slice(1)) {
      const other = webpSize(readFileSync(new URL(`${files[theme]}.webp`, root)));
      if (other.width !== size.width || other.height !== size.height) {
        throw new Error(`${name}: the ${theme} capture is ${other.width}x${other.height}, not ${size.width}x${size.height}`);
      }
    }
    screens[name] = { ...screen, themes, files, ...size };
  }
  return screens;
}

export function pictureHtml(screen, { eager = false } = {}) {
  const [primary, ...rest] = screen.themes;
  const sources = [];
  // The light capture is chosen only when the reader asks for light; dark is
  // the fallback, as it is for the app.
  for (const theme of rest) {
    const media = `(prefers-color-scheme: ${theme})`;
    sources.push(`<source media="${media}" type="image/avif" srcset="/${screen.files[theme]}.avif" />`);
    sources.push(`<source media="${media}" type="image/webp" srcset="/${screen.files[theme]}.webp" />`);
  }
  sources.push(`<source type="image/avif" srcset="/${screen.files[primary]}.avif" />`);
  const loading = eager ? `fetchpriority="high"` : `loading="lazy"`;
  const img = `<img src="/${screen.files[primary]}.webp" width="${screen.width}" height="${screen.height}" alt="${escapeHtml(screen.alt)}" decoding="async" ${loading} />`;
  const caption = screen.caption ? `<figcaption>${escapeHtml(screen.caption)}</figcaption>` : "";
  return `<picture>${sources.join("")}${img}</picture>${caption}`;
}

export function renderPage(html, { screens, installCommand }) {
  const used = new Set();
  const out = html
    .replace(/<!--screenshot:([a-z0-9-]+)(:eager)?-->/g, (_, name, eager) => {
      const screen = screens[name];
      if (!screen) throw new Error(`index.html names screenshot ${name}, which screens.json does not list`);
      used.add(name);
      return pictureHtml(screen, { eager: Boolean(eager) });
    })
    .replace("<!--install-command-->", () => escapeHtml(installCommand.trimEnd()));
  const unused = Object.keys(screens).filter((name) => !used.has(name));
  if (unused.length) throw new Error(`screens.json lists screenshots the page doesn't show: ${unused.join(", ")}`);
  return out;
}
