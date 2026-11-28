import assert from "node:assert/strict";
import { existsSync, readFileSync, statSync } from "node:fs";
import test from "node:test";
import { renderDownloads, SITE, VISITOR_PLATFORMS } from "../lib/downloads.js";
import { decodeQr } from "./qr-decode.js";
import { PLATFORMS } from "../lib/release.js";
import { loadScreens, renderPage, sharedThemes } from "../lib/screens.js";

const root = new URL("../", import.meta.url);
const repository = new URL("../", root);
const screens = loadScreens(root);
const source = readFileSync(new URL("index.html", root), "utf8");
// The page as built when GitHub can't be reached, and once every download is released.
const page = renderDownloads(renderPage(source, { screens }), { release: null });
const released = renderDownloads(renderPage(source, { screens }), {
  release: { assets: Object.values(PLATFORMS).map(({ asset }) => ({ name: asset })) },
});

test("every screenshot has both formats in every theme it lists, at a sensible weight", () => {
  for (const [name, screen] of Object.entries(screens)) {
    assert.ok(screen.alt.length > 20, `${name} needs alt text that describes it`);
    for (const file of Object.values(screen.files)) {
      for (const format of ["avif", "webp"]) {
        const path = new URL(`${file}.${format}`, root);
        assert.ok(existsSync(path), `${file}.${format} is missing`);
        assert.ok(statSync(path).size < 400_000, `${file}.${format} is over 400 kB`);
      }
    }
  }
});

test("images are sized, described and lazy below the fold", () => {
  const images = [...page.matchAll(/<img\b[^>]*>/g)].map(([tag]) => tag);
  assert.ok(images.length > 4);
  for (const tag of images) {
    assert.match(tag, /\balt="/, tag);
    if (tag.includes("/screens/")) {
      assert.match(tag, /\bwidth="\d+" height="\d+"/, tag);
    }
  }
  const shots = images.filter((tag) => tag.includes("/screens/"));
  assert.match(shots[0], /fetchpriority="high"/, "the hero loads first");
  for (const tag of shots.slice(1)) assert.match(tag, /loading="lazy"/, tag);
});

test("each screenshot links directly to its fallback image", () => {
  const previews = [...page.matchAll(/<a class="shot-expand" href="([^"]+)">([\s\S]*?)<\/a>/g)];
  assert.equal(previews.length, Object.keys(screens).length);
  for (const [, path, content] of previews) {
    assert.ok(existsSync(new URL(path.slice(1), root)), `${path} is missing`);
    assert.ok(content.includes(`<img src="${path}"`), `${path} is not the preview's fallback image`);
  }
});

// GitHub's anchor for a Markdown heading.
const slug = (heading) =>
  heading
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, "")
    .replace(/\s/g, "-");

test("in-page links and repository links lead somewhere", () => {
  const ids = new Set([...page.matchAll(/\bid="([^"]+)"/g)].map(([, id]) => id));
  for (const [, target] of page.matchAll(/href="#([^"]*)"/g)) {
    assert.ok(ids.has(target), `#${target} has no target`);
  }
  for (const [, path, anchor] of page.matchAll(/https:\/\/github\.com\/CarsonBurke\/cagents\/blob\/main\/([^"#]+)(?:#([^"]+))?/g)) {
    const file = new URL(path, repository);
    assert.ok(existsSync(file), `${path} is not in the repository`);
    if (anchor) {
      const headings = [...readFileSync(file, "utf8").matchAll(/^#{1,6} (.+)$/gm)].map(([, text]) => slug(text));
      assert.ok(headings.includes(anchor), `${path} has no heading for #${anchor}`);
    }
  }
});

test("install actions lead to the installation guide, never to a command", () => {
  const guide = "https://github.com/CarsonBurke/cagents/blob/main/docs/installation.md";
  for (const html of [page, released]) {
    const installs = [...html.matchAll(/<a class="button primary[^"]*"(?: data-for="([^"]+)")? href="([^"]+)"/g)];
    const generic = installs.filter(([, platforms]) => !platforms || platforms.split(" ").includes("other"));
    assert.ok(generic.length >= 3, "the navigation, hero and closing band each offer Install");
    for (const [, , href] of generic) assert.equal(href, guide);
    for (const [, , href] of installs) {
      assert.match(href, /^(?:https:\/\/github\.com\/CarsonBurke\/cagents\/(?:blob\/main\/(?:docs\/installation|mobile\/README)\.md|releases\/latest\/download\/)|#android$)/, href);
    }
  }
  for (const platform of ["#install-with-the-verified-installer", "#install-on-apple-silicon-macos"]) {
    assert.ok(page.includes(`href="${guide}${platform}"`), `no platform link to ${platform}`);
  }
  assert.doesNotMatch(
    page,
    /curl\s|install\.sh|\bsh -c\b|\b(?:brew|apt|apt-get|dnf|pacman|yay|flatpak|snap) install\b|\bdpkg -i\b|\brpm -i/,
    "the page shows no install command",
  );
  assert.ok(page.includes('href="https://github.com/CarsonBurke/cagents/releases/latest"'));
  const text = page.replace(/<svg[\s\S]*?<\/svg>/g, "").replace(/<[^>]*>/g, " ");
  assert.doesNotMatch(text, /\bv?\d+\.\d+\.\d+\b/, "the page names no release version");
});

test("every screenshot on the page offers the same themes", () => {
  const pictures = [...page.matchAll(/<picture[^>]*>([\s\S]*?)<\/picture>/g)].map(([, content]) =>
    [...content.matchAll(/media="([^"]+)"/g)].map(([, media]) => media).join(","),
  );
  assert.equal(pictures.length, Object.keys(screens).length);
  assert.equal(new Set(pictures).size, 1, `screenshots mix theme sets: ${[...new Set(pictures)].join(" | ")}`);
});

test("the keyboard fact shows Linux shortcuts until the script detects a Mac", () => {
  const keys = [...page.matchAll(/<span class="keys" data-platform="(\w+)"( hidden)?>/g)];
  assert.deepEqual(
    keys.map(([, platform, hidden]) => [platform, Boolean(hidden)]),
    [["linux", false], ["mac", true]],
  );
});

test("headings descend without skipping a level", () => {
  const levels = [...page.matchAll(/<h([1-6])\b/g)].map(([, level]) => Number(level));
  assert.equal(levels[0], 1);
  assert.equal(levels.filter((level) => level === 1).length, 1);
  levels.reduce((previous, level) => {
    assert.ok(level <= previous + 1, `h${level} follows h${previous}`);
    return level;
  });
});

test("the README's screenshots are in the repository", () => {
  const readme = readFileSync(new URL("README.md", repository), "utf8");
  const paths = [
    ...readme.matchAll(/(?:src|srcset)="(website\/[^"]+)"/g),
    ...readme.matchAll(/\]\((website\/[^)]+)\)/g),
  ].map(([, path]) => path);
  assert.ok(paths.length >= 5, "the README shows the hero and its tour");
  for (const path of paths) assert.ok(existsSync(new URL(path, repository)), `${path} is missing`);
});

// Made-up captures with the given themes, without reading any files.
const fakeScreens = (themesByName) =>
  Object.fromEntries(
    Object.entries(themesByName).map(([name, themes]) => [
      name,
      {
        alt: `${name} capture`,
        themes,
        files: Object.fromEntries(themes.map((theme) => [theme, `src/screens/${name}-${theme}`])),
        width: 10,
        height: 10,
      },
    ]),
  );
const fakePage = (names) => names.map((name) => `<figure class="shot"><!--screenshot:${name}--></figure>`).join("");

test("captures that all have a light variant show it in light mode", () => {
  const screens = fakeScreens({ one: ["light", "dark"], two: ["dark", "light"] });
  const html = renderPage(fakePage(["one", "two"]), { screens });
  const pictures = [...html.matchAll(/<picture data-themes="([^"]+)">([\s\S]*?)<\/picture>/g)];
  assert.equal(pictures.length, 2);
  for (const [, themes, content] of pictures) {
    assert.equal(themes, "dark light");
    assert.match(content, /media="\(prefers-color-scheme: light\)"[^>]*srcset="\/src\/screens\/\w+-light\.webp"/);
    assert.match(content, /<img src="\/src\/screens\/\w+-dark\.webp"/, "dark stays the fallback");
  }
});

test("one dark-only capture keeps every capture dark", () => {
  const screens = fakeScreens({ one: ["dark", "light"], two: ["dark"] });
  const html = renderPage(fakePage(["one", "two"]), { screens });
  assert.doesNotMatch(html, /prefers-color-scheme|-light\./);
  assert.equal([...html.matchAll(/<picture data-themes="dark">/g)].length, 2);
});

test("captures that share no theme are an error", () => {
  const screens = fakeScreens({ one: ["dark"], two: ["light"] });
  assert.throws(() => renderPage(fakePage(["one", "two"]), { screens }), /share no theme/);
  assert.deepEqual(sharedThemes(["two"], screens), ["light"]);
});

test("the stylesheet's light-capture switch matches the markup", () => {
  const css = readFileSync(new URL("src/styles.css", root), "utf8");
  assert.ok(css.includes('picture[data-themes~="light"]'), "styles.css no longer keys light surfaces on data-themes");
  const html = renderPage(fakePage(["one"]), { screens: fakeScreens({ one: ["dark", "light"] }) });
  assert.ok(html.includes('<picture data-themes="dark light">'));
});

test("download links fall back to guides until the files are released", () => {
  const assets = /releases\/latest\/download\//;
  assert.doesNotMatch(page, assets, "an unreleased file would be a dead link");
  assert.ok(page.includes(`href="${PLATFORMS.android.fallback}"`));
  for (const { asset } of Object.values(PLATFORMS)) {
    assert.ok(released.includes(`href="https://github.com/CarsonBurke/cagents/releases/latest/download/${asset}"`), asset);
  }
  for (const html of [page, released]) {
    assert.doesNotMatch(html, /%%|<!--(?:platform-script|qr:)/, "every download placeholder is filled");
  }
  assert.match(page, /data-for="linux" href="[^"]+">Install on Linux</);
  assert.match(released, /data-for="linux" href="[^"]+">Download for Linux</);
  // Until the APK exists, the Android visitor reads about the app rather than
  // being promised a download, and sees no note about installing one.
  assert.match(page, /data-for="android" href="#android">About the Android app</);
  assert.doesNotMatch(page, /allow installs/);
  assert.match(released, /allow installs/);
  // Open Anyway applies only to the downloaded disk image.
  assert.doesNotMatch(page, /Open Anyway/);
  assert.match(released, /Open Anyway/);
});

test("the QR code on the page opens the Android section", () => {
  assert.match(page, /<section[^>]* id="android"/);
  const svg = /<svg class="qr"[\s\S]*?<\/svg>/.exec(page)[0];
  assert.match(svg, /role="img" aria-label="[^"]+"/);
  assert.equal(decodeQr(svg), `${SITE}#android`);
});

test("every platform the page shows content for is one the visitor can have", () => {
  const css = readFileSync(new URL("src/styles.css", root), "utf8");
  const named = new Set([...page.matchAll(/data-for="([^"]+)"/g)].flatMap(([, list]) => list.split(" ")));
  for (const platform of named) assert.ok(VISITOR_PLATFORMS.includes(platform), `data-for names ${platform}`);
  for (const platform of VISITOR_PLATFORMS) {
    assert.ok(named.has(platform), `nothing on the page is for ${platform} visitors`);
    assert.ok(css.includes(`:root[data-platform="${platform}"] [data-for]:not([data-for~="${platform}"])`), `styles.css never hides other platforms' content from ${platform} visitors`);
  }
  for (const [, platform] of page.matchAll(/data-download-item="(\w+)"/g)) assert.ok(VISITOR_PLATFORMS.includes(platform));
  // The hero offers each visitor exactly one primary action.
  const hero = /<section class="hero"[\s\S]*?<\/section>/.exec(page)[0];
  for (const platform of VISITOR_PLATFORMS) {
    const shown = [...hero.matchAll(/<a class="button primary" data-for="([^"]+)"/g)].filter(([, list]) => list.split(" ").includes(platform));
    assert.equal(shown.length, platform === "ios" ? 0 : 1, platform);
  }
  assert.match(page, /<head>[\s\S]*<script>\(\(\) => \{\n  const detectPlatform = function detectPlatform[\s\S]*<\/head>/);
});
