import assert from "node:assert/strict";
import { existsSync, readFileSync, statSync } from "node:fs";
import test from "node:test";
import { loadScreens, renderPage } from "../lib/screens.js";

const root = new URL("../", import.meta.url);
const repository = new URL("../", root);
const screens = loadScreens(root);
const page = renderPage(readFileSync(new URL("index.html", root), "utf8"), {
  screens,
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
  const installs = [...page.matchAll(/<a class="button primary[^"]*" href="([^"]+)">/g)].map(([, href]) => href);
  assert.ok(installs.length >= 3, "the navigation, hero and closing band each offer Install");
  for (const href of installs) assert.equal(href, guide);
  for (const platform of ["#install-with-the-verified-installer", "#install-on-apple-silicon-macos"]) {
    assert.ok(page.includes(`href="${guide}${platform}"`), `no platform link to ${platform}`);
  }
  assert.doesNotMatch(page, /curl\s|install\.sh|\bsh -c\b/, "the page shows no install command");
  assert.ok(page.includes('href="https://github.com/CarsonBurke/cagents/releases/latest"'));
  const text = page.replace(/<svg[\s\S]*?<\/svg>/g, "").replace(/<[^>]*>/g, " ");
  assert.doesNotMatch(text, /\bv?\d+\.\d+\.\d+\b/, "the page names no release version");
});

test("every screenshot on the page offers the same themes", () => {
  const pictures = [...page.matchAll(/<picture>([\s\S]*?)<\/picture>/g)].map(([, content]) =>
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
